// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Canvas Session Repository — ADR-106
//!
//! Production [`CanvasSessionRepository`] implementation backed by the
//! `canvas_sessions` table introduced in migration
//! `020_canvas_sessions.sql`.
//!
//! ## Schema Summary
//!
//! ```sql
//! canvas_sessions (id, tenant_id, conversation_id, workspace_volume_id,
//!                  git_binding_id, workspace_mode_kind, workspace_mode_label,
//!                  status, created_at, last_active_at)
//! ```
//!
//! The `workspace_mode` variant + payload is encoded as two columns:
//! `workspace_mode_kind` (`'Ephemeral' | 'Persistent' | 'GitLinked'`) and
//! `workspace_mode_label` (the `volume_label` for Persistent, `NULL` otherwise).
//! The `binding_id` payload for the `GitLinked` variant is recovered from the
//! row's `git_binding_id` column — there is exactly one source of truth for the
//! bound repository.

use crate::domain::canvas::{
    CanvasSession, CanvasSessionId, CanvasSessionRepository, CanvasSessionStatus, ConversationId,
    WorkspaceMode,
};
use crate::domain::git_repo::GitRepoBindingId;
use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::{TenantId, VolumeId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

pub struct PostgresCanvasSessionRepository {
    pool: PgPool,
}

impl PostgresCanvasSessionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

// ============================================================================
// Mapping helpers
// ============================================================================

/// Encode a [`WorkspaceMode`] into `(kind, label)` columns.
fn workspace_mode_to_db(mode: &WorkspaceMode) -> (&'static str, Option<&str>) {
    match mode {
        WorkspaceMode::Ephemeral => ("Ephemeral", None),
        WorkspaceMode::Persistent { volume_label } => ("Persistent", Some(volume_label.as_str())),
        WorkspaceMode::GitLinked { .. } => ("GitLinked", None),
    }
}

/// Reconstruct a [`WorkspaceMode`] from its `(kind, label, git_binding_id)`
/// column triplet.
fn db_to_workspace_mode(
    kind: &str,
    label: Option<String>,
    git_binding_id: Option<GitRepoBindingId>,
) -> Result<WorkspaceMode, RepositoryError> {
    match kind {
        "Ephemeral" => Ok(WorkspaceMode::Ephemeral),
        "Persistent" => {
            let volume_label = label.ok_or_else(|| {
                RepositoryError::Serialization(
                    "Persistent workspace_mode requires workspace_mode_label".to_string(),
                )
            })?;
            Ok(WorkspaceMode::Persistent { volume_label })
        }
        "GitLinked" => {
            let binding_id = git_binding_id.ok_or_else(|| {
                RepositoryError::Serialization(
                    "GitLinked workspace_mode requires git_binding_id".to_string(),
                )
            })?;
            Ok(WorkspaceMode::GitLinked { binding_id })
        }
        other => Err(RepositoryError::Serialization(format!(
            "Unknown workspace_mode_kind: {other}"
        ))),
    }
}

fn status_to_db(status: &CanvasSessionStatus) -> &'static str {
    match status {
        CanvasSessionStatus::Initializing => "Initializing",
        CanvasSessionStatus::Ready => "Ready",
        CanvasSessionStatus::Idle => "Idle",
        CanvasSessionStatus::Terminated => "Terminated",
    }
}

fn db_to_status(s: &str) -> Result<CanvasSessionStatus, RepositoryError> {
    match s {
        "Initializing" => Ok(CanvasSessionStatus::Initializing),
        "Ready" => Ok(CanvasSessionStatus::Ready),
        "Idle" => Ok(CanvasSessionStatus::Idle),
        "Terminated" => Ok(CanvasSessionStatus::Terminated),
        other => Err(RepositoryError::Serialization(format!(
            "Unknown canvas session status: {other}"
        ))),
    }
}

/// Hydrate a [`CanvasSession`] from a `canvas_sessions` row.
fn hydrate_session(row: &sqlx::postgres::PgRow) -> Result<CanvasSession, RepositoryError> {
    let id: Uuid = row
        .try_get("id")
        .map_err(|e| RepositoryError::Serialization(format!("id: {e}")))?;
    let tenant_id_str: String = row
        .try_get("tenant_id")
        .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?;
    let conversation_id: Uuid = row
        .try_get("conversation_id")
        .map_err(|e| RepositoryError::Serialization(format!("conversation_id: {e}")))?;
    let workspace_volume_id: Uuid = row
        .try_get("workspace_volume_id")
        .map_err(|e| RepositoryError::Serialization(format!("workspace_volume_id: {e}")))?;
    let git_binding_id: Option<Uuid> = row
        .try_get("git_binding_id")
        .map_err(|e| RepositoryError::Serialization(format!("git_binding_id: {e}")))?;
    let workspace_mode_kind: String = row
        .try_get("workspace_mode_kind")
        .map_err(|e| RepositoryError::Serialization(format!("workspace_mode_kind: {e}")))?;
    let workspace_mode_label: Option<String> = row
        .try_get("workspace_mode_label")
        .map_err(|e| RepositoryError::Serialization(format!("workspace_mode_label: {e}")))?;
    let status_text: String = row
        .try_get("status")
        .map_err(|e| RepositoryError::Serialization(format!("status: {e}")))?;
    let created_at: DateTime<Utc> = row
        .try_get("created_at")
        .map_err(|e| RepositoryError::Serialization(format!("created_at: {e}")))?;
    let last_active_at: DateTime<Utc> = row
        .try_get("last_active_at")
        .map_err(|e| RepositoryError::Serialization(format!("last_active_at: {e}")))?;

    let tenant_id = TenantId::new(tenant_id_str)
        .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?;
    let git_binding_id = git_binding_id.map(GitRepoBindingId);
    let workspace_mode =
        db_to_workspace_mode(&workspace_mode_kind, workspace_mode_label, git_binding_id)?;

    Ok(CanvasSession {
        id: CanvasSessionId(id),
        tenant_id,
        conversation_id: ConversationId(conversation_id),
        workspace_volume_id: VolumeId(workspace_volume_id),
        git_binding_id,
        workspace_mode,
        status: db_to_status(&status_text)?,
        created_at,
        last_active_at,
        domain_events: Vec::new(),
    })
}

// ============================================================================
// Repository implementation
// ============================================================================

#[async_trait]
impl CanvasSessionRepository for PostgresCanvasSessionRepository {
    async fn save(&self, session: &CanvasSession) -> Result<(), RepositoryError> {
        let (mode_kind, mode_label) = workspace_mode_to_db(&session.workspace_mode);
        let status_text = status_to_db(&session.status);

        sqlx::query(
            r#"
            INSERT INTO canvas_sessions (
                id, tenant_id, conversation_id, workspace_volume_id,
                git_binding_id, workspace_mode_kind, workspace_mode_label,
                status, created_at, last_active_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT (id) DO UPDATE SET
                tenant_id             = EXCLUDED.tenant_id,
                conversation_id       = EXCLUDED.conversation_id,
                workspace_volume_id   = EXCLUDED.workspace_volume_id,
                git_binding_id        = EXCLUDED.git_binding_id,
                workspace_mode_kind   = EXCLUDED.workspace_mode_kind,
                workspace_mode_label  = EXCLUDED.workspace_mode_label,
                status                = EXCLUDED.status,
                last_active_at        = EXCLUDED.last_active_at
            "#,
        )
        .bind(session.id.0)
        .bind(session.tenant_id.as_str())
        .bind(session.conversation_id.0)
        .bind(session.workspace_volume_id.0)
        .bind(session.git_binding_id.map(|id| id.0))
        .bind(mode_kind)
        .bind(mode_label)
        .bind(status_text)
        .bind(session.created_at)
        .bind(session.last_active_at)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("Failed to save canvas_session {}: {e}", session.id))
        })?;

        Ok(())
    }

    async fn find_by_id(
        &self,
        id: &CanvasSessionId,
    ) -> Result<Option<CanvasSession>, RepositoryError> {
        let row = sqlx::query("SELECT * FROM canvas_sessions WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                RepositoryError::Database(format!("Failed to find canvas_session {id}: {e}"))
            })?;

        match row {
            Some(row) => Ok(Some(hydrate_session(&row)?)),
            None => Ok(None),
        }
    }

    async fn find_by_conversation_id(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<CanvasSession>, RepositoryError> {
        let row = sqlx::query("SELECT * FROM canvas_sessions WHERE conversation_id = $1")
            .bind(conversation_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                RepositoryError::Database(format!(
                    "Failed to find canvas_session for conversation {conversation_id}: {e}"
                ))
            })?;

        match row {
            Some(row) => Ok(Some(hydrate_session(&row)?)),
            None => Ok(None),
        }
    }

    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<CanvasSession>, RepositoryError> {
        let rows =
            sqlx::query("SELECT * FROM canvas_sessions WHERE tenant_id = $1 ORDER BY created_at")
                .bind(tenant_id.as_str())
                .fetch_all(&self.pool)
                .await
                .map_err(|e| {
                    RepositoryError::Database(format!(
                        "Failed to list canvas_sessions for tenant {}: {e}",
                        tenant_id.as_str()
                    ))
                })?;

        let mut sessions = Vec::with_capacity(rows.len());
        for row in &rows {
            sessions.push(hydrate_session(row)?);
        }
        Ok(sessions)
    }

    async fn delete(&self, id: &CanvasSessionId) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM canvas_sessions WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                RepositoryError::Database(format!("Failed to delete canvas_session {id}: {e}"))
            })?;
        Ok(())
    }
}
