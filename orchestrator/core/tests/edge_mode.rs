// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 integration tests.
//!
//! These tests exercise the in-process composition of the edge mode services
//! against an `EdgeConnectionRegistry` and a stub `EdgeDaemonRepository`. The
//! end-to-end gRPC round-trip (in-process tonic server over `tokio::io::duplex`)
//! is intentionally scoped narrowly because the `aegis_orchestrator_proto`
//! generated client/server pair fully exercises tonic surfaces.

use std::collections::HashMap;
use std::sync::Arc;

use aegis_orchestrator_core::application::edge::manage_groups::ManageGroupsService;
use aegis_orchestrator_core::application::edge::manage_tags::{ManageTagsError, ManageTagsService};
use aegis_orchestrator_core::application::edge::revoke_edge::{RevokeEdgeError, RevokeEdgeService};
use aegis_orchestrator_core::domain::cluster::NodePeerStatus;
use aegis_orchestrator_core::domain::edge::{
    EdgeCapabilities, EdgeConnectionState, EdgeDaemon, EdgeDaemonRepository, EdgeGroup,
    EdgeGroupId, EdgeGroupRepoError, EdgeGroupRepository, EdgeSelector, LabelMatch, TagMatch,
};
use aegis_orchestrator_core::domain::shared_kernel::{NodeId, TenantId};
use aegis_orchestrator_core::infrastructure::edge::EdgeConnectionRegistry;
use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Mutex;

#[derive(Default)]
struct StubRepo {
    edges: Mutex<HashMap<NodeId, EdgeDaemon>>,
}

#[async_trait]
impl EdgeDaemonRepository for StubRepo {
    async fn upsert(&self, edge: &EdgeDaemon) -> anyhow::Result<()> {
        self.edges.lock().await.insert(edge.node_id, edge.clone());
        Ok(())
    }
    async fn get(&self, node_id: &NodeId) -> anyhow::Result<Option<EdgeDaemon>> {
        Ok(self.edges.lock().await.get(node_id).cloned())
    }
    async fn list_by_tenant(&self, tenant_id: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>> {
        Ok(self
            .edges
            .lock()
            .await
            .values()
            .filter(|e| &e.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    async fn update_status(&self, node_id: &NodeId, status: NodePeerStatus) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.status = status;
        }
        Ok(())
    }
    async fn update_tags(&self, node_id: &NodeId, tags: &[String]) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.capabilities.tags = tags.to_vec();
        }
        Ok(())
    }
    async fn update_capabilities(
        &self,
        node_id: &NodeId,
        capabilities: &EdgeCapabilities,
    ) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.capabilities = capabilities.clone();
        }
        Ok(())
    }
    async fn delete(&self, node_id: &NodeId) -> anyhow::Result<()> {
        self.edges.lock().await.remove(node_id);
        Ok(())
    }
}

#[derive(Default)]
struct StubGroups {
    groups: Mutex<HashMap<EdgeGroupId, EdgeGroup>>,
}

#[async_trait]
impl EdgeGroupRepository for StubGroups {
    async fn create(&self, group: &EdgeGroup) -> Result<(), EdgeGroupRepoError> {
        let mut g = self.groups.lock().await;
        if g.values()
            .any(|x| x.tenant_id == group.tenant_id && x.name == group.name)
        {
            return Err(EdgeGroupRepoError::GroupExists);
        }
        g.insert(group.id, group.clone());
        Ok(())
    }
    async fn get(&self, id: &EdgeGroupId) -> Result<Option<EdgeGroup>, EdgeGroupRepoError> {
        Ok(self.groups.lock().await.get(id).cloned())
    }
    async fn list_by_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<EdgeGroup>, EdgeGroupRepoError> {
        Ok(self
            .groups
            .lock()
            .await
            .values()
            .filter(|g| &g.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    async fn update(&self, group: &EdgeGroup) -> Result<(), EdgeGroupRepoError> {
        self.groups.lock().await.insert(group.id, group.clone());
        Ok(())
    }
    async fn delete(&self, id: &EdgeGroupId) -> Result<(), EdgeGroupRepoError> {
        self.groups.lock().await.remove(id);
        Ok(())
    }
}

fn make_edge(tenant: &TenantId, tags: &[&str], os: &str) -> EdgeDaemon {
    EdgeDaemon {
        node_id: NodeId::new(),
        tenant_id: tenant.clone(),
        public_key: vec![0; 32],
        capabilities: EdgeCapabilities {
            os: os.to_string(),
            arch: "x86_64".into(),
            local_tools: vec!["docker".into()],
            mount_points: vec!["/".into()],
            custom_labels: Default::default(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        },
        status: NodePeerStatus::Active,
        connection: EdgeConnectionState::Disconnected { since: Utc::now() },
        last_heartbeat_at: None,
        enrolled_at: Utc::now(),
    }
}

#[tokio::test]
async fn cross_tenant_revoke_refused() {
    let repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubRepo::default());
    let registry = EdgeConnectionRegistry::new();
    let svc = RevokeEdgeService::new(repo.clone(), registry);
    let tenant_a = TenantId::new("tenant-a").unwrap();
    let tenant_b = TenantId::new("tenant-b").unwrap();
    let edge = make_edge(&tenant_a, &[], "linux");
    let id = edge.node_id;
    repo.upsert(&edge).await.unwrap();
    let r = svc.revoke(&tenant_b, id).await;
    assert!(
        matches!(r, Err(RevokeEdgeError::Forbidden)),
        "expected cross-tenant revoke to fail with Forbidden, got {r:?}"
    );

    // Side-effect check: re-fetch the edge and assert it's still Active
    let persisted = repo.get(&id).await.unwrap().expect("edge still present");
    assert_eq!(
        persisted.status,
        NodePeerStatus::Active,
        "wrong-tenant revoke must not mutate status"
    );
    assert_eq!(
        persisted.tenant_id, tenant_a,
        "wrong-tenant revoke must not mutate tenant"
    );
}

#[tokio::test]
async fn cross_tenant_tag_mutation_refused() {
    let repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubRepo::default());
    let svc = ManageTagsService::new(repo.clone());
    let tenant_a = TenantId::new("tenant-a").unwrap();
    let tenant_b = TenantId::new("tenant-b").unwrap();
    let edge = make_edge(&tenant_a, &[], "linux");
    let id = edge.node_id;
    repo.upsert(&edge).await.unwrap();

    // add_tags from foreign tenant must fail with Forbidden.
    let r = svc.add_tags(&tenant_b, id, vec!["prod".into()]).await;
    assert!(
        matches!(r, Err(ManageTagsError::Forbidden)),
        "expected cross-tenant add_tags to fail with Forbidden, got {r:?}"
    );

    // Side-effect check: tags unchanged.
    let persisted = repo.get(&id).await.unwrap().expect("edge still present");
    assert!(
        persisted.capabilities.tags.is_empty(),
        "wrong-tenant add_tags must not mutate tags, got {:?}",
        persisted.capabilities.tags
    );

    // Seed a tag from the right tenant, then attempt remove_tags from foreign tenant.
    svc.add_tags(&tenant_a, id, vec!["prod".into()])
        .await
        .unwrap();
    let r = svc.remove_tags(&tenant_b, id, vec!["prod".into()]).await;
    assert!(
        matches!(r, Err(ManageTagsError::Forbidden)),
        "expected cross-tenant remove_tags to fail with Forbidden, got {r:?}"
    );

    // Side-effect check: tags still contain "prod".
    let persisted = repo.get(&id).await.unwrap().expect("edge still present");
    assert_eq!(
        persisted.capabilities.tags,
        vec!["prod".to_string()],
        "wrong-tenant remove_tags must not mutate tags"
    );
}

#[tokio::test]
async fn add_remove_tags_round_trip() {
    let repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubRepo::default());
    let svc = ManageTagsService::new(repo.clone());
    let tenant = TenantId::new("tenant-a").unwrap();
    let edge = make_edge(&tenant, &[], "linux");
    let id = edge.node_id;
    repo.upsert(&edge).await.unwrap();
    let after_add = svc
        .add_tags(&tenant, id, vec!["prod".into(), "db-host".into()])
        .await
        .unwrap();
    assert_eq!(after_add.len(), 2);
    let after_rm = svc
        .remove_tags(&tenant, id, vec!["prod".into()])
        .await
        .unwrap();
    assert_eq!(after_rm, vec!["db-host".to_string()]);
}

#[tokio::test]
async fn group_exists_unique_per_tenant() {
    let repo: Arc<dyn EdgeGroupRepository> = Arc::new(StubGroups::default());
    let svc = ManageGroupsService::new(repo);
    let tenant = TenantId::new("tenant-a").unwrap();
    let _ = svc
        .create(
            tenant.clone(),
            "prod-fleet".into(),
            EdgeSelector::default(),
            vec![],
            "user-1".into(),
        )
        .await
        .unwrap();
    let dup = svc
        .create(
            tenant,
            "prod-fleet".into(),
            EdgeSelector::default(),
            vec![],
            "user-1".into(),
        )
        .await;
    assert!(matches!(
        dup,
        Err(aegis_orchestrator_core::application::edge::manage_groups::ManageGroupError::Exists)
    ));
}

#[test]
fn selector_matches_compound() {
    let caps = EdgeCapabilities {
        os: "linux".into(),
        arch: "x86_64".into(),
        local_tools: vec!["docker".into()],
        custom_labels: [("region".to_string(), "us".to_string())]
            .into_iter()
            .collect(),
        tags: vec!["prod".into()],
        mount_points: vec![],
    };
    let sel = EdgeSelector {
        os: Some("linux".into()),
        arch: Some("x86_64".into()),
        tools: vec!["docker".into()],
        labels: vec![LabelMatch::Equals("region".into(), "us".into())],
        tags: vec![TagMatch::Has("prod".into())],
    };
    assert!(caps.satisfies(&sel));
}

#[test]
fn selector_rejects_compound_mismatch() {
    let caps = EdgeCapabilities {
        os: "linux".into(),
        arch: "x86_64".into(),
        local_tools: vec!["docker".into()],
        custom_labels: [("region".to_string(), "us".to_string())]
            .into_iter()
            .collect(),
        tags: vec!["prod".into()],
        mount_points: vec![],
    };

    // Wrong OS.
    let sel = EdgeSelector {
        os: Some("windows".into()),
        arch: Some("x86_64".into()),
        tools: vec![],
        labels: vec![],
        tags: vec![],
    };
    assert!(!caps.satisfies(&sel), "wrong os must not satisfy");

    // Missing required tool.
    let sel = EdgeSelector {
        os: Some("linux".into()),
        arch: None,
        tools: vec!["kubectl".into()],
        labels: vec![],
        tags: vec![],
    };
    assert!(
        !caps.satisfies(&sel),
        "absent required tool must not satisfy"
    );

    // Missing required tag.
    let sel = EdgeSelector {
        os: None,
        arch: None,
        tools: vec![],
        labels: vec![],
        tags: vec![TagMatch::Has("staging".into())],
    };
    assert!(
        !caps.satisfies(&sel),
        "missing required tag must not satisfy"
    );

    // Mismatched label value.
    let sel = EdgeSelector {
        os: None,
        arch: None,
        tools: vec![],
        labels: vec![LabelMatch::Equals("region".into(), "eu".into())],
        tags: vec![],
    };
    assert!(
        !caps.satisfies(&sel),
        "mismatched label value must not satisfy"
    );

    // Combined mismatch (compound failure across multiple fields).
    let sel = EdgeSelector {
        os: Some("windows".into()),
        arch: Some("x86_64".into()),
        tools: vec!["kubectl".into()],
        labels: vec![LabelMatch::Equals("region".into(), "us".into())],
        tags: vec![TagMatch::Has("staging".into())],
    };
    assert!(!caps.satisfies(&sel), "combined mismatch must not satisfy");
}
