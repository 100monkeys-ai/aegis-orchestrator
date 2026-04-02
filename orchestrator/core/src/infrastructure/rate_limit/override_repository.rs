// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Rate Limit Override Repository (ADR-072)
//!
//! CRUD operations for the `rate_limit_overrides` table, plus current-usage
//! queries against `rate_limit_counters`.  Used by the admin HTTP endpoints
//! at `/v1/admin/rate-limits/overrides` and `/v1/admin/rate-limits/usage`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

// ── Row Types ────────────────────────────────────────────────────────────────

/// A single row from `rate_limit_overrides`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitOverrideRow {
    pub id: Uuid,
    pub tenant_id: Option<String>,
    pub user_id: Option<String>,
    pub resource_type: String,
    pub bucket: String,
    pub limit_value: i64,
    pub burst_value: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A single row from `rate_limit_counters` (current usage snapshot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRow {
    pub scope_type: String,
    pub scope_id: String,
    pub resource_type: String,
    pub bucket: String,
    pub window_start: DateTime<Utc>,
    pub counter: i64,
}

// ── Request Types ────────────────────────────────────────────────────────────

/// Body for `POST /v1/admin/rate-limits/overrides`.
#[derive(Debug, Deserialize)]
pub struct CreateOverrideRequest {
    pub tenant_id: Option<String>,
    pub user_id: Option<String>,
    pub resource_type: String,
    pub bucket: String,
    pub limit_value: i64,
    pub burst_value: Option<i64>,
}

// ── Repository ───────────────────────────────────────────────────────────────

/// Thin repository around the `rate_limit_overrides` and
/// `rate_limit_counters` tables.
pub struct RateLimitOverrideRepository {
    pool: PgPool,
}

impl RateLimitOverrideRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Map a [`sqlx::postgres::PgRow`] into a [`RateLimitOverrideRow`].
    fn map_override_row(row: &sqlx::postgres::PgRow) -> RateLimitOverrideRow {
        RateLimitOverrideRow {
            id: row.get("id"),
            tenant_id: row.get("tenant_id"),
            user_id: row.get("user_id"),
            resource_type: row.get("resource_type"),
            bucket: row.get("bucket"),
            limit_value: row.get("limit_value"),
            burst_value: row.get("burst_value"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }

    /// Map a [`sqlx::postgres::PgRow`] into a [`UsageRow`].
    fn map_usage_row(row: &sqlx::postgres::PgRow) -> UsageRow {
        UsageRow {
            scope_type: row.get("scope_type"),
            scope_id: row.get("scope_id"),
            resource_type: row.get("resource_type"),
            bucket: row.get("bucket"),
            window_start: row.get("window_start"),
            counter: row.get("counter"),
        }
    }

    /// List overrides, optionally filtered by `tenant_id` and/or `user_id`.
    pub async fn list(
        &self,
        tenant_id: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<Vec<RateLimitOverrideRow>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT id, tenant_id, user_id, resource_type, bucket,
                   limit_value, burst_value, created_at, updated_at
            FROM rate_limit_overrides
            WHERE ($1::text IS NULL OR tenant_id = $1)
              AND ($2::text IS NULL OR user_id = $2)
            ORDER BY created_at DESC
            "#,
        )
        .bind(tenant_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(Self::map_override_row).collect())
    }

    /// Insert or update an override.  The unique constraint is
    /// `(tenant_id, user_id, resource_type, bucket)`.
    pub async fn upsert(
        &self,
        req: &CreateOverrideRequest,
    ) -> Result<RateLimitOverrideRow, sqlx::Error> {
        let row = sqlx::query(
            r#"
            INSERT INTO rate_limit_overrides
                (tenant_id, user_id, resource_type, bucket, limit_value, burst_value)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (tenant_id, user_id, resource_type, bucket)
            DO UPDATE SET
                limit_value = EXCLUDED.limit_value,
                burst_value = EXCLUDED.burst_value,
                updated_at  = NOW()
            RETURNING id, tenant_id, user_id, resource_type, bucket,
                      limit_value, burst_value, created_at, updated_at
            "#,
        )
        .bind(&req.tenant_id)
        .bind(&req.user_id)
        .bind(&req.resource_type)
        .bind(&req.bucket)
        .bind(req.limit_value)
        .bind(req.burst_value)
        .fetch_one(&self.pool)
        .await?;

        Ok(Self::map_override_row(&row))
    }

    /// Delete an override by `id`.  Returns `true` if a row was actually removed.
    pub async fn delete(&self, id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM rate_limit_overrides WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Return current counter rows for a given scope (e.g. `scope_type = "user"`,
    /// `scope_id = "u-123"`).
    pub async fn get_usage(
        &self,
        scope_type: &str,
        scope_id: &str,
    ) -> Result<Vec<UsageRow>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT scope_type, scope_id, resource_type, bucket,
                   window_start, counter
            FROM rate_limit_counters
            WHERE scope_type = $1 AND scope_id = $2
            ORDER BY resource_type, bucket, window_start DESC
            "#,
        )
        .bind(scope_type)
        .bind(scope_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(Self::map_usage_row).collect())
    }
}
