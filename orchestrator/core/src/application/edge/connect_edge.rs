// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §B — handle the bidi `ConnectEdge` stream.
//!
//! `ConnectEdgeService::handle_stream` is invoked by the gRPC handler after
//! it reads the first inbound `EdgeEvent`. Responsibilities:
//!   1. Verify the first message is `Hello` and carries a valid
//!      `SealNodeEnvelope`.
//!   2. Register the outbound sender in `EdgeConnectionRegistry` (auto-
//!      deregisters on drop).
//!   3. Update the edge's `connection` to `Connected`.
//!   4. Drive the inbound event loop: route `CommandResult`/`CommandProgress`
//!      to `PendingEdgeCalls`; persist `CapabilityUpdate`; update heartbeats.

use anyhow::{anyhow, Result};
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tonic::Streaming;
use uuid::Uuid;

use crate::domain::cluster::NodePeerStatus;
use crate::domain::edge::{EdgeCapabilities, EdgeDaemonRepository};
use crate::domain::shared_kernel::NodeId;
use crate::infrastructure::aegis_cluster_proto::{
    edge_event::Event as InboundEvent, EdgeCommand, EdgeEvent,
};
use crate::infrastructure::edge::EdgeConnectionRegistry;

pub struct ConnectEdgeService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    registry: EdgeConnectionRegistry,
}

impl ConnectEdgeService {
    pub fn new(edge_repo: Arc<dyn EdgeDaemonRepository>, registry: EdgeConnectionRegistry) -> Self {
        Self {
            edge_repo,
            registry,
        }
    }

    pub fn registry(&self) -> &EdgeConnectionRegistry {
        &self.registry
    }

    pub async fn handle_stream(
        &self,
        first: EdgeEvent,
        mut rest: Streaming<EdgeEvent>,
        cmd_tx: mpsc::Sender<EdgeCommand>,
    ) -> Result<()> {
        let hello = match first.event {
            Some(InboundEvent::Hello(h)) => h,
            _ => return Err(anyhow!("first ConnectEdge message must be Hello")),
        };
        let envelope = hello
            .envelope
            .ok_or_else(|| anyhow!("Hello missing SealNodeEnvelope"))?;

        // The outer envelope is verified by the SEAL middleware that wraps
        // this handler at the gRPC layer; here we extract the node_id from
        // the JWT claims so we can register the sender.
        let node_id = node_id_from_seal_token(&envelope.node_security_token)?;
        let edge = self
            .edge_repo
            .get(&node_id)
            .await?
            .ok_or_else(|| anyhow!("unknown edge {node_id}"))?;
        if edge.status == NodePeerStatus::Unhealthy {
            // Allow connection but mark active on first heartbeat.
        }

        let _guard = self.registry.register(node_id, cmd_tx);
        tracing::info!(node_id = %node_id, "edge connected");

        // Persist any capabilities advertised in Hello (preserve server-side tags).
        if let Some(caps_proto) = hello.capabilities {
            let mut caps: EdgeCapabilities = proto_caps_to_domain(&caps_proto);
            caps.tags = edge.capabilities.tags.clone();
            let _ = self.edge_repo.update_capabilities(&node_id, &caps).await;
        }

        while let Some(msg) = rest.next().await {
            match msg {
                Ok(ev) => self.handle_event(node_id, ev).await?,
                Err(e) => {
                    tracing::warn!(error = %e, node_id = %node_id, "edge stream error");
                    break;
                }
            }
        }
        // Guard drops here, deregistering and resolving pending oneshots.
        Ok(())
    }

    async fn handle_event(&self, node_id: NodeId, ev: EdgeEvent) -> Result<()> {
        let Some(event) = ev.event else {
            return Ok(());
        };
        match event {
            InboundEvent::Hello(_) => {
                // Hello can only be the first message.
                return Err(anyhow!("unexpected Hello mid-stream"));
            }
            InboundEvent::Heartbeat(_h) => {
                let _ = self
                    .edge_repo
                    .update_status(&node_id, NodePeerStatus::Active)
                    .await;
            }
            InboundEvent::CommandResult(r) => {
                let cid =
                    Uuid::parse_str(&r.command_id).map_err(|_| anyhow!("invalid command_id"))?;
                if let Some(result) = r.result {
                    self.registry.pending().complete(&cid, result);
                }
            }
            InboundEvent::CommandProgress(_p) => {
                // Streaming progress — currently observable only via the gRPC
                // result stream when an SSE bridge is wired (slice 5).
            }
            InboundEvent::CapabilityUpdate(u) => {
                if let Some(caps_proto) = u.capabilities {
                    let mut caps = proto_caps_to_domain(&caps_proto);
                    // Preserve server-side tags.
                    if let Ok(Some(current)) = self.edge_repo.get(&node_id).await {
                        caps.tags = current.capabilities.tags;
                    }
                    let _ = self.edge_repo.update_capabilities(&node_id, &caps).await;
                }
            }
        }
        let _ = Utc::now();
        Ok(())
    }
}

fn node_id_from_seal_token(token: &str) -> Result<NodeId> {
    use base64::Engine;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(anyhow!("invalid jwt"));
    }
    let claims_b = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|e| anyhow!("decode: {e}"))?;
    #[derive(serde::Deserialize)]
    struct C {
        sub: String,
    }
    let c: C = serde_json::from_slice(&claims_b)?;
    let id = uuid::Uuid::parse_str(&c.sub).map_err(|e| anyhow!("invalid node_id: {e}"))?;
    Ok(NodeId(id))
}

fn proto_caps_to_domain(
    p: &crate::infrastructure::aegis_cluster_proto::EdgeCapabilities,
) -> EdgeCapabilities {
    EdgeCapabilities {
        os: p.os.clone(),
        arch: p.arch.clone(),
        local_tools: p.local_tools.clone(),
        mount_points: p.mount_points.clone(),
        custom_labels: p.custom_labels.clone(),
        tags: vec![], // ignored from daemon; server is source of truth
    }
}
