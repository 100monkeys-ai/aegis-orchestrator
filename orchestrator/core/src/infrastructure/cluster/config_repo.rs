// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Config Layer Repository (ADR-060)
//!
//! Implements `ConfigLayerRepository` with hierarchical merge semantics.

use crate::domain::cluster::{
    ConfigLayerRepository, ConfigScope, ConfigSnapshot, ConfigType, MergedConfig, NodeId,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use sqlx::Row;
use std::sync::Arc;

pub struct PgConfigLayerRepository {
    pool: Arc<PgPool>,
}

impl PgConfigLayerRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    fn compute_version(payload: &serde_json::Value) -> String {
        let serialized = serde_json::to_string(payload).unwrap_or_default();
        let hash = Sha256::digest(serialized.as_bytes());
        hex::encode(hash)
    }

    fn scope_to_str(scope: &ConfigScope) -> &'static str {
        match scope {
            ConfigScope::Global => "global",
            ConfigScope::Tenant => "tenant",
            ConfigScope::Node => "node",
        }
    }

    fn str_to_scope(s: &str) -> ConfigScope {
        match s {
            "global" => ConfigScope::Global,
            "tenant" => ConfigScope::Tenant,
            "node" => ConfigScope::Node,
            _ => ConfigScope::Global,
        }
    }

    fn type_to_str(ct: &ConfigType) -> &'static str {
        match ct {
            ConfigType::AegisConfig => "aegis-config",
            ConfigType::RuntimeRegistry => "runtime-registry",
        }
    }

    fn str_to_type(s: &str) -> ConfigType {
        match s {
            "aegis-config" => ConfigType::AegisConfig,
            "runtime-registry" => ConfigType::RuntimeRegistry,
            _ => ConfigType::AegisConfig,
        }
    }
}

#[async_trait]
impl ConfigLayerRepository for PgConfigLayerRepository {
    async fn get_layer(
        &self,
        scope: &ConfigScope,
        scope_key: &str,
        config_type: &ConfigType,
    ) -> anyhow::Result<Option<ConfigSnapshot>> {
        let row = sqlx::query(
            r#"
            SELECT scope, scope_key, config_type, config_payload, config_version, updated_at
            FROM config_layers
            WHERE scope = $1 AND scope_key = $2 AND config_type = $3
            "#,
        )
        .bind(Self::scope_to_str(scope))
        .bind(scope_key)
        .bind(Self::type_to_str(config_type))
        .fetch_optional(self.pool.as_ref())
        .await?;

        Ok(row.map(|r| {
            let scope_str: String = r.get("scope");
            let type_str: String = r.get("config_type");
            ConfigSnapshot {
                scope: Self::str_to_scope(&scope_str),
                scope_key: r.get("scope_key"),
                config_type: Self::str_to_type(&type_str),
                payload: r.get("config_payload"),
                version: r.get("config_version"),
                updated_at: r.get("updated_at"),
            }
        }))
    }

    async fn upsert_layer(
        &self,
        scope: &ConfigScope,
        scope_key: &str,
        config_type: &ConfigType,
        payload: serde_json::Value,
    ) -> anyhow::Result<ConfigSnapshot> {
        let version = Self::compute_version(&payload);
        let row = sqlx::query(
            r#"
            INSERT INTO config_layers (scope, scope_key, config_type, config_payload, config_version, updated_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            ON CONFLICT (scope, scope_key, config_type) DO UPDATE SET
                config_payload = EXCLUDED.config_payload,
                config_version = EXCLUDED.config_version,
                updated_at = NOW()
            RETURNING scope, scope_key, config_type, config_payload, config_version, updated_at
            "#,
        )
        .bind(Self::scope_to_str(scope))
        .bind(scope_key)
        .bind(Self::type_to_str(config_type))
        .bind(&payload)
        .bind(&version)
        .fetch_one(self.pool.as_ref())
        .await?;

        let scope_str: String = row.get("scope");
        let type_str: String = row.get("config_type");
        Ok(ConfigSnapshot {
            scope: Self::str_to_scope(&scope_str),
            scope_key: row.get("scope_key"),
            config_type: Self::str_to_type(&type_str),
            payload: row.get("config_payload"),
            version: row.get("config_version"),
            updated_at: row.get("updated_at"),
        })
    }

    async fn get_merged_config(
        &self,
        node_id: &NodeId,
        tenant_id: Option<&str>,
        config_type: &ConfigType,
    ) -> anyhow::Result<MergedConfig> {
        let mut merged = serde_json::Value::Object(serde_json::Map::new());

        if let Some(global) = self
            .get_layer(&ConfigScope::Global, "", config_type)
            .await?
        {
            json_deep_merge(&mut merged, &global.payload);
        }
        if let Some(tid) = tenant_id {
            if let Some(tenant) = self
                .get_layer(&ConfigScope::Tenant, tid, config_type)
                .await?
            {
                json_deep_merge(&mut merged, &tenant.payload);
            }
        }
        if let Some(node) = self
            .get_layer(&ConfigScope::Node, &node_id.to_string(), config_type)
            .await?
        {
            json_deep_merge(&mut merged, &node.payload);
        }

        let version = Self::compute_version(&merged);
        Ok(MergedConfig {
            payload: merged,
            version,
        })
    }

    async fn list_layers(&self, config_type: &ConfigType) -> anyhow::Result<Vec<ConfigSnapshot>> {
        let rows = sqlx::query(
            r#"
            SELECT scope, scope_key, config_type, config_payload, config_version, updated_at
            FROM config_layers
            WHERE config_type = $1
            ORDER BY scope, scope_key
            "#,
        )
        .bind(Self::type_to_str(config_type))
        .fetch_all(self.pool.as_ref())
        .await?;

        Ok(rows
            .iter()
            .map(|r| {
                let scope_str: String = r.get("scope");
                let type_str: String = r.get("config_type");
                ConfigSnapshot {
                    scope: Self::str_to_scope(&scope_str),
                    scope_key: r.get("scope_key"),
                    config_type: Self::str_to_type(&type_str),
                    payload: r.get("config_payload"),
                    version: r.get("config_version"),
                    updated_at: r.get("updated_at"),
                }
            })
            .collect())
    }

    async fn delete_layer(
        &self,
        scope: &ConfigScope,
        scope_key: &str,
        config_type: &ConfigType,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            r#"
            DELETE FROM config_layers
            WHERE scope = $1 AND scope_key = $2 AND config_type = $3
            "#,
        )
        .bind(Self::scope_to_str(scope))
        .bind(scope_key)
        .bind(Self::type_to_str(config_type))
        .execute(self.pool.as_ref())
        .await?;

        Ok(result.rows_affected() > 0)
    }
}

/// Deep merge two JSON objects. Values in `overlay` override `base`.
fn json_deep_merge(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                let base_value = base_map
                    .entry(key.clone())
                    .or_insert(serde_json::Value::Null);
                json_deep_merge(base_value, overlay_value);
            }
        }
        (base, overlay) => {
            *base = overlay.clone();
        }
    }
}
