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

    pub(crate) async fn envelope(&self, payload: Vec<u8>) -> Result<SealNodeEnvelope> {
        Ok(crate::infrastructure::cluster::node_envelope::seal(
            &self.signing_key,
            self.token.token().await?,
            payload,
        ))
    }

    pub(crate) async fn client(&self, address: &str) -> Result<NodeClusterServiceClient<Channel>> {
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

impl GrpcPeerTransport {
    /// Subscribe to the relayed-category events the node at `address`
    /// originates (ADR-152 D3).
    pub async fn stream_cluster_events(
        &self,
        address: &str,
        from: NodeProcess,
    ) -> Result<tonic::Streaming<crate::infrastructure::aegis_cluster_proto::ClusterEventEnvelope>>
    {
        let inner = crate::infrastructure::aegis_cluster_proto::StreamClusterEventsInner {
            node_id: from.node_id.0.to_string(),
            incarnation: from.incarnation.0,
        };
        let envelope = self.envelope(inner.encode_to_vec()).await?;
        Ok(self
            .client(address)
            .await?
            .stream_cluster_events(
                crate::infrastructure::aegis_cluster_proto::StreamClusterEventsRequest {
                    envelope: Some(envelope),
                },
            )
            .await
            .with_context(|| format!("StreamClusterEvents {address}"))?
            .into_inner())
    }
}

/// Sends a new execution to a peer with room (ADR-152 D5.3), vouching for
/// the caller inside the SEAL node envelope (D5.5).
pub struct GrpcExecutionForwarder {
    transport: Arc<GrpcPeerTransport>,
    me: NodeId,
}

impl GrpcExecutionForwarder {
    pub fn new(transport: Arc<GrpcPeerTransport>, me: NodeId) -> Self {
        Self { transport, me }
    }
}

#[async_trait::async_trait]
impl crate::application::cluster::ExecutionForwarder for GrpcExecutionForwarder {
    async fn forward(
        &self,
        peer: &crate::domain::cluster_membership::PlacementCandidate,
        start: &crate::application::cluster::ForwardedStart,
    ) -> Result<NodeId> {
        let inner = crate::infrastructure::aegis_cluster_proto::ForwardExecutionInner {
            execution_id: start.execution_id.to_string(),
            agent_id: start.agent_id.to_string(),
            input: serde_json::to_string(&start.input)?,
            tenant_id: start.tenant_id.as_str().to_string(),
            originating_node_id: self.me.0.to_string(),
            security_context_name: start.security_context_name.clone(),
            caller_identity_json: match &start.caller {
                Some(caller) => serde_json::to_string(caller)?,
                None => String::new(),
            },
            vouched_at_unix: chrono::Utc::now().timestamp(),
        };
        let envelope = self.transport.envelope(inner.encode_to_vec()).await?;
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            self.transport
                .client(&peer.grpc_address)
                .await?
                .forward_execution(
                    crate::infrastructure::aegis_cluster_proto::ForwardExecutionRequest {
                        envelope: Some(envelope),
                    },
                )
                .await
                .with_context(|| format!("ForwardExecution {}", peer.grpc_address))
        })
        .await
        .context("the peer did not claim the execution within 5 s")??
        .into_inner();
        NodeId::from_string(&answer.owner_node_id).context("the forward's answer names no owner")
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

/// Runs an edge dispatch on the stream's holder (ADR-152 D8): this member's
/// own Relay Coordinator over the pod network, or the member whose Relay
/// Coordinator holds it, over the members' mutual TLS.
pub struct ClusterEdgeInvoker {
    owners: Arc<dyn crate::infrastructure::edge::EdgeStreamOwners>,
    me: NodeId,
    nodes: Arc<dyn crate::domain::cluster::NodeClusterRepository>,
    peers: Arc<GrpcPeerTransport>,
    /// This member's Relay Coordinator: a transport without TLS (the pod
    /// network) and its endpoint.
    local_relay: Option<(Arc<GrpcPeerTransport>, String)>,
}

impl ClusterEdgeInvoker {
    pub fn new(
        owners: Arc<dyn crate::infrastructure::edge::EdgeStreamOwners>,
        me: NodeId,
        nodes: Arc<dyn crate::domain::cluster::NodeClusterRepository>,
        peers: Arc<GrpcPeerTransport>,
        local_relay: Option<(Arc<GrpcPeerTransport>, String)>,
    ) -> Self {
        Self {
            owners,
            me,
            nodes,
            peers,
            local_relay,
        }
    }
}

impl GrpcPeerTransport {
    /// InvokeEdgeTool at `address` (ADR-152 D8).
    pub async fn invoke_edge_tool(
        &self,
        address: &str,
        req: &crate::application::edge::dispatch_to_edge::DispatchRequest,
    ) -> std::result::Result<crate::infrastructure::aegis_cluster_proto::EdgeResult, tonic::Status>
    {
        let inner = crate::infrastructure::aegis_cluster_proto::InvokeEdgeToolInner {
            edge_node_id: req.node_id.0.to_string(),
            tenant_id: req.tenant_id.as_str().to_string(),
            tool_name: req.tool_name.clone(),
            args: Some(req.args.clone()),
            security_context_name: req.security_context_name.clone(),
            user_seal_envelope: Some(req.user_seal_envelope.clone()),
            deadline: Some(prost_types::Duration {
                seconds: req.deadline.as_secs() as i64,
                nanos: 0,
            }),
        };
        let envelope = self
            .envelope(inner.encode_to_vec())
            .await
            .map_err(|e| tonic::Status::internal(e.to_string()))?;
        let answer = self
            .client(address)
            .await
            .map_err(|e| tonic::Status::unavailable(e.to_string()))?
            .invoke_edge_tool(
                crate::infrastructure::aegis_cluster_proto::InvokeEdgeToolRequest {
                    envelope: Some(envelope),
                },
            )
            .await?
            .into_inner();
        answer
            .result
            .ok_or_else(|| tonic::Status::internal("InvokeEdgeTool answered no result"))
    }
}

fn edge_error(
    edge: NodeId,
    deadline: std::time::Duration,
    status: tonic::Status,
) -> crate::domain::edge::EdgeRouterError {
    use crate::domain::edge::EdgeRouterError;
    match status.code() {
        tonic::Code::FailedPrecondition | tonic::Code::NotFound => {
            EdgeRouterError::EdgeUnavailable { node_id: edge }
        }
        tonic::Code::DeadlineExceeded => EdgeRouterError::Timeout(deadline),
        tonic::Code::PermissionDenied => EdgeRouterError::CrossTenantAccessDenied {
            node_id: edge,
            tenant: String::new(),
        },
        _ => EdgeRouterError::EdgeDisconnected,
    }
}

#[async_trait::async_trait]
impl crate::application::edge::dispatch_to_edge::RemoteEdgeInvoker for ClusterEdgeInvoker {
    async fn invoke(
        &self,
        req: &crate::application::edge::dispatch_to_edge::DispatchRequest,
        from_peer: bool,
    ) -> std::result::Result<
        crate::infrastructure::aegis_cluster_proto::EdgeResult,
        crate::domain::edge::EdgeRouterError,
    > {
        use crate::domain::edge::EdgeRouterError;
        let unavailable = || EdgeRouterError::EdgeUnavailable {
            node_id: req.node_id,
        };
        let owner = self.owners.owner_of(req.node_id).await.map_err(|e| {
            tracing::warn!(error = %e, "edge stream owners unreadable");
            unavailable()
        })?;
        let deadline = req.deadline + std::time::Duration::from_secs(5);
        let call = |transport: Arc<GrpcPeerTransport>, address: String| async move {
            tokio::time::timeout(deadline, transport.invoke_edge_tool(&address, req))
                .await
                .map_err(|_| EdgeRouterError::Timeout(req.deadline))?
                .map_err(|s| edge_error(req.node_id, req.deadline, s))
        };
        match owner {
            None => Err(unavailable()),
            Some(member) if member == self.me => match &self.local_relay {
                Some((transport, endpoint)) => call(transport.clone(), endpoint.clone()).await,
                None => Err(unavailable()),
            },
            Some(_) if from_peer => Err(unavailable()),
            Some(member) => {
                let peer = self
                    .nodes
                    .find_peer(&member)
                    .await
                    .ok()
                    .flatten()
                    .ok_or_else(unavailable)?;
                call(self.peers.clone(), peer.grpc_address).await
            }
        }
    }
}
