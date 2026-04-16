// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! PostgreSQL repository for API key management (ADR-093).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

// ── Row Types ────────────────────────────────────────────────────────────────

/// A single row from `api_keys`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRow {
    pub id: Uuid,
    pub user_id: String,
    pub name: String,
    pub key_hash: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub status: String,
    pub tenant_id: String,
    pub aegis_role: Option<String>,
    pub zaru_tier: Option<String>,
}

/// Data required to insert a new `api_keys` row.
#[derive(Debug)]
pub struct CreateApiKeyRow {
    pub id: Uuid,
    pub user_id: String,
    pub name: String,
    pub key_hash: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub tenant_id: String,
    pub aegis_role: Option<String>,
    pub zaru_tier: Option<String>,
}

// ── Repository ───────────────────────────────────────────────────────────────

/// Thin repository over the `api_keys` table.
pub struct PostgresApiKeyRepository {
    pool: PgPool,
}

impl PostgresApiKeyRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Map a [`sqlx::postgres::PgRow`] into an [`ApiKeyRow`].
    fn map_row(row: &sqlx::postgres::PgRow) -> ApiKeyRow {
        ApiKeyRow {
            id: row.get("id"),
            user_id: row.get("user_id"),
            name: row.get("name"),
            key_hash: row.get("key_hash"),
            scopes: row.get("scopes"),
            expires_at: row.get("expires_at"),
            last_used_at: row.get("last_used_at"),
            created_at: row.get("created_at"),
            status: row.get("status"),
            tenant_id: row.get("tenant_id"),
            aegis_role: row.get("aegis_role"),
            zaru_tier: row.get("zaru_tier"),
        }
    }

    /// Column list shared by all SELECT queries.
    const SELECT_COLS: &str = "id, user_id, name, key_hash, scopes, expires_at, last_used_at, created_at, status, tenant_id, aegis_role, zaru_tier";

    /// List all API keys belonging to `user_id`, newest first.
    pub async fn list_for_user(&self, user_id: &str) -> Result<Vec<ApiKeyRow>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM api_keys WHERE user_id = $1 ORDER BY created_at DESC",
            Self::SELECT_COLS
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().map(Self::map_row).collect())
    }

    /// Find an active, non-expired API key by its SHA-256 hash.
    /// Also bumps `last_used_at` to track usage.
    pub async fn find_by_key_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRow>, sqlx::Error> {
        let sql = format!(
            r#"UPDATE api_keys
               SET last_used_at = NOW()
               WHERE key_hash = $1 AND status = 'active'
                 AND (expires_at IS NULL OR expires_at > NOW())
               RETURNING {}"#,
            Self::SELECT_COLS
        );
        let row = sqlx::query(&sql)
            .bind(key_hash)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.as_ref().map(Self::map_row))
    }

    /// Insert a new API key row and return the persisted record.
    pub async fn create(&self, row: &CreateApiKeyRow) -> Result<ApiKeyRow, sqlx::Error> {
        let sql = format!(
            r#"INSERT INTO api_keys (id, user_id, name, key_hash, scopes, expires_at, tenant_id, aegis_role, zaru_tier)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
               RETURNING {}"#,
            Self::SELECT_COLS
        );
        let result = sqlx::query(&sql)
            .bind(row.id)
            .bind(&row.user_id)
            .bind(&row.name)
            .bind(&row.key_hash)
            .bind(&row.scopes)
            .bind(row.expires_at)
            .bind(&row.tenant_id)
            .bind(&row.aegis_role)
            .bind(&row.zaru_tier)
            .fetch_one(&self.pool)
            .await?;

        Ok(Self::map_row(&result))
    }

    /// Revoke an API key.  Returns `true` if the key was found and revoked,
    /// `false` if it did not exist or was already revoked.
    pub async fn revoke(&self, id: Uuid, user_id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE api_keys SET status = 'revoked'
            WHERE id = $1 AND user_id = $2 AND status = 'active'
            "#,
        )
        .bind(id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }
}
