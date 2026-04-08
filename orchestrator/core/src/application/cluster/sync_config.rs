// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SyncConfig Use Case (ADR-060)
//!
//! Worker polls controller to check if its config is up-to-date.

use crate::domain::cluster::{ConfigLayerRepository, ConfigType, NodeClusterRepository, NodeId};
use std::sync::Arc;

pub struct SyncConfigUseCase {
    config_repo: Arc<dyn ConfigLayerRepository>,
    cluster_repo: Arc<dyn NodeClusterRepository>,
}

impl SyncConfigUseCase {
    pub fn new(
        config_repo: Arc<dyn ConfigLayerRepository>,
        cluster_repo: Arc<dyn NodeClusterRepository>,
    ) -> Self {
        Self {
            config_repo,
            cluster_repo,
        }
    }

    pub async fn execute(
        &self,
        node_id: &NodeId,
        tenant_id: Option<&str>,
        current_config_version: &str,
    ) -> anyhow::Result<SyncConfigResult> {
        let merged = self
            .config_repo
            .get_merged_config(node_id, tenant_id, &ConfigType::AegisConfig)
            .await?;

        if merged.version == current_config_version {
            return Ok(SyncConfigResult::UpToDate);
        }

        // Record that we've sent this version
        self.cluster_repo
            .record_config_version(node_id, &merged.version)
            .await?;

        Ok(SyncConfigResult::Updated {
            latest_version: merged.version,
            config_delta: serde_json::to_string(&merged.payload)?,
        })
    }
}

pub enum SyncConfigResult {
    UpToDate,
    Updated {
        latest_version: String,
        config_delta: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cluster::{
        ConfigLayerRepository, ConfigScope, ConfigSnapshot, ConfigType, MergedConfig,
        NodeClusterRepository, NodePeer, NodePeerStatus, RegisteredNode, ResourceSnapshot,
    };
    use chrono::Utc;
    use std::collections::HashMap;

    /// Mock repository that records which tenant_id was passed to get_merged_config.
    struct MockConfigLayerRepo {
        tenant_id_seen: std::sync::Mutex<Option<Option<String>>>,
    }

    impl MockConfigLayerRepo {
        fn new() -> Self {
            Self {
                tenant_id_seen: std::sync::Mutex::new(None),
            }
        }

        fn last_tenant_id(&self) -> Option<Option<String>> {
            self.tenant_id_seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ConfigLayerRepository for MockConfigLayerRepo {
        async fn get_layer(
            &self,
            _scope: &ConfigScope,
            _scope_key: &str,
            _config_type: &ConfigType,
        ) -> anyhow::Result<Option<ConfigSnapshot>> {
            Ok(None)
        }

        async fn upsert_layer(
            &self,
            _scope: &ConfigScope,
            _scope_key: &str,
            _config_type: &ConfigType,
            _payload: serde_json::Value,
        ) -> anyhow::Result<ConfigSnapshot> {
            unimplemented!()
        }

        async fn get_merged_config(
            &self,
            _node_id: &NodeId,
            tenant_id: Option<&str>,
            _config_type: &ConfigType,
        ) -> anyhow::Result<MergedConfig> {
            *self.tenant_id_seen.lock().unwrap() = Some(tenant_id.map(String::from));
            Ok(MergedConfig {
                payload: serde_json::json!({"runtime": {}, "storage": {}, "llm": {}}),
                version: "v42".to_string(),
            })
        }

        async fn list_layers(
            &self,
            _config_type: &ConfigType,
        ) -> anyhow::Result<Vec<ConfigSnapshot>> {
            Ok(vec![])
        }

        async fn delete_layer(
            &self,
            _scope: &ConfigScope,
            _scope_key: &str,
            _config_type: &ConfigType,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    struct MockClusterRepo;

    #[async_trait::async_trait]
    impl NodeClusterRepository for MockClusterRepo {
        async fn upsert_peer(&self, _peer: &NodePeer) -> anyhow::Result<()> {
            Ok(())
        }
        async fn find_peer(&self, _node_id: &NodeId) -> anyhow::Result<Option<NodePeer>> {
            Ok(None)
        }
        async fn list_peers_by_status(
            &self,
            _status: NodePeerStatus,
        ) -> anyhow::Result<Vec<NodePeer>> {
            Ok(vec![])
        }
        async fn record_heartbeat(
            &self,
            _node_id: &NodeId,
            _snapshot: ResourceSnapshot,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn mark_unhealthy(&self, _node_id: &NodeId) -> anyhow::Result<()> {
            Ok(())
        }
        async fn start_drain(&self, _node_id: &NodeId) -> anyhow::Result<()> {
            Ok(())
        }
        async fn deregister(&self, _node_id: &NodeId, _reason: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_config_version(&self, _node_id: &NodeId) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        async fn record_config_version(
            &self,
            _node_id: &NodeId,
            _hash: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_all_peers(&self) -> anyhow::Result<Vec<NodePeer>> {
            Ok(vec![])
        }
        async fn count_by_status(&self) -> anyhow::Result<HashMap<NodePeerStatus, usize>> {
            Ok(HashMap::new())
        }
        async fn find_registered_node(
            &self,
            _node_id: &NodeId,
        ) -> anyhow::Result<Option<RegisteredNode>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn test_sync_config_with_tenant_id() {
        let config_repo = Arc::new(MockConfigLayerRepo::new());
        let cluster_repo: Arc<dyn NodeClusterRepository> = Arc::new(MockClusterRepo);
        let uc = SyncConfigUseCase::new(config_repo.clone(), cluster_repo);
        let node_id = NodeId(uuid::Uuid::new_v4());

        let _result = uc
            .execute(&node_id, Some("tenant-abc"), "old-version")
            .await
            .unwrap();

        // Verify tenant_id was forwarded to the repository
        let seen = config_repo.last_tenant_id().unwrap();
        assert_eq!(seen, Some("tenant-abc".to_string()));
    }

    #[tokio::test]
    async fn test_sync_config_without_tenant_id() {
        let config_repo = Arc::new(MockConfigLayerRepo::new());
        let cluster_repo: Arc<dyn NodeClusterRepository> = Arc::new(MockClusterRepo);
        let uc = SyncConfigUseCase::new(config_repo.clone(), cluster_repo);
        let node_id = NodeId(uuid::Uuid::new_v4());

        let _result = uc.execute(&node_id, None, "old-version").await.unwrap();

        let seen = config_repo.last_tenant_id().unwrap();
        assert_eq!(seen, None);
    }

    #[tokio::test]
    async fn test_sync_config_returns_up_to_date_when_version_matches() {
        let config_repo = Arc::new(MockConfigLayerRepo::new());
        let cluster_repo: Arc<dyn NodeClusterRepository> = Arc::new(MockClusterRepo);
        let uc = SyncConfigUseCase::new(config_repo.clone(), cluster_repo);
        let node_id = NodeId(uuid::Uuid::new_v4());

        let result = uc.execute(&node_id, None, "v42").await.unwrap();
        assert!(matches!(result, SyncConfigResult::UpToDate));
    }
}
