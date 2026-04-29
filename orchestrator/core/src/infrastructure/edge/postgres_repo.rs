// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Postgres-backed [`EdgeDaemonRepository`] (ADR-117).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;

use crate::domain::cluster::NodePeerStatus;
use crate::domain::edge::{
    EdgeCapabilities, EdgeConnectionState, EdgeDaemon, EdgeDaemonRepository,
};
use crate::domain::shared_kernel::{NodeId, TenantId};

pub struct PgEdgeDaemonRepository {
    pool: PgPool,
}

impl PgEdgeDaemonRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn status_to_str(s: NodePeerStatus) -> &'static str {
    match s {
        NodePeerStatus::Active => "active",
        NodePeerStatus::Draining => "draining",
        NodePeerStatus::Unhealthy => "unhealthy",
    }
}

fn parse_status(s: &str) -> NodePeerStatus {
    match s {
        "active" => NodePeerStatus::Active,
        "draining" => NodePeerStatus::Draining,
        _ => NodePeerStatus::Unhealthy,
    }
}

fn row_to_edge(row: &sqlx::postgres::PgRow) -> anyhow::Result<EdgeDaemon> {
    let node_id: uuid::Uuid = row.try_get("node_id")?;
    let tenant_id_str: String = row.try_get("tenant_id")?;
    let public_key: Vec<u8> = row.try_get("public_key")?;
    let caps_json: serde_json::Value = row.try_get("capabilities_json")?;
    let mut capabilities: EdgeCapabilities = serde_json::from_value(caps_json)?;
    let tags: Vec<String> = row.try_get("tags")?;
    capabilities.tags = tags;
    let status: String = row.try_get("status")?;
    let enrolled_at: DateTime<Utc> = row.try_get("enrolled_at")?;
    let last_heartbeat_at: Option<DateTime<Utc>> = row.try_get("last_heartbeat_at")?;
    Ok(EdgeDaemon {
        node_id: NodeId(node_id),
        tenant_id: TenantId::new(tenant_id_str)
            .map_err(|e| anyhow::anyhow!("invalid tenant_id: {e}"))?,
        public_key,
        capabilities,
        status: parse_status(&status),
        connection: EdgeConnectionState::Disconnected { since: Utc::now() },
        last_heartbeat_at,
        enrolled_at,
    })
}

#[async_trait]
impl EdgeDaemonRepository for PgEdgeDaemonRepository {
    async fn upsert(&self, edge: &EdgeDaemon) -> anyhow::Result<()> {
        let caps_json = serde_json::to_value(&edge.capabilities)?;
        sqlx::query(
            r#"
            INSERT INTO edge_daemons (
                node_id, tenant_id, public_key, capabilities_json, tags, status,
                enrolled_at, last_heartbeat_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (node_id) DO UPDATE SET
                tenant_id = EXCLUDED.tenant_id,
                public_key = EXCLUDED.public_key,
                capabilities_json = EXCLUDED.capabilities_json,
                tags = EXCLUDED.tags,
                status = EXCLUDED.status,
                last_heartbeat_at = EXCLUDED.last_heartbeat_at
            "#,
        )
        .bind(edge.node_id.0)
        .bind(edge.tenant_id.as_str())
        .bind(&edge.public_key)
        .bind(&caps_json)
        .bind(&edge.capabilities.tags)
        .bind(status_to_str(edge.status))
        .bind(edge.enrolled_at)
        .bind(edge.last_heartbeat_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, node_id: &NodeId) -> anyhow::Result<Option<EdgeDaemon>> {
        let row = sqlx::query(
            r#"
            SELECT node_id, tenant_id, public_key, capabilities_json, tags, status,
                   enrolled_at, last_heartbeat_at
            FROM edge_daemons WHERE node_id = $1
            "#,
        )
        .bind(node_id.0)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_edge(&r)).transpose()
    }

    async fn list_by_tenant(&self, tenant_id: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>> {
        let rows = sqlx::query(
            r#"
            SELECT node_id, tenant_id, public_key, capabilities_json, tags, status,
                   enrolled_at, last_heartbeat_at
            FROM edge_daemons WHERE tenant_id = $1
            "#,
        )
        .bind(tenant_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_edge).collect()
    }

    async fn update_status(&self, node_id: &NodeId, status: NodePeerStatus) -> anyhow::Result<()> {
        sqlx::query("UPDATE edge_daemons SET status = $1 WHERE node_id = $2")
            .bind(status_to_str(status))
            .bind(node_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_tags(&self, node_id: &NodeId, tags: &[String]) -> anyhow::Result<()> {
        sqlx::query("UPDATE edge_daemons SET tags = $1 WHERE node_id = $2")
            .bind(tags)
            .bind(node_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_capabilities(
        &self,
        node_id: &NodeId,
        capabilities: &EdgeCapabilities,
    ) -> anyhow::Result<()> {
        let caps_json = serde_json::to_value(capabilities)?;
        sqlx::query("UPDATE edge_daemons SET capabilities_json = $1 WHERE node_id = $2")
            .bind(caps_json)
            .bind(node_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete(&self, node_id: &NodeId) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM edge_daemons WHERE node_id = $1")
            .bind(node_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
