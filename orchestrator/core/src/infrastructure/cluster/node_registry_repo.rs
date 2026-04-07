// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Node Registry Repository (ADR-061)
//!
//! Implements [`NodeRegistryRepository`] — the bounded-context-separated
//! repository for node configuration assignment and registry queries.
//! Operates against the dedicated `registered_nodes` table (separated from
//! transient `cluster_nodes` peer state).

use crate::domain::cluster::{
    ConfigScope, ConfigType, NodeConfigAssignment, NodeId, NodeRegistryRepository, NodeRole,
    RegisteredNode, RegistryStatus, RuntimeRegistryAssignment,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;
use std::collections::HashMap;

pub struct PgNodeRegistryRepository {
    pool: PgPool,
}

impl PgNodeRegistryRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl NodeRegistryRepository for PgNodeRegistryRepository {
    async fn find_registered_node(
        &self,
        node_id: &NodeId,
    ) -> anyhow::Result<Option<RegisteredNode>> {
        let row = sqlx::query(
            r#"
            SELECT node_id, hostname, role, software_version, metadata,
                   registry_status, current_config_version,
                   registered_at, decommissioned_at
            FROM registered_nodes
            WHERE node_id = $1
            "#,
        )
        .bind(node_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_registered_node(&r)))
    }

    async fn list_registered_nodes(&self) -> anyhow::Result<Vec<RegisteredNode>> {
        let rows = sqlx::query(
            r#"
            SELECT node_id, hostname, role, software_version, metadata,
                   registry_status, current_config_version,
                   registered_at, decommissioned_at
            FROM registered_nodes
            ORDER BY registered_at
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_registered_node).collect())
    }

    async fn upsert_registered_node(&self, node: &RegisteredNode) -> anyhow::Result<()> {
        let metadata_json = serde_json::to_value(&node.metadata)?;
        let status_str = match node.registry_status {
            RegistryStatus::Pending => "pending",
            RegistryStatus::Active => "active",
            RegistryStatus::Decommissioned => "decommissioned",
        };
        sqlx::query(
            r#"
            INSERT INTO registered_nodes (
                node_id, hostname, role, software_version, metadata,
                registry_status, current_config_version,
                registered_at, decommissioned_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (node_id) DO UPDATE SET
                hostname = EXCLUDED.hostname,
                role = EXCLUDED.role,
                software_version = EXCLUDED.software_version,
                metadata = EXCLUDED.metadata,
                registry_status = EXCLUDED.registry_status,
                current_config_version = EXCLUDED.current_config_version,
                decommissioned_at = EXCLUDED.decommissioned_at
            "#,
        )
        .bind(node.node_id.0)
        .bind(&node.hostname)
        .bind(format!("{:?}", node.role).to_lowercase())
        .bind(&node.software_version)
        .bind(metadata_json)
        .bind(status_str)
        .bind(&node.config_version)
        .bind(node.registered_at)
        .bind(node.decommissioned_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn assign_config(&self, assignment: &NodeConfigAssignment) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            INSERT INTO config_layers (scope, scope_key, config_type, payload, version, updated_at)
            VALUES ($1, $2, $3, '{}'::jsonb, '', $4)
            ON CONFLICT (scope, scope_key, config_type) DO UPDATE SET
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(assignment.scope.to_string())
        .bind(assignment.node_id.0.to_string())
        .bind(assignment.config_type.to_string())
        .bind(assignment.assigned_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_config_assignments(
        &self,
        node_id: &NodeId,
    ) -> anyhow::Result<Vec<NodeConfigAssignment>> {
        let rows = sqlx::query(
            r#"
            SELECT scope, config_type, updated_at
            FROM config_layers
            WHERE scope = 'node' AND scope_key = $1
            "#,
        )
        .bind(node_id.0.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut assignments = Vec::with_capacity(rows.len());
        for row in &rows {
            let scope_str: String = row.get("scope");
            let config_type_str: String = row.get("config_type");
            let updated_at: DateTime<Utc> = row.get("updated_at");

            let scope = match scope_str.as_str() {
                "global" => ConfigScope::Global,
                "tenant" => ConfigScope::Tenant,
                "node" => ConfigScope::Node,
                _ => continue,
            };
            let config_type = match config_type_str.as_str() {
                "aegis-config" => ConfigType::AegisConfig,
                "runtime-registry" => ConfigType::RuntimeRegistry,
                _ => continue,
            };

            assignments.push(NodeConfigAssignment {
                node_id: *node_id,
                config_type,
                scope,
                assigned_at: updated_at,
            });
        }
        Ok(assignments)
    }

    async fn assign_runtime_registry(
        &self,
        assignment: &RuntimeRegistryAssignment,
    ) -> anyhow::Result<()> {
        // Store as a config_layers entry with config_type='runtime-registry'
        // and scope='node', scope_key=node_id, payload containing registry_name.
        let payload = serde_json::json!({ "registry_name": assignment.registry_name });
        sqlx::query(
            r#"
            INSERT INTO config_layers (scope, scope_key, config_type, payload, version, updated_at)
            VALUES ('node', $1, 'runtime-registry', $2, '', $3)
            ON CONFLICT (scope, scope_key, config_type) DO UPDATE SET
                payload = EXCLUDED.payload,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(assignment.node_id.0.to_string())
        .bind(payload)
        .bind(assignment.assigned_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_runtime_registry_assignments(
        &self,
        node_id: &NodeId,
    ) -> anyhow::Result<Vec<RuntimeRegistryAssignment>> {
        let rows = sqlx::query(
            r#"
            SELECT payload, updated_at
            FROM config_layers
            WHERE scope = 'node' AND scope_key = $1 AND config_type = 'runtime-registry'
            "#,
        )
        .bind(node_id.0.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut assignments = Vec::with_capacity(rows.len());
        for row in &rows {
            let payload: serde_json::Value = row.get("payload");
            let updated_at: DateTime<Utc> = row.get("updated_at");
            let registry_name = payload["registry_name"].as_str().unwrap_or("").to_string();
            assignments.push(RuntimeRegistryAssignment {
                node_id: *node_id,
                registry_name,
                assigned_at: updated_at,
            });
        }
        Ok(assignments)
    }
}

fn row_to_registered_node(r: &sqlx::postgres::PgRow) -> RegisteredNode {
    let role_str: String = r.get("role");
    let status_str: String = r.get("registry_status");
    let hostname: String = r.get("hostname");
    let software_version: String = r.get("software_version");
    let metadata_json: serde_json::Value = r.get("metadata");
    let config_version: Option<String> = r.get("current_config_version");
    let decommissioned_at: Option<DateTime<Utc>> = r.get("decommissioned_at");

    let metadata: HashMap<String, String> = metadata_json
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    RegisteredNode {
        node_id: NodeId(r.get("node_id")),
        hostname,
        role: match role_str.as_str() {
            "controller" => NodeRole::Controller,
            "worker" => NodeRole::Worker,
            "hybrid" => NodeRole::Hybrid,
            _ => NodeRole::Worker,
        },
        registry_status: match status_str.as_str() {
            "pending" => RegistryStatus::Pending,
            "active" => RegistryStatus::Active,
            "decommissioned" => RegistryStatus::Decommissioned,
            _ => RegistryStatus::Pending,
        },
        software_version,
        metadata,
        config_version,
        registered_at: r.get("registered_at"),
        decommissioned_at,
    }
}
