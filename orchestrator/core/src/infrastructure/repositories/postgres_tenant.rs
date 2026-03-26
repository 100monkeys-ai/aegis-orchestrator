// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Tenant Repository — ADR-056
//!
//! Production `TenantRepository` implementation backed by the `tenants` table
//! in PostgreSQL via `sqlx`.

use crate::domain::repository::{RepositoryError, TenantRepository};
use crate::domain::tenancy::{Tenant, TenantQuotas, TenantStatus, TenantTier};
use crate::domain::tenant::TenantId;
use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;

pub struct PostgresTenantRepository {
    pool: PgPool,
}

impl PostgresTenantRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn parse_status(s: &str) -> TenantStatus {
    match s {
        "active" => TenantStatus::Active,
        "suspended" => TenantStatus::Suspended,
        "deleted" => TenantStatus::Deleted,
        _ => TenantStatus::Active,
    }
}

fn status_to_str(status: &TenantStatus) -> &'static str {
    match status {
        TenantStatus::Active => "active",
        TenantStatus::Suspended => "suspended",
        TenantStatus::Deleted => "deleted",
    }
}

fn parse_tier(s: &str) -> TenantTier {
    match s {
        "consumer" => TenantTier::Consumer,
        "enterprise" => TenantTier::Enterprise,
        "system" => TenantTier::System,
        _ => TenantTier::Enterprise,
    }
}

fn tier_to_str(tier: &TenantTier) -> &'static str {
    match tier {
        TenantTier::Consumer => "consumer",
        TenantTier::Enterprise => "enterprise",
        TenantTier::System => "system",
    }
}

fn row_to_tenant(row: &sqlx::postgres::PgRow) -> Result<Tenant, RepositoryError> {
    let slug_str: String = row.get("slug");
    let slug = TenantId::from_string(&slug_str)
        .map_err(|e| RepositoryError::Serialization(format!("Invalid tenant slug: {e}")))?;

    let display_name: String = row.get("display_name");
    let status_str: String = row.get("status");
    let tier_str: String = row.get("tier");
    let keycloak_realm: String = row.get("keycloak_realm");
    let openbao_namespace: String = row.get("openbao_namespace");
    let max_concurrent_executions: i32 = row.get("max_concurrent_executions");
    let max_agents: i32 = row.get("max_agents");
    // NUMERIC(10,2) is cast to FLOAT8 in the SQL query for sqlx compatibility
    let max_storage_gb: f64 = row.get("max_storage_gb_f");
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    let deleted_at: Option<chrono::DateTime<chrono::Utc>> = row.get("deleted_at");

    Ok(Tenant {
        slug,
        display_name,
        status: parse_status(&status_str),
        tier: parse_tier(&tier_str),
        keycloak_realm,
        openbao_namespace,
        quotas: TenantQuotas {
            max_concurrent_executions: max_concurrent_executions as u32,
            max_agents: max_agents as u32,
            max_storage_gb,
        },
        created_at,
        updated_at,
        deleted_at,
    })
}

#[async_trait]
impl TenantRepository for PostgresTenantRepository {
    async fn find_by_slug(&self, slug: &TenantId) -> Result<Option<Tenant>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT slug, display_name, status, tier, keycloak_realm, openbao_namespace,
                   max_concurrent_executions, max_agents,
                   max_storage_gb::FLOAT8 AS max_storage_gb_f,
                   created_at, updated_at, deleted_at
            FROM tenants
            WHERE slug = $1 AND status != 'deleted'
            "#,
        )
        .bind(slug.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        match row {
            Some(ref r) => Ok(Some(row_to_tenant(r)?)),
            None => Ok(None),
        }
    }

    async fn find_all_active(&self) -> Result<Vec<Tenant>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT slug, display_name, status, tier, keycloak_realm, openbao_namespace,
                   max_concurrent_executions, max_agents,
                   max_storage_gb::FLOAT8 AS max_storage_gb_f,
                   created_at, updated_at, deleted_at
            FROM tenants
            WHERE status = 'active'
            ORDER BY slug
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        rows.iter().map(row_to_tenant).collect()
    }

    async fn insert(&self, tenant: &Tenant) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            INSERT INTO tenants (
                slug, display_name, status, tier, keycloak_realm, openbao_namespace,
                max_concurrent_executions, max_agents, max_storage_gb,
                created_at, updated_at, deleted_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(tenant.slug.as_str())
        .bind(&tenant.display_name)
        .bind(status_to_str(&tenant.status))
        .bind(tier_to_str(&tenant.tier))
        .bind(&tenant.keycloak_realm)
        .bind(&tenant.openbao_namespace)
        .bind(tenant.quotas.max_concurrent_executions as i32)
        .bind(tenant.quotas.max_agents as i32)
        .bind(tenant.quotas.max_storage_gb)
        .bind(tenant.created_at)
        .bind(tenant.updated_at)
        .bind(tenant.deleted_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to insert tenant: {e}")))?;

        Ok(())
    }

    async fn update_status(
        &self,
        slug: &TenantId,
        status: &TenantStatus,
    ) -> Result<(), RepositoryError> {
        let deleted_at = if matches!(status, TenantStatus::Deleted) {
            Some(chrono::Utc::now())
        } else {
            None
        };

        sqlx::query(
            r#"
            UPDATE tenants
            SET status = $2, updated_at = NOW(), deleted_at = COALESCE($3, deleted_at)
            WHERE slug = $1
            "#,
        )
        .bind(slug.as_str())
        .bind(status_to_str(status))
        .bind(deleted_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to update tenant status: {e}")))?;

        Ok(())
    }

    async fn update_quotas(
        &self,
        slug: &TenantId,
        quotas: &TenantQuotas,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            UPDATE tenants
            SET max_concurrent_executions = $2, max_agents = $3, max_storage_gb = $4,
                updated_at = NOW()
            WHERE slug = $1
            "#,
        )
        .bind(slug.as_str())
        .bind(quotas.max_concurrent_executions as i32)
        .bind(quotas.max_agents as i32)
        .bind(quotas.max_storage_gb)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to update tenant quotas: {e}")))?;

        Ok(())
    }
}
