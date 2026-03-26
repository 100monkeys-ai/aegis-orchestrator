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
        current_config_version: &str,
    ) -> anyhow::Result<SyncConfigResult> {
        let merged = self
            .config_repo
            .get_merged_config(node_id, None, &ConfigType::AegisConfig)
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
