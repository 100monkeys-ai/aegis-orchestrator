// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Probe and Fence over gRPC (ADR-152 D1.3, D6)
//!
//! The client side of the membership RPCs. Every request is a
//! `SealNodeEnvelope`: a `NodeSecurityToken` this node issued itself with
//! the cluster's Transit key ([`NodeTokenIssuer`]), and an Ed25519
//! signature over the payload with the node's own key, whose public half it
//! wrote into `cluster_nodes` when it joined. One channel is kept per peer
//! address.

use crate::infrastructure::cluster::cluster_tls::{self, ClusterTls};
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use prost::Message;
use tokio::sync::Mutex;
use tonic::transport::Channel;

use crate::application::cluster::node_token::{NodeTokenIssuer, NODE_TOKEN_TTL};
use crate::application::cluster::peer_prober::{FenceAnswer, PeerTransport, ProbeAnswer};
use crate::domain::cluster::{NodeId, NodeRole, NodeTokenClaims};
use crate::domain::cluster_membership::{MemberState, NodeIncarnation, NodeProcess};
use crate::infrastructure::aegis_cluster_proto::{
    node_cluster_service_client::NodeClusterServiceClient, FenceInner, FenceRequest,
    MemberState as ProtoMemberState, ProbeInner, ProbeRequest, SealNodeEnvelope,
};

/// A token is re-issued when less than this is left of it.
const REISSUE_MARGIN: chrono::Duration = chrono::Duration::minutes(5);

/// The protocol's state, as the domain reads it. An unspecified state is
/// never read as active.
pub fn member_state_from_proto(state: ProtoMemberState) -> Option<MemberState> {
    match state {
        ProtoMemberState::Active => Some(MemberState::Active),
        ProtoMemberState::Draining => Some(MemberState::Draining),
        ProtoMemberState::Fenced => Some(MemberState::Fenced),
        ProtoMemberState::Unspecified => None,
    }
}

pub fn member_state_to_proto(state: MemberState) -> ProtoMemberState {
    match state {
        MemberState::Active => ProtoMemberState::Active,
        MemberState::Draining => ProtoMemberState::Draining,
        MemberState::Fenced => ProtoMemberState::Fenced,
    }
}

/// This node's own `NodeSecurityToken`, issued when first needed and again
/// before it runs out.
pub struct SelfIssuedToken {
    issuer: NodeTokenIssuer,
    node_id: NodeId,
    role: NodeRole,
    capabilities_hash: String,
    current: Mutex<Option<(String, DateTime<Utc>)>>,
}

impl SelfIssuedToken {
    pub fn new(
        issuer: NodeTokenIssuer,
        node_id: NodeId,
        role: NodeRole,
        capabilities_hash: String,
    ) -> Self {
        Self {
            issuer,
            node_id,
            role,
            capabilities_hash,
            current: Mutex::new(None),
        }
    }

    pub async fn token(&self) -> Result<String> {
        let mut current = self.current.lock().await;
        if let Some((token, exp)) = current.as_ref() {
            if *exp - Utc::now() > REISSUE_MARGIN {
                return Ok(token.clone());
            }
        }
        let now = Utc::now();
        let exp = now + NODE_TOKEN_TTL;
        let claims = NodeTokenClaims {
            node_id: self.node_id,
            role: self.role,
            capabilities_hash: self.capabilities_hash.clone(),
            iat: now.timestamp(),
            exp: exp.timestamp(),
            tid: None,
            cep: None,
        };
        let token = self
            .issuer
            .issue(&claims)
            .await
            .context("issuing this node's NodeSecurityToken")?;
        *current = Some((token.clone(), exp));
        Ok(token)
    }
}

/// `Probe` and `Fence` to peers over gRPC.
pub struct GrpcPeerTransport {
    signing_key: Arc<ed25519_dalek::SigningKey>,
    token: Arc<SelfIssuedToken>,
    channels: Mutex<HashMap<String, NodeClusterServiceClient<Channel>>>,
    tls: Option<ClusterTls>,
}

impl GrpcPeerTransport {
    pub fn new(signing_key: Arc<ed25519_dalek::SigningKey>, token: Arc<SelfIssuedToken>) -> Self {
        Self {
            signing_key,
            token,
            channels: Mutex::new(HashMap::new()),
            tls: None,
        }
    }

    /// Dial peers over mutual TLS (`spec.cluster.tls`).
    pub fn with_tls(mut self, tls: Option<ClusterTls>) -> Self {
        self.tls = tls;
        self
    }

    async fn envelope(&self, payload: Vec<u8>) -> Result<SealNodeEnvelope> {
        Ok(crate::infrastructure::cluster::node_envelope::seal(
            &self.signing_key,
            self.token.token().await?,
            payload,
        ))
    }

    async fn client(&self, address: &str) -> Result<NodeClusterServiceClient<Channel>> {
        let mut channels = self.channels.lock().await;
        if let Some(client) = channels.get(address) {
            return Ok(client.clone());
        }
        let channel = cluster_tls::endpoint(address, self.tls.as_ref())?
            .connect_timeout(std::time::Duration::from_secs(1))
            .connect_lazy();
        let client = NodeClusterServiceClient::new(channel);
        channels.insert(address.to_string(), client.clone());
        Ok(client)
    }
}

#[async_trait::async_trait]
impl PeerTransport for GrpcPeerTransport {
    async fn probe(&self, address: &str, from: NodeProcess) -> Result<ProbeAnswer> {
        let inner = ProbeInner {
            node_id: from.node_id.0.to_string(),
            incarnation: from.incarnation.0,
        };
        let envelope = self.envelope(inner.encode_to_vec()).await?;
        let answer = self
            .client(address)
            .await?
            .probe(ProbeRequest {
                envelope: Some(envelope),
            })
            .await
            .with_context(|| format!("Probe {address}"))?
            .into_inner();
        Ok(ProbeAnswer {
            node_id: NodeId::from_string(&answer.node_id)
                .context("the probe answer names no node")?,
            incarnation: NodeIncarnation(answer.incarnation),
            state: member_state_from_proto(answer.state())
                .context("the probe answer carries no state")?,
            last_renewed_at: answer
                .last_renewed_at
                .and_then(|t| DateTime::from_timestamp(t.seconds, t.nanos.max(0) as u32)),
        })
    }

    async fn fence(&self, address: &str, target: NodeProcess, reason: &str) -> Result<FenceAnswer> {
        let inner = FenceInner {
            target_node_id: target.node_id.0.to_string(),
            target_incarnation: target.incarnation.0,
            reason: reason.to_string(),
        };
        let envelope = self.envelope(inner.encode_to_vec()).await?;
        let answer = self
            .client(address)
            .await?
            .fence(FenceRequest {
                envelope: Some(envelope),
            })
            .await
            .with_context(|| format!("Fence {address}"))?
            .into_inner();
        Ok(FenceAnswer {
            fenced: answer.fenced,
            incarnation: NodeIncarnation(answer.incarnation),
        })
    }
}

impl GrpcPeerTransport {
    /// Ask the node at `address` to cancel an execution it owns (ADR-152
    /// D5.4). `Ok(true)` when it accepted.
    pub async fn cancel_execution(
        &self,
        address: &str,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &crate::domain::tenant::TenantId,
    ) -> Result<bool> {
        let inner = crate::infrastructure::aegis_cluster_proto::CancelExecutionInner {
            execution_id: execution_id.0.to_string(),
            tenant_id: tenant_id.as_str().to_string(),
        };
        let envelope = self.envelope(inner.encode_to_vec()).await?;
        let answer = self
            .client(address)
            .await?
            .cancel_execution(
                crate::infrastructure::aegis_cluster_proto::CancelExecutionRequest {
                    envelope: Some(envelope),
                },
            )
            .await
            .with_context(|| format!("CancelExecution {address}"))?
            .into_inner();
        Ok(answer.accepted)
    }
}

/// Forwards a cancel to the owner at the cluster address it registered
/// (`cluster_nodes.grpc_address`), over the peer transport's mutual TLS and
/// SEAL node envelope (ADR-152 D5.4).
pub struct GrpcCancelForwarder {
    transport: Arc<GrpcPeerTransport>,
    nodes: Arc<dyn crate::domain::cluster::NodeClusterRepository>,
}

impl GrpcCancelForwarder {
    pub fn new(
        transport: Arc<GrpcPeerTransport>,
        nodes: Arc<dyn crate::domain::cluster::NodeClusterRepository>,
    ) -> Self {
        Self { transport, nodes }
    }
}

#[async_trait::async_trait]
impl crate::application::cluster::CancelForwarder for GrpcCancelForwarder {
    async fn forward(
        &self,
        owner: NodeId,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &crate::domain::tenant::TenantId,
    ) -> Result<bool> {
        let peer = self
            .nodes
            .find_peer(&owner)
            .await?
            .with_context(|| format!("the owner {owner} is not a registered node"))?;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.transport
                .cancel_execution(&peer.grpc_address, execution_id, tenant_id),
        )
        .await
        .context("the owner did not answer the cancel within 2 s")?
    }
}
