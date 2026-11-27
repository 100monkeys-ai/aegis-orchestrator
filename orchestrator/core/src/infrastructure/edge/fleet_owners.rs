// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Which cluster member runs each fleet command (AEGIS ADR-152 D4.7).

use anyhow::Result;
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::cluster::FleetCommandId;
use crate::domain::shared_kernel::{NodeId, TenantId};

#[async_trait]
pub trait FleetCommandOwners: Send + Sync {
    async fn record(&self, id: FleetCommandId, member: NodeId, tenant: &TenantId) -> Result<()>;
    async fn release(&self, id: FleetCommandId) -> Result<()>;
    async fn owner_of(&self, id: FleetCommandId) -> Result<Option<NodeId>>;
}

/// Records the command, and drops this member's rows older than a day: a
/// release that raced ahead of its record leaves one behind, which only ever
/// makes a forwarded cancel answer "not found".
pub const RECORD_FLEET_SQL: &str = r#"
WITH pruned AS (
    DELETE FROM fleet_command_owners
    WHERE member_node_id = $2 AND started_at < now() - interval '1 day'
)
INSERT INTO fleet_command_owners (fleet_command_id, member_node_id, tenant_id, started_at)
VALUES ($1, $2, $3, now())
ON CONFLICT (fleet_command_id) DO NOTHING
"#;
pub const RELEASE_FLEET_SQL: &str = "DELETE FROM fleet_command_owners WHERE fleet_command_id = $1";

pub struct PgFleetCommandOwners {
    pool: PgPool,
}

impl PgFleetCommandOwners {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl FleetCommandOwners for PgFleetCommandOwners {
    async fn record(&self, id: FleetCommandId, member: NodeId, tenant: &TenantId) -> Result<()> {
        sqlx::query(RECORD_FLEET_SQL)
            .bind(id.0)
            .bind(member.0)
            .bind(tenant.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn release(&self, id: FleetCommandId) -> Result<()> {
        sqlx::query(RELEASE_FLEET_SQL)
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn owner_of(&self, id: FleetCommandId) -> Result<Option<NodeId>> {
        let member: Option<Uuid> = sqlx::query_scalar(
            "SELECT member_node_id FROM fleet_command_owners WHERE fleet_command_id = $1",
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(member.map(NodeId))
    }
}
