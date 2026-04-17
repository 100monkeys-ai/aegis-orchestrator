// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Script Repository — ADR-110 §D7
//!
//! Production [`ScriptRepository`] implementation backed by the `scripts`
//! and `script_versions` tables introduced in migration `021_scripts.sql`.
//!
//! ## Schema Summary
//!
//! ```sql
//! scripts (id, tenant_id, created_by, name, description, code, tags,
//!          visibility, version, created_at, updated_at, deleted_at)
//!
//! script_versions (script_id, version, name, description, code, tags,
//!                  updated_by, updated_at)
//! ```
//!
//! The `save` method is an atomic upsert: it inserts or updates the
//! current-version row in `scripts` AND appends a row to
//! `script_versions` in a single transaction.

use crate::domain::repository::RepositoryError;
use crate::domain::script::{Script, ScriptId, ScriptRepository, ScriptVersionSummary, Visibility};
use crate::domain::shared_kernel::TenantId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

pub struct PostgresScriptRepository {
    pool: PgPool,
}

impl PostgresScriptRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

// ============================================================================
// Mapping helpers
// ============================================================================

fn hydrate_script(row: &sqlx::postgres::PgRow) -> Result<Script, RepositoryError> {
    let id: Uuid = row
        .try_get("id")
        .map_err(|e| RepositoryError::Serialization(format!("id: {e}")))?;
    let tenant_id_str: String = row
        .try_get("tenant_id")
        .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?;
    let created_by: String = row
        .try_get("created_by")
        .map_err(|e| RepositoryError::Serialization(format!("created_by: {e}")))?;
    let name: String = row
        .try_get("name")
        .map_err(|e| RepositoryError::Serialization(format!("name: {e}")))?;
    let description: String = row
        .try_get("description")
        .map_err(|e| RepositoryError::Serialization(format!("description: {e}")))?;
    let code: String = row
        .try_get("code")
        .map_err(|e| RepositoryError::Serialization(format!("code: {e}")))?;
    let tags: Vec<String> = row
        .try_get("tags")
        .map_err(|e| RepositoryError::Serialization(format!("tags: {e}")))?;
    let visibility_text: String = row
        .try_get("visibility")
        .map_err(|e| RepositoryError::Serialization(format!("visibility: {e}")))?;
    let version: i32 = row
        .try_get("version")
        .map_err(|e| RepositoryError::Serialization(format!("version: {e}")))?;
    let created_at: DateTime<Utc> = row
        .try_get("created_at")
        .map_err(|e| RepositoryError::Serialization(format!("created_at: {e}")))?;
    let updated_at: DateTime<Utc> = row
        .try_get("updated_at")
        .map_err(|e| RepositoryError::Serialization(format!("updated_at: {e}")))?;
    let deleted_at: Option<DateTime<Utc>> = row
        .try_get("deleted_at")
        .map_err(|e| RepositoryError::Serialization(format!("deleted_at: {e}")))?;

    let tenant_id = TenantId::new(tenant_id_str)
        .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?;

    let visibility = Visibility::from_str_ci(&visibility_text).ok_or_else(|| {
        RepositoryError::Serialization(format!("unknown visibility value: {visibility_text}"))
    })?;

    Ok(Script {
        id: ScriptId(id),
        tenant_id,
        created_by,
        name,
        description,
        code,
        tags,
        visibility,
        version: version as u32,
        created_at,
        updated_at,
        deleted_at,
        domain_events: Vec::new(),
    })
}

/// Hydrate a [`Script`] from a `script_versions` row joined with its
/// parent `scripts` row. Used by [`ScriptRepository::get_version`].
fn hydrate_script_from_version(row: &sqlx::postgres::PgRow) -> Result<Script, RepositoryError> {
    let id: Uuid = row
        .try_get("script_id")
        .map_err(|e| RepositoryError::Serialization(format!("script_id: {e}")))?;
    let tenant_id_str: String = row
        .try_get("tenant_id")
        .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?;
    let created_by: String = row
        .try_get("created_by")
        .map_err(|e| RepositoryError::Serialization(format!("created_by: {e}")))?;
    let name: String = row
        .try_get("name")
        .map_err(|e| RepositoryError::Serialization(format!("name: {e}")))?;
    let description: String = row
        .try_get("description")
        .map_err(|e| RepositoryError::Serialization(format!("description: {e}")))?;
    let code: String = row
        .try_get("code")
        .map_err(|e| RepositoryError::Serialization(format!("code: {e}")))?;
    let tags: Vec<String> = row
        .try_get("tags")
        .map_err(|e| RepositoryError::Serialization(format!("tags: {e}")))?;
    let version: i32 = row
        .try_get("version")
        .map_err(|e| RepositoryError::Serialization(format!("version: {e}")))?;
    let created_at: DateTime<Utc> = row
        .try_get("created_at")
        .map_err(|e| RepositoryError::Serialization(format!("created_at: {e}")))?;
    let updated_at: DateTime<Utc> = row
        .try_get("updated_at")
        .map_err(|e| RepositoryError::Serialization(format!("updated_at: {e}")))?;

    let tenant_id = TenantId::new(tenant_id_str)
        .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?;

    Ok(Script {
        id: ScriptId(id),
        tenant_id,
        created_by,
        name,
        description,
        code,
        tags,
        visibility: Visibility::Private,
        version: version as u32,
        created_at,
        updated_at,
        // Historical snapshots never carry a delete marker — deletion is
        // recorded on the parent `scripts` row.
        deleted_at: None,
        domain_events: Vec::new(),
    })
}

// ============================================================================
// Repository implementation
// ============================================================================

#[async_trait]
impl ScriptRepository for PostgresScriptRepository {
    async fn save(&self, script: &Script) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            RepositoryError::Database(format!("failed to begin script save tx: {e}"))
        })?;

        sqlx::query(
            r#"
            INSERT INTO scripts (
                id, tenant_id, created_by, name, description, code, tags,
                visibility, version, created_at, updated_at, deleted_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (id) DO UPDATE SET
                name        = EXCLUDED.name,
                description = EXCLUDED.description,
                code        = EXCLUDED.code,
                tags        = EXCLUDED.tags,
                visibility  = EXCLUDED.visibility,
                version     = EXCLUDED.version,
                updated_at  = EXCLUDED.updated_at,
                deleted_at  = EXCLUDED.deleted_at
            "#,
        )
        .bind(script.id.0)
        .bind(script.tenant_id.as_str())
        .bind(&script.created_by)
        .bind(&script.name)
        .bind(&script.description)
        .bind(&script.code)
        .bind(&script.tags)
        .bind(script.visibility.as_str())
        .bind(script.version as i32)
        .bind(script.created_at)
        .bind(script.updated_at)
        .bind(script.deleted_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("failed to save script {}: {e}", script.id))
        })?;

        // Append a history row ONLY for active (non-deleted) rows. A
        // soft-delete save preserves the existing history untouched.
        // The `ON CONFLICT` clause below is belt-and-suspenders for
        // idempotent `save` calls on the same version (e.g. when the
        // service rewrites an unchanged aggregate).
        if script.deleted_at.is_none() {
            sqlx::query(
                r#"
                INSERT INTO script_versions (
                    script_id, version, name, description, code, tags,
                    updated_by, updated_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (script_id, version) DO UPDATE SET
                    name        = EXCLUDED.name,
                    description = EXCLUDED.description,
                    code        = EXCLUDED.code,
                    tags        = EXCLUDED.tags,
                    updated_by  = EXCLUDED.updated_by,
                    updated_at  = EXCLUDED.updated_at
                "#,
            )
            .bind(script.id.0)
            .bind(script.version as i32)
            .bind(&script.name)
            .bind(&script.description)
            .bind(&script.code)
            .bind(&script.tags)
            .bind(&script.created_by)
            .bind(script.updated_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                RepositoryError::Database(format!(
                    "failed to append version-history row for script {} v{}: {e}",
                    script.id, script.version
                ))
            })?;
        }

        tx.commit().await.map_err(|e| {
            RepositoryError::Database(format!("failed to commit script save tx: {e}"))
        })?;
        Ok(())
    }

    async fn find_by_id(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
    ) -> Result<Option<Script>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT * FROM scripts
            WHERE id = $1 AND tenant_id = $2 AND deleted_at IS NULL
            "#,
        )
        .bind(id.0)
        .bind(tenant_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("failed to find script {id}: {e}")))?;

        match row {
            Some(row) => Ok(Some(hydrate_script(&row)?)),
            None => Ok(None),
        }
    }

    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<Vec<Script>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM scripts
            WHERE tenant_id = $1 AND created_by = $2 AND deleted_at IS NULL
            ORDER BY updated_at DESC
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(created_by)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!(
                "failed to list scripts for tenant {} owner {created_by}: {e}",
                tenant_id.as_str()
            ))
        })?;

        let mut scripts = Vec::with_capacity(rows.len());
        for row in &rows {
            scripts.push(hydrate_script(row)?);
        }
        Ok(scripts)
    }

    async fn find_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Script>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT * FROM scripts
            WHERE tenant_id = $1 AND name = $2 AND deleted_at IS NULL
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("failed to look up script by name {name}: {e}"))
        })?;

        match row {
            Some(row) => Ok(Some(hydrate_script(&row)?)),
            None => Ok(None),
        }
    }

    async fn list_versions(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
    ) -> Result<Vec<ScriptVersionSummary>, RepositoryError> {
        // Join against `scripts` to enforce tenant isolation even for
        // soft-deleted parents (so the owner can still audit the
        // history after deleting).
        let rows = sqlx::query(
            r#"
            SELECT sv.version, sv.updated_at, sv.updated_by
            FROM script_versions sv
            INNER JOIN scripts s ON s.id = sv.script_id
            WHERE sv.script_id = $1 AND s.tenant_id = $2
            ORDER BY sv.version ASC
            "#,
        )
        .bind(id.0)
        .bind(tenant_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("failed to list versions for script {id}: {e}"))
        })?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let version: i32 = row
                .try_get("version")
                .map_err(|e| RepositoryError::Serialization(format!("version: {e}")))?;
            let updated_at: DateTime<Utc> = row
                .try_get("updated_at")
                .map_err(|e| RepositoryError::Serialization(format!("updated_at: {e}")))?;
            let updated_by: String = row
                .try_get("updated_by")
                .map_err(|e| RepositoryError::Serialization(format!("updated_by: {e}")))?;
            out.push(ScriptVersionSummary {
                version: version as u32,
                updated_at,
                updated_by,
            });
        }
        Ok(out)
    }

    async fn get_version(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
        version: u32,
    ) -> Result<Option<Script>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT
                sv.script_id   AS script_id,
                sv.version     AS version,
                sv.name        AS name,
                sv.description AS description,
                sv.code        AS code,
                sv.tags        AS tags,
                sv.updated_at  AS updated_at,
                s.tenant_id    AS tenant_id,
                s.created_by   AS created_by,
                s.created_at   AS created_at
            FROM script_versions sv
            INNER JOIN scripts s ON s.id = sv.script_id
            WHERE sv.script_id = $1 AND s.tenant_id = $2 AND sv.version = $3
            "#,
        )
        .bind(id.0)
        .bind(tenant_id.as_str())
        .bind(version as i32)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!(
                "failed to fetch script {id} version {version}: {e}"
            ))
        })?;

        match row {
            Some(row) => Ok(Some(hydrate_script_from_version(&row)?)),
            None => Ok(None),
        }
    }

    async fn soft_delete(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            UPDATE scripts
            SET deleted_at = NOW(), updated_at = NOW()
            WHERE id = $1 AND tenant_id = $2 AND deleted_at IS NULL
            "#,
        )
        .bind(id.0)
        .bind(tenant_id.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("failed to soft-delete script {id}: {e}"))
        })?;
        Ok(())
    }

    async fn count_by_owner(
        &self,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<u32, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT COUNT(*)::BIGINT AS cnt
            FROM scripts
            WHERE tenant_id = $1 AND created_by = $2 AND deleted_at IS NULL
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(created_by)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("failed to count scripts for {created_by}: {e}"))
        })?;

        let count: i64 = row
            .try_get("cnt")
            .map_err(|e| RepositoryError::Serialization(format!("cnt: {e}")))?;
        Ok(count as u32)
    }
}
