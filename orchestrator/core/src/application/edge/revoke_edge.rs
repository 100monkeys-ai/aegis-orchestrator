// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C — operator-initiated edge revocation.

use std::sync::Arc;

use crate::domain::edge::EdgeDaemonRepository;
use crate::domain::shared_kernel::{NodeId, TenantId};
use crate::infrastructure::edge::EdgeConnectionRegistry;

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum RevokeEdgeError {
    #[error("edge daemon not found")]
    NotFound,
    #[error("repository error: {0}")]
    Repo(String),
}

pub struct RevokeEdgeService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    registry: EdgeConnectionRegistry,
}

impl RevokeEdgeService {
    pub fn new(edge_repo: Arc<dyn EdgeDaemonRepository>, registry: EdgeConnectionRegistry) -> Self {
        Self {
            edge_repo,
            registry,
        }
    }

    /// Revoke an edge daemon's enrollment.
    ///
    /// ADR-117 §C contract:
    ///
    /// 1. Verify the row belongs to the caller's tenant (cross-tenant
    ///    revoke must 404 — never silently mutate another tenant's host).
    /// 2. **Hard-delete** the `edge_daemons` row. Marking it `Unhealthy`
    ///    is not enough: `list_by_tenant` has no status filter, so any
    ///    non-deleted row would still appear in Zaru's host list, leaving
    ///    the operator unable to actually remove a host. Deletion also
    ///    causes the next `ConnectEdge` handshake from the same daemon
    ///    to fail with `unknown edge` (see
    ///    [`crate::application::edge::connect_edge::ConnectEdgeService::handle_stream`]),
    ///    so the now-stale `NodeSecurityToken` cannot be used to
    ///    re-attach.
    /// 3. **Evict the live `ConnectEdge` stream** from
    ///    [`EdgeConnectionRegistry`]. Removing the sender closes the
    ///    `mpsc` channel, which terminates the `ReceiverStream` the
    ///    gRPC handler is consuming — the daemon observes its bidi
    ///    stream being closed by the server and stops trying to use
    ///    the now-invalid `NodeSecurityToken`. Any pending oneshots
    ///    tagged with this node are failed with `EdgeDisconnected` so
    ///    in-flight callers terminate immediately rather than block
    ///    until their per-target deadline.
    pub async fn revoke(&self, tenant: &TenantId, node_id: NodeId) -> Result<(), RevokeEdgeError> {
        let edge = self
            .edge_repo
            .get(&node_id)
            .await
            .map_err(|e| RevokeEdgeError::Repo(e.to_string()))?
            .ok_or(RevokeEdgeError::NotFound)?;
        if &edge.tenant_id != tenant {
            // Tenant isolation: a cross-tenant revoke MUST surface as
            // `NotFound` (mapped to HTTP 404) so the foreign tenant
            // cannot probe for the existence of another tenant's host.
            // See ADR-083 §4.5–§4.8 and security audit 002 findings
            // 4.33 / 4.37.7. The PATCH path on the same resource enforces
            // the same contract via an inline tenant gate.
            return Err(RevokeEdgeError::NotFound);
        }
        self.edge_repo
            .delete(&node_id)
            .await
            .map_err(|e| RevokeEdgeError::Repo(e.to_string()))?;
        self.registry.evict(&node_id);
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Regression suite for the Zaru "Revoke" action on `/vault/edge-hosts`.
    //!
    //! Bug: clicking Revoke surfaced a 204 from the orchestrator, but the
    //! host stayed in the list and the daemon kept its bidi `ConnectEdge`
    //! stream open. Root cause was in this service:
    //!
    //!   - It updated `status = Unhealthy` instead of deleting the row.
    //!     `list_by_tenant` has no status filter, so the host kept
    //!     appearing in Zaru's list — the user perceived the action as a
    //!     no-op.
    //!   - It only failed pending oneshots; the live `ConnectEdge` stream
    //!     was never evicted from `EdgeConnectionRegistry`, so the daemon
    //!     could continue dispatching commands using its now-revoked
    //!     `NodeSecurityToken` until its next reconnect.
    //!
    //! The tests below pin the new contract: revoke deletes the row AND
    //! evicts the live stream sender so the daemon's gRPC stream drops.

    use super::*;
    use crate::domain::cluster::NodePeerStatus;
    use crate::domain::edge::{EdgeCapabilities, EdgeConnectionState, EdgeDaemon};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    struct InMemoryRepo {
        edges: Mutex<HashMap<NodeId, EdgeDaemon>>,
    }

    impl InMemoryRepo {
        fn new() -> Self {
            Self {
                edges: Mutex::new(HashMap::new()),
            }
        }
        async fn seed(&self, edge: EdgeDaemon) {
            self.edges.lock().await.insert(edge.node_id, edge);
        }
    }

    #[async_trait]
    impl EdgeDaemonRepository for InMemoryRepo {
        async fn upsert(&self, edge: &EdgeDaemon) -> anyhow::Result<()> {
            self.edges.lock().await.insert(edge.node_id, edge.clone());
            Ok(())
        }
        async fn get(&self, node_id: &NodeId) -> anyhow::Result<Option<EdgeDaemon>> {
            Ok(self.edges.lock().await.get(node_id).cloned())
        }
        async fn list_by_tenant(&self, tenant: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>> {
            Ok(self
                .edges
                .lock()
                .await
                .values()
                .filter(|e| &e.tenant_id == tenant)
                .cloned()
                .collect())
        }
        async fn update_status(&self, id: &NodeId, status: NodePeerStatus) -> anyhow::Result<()> {
            if let Some(e) = self.edges.lock().await.get_mut(id) {
                e.status = status;
            }
            Ok(())
        }
        async fn record_heartbeat(&self, _: &NodeId) -> anyhow::Result<()> {
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
        async fn delete(&self, id: &NodeId) -> anyhow::Result<()> {
            self.edges.lock().await.remove(id);
            Ok(())
        }
    }

    fn seed_edge(node_id: NodeId, tenant: &TenantId) -> EdgeDaemon {
        EdgeDaemon {
            node_id,
            tenant_id: tenant.clone(),
            public_key: vec![0u8; 32],
            capabilities: EdgeCapabilities::default(),
            status: NodePeerStatus::Active,
            connection: EdgeConnectionState::Disconnected {
                since: chrono::Utc::now(),
            },
            last_heartbeat_at: None,
            enrolled_at: chrono::Utc::now(),
            display_name: "test-host".into(),
        }
    }

    /// Regression: Zaru's "Revoke" button must remove the host from the
    /// `list_by_tenant` projection. Before this fix the service set
    /// `status = Unhealthy` but `list_by_tenant` has no status filter, so
    /// the host stayed in the UI and the operator perceived Revoke as a
    /// no-op.
    #[tokio::test]
    async fn revoke_deletes_row_and_removes_from_list_by_tenant() {
        let tenant = TenantId::new("t-consumer").unwrap();
        let nid = NodeId::new();
        let repo = Arc::new(InMemoryRepo::new());
        repo.seed(seed_edge(nid, &tenant)).await;
        let registry = EdgeConnectionRegistry::new();
        let svc = RevokeEdgeService::new(repo.clone(), registry);

        svc.revoke(&tenant, nid).await.expect("revoke must succeed");

        assert!(
            repo.get(&nid).await.unwrap().is_none(),
            "revoke MUST hard-delete the edge_daemons row"
        );
        assert!(
            repo.list_by_tenant(&tenant).await.unwrap().is_empty(),
            "revoked host MUST disappear from list_by_tenant — \
             status=Unhealthy is not enough because the projection has no status filter"
        );
    }

    /// Regression: revoke must evict the live `ConnectEdge` stream so the
    /// daemon's bidi channel drops immediately. Before this fix only the
    /// pending oneshots were failed; the registered sender stayed in
    /// `EdgeConnectionRegistry`, so the daemon kept its gRPC stream open
    /// and could continue using the now-invalid `NodeSecurityToken` until
    /// its next reconnect attempt.
    #[tokio::test]
    async fn revoke_evicts_live_connect_edge_stream() {
        use crate::infrastructure::aegis_cluster_proto::EdgeCommand;
        use tokio::sync::mpsc;

        let tenant = TenantId::new("t-consumer").unwrap();
        let nid = NodeId::new();
        let repo = Arc::new(InMemoryRepo::new());
        repo.seed(seed_edge(nid, &tenant)).await;

        let registry = EdgeConnectionRegistry::new();
        let (tx, mut rx) = mpsc::channel::<EdgeCommand>(4);
        // Simulate the gRPC handler having registered its outbound sender.
        // We deliberately leak the guard so it does not auto-deregister
        // before revoke runs — production parity, where the guard lives
        // for the lifetime of the bidi stream task.
        let guard = registry.register(nid, tx);
        std::mem::forget(guard);
        assert!(
            registry.is_connected(&nid),
            "precondition: live stream must be registered"
        );

        let svc = RevokeEdgeService::new(repo.clone(), registry.clone());
        svc.revoke(&tenant, nid).await.expect("revoke must succeed");

        assert!(
            !registry.is_connected(&nid),
            "revoke MUST evict the registered ConnectEdge sender so the \
             daemon's bidi stream drops (ADR-117 §C)"
        );
        // The mpsc receiver must observe the channel being closed once
        // the only sender has been dropped from the registry. `recv()`
        // returning `None` is the canonical signal — the gRPC handler's
        // `ReceiverStream` translates that into stream termination, which
        // the daemon sees as the server closing its end of the bidi.
        assert!(
            rx.recv().await.is_none(),
            "evicting the sender MUST close the mpsc channel so the \
             gRPC ReceiverStream terminates and the daemon observes its \
             bidi stream being dropped"
        );
    }

    /// Regression: cross-tenant revoke MUST surface as `NotFound`
    /// (HTTP 404) without mutating the foreign tenant's row. Returning
    /// `Forbidden` (HTTP 403) leaks resource existence to the wrong
    /// tenant — see ADR-083 §4.5–§4.8 and security audit 002 findings
    /// 4.33 / 4.37.7. Same tenant-isolation contract as PATCH.
    #[tokio::test]
    async fn revoke_cross_tenant_returns_not_found_and_keeps_row() {
        let tenant_a = TenantId::new("t-a").unwrap();
        let tenant_b = TenantId::new("t-b").unwrap();
        let nid = NodeId::new();
        let repo = Arc::new(InMemoryRepo::new());
        repo.seed(seed_edge(nid, &tenant_a)).await;
        let svc = RevokeEdgeService::new(repo.clone(), EdgeConnectionRegistry::new());

        let err = svc
            .revoke(&tenant_b, nid)
            .await
            .expect_err("cross-tenant revoke must fail");
        assert_eq!(err, RevokeEdgeError::NotFound);
        assert!(
            repo.get(&nid).await.unwrap().is_some(),
            "cross-tenant revoke MUST NOT delete the foreign tenant's row"
        );
    }

    /// Regression: revoking a host that was never connected (no live
    /// stream registered) must still succeed and delete the row. The
    /// service must not require the daemon to be online to revoke its
    /// enrollment.
    #[tokio::test]
    async fn revoke_succeeds_when_daemon_not_connected() {
        let tenant = TenantId::new("t-consumer").unwrap();
        let nid = NodeId::new();
        let repo = Arc::new(InMemoryRepo::new());
        repo.seed(seed_edge(nid, &tenant)).await;
        let registry = EdgeConnectionRegistry::new();
        let svc = RevokeEdgeService::new(repo.clone(), registry);

        svc.revoke(&tenant, nid)
            .await
            .expect("revoke must succeed even with no live stream");
        assert!(repo.get(&nid).await.unwrap().is_none());
    }
}
