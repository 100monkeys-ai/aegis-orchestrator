// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # gRPC Client for Cluster Coordination
//!
//! Implements the client side of the `NodeClusterService` protocol (ADR-059).
//! Handles the two-step attestation handshake, SMCP envelope wrapping with
//! Ed25519 signatures, and all authenticated lifecycle RPCs (register,
//! heartbeat, deregister).

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use ed25519_dalek::Signer;
use prost::Message;
use tonic::transport::Channel;

use crate::domain::cluster::NodeId;
use crate::infrastructure::aegis_cluster_proto::{
    node_cluster_service_client::NodeClusterServiceClient, AttestNodeRequest, ChallengeNodeRequest,
    DeregisterNodeInner, DeregisterNodeRequest, NodeCapabilities, NodeCommand, NodeHeartbeatInner,
    NodeHeartbeatRequest, NodeStatus, RegisterNodeInner, RegisterNodeRequest,
    SmcpNodeEnvelope as ProtoEnvelope,
};

/// gRPC client for inter-node cluster communication.
///
/// Wraps the generated `NodeClusterServiceClient` and adds SMCP envelope
/// signing using an Ed25519 keypair. The two-step attestation handshake
/// (`attest_and_challenge`) must complete before any authenticated RPC.
pub struct NodeClusterClient {
    endpoint: String,
    client: Option<NodeClusterServiceClient<Channel>>,
    signing_key: Arc<ed25519_dalek::SigningKey>,
    token: Arc<tokio::sync::RwLock<Option<String>>>,
    node_id: NodeId,
}

impl NodeClusterClient {
    pub fn new(
        endpoint: String,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        node_id: NodeId,
    ) -> Self {
        Self {
            endpoint,
            client: None,
            signing_key,
            token: Arc::new(tokio::sync::RwLock::new(None)),
            node_id,
        }
    }

    /// Establish a gRPC channel to the controller endpoint.
    pub async fn connect(&mut self) -> Result<()> {
        let channel = Channel::from_shared(self.endpoint.clone())
            .context("Invalid endpoint URL")?
            .connect()
            .await
            .context("Failed to connect to cluster controller")?;
        self.client = Some(NodeClusterServiceClient::new(channel));
        Ok(())
    }

    /// Execute the two-step attestation handshake (AttestNode + ChallengeNode).
    ///
    /// On success the returned JWT is stored internally and used to sign all
    /// subsequent authenticated RPCs.
    pub async fn attest_and_challenge(
        &mut self,
        role: i32,
        public_key: Vec<u8>,
        capabilities: NodeCapabilities,
        grpc_address: String,
    ) -> Result<String> {
        let node_id_str = self.node_id.0.to_string();
        let signing_key = self.signing_key.clone();
        let client = self.client_mut()?;

        // Step 1: AttestNode -- present identity, receive challenge nonce
        let attest_resp = client
            .attest_node(tonic::Request::new(AttestNodeRequest {
                node_id: node_id_str.clone(),
                role,
                public_key,
                capabilities: Some(capabilities),
                grpc_address,
            }))
            .await
            .context("AttestNode RPC failed")?
            .into_inner();

        // Step 2: Sign the nonce and send ChallengeNode
        let signature = signing_key.sign(&attest_resp.challenge_nonce);

        let challenge_resp = client
            .challenge_node(tonic::Request::new(ChallengeNodeRequest {
                challenge_id: attest_resp.challenge_id,
                node_id: node_id_str,
                challenge_signature: signature.to_bytes().to_vec(),
            }))
            .await
            .context("ChallengeNode RPC failed")?
            .into_inner();

        let token = challenge_resp.node_security_token;
        *self.token.write().await = Some(token.clone());

        Ok(token)
    }

    /// Register this node's capabilities with the cluster controller.
    ///
    /// Must be called after a successful `attest_and_challenge`.
    pub async fn register(
        &mut self,
        capabilities: NodeCapabilities,
        grpc_address: String,
    ) -> Result<String> {
        let inner = RegisterNodeInner {
            node_id: self.node_id.0.to_string(),
            capabilities: Some(capabilities),
            grpc_address,
        };
        let inner_bytes = inner.encode_to_vec();
        let envelope = self.wrap_in_envelope(&inner_bytes).await?;

        let resp = self
            .client_mut()?
            .register_node(tonic::Request::new(RegisterNodeRequest {
                envelope: Some(envelope),
            }))
            .await
            .context("RegisterNode RPC failed")?
            .into_inner();

        if !resp.accepted {
            bail!("RegisterNode rejected: {}", resp.message);
        }

        Ok(resp.cluster_id)
    }

    /// Send a periodic heartbeat to the controller.
    ///
    /// Returns any pending commands the controller wants this node to execute.
    pub async fn heartbeat(
        &mut self,
        cpu_utilization: f64,
        active_executions: u32,
    ) -> Result<Vec<NodeCommand>> {
        let inner = NodeHeartbeatInner {
            node_id: self.node_id.0.to_string(),
            status: NodeStatus::Active.into(),
            active_executions,
            available_memory_gb: 0,
            cpu_utilization_percent: cpu_utilization as f32,
        };
        let inner_bytes = inner.encode_to_vec();
        let envelope = self.wrap_in_envelope(&inner_bytes).await?;

        let resp = self
            .client_mut()?
            .heartbeat(tonic::Request::new(NodeHeartbeatRequest {
                envelope: Some(envelope),
            }))
            .await
            .context("Heartbeat RPC failed")?
            .into_inner();

        Ok(resp.pending_commands)
    }

    /// Gracefully deregister this node from the cluster.
    pub async fn deregister(&mut self, reason: String) -> Result<bool> {
        let inner = DeregisterNodeInner {
            node_id: self.node_id.0.to_string(),
            reason,
        };
        let inner_bytes = inner.encode_to_vec();
        let envelope = self.wrap_in_envelope(&inner_bytes).await?;

        let resp = self
            .client_mut()?
            .deregister_node(tonic::Request::new(DeregisterNodeRequest {
                envelope: Some(envelope),
            }))
            .await
            .context("DeregisterNode RPC failed")?
            .into_inner();

        Ok(resp.accepted)
    }

    // ── Private helpers ──────────────────────────────────────────────────

    /// Build an `SmcpNodeEnvelope` around an already-serialised inner payload.
    async fn wrap_in_envelope(&self, inner_payload: &[u8]) -> Result<ProtoEnvelope> {
        let token = self.token.read().await;
        let token_str = token
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No security token -- call attest_and_challenge first"))?
            .clone();

        let signature = self.signing_key.sign(inner_payload);

        Ok(ProtoEnvelope {
            node_security_token: token_str,
            signature: signature.to_bytes().to_vec(),
            inner_payload: inner_payload.to_vec(),
        })
    }

    /// Return a mutable reference to the connected client, or error if not connected.
    fn client_mut(&mut self) -> Result<&mut NodeClusterServiceClient<Channel>> {
        self.client
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Not connected -- call connect() first"))
    }
}
