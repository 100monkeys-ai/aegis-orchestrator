// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PushConfig Use Case (ADR-060)
//!
//! Controller pushes config update to a specific node.

use crate::domain::cluster::{ConfigLayerRepository, ConfigScope, ConfigType, NodeId};
use std::sync::Arc;

pub struct PushConfigUseCase {
    config_repo: Arc<dyn ConfigLayerRepository>,
}

impl PushConfigUseCase {
    pub fn new(config_repo: Arc<dyn ConfigLayerRepository>) -> Self {
        Self { config_repo }
    }

    pub async fn execute(
        &self,
        target_node_id: &NodeId,
        _config_version: &str,
        config_payload: &str,
    ) -> anyhow::Result<bool> {
        let payload: serde_json::Value = serde_json::from_str(config_payload)?;

        // Store as node-scoped override
        self.config_repo
            .upsert_layer(
                &ConfigScope::Node,
                &target_node_id.to_string(),
                &ConfigType::AegisConfig,
                payload,
            )
            .await?;

        Ok(true)
    }
}
