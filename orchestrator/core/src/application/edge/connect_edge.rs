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
use crate::infrastructure::cluster::node_envelope::{EdgeDaemonKeys, NodeEnvelopeVerifier};
use crate::infrastructure::edge::EdgeConnectionRegistry;

pub struct ConnectEdgeService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    registry: EdgeConnectionRegistry,
    envelope_verifier: Arc<NodeEnvelopeVerifier>,
}

impl ConnectEdgeService {
    pub fn new(
        edge_repo: Arc<dyn EdgeDaemonRepository>,
        registry: EdgeConnectionRegistry,
        envelope_verifier: Arc<NodeEnvelopeVerifier>,
    ) -> Self {
        Self {
            edge_repo,
            registry,
            envelope_verifier,
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

        // The stream belongs to the edge whose envelope opens: token signed
        // by the cluster's Transit key, Hello signed by the key that edge
        // enrolled, fresh and not replayed.
        let token = self
            .envelope_verifier
            .open(&envelope, &EdgeDaemonKeys(self.edge_repo.clone()))
            .await
            .map_err(|e| anyhow!("Hello refused: {e}"))?;
        let node_id = token.node_id;
        let edge = self
            .edge_repo
            .get(&node_id)
            .await?
            .ok_or_else(|| anyhow!("unknown edge {node_id}"))?;
        if token
            .tenant_id
            .as_deref()
            .is_some_and(|tid| tid != edge.tenant_id.as_str())
        {
            return Err(anyhow!(
                "Hello refused: the token binds edge {node_id} to another tenant"
            ));
        }
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
                // Persist `last_heartbeat_at = NOW()` and `status = active` in
                // a single UPDATE so the operator UX can render an accurate
                // "last seen" timestamp. Previously the handler only touched
                // `status`, leaving `last_heartbeat_at` permanently null on
                // every connected daemon.
                let _ = self.edge_repo.record_heartbeat(&node_id).await;
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
        Ok(())
    }
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

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Regression: ADR-117 §B — `Heartbeat` event handling.
    //!
    //! Bug: `handle_event::Heartbeat` only updated `status` and never touched
    //! `last_heartbeat_at`, so `edge_daemons.last_heartbeat_at` stayed `null`
    //! forever even when the bidi stream was open and heartbeats were
    //! arriving. Zaru's UI rendered "Last seen = —" and the wifi-cross icon
    //! for every connected daemon. The fix swaps the call to
    //! `record_heartbeat`, which stamps `NOW()` AND forces `status = active`
    //! in a single UPDATE.
    use super::*;
    use crate::domain::edge::{
        EdgeCapabilities, EdgeConnectionState, EdgeDaemon, EdgeDaemonRepository,
    };
    use crate::domain::shared_kernel::TenantId;
    use crate::infrastructure::aegis_cluster_proto::{
        edge_event::Event as InboundEvent, EdgeEvent, HeartbeatEvent,
    };
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    /// In-memory `EdgeDaemonRepository` that records every `record_heartbeat`
    /// call, including a stamped timestamp, so the test can assert the bug
    /// is gone.
    struct RecordingRepo {
        edges: Mutex<HashMap<NodeId, EdgeDaemon>>,
        heartbeats: Mutex<u32>,
    }

    impl RecordingRepo {
        fn new(seed: EdgeDaemon) -> Self {
            let mut m = HashMap::new();
            m.insert(seed.node_id, seed);
            Self {
                edges: Mutex::new(m),
                heartbeats: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl EdgeDaemonRepository for RecordingRepo {
        async fn upsert(&self, edge: &EdgeDaemon) -> anyhow::Result<()> {
            self.edges.lock().await.insert(edge.node_id, edge.clone());
            Ok(())
        }
        async fn get(&self, node_id: &NodeId) -> anyhow::Result<Option<EdgeDaemon>> {
            Ok(self.edges.lock().await.get(node_id).cloned())
        }
        async fn list_by_tenant(&self, _: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>> {
            Ok(vec![])
        }
        async fn list_all(&self) -> anyhow::Result<Vec<EdgeDaemon>> {
            Ok(vec![])
        }
        async fn update_status(&self, id: &NodeId, status: NodePeerStatus) -> anyhow::Result<()> {
            if let Some(e) = self.edges.lock().await.get_mut(id) {
                e.status = status;
            }
            Ok(())
        }
        async fn record_heartbeat(&self, id: &NodeId) -> anyhow::Result<()> {
            *self.heartbeats.lock().await += 1;
            if let Some(e) = self.edges.lock().await.get_mut(id) {
                e.last_heartbeat_at = Some(chrono::Utc::now());
                e.status = NodePeerStatus::Active;
            }
            Ok(())
        }
        async fn update_tags(&self, _: &NodeId, _: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn update_display_name(&self, _: &NodeId, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn update_capabilities(
            &self,
            _: &NodeId,
            _: &EdgeCapabilities,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &NodeId) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn seed_edge(node_id: NodeId) -> EdgeDaemon {
        EdgeDaemon {
            node_id,
            tenant_id: TenantId::new("t-test").unwrap(),
            public_key: vec![0u8; 32],
            capabilities: EdgeCapabilities::default(),
            // Start `unhealthy` so the test can also pin that an inbound
            // heartbeat snaps the row back to `active`.
            status: NodePeerStatus::Unhealthy,
            connection: EdgeConnectionState::Disconnected {
                since: chrono::Utc::now(),
            },
            last_heartbeat_at: None,
            enrolled_at: chrono::Utc::now(),
            display_name: "test-host".to_string(),
        }
    }

    /// Regression: an inbound `Heartbeat` event MUST persist
    /// `last_heartbeat_at` and reset `status` to `active`. Before this fix
    /// the handler only invoked `update_status`, leaving
    /// `last_heartbeat_at` permanently null and Zaru's "Last seen" cell
    /// stuck on `—`.
    #[tokio::test]
    async fn heartbeat_event_persists_last_seen_and_marks_active() {
        let node_id = NodeId::new();
        let repo = Arc::new(RecordingRepo::new(seed_edge(node_id)));
        let svc = ConnectEdgeService::new(
            repo.clone(),
            EdgeConnectionRegistry::new(),
            Arc::new(NodeEnvelopeVerifier::new(
                Arc::new(crate::infrastructure::secrets_manager::TestSecretStore::new()),
                "unused".to_string(),
            )),
        );

        let ev = EdgeEvent {
            event: Some(InboundEvent::Heartbeat(HeartbeatEvent {
                envelope: None,
                active_invocations: 0,
            })),
        };
        svc.handle_event(node_id, ev)
            .await
            .expect("heartbeat handler must not error");

        // record_heartbeat called exactly once.
        assert_eq!(*repo.heartbeats.lock().await, 1);

        let stored = repo.get(&node_id).await.unwrap().unwrap();
        assert!(
            stored.last_heartbeat_at.is_some(),
            "last_heartbeat_at must be stamped after a Heartbeat event \
             — Zaru renders this as the host's 'Last seen' timestamp"
        );
        assert_eq!(
            stored.status,
            NodePeerStatus::Active,
            "an inbound heartbeat must reset status to Active so a daemon \
             that briefly went unhealthy recovers without manual intervention"
        );
    }
}
