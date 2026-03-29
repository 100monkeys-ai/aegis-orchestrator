// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Node Registry Repository (ADR-061)
//!
//! Implements [`NodeRegistryRepository`] — the bounded-context-separated
//! repository for node configuration assignment and registry queries.
//! Operates against the existing `cluster_nodes` and `config_layers` tables.

use crate::domain::cluster::{
    ConfigScope, ConfigType, NodeCapabilityAdvertisement, NodeConfigAssignment, NodeId,
    NodePeerStatus, NodeRegistryRepository, NodeRole, RegisteredNode, RuntimeRegistryAssignment,
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
            SELECT node_id, role, grpc_address, status, gpu_count, vram_gb,
                   cpu_cores, available_mem_gb, supported_runtimes, tags,
                   last_heartbeat_at, registered_at,
                   hostname, software_version, metadata, current_config_version
            FROM cluster_nodes
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
            SELECT node_id, role, grpc_address, status, gpu_count, vram_gb,
                   cpu_cores, available_mem_gb, supported_runtimes, tags,
                   last_heartbeat_at, registered_at,
                   hostname, software_version, metadata, current_config_version
            FROM cluster_nodes
            ORDER BY registered_at
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_registered_node).collect())
    }

    async fn upsert_registered_node(&self, node: &RegisteredNode) -> anyhow::Result<()> {
        let metadata_json = serde_json::to_value(&node.metadata)?;
        sqlx::query(
            r#"
            INSERT INTO cluster_nodes (
                node_id, role, public_key, grpc_address, status,
                gpu_count, vram_gb, cpu_cores, available_mem_gb,
                supported_runtimes, tags, registered_at, last_heartbeat_at,
                hostname, software_version, metadata, current_config_version
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
            ON CONFLICT (node_id) DO UPDATE SET
                role = EXCLUDED.role,
                grpc_address = EXCLUDED.grpc_address,
                status = EXCLUDED.status,
                gpu_count = EXCLUDED.gpu_count,
                vram_gb = EXCLUDED.vram_gb,
                cpu_cores = EXCLUDED.cpu_cores,
                available_mem_gb = EXCLUDED.available_mem_gb,
                supported_runtimes = EXCLUDED.supported_runtimes,
                tags = EXCLUDED.tags,
                last_heartbeat_at = EXCLUDED.last_heartbeat_at,
                hostname = EXCLUDED.hostname,
                software_version = EXCLUDED.software_version,
                metadata = EXCLUDED.metadata,
                current_config_version = EXCLUDED.current_config_version
            "#,
        )
        .bind(node.node_id.0)
        .bind(format!("{:?}", node.role).to_lowercase())
        .bind(Vec::<u8>::new()) // public_key — not managed by registry repo
        .bind(&node.grpc_address)
        .bind(format!("{:?}", node.status).to_lowercase())
        .bind(node.capabilities.gpu_count as i32)
        .bind(node.capabilities.vram_gb as i32)
        .bind(node.capabilities.cpu_cores as i32)
        .bind(node.capabilities.available_memory_gb as i32)
        .bind(&node.capabilities.supported_runtimes)
        .bind(&node.capabilities.tags)
        .bind(node.registered_at)
        .bind(node.last_heartbeat_at)
        .bind(&node.hostname)
        .bind(&node.software_version)
        .bind(metadata_json)
        .bind(&node.config_version)
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
    let status_str: String = r.get("status");
    let hostname: String = r.get("hostname");
    let software_version: String = r.get("software_version");
    let metadata_json: serde_json::Value = r.get("metadata");
    let config_version: Option<String> = r.get("current_config_version");

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
        status: match status_str.as_str() {
            "active" => NodePeerStatus::Active,
            "draining" => NodePeerStatus::Draining,
            "unhealthy" => NodePeerStatus::Unhealthy,
            _ => NodePeerStatus::Active,
        },
        capabilities: NodeCapabilityAdvertisement {
            gpu_count: r.get::<i32, _>("gpu_count") as u32,
            vram_gb: r.get::<i32, _>("vram_gb") as u32,
            cpu_cores: r.get::<i32, _>("cpu_cores") as u32,
            available_memory_gb: r.get::<i32, _>("available_mem_gb") as u32,
            supported_runtimes: r.get("supported_runtimes"),
            tags: r.get("tags"),
        },
        grpc_address: r.get("grpc_address"),
        software_version,
        metadata,
        config_version,
        last_heartbeat_at: r.get("last_heartbeat_at"),
        registered_at: r.get("registered_at"),
    }
}
