// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Git Repository Binding Repository — ADR-081
//!
//! Production [`GitRepoBindingRepository`] implementation backed by the
//! `git_repo_bindings` table introduced in migration `019_git_repo_bindings.sql`.
//!
//! ## Schema Summary
//!
//! ```sql
//! git_repo_bindings (id, tenant_id, credential_binding_id, repo_url,
//!                    git_ref_type, git_ref_value, sparse_paths, volume_id,
//!                    label, status, status_error, clone_strategy,
//!                    last_cloned_at, last_commit_sha, auto_refresh,
//!                    webhook_secret_ciphertext, webhook_lookup_hash,
//!                    created_at, updated_at)
//! ```
//!
//! Audit 002 §4.37.13 — `webhook_secret` (cleartext) was replaced with the
//! `webhook_secret_ciphertext` (OpenBao Transit ciphertext) +
//! `webhook_lookup_hash` (deterministic SHA-256 lookup index) pair via
//! migration 031.

use crate::domain::credential::CredentialBindingId;
use crate::domain::git_repo::{
    CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId, GitRepoBindingRepository,
    GitRepoStatus,
};
use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::{TenantId, VolumeId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

pub struct PostgresGitRepoBindingRepository {
    pool: PgPool,
}

impl PostgresGitRepoBindingRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

// ============================================================================
// Mapping helpers
// ============================================================================

fn git_ref_to_db(git_ref: &GitRef) -> (&'static str, &str) {
    match git_ref {
        GitRef::Branch(name) => ("branch", name.as_str()),
        GitRef::Tag(name) => ("tag", name.as_str()),
        GitRef::Commit(sha) => ("commit", sha.as_str()),
    }
}

fn db_to_git_ref(ref_type: &str, ref_value: &str) -> Result<GitRef, RepositoryError> {
    match ref_type {
        "branch" => Ok(GitRef::Branch(ref_value.to_string())),
        "tag" => Ok(GitRef::Tag(ref_value.to_string())),
        "commit" => Ok(GitRef::Commit(ref_value.to_string())),
        other => Err(RepositoryError::Serialization(format!(
            "Unknown git_ref_type: {other}"
        ))),
    }
}

/// Encode [`GitRepoStatus`] into a `(status_text, status_error)` pair.
fn status_to_db(status: &GitRepoStatus) -> (&'static str, Option<&str>) {
    match status {
        GitRepoStatus::Pending => ("Pending", None),
        GitRepoStatus::Cloning => ("Cloning", None),
        GitRepoStatus::Ready => ("Ready", None),
        GitRepoStatus::Refreshing => ("Refreshing", None),
        GitRepoStatus::Failed { error } => ("Failed", Some(error.as_str())),
        GitRepoStatus::Deleted => ("Deleted", None),
    }
}

fn db_to_status(
    status_text: &str,
    status_error: Option<String>,
) -> Result<GitRepoStatus, RepositoryError> {
    match status_text {
        "Pending" => Ok(GitRepoStatus::Pending),
        "Cloning" => Ok(GitRepoStatus::Cloning),
        "Ready" => Ok(GitRepoStatus::Ready),
        "Refreshing" => Ok(GitRepoStatus::Refreshing),
        "Failed" => Ok(GitRepoStatus::Failed {
            error: status_error.unwrap_or_default(),
        }),
        "Deleted" => Ok(GitRepoStatus::Deleted),
        other => Err(RepositoryError::Serialization(format!(
            "Unknown git repo status: {other}"
        ))),
    }
}

/// Encode [`CloneStrategy`] into a text value stored in `clone_strategy`.
///
/// For `EphemeralCli { reason }` we prefix with `EphemeralCli:` so the reason
/// is preserved on round-trip without a separate column.
fn clone_strategy_to_db(strategy: &CloneStrategy) -> String {
    match strategy {
        CloneStrategy::Libgit2 => "Libgit2".to_string(),
        CloneStrategy::EphemeralCli { reason } => format!("EphemeralCli:{reason}"),
    }
}

fn db_to_clone_strategy(s: &str) -> Result<CloneStrategy, RepositoryError> {
    if s == "Libgit2" {
        return Ok(CloneStrategy::Libgit2);
    }
    if let Some(reason) = s.strip_prefix("EphemeralCli:") {
        return Ok(CloneStrategy::EphemeralCli {
            reason: reason.to_string(),
        });
    }
    Err(RepositoryError::Serialization(format!(
        "Unknown clone_strategy: {s}"
    )))
}

/// Hydrate a [`GitRepoBinding`] from a `git_repo_bindings` row.
fn hydrate_binding(row: &sqlx::postgres::PgRow) -> Result<GitRepoBinding, RepositoryError> {
    let id: Uuid = row
        .try_get("id")
        .map_err(|e| RepositoryError::Serialization(format!("id: {e}")))?;
    let tenant_id_str: String = row
        .try_get("tenant_id")
        .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?;
    let credential_binding_id: Option<Uuid> = row
        .try_get("credential_binding_id")
        .map_err(|e| RepositoryError::Serialization(format!("credential_binding_id: {e}")))?;
    let repo_url: String = row
        .try_get("repo_url")
        .map_err(|e| RepositoryError::Serialization(format!("repo_url: {e}")))?;
    let git_ref_type: String = row
        .try_get("git_ref_type")
        .map_err(|e| RepositoryError::Serialization(format!("git_ref_type: {e}")))?;
    let git_ref_value: String = row
        .try_get("git_ref_value")
        .map_err(|e| RepositoryError::Serialization(format!("git_ref_value: {e}")))?;
    let sparse_paths_json: Option<serde_json::Value> = row
        .try_get("sparse_paths")
        .map_err(|e| RepositoryError::Serialization(format!("sparse_paths: {e}")))?;
    let volume_id: Uuid = row
        .try_get("volume_id")
        .map_err(|e| RepositoryError::Serialization(format!("volume_id: {e}")))?;
    let label: String = row
        .try_get("label")
        .map_err(|e| RepositoryError::Serialization(format!("label: {e}")))?;
    let status_text: String = row
        .try_get("status")
        .map_err(|e| RepositoryError::Serialization(format!("status: {e}")))?;
    let status_error: Option<String> = row
        .try_get("status_error")
        .map_err(|e| RepositoryError::Serialization(format!("status_error: {e}")))?;
    let clone_strategy_text: String = row
        .try_get("clone_strategy")
        .map_err(|e| RepositoryError::Serialization(format!("clone_strategy: {e}")))?;
    let last_cloned_at: Option<DateTime<Utc>> = row
        .try_get("last_cloned_at")
        .map_err(|e| RepositoryError::Serialization(format!("last_cloned_at: {e}")))?;
    let last_commit_sha: Option<String> = row
        .try_get("last_commit_sha")
        .map_err(|e| RepositoryError::Serialization(format!("last_commit_sha: {e}")))?;
    let auto_refresh: bool = row
        .try_get("auto_refresh")
        .map_err(|e| RepositoryError::Serialization(format!("auto_refresh: {e}")))?;
    // Audit 002 §4.37.13 — store the Transit ciphertext + lookup hash
    // pair instead of the cleartext webhook secret.
    let webhook_secret_ciphertext: Option<String> = row
        .try_get("webhook_secret_ciphertext")
        .map_err(|e| RepositoryError::Serialization(format!("webhook_secret_ciphertext: {e}")))?;
    let webhook_lookup_hash: Option<String> = row
        .try_get("webhook_lookup_hash")
        .map_err(|e| RepositoryError::Serialization(format!("webhook_lookup_hash: {e}")))?;
    let created_at: DateTime<Utc> = row
        .try_get("created_at")
        .map_err(|e| RepositoryError::Serialization(format!("created_at: {e}")))?;
    let updated_at: DateTime<Utc> = row
        .try_get("updated_at")
        .map_err(|e| RepositoryError::Serialization(format!("updated_at: {e}")))?;

    let tenant_id = TenantId::new(tenant_id_str)
        .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?;

    let sparse_paths: Option<Vec<String>> = match sparse_paths_json {
        Some(v) => Some(
            serde_json::from_value(v)
                .map_err(|e| RepositoryError::Serialization(format!("sparse_paths deser: {e}")))?,
        ),
        None => None,
    };

    Ok(GitRepoBinding {
        id: GitRepoBindingId(id),
        tenant_id,
        credential_binding_id: credential_binding_id.map(CredentialBindingId),
        repo_url,
        git_ref: db_to_git_ref(&git_ref_type, &git_ref_value)?,
        sparse_paths,
        volume_id: VolumeId(volume_id),
        label,
        status: db_to_status(&status_text, status_error)?,
        clone_strategy: db_to_clone_strategy(&clone_strategy_text)?,
        last_cloned_at,
        last_commit_sha,
        auto_refresh,
        // Audit 002 §4.37.13 — cleartext is transient, never persisted.
        // Hydrate as None; the application service decrypts the
        // ciphertext on demand at verify time.
        webhook_secret: None,
        webhook_secret_ciphertext,
        webhook_lookup_hash,
        created_at,
        updated_at,
        domain_events: Vec::new(),
    })
}

// ============================================================================
// Repository implementation
// ============================================================================

#[async_trait]
impl GitRepoBindingRepository for PostgresGitRepoBindingRepository {
    async fn save(&self, binding: &GitRepoBinding) -> Result<(), RepositoryError> {
        let (git_ref_type, git_ref_value) = git_ref_to_db(&binding.git_ref);
        let (status_text, status_error) = status_to_db(&binding.status);
        let clone_strategy_text = clone_strategy_to_db(&binding.clone_strategy);
        let sparse_paths_json = binding
            .sparse_paths
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| RepositoryError::Serialization(format!("sparse_paths: {e}")))?;

        sqlx::query(
            r#"
            INSERT INTO git_repo_bindings (
                id, tenant_id, credential_binding_id, repo_url,
                git_ref_type, git_ref_value, sparse_paths, volume_id,
                label, status, status_error, clone_strategy,
                last_cloned_at, last_commit_sha, auto_refresh,
                webhook_secret_ciphertext, webhook_lookup_hash,
                created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
            ON CONFLICT (id) DO UPDATE SET
                tenant_id                  = EXCLUDED.tenant_id,
                credential_binding_id      = EXCLUDED.credential_binding_id,
                repo_url                   = EXCLUDED.repo_url,
                git_ref_type               = EXCLUDED.git_ref_type,
                git_ref_value              = EXCLUDED.git_ref_value,
                sparse_paths               = EXCLUDED.sparse_paths,
                volume_id                  = EXCLUDED.volume_id,
                label                      = EXCLUDED.label,
                status                     = EXCLUDED.status,
                status_error               = EXCLUDED.status_error,
                clone_strategy             = EXCLUDED.clone_strategy,
                last_cloned_at             = EXCLUDED.last_cloned_at,
                last_commit_sha            = EXCLUDED.last_commit_sha,
                auto_refresh               = EXCLUDED.auto_refresh,
                webhook_secret_ciphertext  = EXCLUDED.webhook_secret_ciphertext,
                webhook_lookup_hash        = EXCLUDED.webhook_lookup_hash,
                updated_at                 = EXCLUDED.updated_at
            "#,
        )
        .bind(binding.id.0)
        .bind(binding.tenant_id.as_str())
        .bind(binding.credential_binding_id.map(|id| id.0))
        .bind(&binding.repo_url)
        .bind(git_ref_type)
        .bind(git_ref_value)
        .bind(sparse_paths_json)
        .bind(binding.volume_id.0)
        .bind(&binding.label)
        .bind(status_text)
        .bind(status_error)
        .bind(&clone_strategy_text)
        .bind(binding.last_cloned_at)
        .bind(binding.last_commit_sha.as_deref())
        .bind(binding.auto_refresh)
        // Audit 002 §4.37.13 — persist the encrypted/hashed pair.
        .bind(binding.webhook_secret_ciphertext.as_deref())
        .bind(binding.webhook_lookup_hash.as_deref())
        .bind(binding.created_at)
        .bind(binding.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!(
                "Failed to save git_repo_binding {}: {e}",
                binding.id
            ))
        })?;

        Ok(())
    }

    async fn find_by_id(
        &self,
        id: &GitRepoBindingId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        let row = sqlx::query("SELECT * FROM git_repo_bindings WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                RepositoryError::Database(format!("Failed to find git_repo_binding {id}: {e}"))
            })?;

        match row {
            Some(row) => Ok(Some(hydrate_binding(&row)?)),
            None => Ok(None),
        }
    }

    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<Vec<GitRepoBinding>, RepositoryError> {
        // ADR-081 Phase 1: bindings are tenant-scoped. The `owner` parameter
        // is retained for API symmetry with the credential repository and is
        // matched at the service layer via the authenticated user's tenant.
        let rows =
            sqlx::query("SELECT * FROM git_repo_bindings WHERE tenant_id = $1 ORDER BY created_at")
                .bind(tenant_id.as_str())
                .fetch_all(&self.pool)
                .await
                .map_err(|e| {
                    RepositoryError::Database(format!(
                        "Failed to list git_repo_bindings for tenant {}: {e}",
                        tenant_id.as_str()
                    ))
                })?;

        let mut bindings = Vec::with_capacity(rows.len());
        for row in &rows {
            bindings.push(hydrate_binding(row)?);
        }
        Ok(bindings)
    }

    async fn find_by_volume_id(
        &self,
        volume_id: &VolumeId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        let row = sqlx::query("SELECT * FROM git_repo_bindings WHERE volume_id = $1")
            .bind(volume_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                RepositoryError::Database(format!(
                    "Failed to find git_repo_binding for volume {volume_id}: {e}"
                ))
            })?;

        match row {
            Some(row) => Ok(Some(hydrate_binding(&row)?)),
            None => Ok(None),
        }
    }

    async fn find_by_webhook_lookup_hash(
        &self,
        hash: &str,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        // Audit 002 §4.37.13 — query by deterministic HMAC hash, never by
        // cleartext secret. The cleartext is no longer stored.
        let row = sqlx::query("SELECT * FROM git_repo_bindings WHERE webhook_lookup_hash = $1")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                RepositoryError::Database(format!(
                    "Failed to look up git_repo_binding by webhook_lookup_hash: {e}"
                ))
            })?;

        match row {
            Some(row) => Ok(Some(hydrate_binding(&row)?)),
            None => Ok(None),
        }
    }

    async fn count_by_owner(
        &self,
        tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<u32, RepositoryError> {
        let row = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS cnt FROM git_repo_bindings WHERE tenant_id = $1",
        )
        .bind(tenant_id.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!(
                "Failed to count git_repo_bindings for tenant {}: {e}",
                tenant_id.as_str()
            ))
        })?;

        let count: i64 = row
            .try_get("cnt")
            .map_err(|e| RepositoryError::Serialization(format!("cnt: {e}")))?;
        Ok(count as u32)
    }

    async fn delete(&self, id: &GitRepoBindingId) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM git_repo_bindings WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                RepositoryError::Database(format!("Failed to delete git_repo_binding {id}: {e}"))
            })?;
        Ok(())
    }
}
