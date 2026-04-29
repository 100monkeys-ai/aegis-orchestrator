// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Postgres-backed [`EdgeGroupRepository`] (ADR-117).
//!
//! Unique violations on `(tenant_id, name)` surface as `GroupExists`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

use crate::domain::edge::{
    EdgeGroup, EdgeGroupId, EdgeGroupRepoError, EdgeGroupRepository, EdgeSelector,
};
use crate::domain::shared_kernel::{NodeId, TenantId};

pub struct PgEdgeGroupRepository {
    pool: PgPool,
}

impl PgEdgeGroupRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn row_to_group(row: &sqlx::postgres::PgRow) -> Result<EdgeGroup, EdgeGroupRepoError> {
    let id: Uuid = row
        .try_get("id")
        .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
    let tenant_id_str: String = row
        .try_get("tenant_id")
        .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
    let name: String = row
        .try_get("name")
        .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
    let selector_json: serde_json::Value = row
        .try_get("selector_json")
        .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
    let selector: EdgeSelector =
        serde_json::from_value(selector_json).map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
    let pinned: Vec<Uuid> = row
        .try_get("pinned_members")
        .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
    let created_by: String = row
        .try_get("created_by")
        .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
    let created_at: DateTime<Utc> = row
        .try_get("created_at")
        .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
    Ok(EdgeGroup {
        id: EdgeGroupId(id),
        tenant_id: TenantId::new(tenant_id_str)
            .map_err(|e| EdgeGroupRepoError::Other(anyhow::anyhow!("invalid tenant: {e}")))?,
        name,
        selector,
        pinned_members: pinned.into_iter().map(NodeId).collect(),
        created_by,
        created_at,
    })
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db) = err {
        return db.code().as_deref() == Some("23505");
    }
    false
}

#[async_trait]
impl EdgeGroupRepository for PgEdgeGroupRepository {
    async fn create(&self, group: &EdgeGroup) -> Result<(), EdgeGroupRepoError> {
        let selector_json = serde_json::to_value(&group.selector)
            .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
        let pinned: Vec<Uuid> = group.pinned_members.iter().map(|n| n.0).collect();
        let res = sqlx::query(
            r#"
            INSERT INTO edge_groups (
                id, tenant_id, name, selector_json, pinned_members, created_by, created_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(group.id.0)
        .bind(group.tenant_id.as_str())
        .bind(&group.name)
        .bind(selector_json)
        .bind(&pinned)
        .bind(&group.created_by)
        .bind(group.created_at)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if is_unique_violation(&e) => Err(EdgeGroupRepoError::GroupExists),
            Err(e) => Err(EdgeGroupRepoError::Other(e.into())),
        }
    }

    async fn get(&self, id: &EdgeGroupId) -> Result<Option<EdgeGroup>, EdgeGroupRepoError> {
        let row = sqlx::query("SELECT * FROM edge_groups WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
        row.map(|r| row_to_group(&r)).transpose()
    }

    async fn list_by_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<EdgeGroup>, EdgeGroupRepoError> {
        let rows = sqlx::query("SELECT * FROM edge_groups WHERE tenant_id = $1")
            .bind(tenant_id.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
        rows.iter().map(row_to_group).collect()
    }

    async fn update(&self, group: &EdgeGroup) -> Result<(), EdgeGroupRepoError> {
        let selector_json = serde_json::to_value(&group.selector)
            .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
        let pinned: Vec<Uuid> = group.pinned_members.iter().map(|n| n.0).collect();
        let res = sqlx::query(
            r#"
            UPDATE edge_groups
            SET selector_json = $1, pinned_members = $2, name = $3
            WHERE id = $4
            "#,
        )
        .bind(selector_json)
        .bind(&pinned)
        .bind(&group.name)
        .bind(group.id.0)
        .execute(&self.pool)
        .await;
        match res {
            Ok(r) if r.rows_affected() == 0 => Err(EdgeGroupRepoError::NotFound),
            Ok(_) => Ok(()),
            Err(e) if is_unique_violation(&e) => Err(EdgeGroupRepoError::GroupExists),
            Err(e) => Err(EdgeGroupRepoError::Other(e.into())),
        }
    }

    async fn delete(&self, id: &EdgeGroupId) -> Result<(), EdgeGroupRepoError> {
        let r = sqlx::query("DELETE FROM edge_groups WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| EdgeGroupRepoError::Other(e.into()))?;
        if r.rows_affected() == 0 {
            return Err(EdgeGroupRepoError::NotFound);
        }
        Ok(())
    }
}
