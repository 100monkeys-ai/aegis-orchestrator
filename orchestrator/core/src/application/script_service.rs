// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Script Application Service (BC-7 Storage Gateway, ADR-110 §D7)
//!
//! [`ScriptService`] — the primary interface for creating, listing,
//! reading, updating, and soft-deleting persisted TypeScript programs.
//!
//! All business logic lives here; the HTTP layer is a thin shell that
//! translates requests into command structs and maps errors onto status
//! codes.
//!
//! ## Ownership Model
//!
//! Scripts are tenant-scoped AND owner-scoped. Even within the same
//! tenant, a read from a non-creator returns [`ScriptServiceError::NotFound`]
//! — never 403 — to avoid leaking existence of other users' scripts
//! (mirrors the `GitRepoService` pattern).

use std::sync::Arc;

use thiserror::Error;
use tracing::{info, instrument};

use crate::domain::iam::ZaruTier;
use crate::domain::repository::RepositoryError;
use crate::domain::script::{
    Script, ScriptError, ScriptId, ScriptRepository, ScriptVersionSummary,
};
use crate::domain::script_tier_limits::ScriptTierLimits;
use crate::domain::shared_kernel::TenantId;
use crate::infrastructure::event_bus::EventBus;

// ============================================================================
// Command Types
// ============================================================================

/// Request to create a new [`Script`].
#[derive(Debug, Clone)]
pub struct CreateScriptCommand {
    pub tenant_id: TenantId,
    pub created_by: String,
    pub zaru_tier: ZaruTier,
    pub name: String,
    pub description: String,
    pub code: String,
    pub tags: Vec<String>,
}

/// Request to update an existing [`Script`]. All four fields are
/// replaced; the aggregate bumps the version counter on success.
#[derive(Debug, Clone)]
pub struct UpdateScriptCommand {
    pub name: String,
    pub description: String,
    pub code: String,
    pub tags: Vec<String>,
}

// ============================================================================
// Errors
// ============================================================================

/// Service-layer errors. Handlers map these onto HTTP status codes.
#[derive(Debug, Error)]
pub enum ScriptServiceError {
    /// Script does not exist in `tenant_id`, belongs to a different owner
    /// (we never leak existence across owners), or has been soft-deleted.
    #[error("script not found")]
    NotFound,

    /// A script with the same name already exists for this tenant. Maps
    /// to HTTP `409 Conflict`.
    #[error("a script with this name already exists")]
    DuplicateName,

    /// Tier quota exceeded for the caller's `ZaruTier`. Maps to HTTP
    /// `422 Unprocessable Entity`.
    #[error("script tier limit exceeded: {max} active scripts allowed")]
    TierLimitExceeded { max: u32 },

    /// Domain-level validation failure. Maps to HTTP `400 Bad Request`.
    #[error("domain validation failed: {0}")]
    Domain(#[from] ScriptError),

    /// Underlying persistence failure. Maps to HTTP `500 Internal Server Error`.
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),
}

// ============================================================================
// Service
// ============================================================================

/// Application service for the [`Script`] aggregate lifecycle.
pub struct ScriptService {
    repo: Arc<dyn ScriptRepository>,
    event_bus: Arc<EventBus>,
}

impl ScriptService {
    pub fn new(repo: Arc<dyn ScriptRepository>, event_bus: Arc<EventBus>) -> Self {
        Self { repo, event_bus }
    }

    // -----------------------------------------------------------------------
    // create
    // -----------------------------------------------------------------------

    /// Validate the command, enforce tier + name-uniqueness gates, and
    /// persist a new [`Script`].
    #[instrument(skip(self, cmd), fields(owner = %cmd.created_by, name = %cmd.name))]
    pub async fn create(&self, cmd: CreateScriptCommand) -> Result<Script, ScriptServiceError> {
        let limits = ScriptTierLimits::for_tier(cmd.zaru_tier.clone());

        // Enforce the per-owner script count quota.
        if let Some(max) = limits.max_scripts {
            let current = self
                .repo
                .count_by_owner(&cmd.tenant_id, &cmd.created_by)
                .await?;
            if current >= max {
                return Err(ScriptServiceError::TierLimitExceeded { max });
            }
        }

        // Enforce the tier-gated code-size cap. Domain-level length
        // validation runs a second time inside `Script::new`, but the
        // tier gate can be stricter in future.
        if cmd.code.len() > limits.max_code_bytes as usize {
            return Err(ScriptServiceError::Domain(ScriptError::CodeTooLarge));
        }

        // Reject on duplicate active name within the tenant. The DB
        // partial-unique index is the authoritative gate, but this
        // service-level check produces a clean error before the write.
        if self
            .repo
            .find_by_name(&cmd.tenant_id, &cmd.name)
            .await?
            .is_some()
        {
            return Err(ScriptServiceError::DuplicateName);
        }

        let mut script = Script::new(
            cmd.tenant_id,
            cmd.created_by,
            cmd.name,
            cmd.description,
            cmd.code,
            cmd.tags,
        )?;

        self.repo.save(&script).await?;
        self.drain_and_publish(&mut script);
        info!(script_id = %script.id, "script created");
        Ok(script)
    }

    // -----------------------------------------------------------------------
    // list
    // -----------------------------------------------------------------------

    /// List the caller's active (non-soft-deleted) scripts.
    #[instrument(skip(self))]
    pub async fn list(
        &self,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<Vec<Script>, ScriptServiceError> {
        let scripts = self.repo.find_by_owner(tenant_id, created_by).await?;
        Ok(scripts)
    }

    // -----------------------------------------------------------------------
    // get
    // -----------------------------------------------------------------------

    /// Load a script by id, enforcing tenant AND owner gates.
    ///
    /// Returns [`ScriptServiceError::NotFound`] for missing records,
    /// soft-deleted records, tenant mismatches, AND owner mismatches —
    /// a non-owner must never be able to distinguish "exists elsewhere"
    /// from "does not exist".
    #[instrument(skip(self), fields(script_id = %id))]
    pub async fn get(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<Script, ScriptServiceError> {
        let script = self
            .repo
            .find_by_id(id, tenant_id)
            .await?
            .ok_or(ScriptServiceError::NotFound)?;

        if script.created_by != created_by {
            return Err(ScriptServiceError::NotFound);
        }
        Ok(script)
    }

    // -----------------------------------------------------------------------
    // list_versions
    // -----------------------------------------------------------------------

    /// Return the version-history audit trail for a script. Owner gate
    /// enforced before hitting the history table.
    #[instrument(skip(self), fields(script_id = %id))]
    pub async fn list_versions(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<Vec<ScriptVersionSummary>, ScriptServiceError> {
        // Enforce ownership against the CURRENT script row. We
        // deliberately do not expose history for soft-deleted scripts
        // through this path (the REST surface only calls it from
        // `GET /v1/scripts/:id` which requires an active row).
        let _ = self.get(id, tenant_id, created_by).await?;
        let versions = self.repo.list_versions(id, tenant_id).await?;
        Ok(versions)
    }

    // -----------------------------------------------------------------------
    // update
    // -----------------------------------------------------------------------

    /// Mutate an existing script's fields and bump the version counter.
    /// Verifies tenant + ownership before applying the change.
    #[instrument(skip(self, cmd), fields(script_id = %id))]
    pub async fn update(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
        created_by: &str,
        cmd: UpdateScriptCommand,
    ) -> Result<Script, ScriptServiceError> {
        let mut script = self.get(id, tenant_id, created_by).await?;

        // If the name is actually changing, verify no OTHER active
        // script in this tenant already claims it.
        if script.name != cmd.name {
            if let Some(other) = self.repo.find_by_name(tenant_id, &cmd.name).await? {
                if other.id != script.id {
                    return Err(ScriptServiceError::DuplicateName);
                }
            }
        }

        script.update(cmd.name, cmd.description, cmd.code, cmd.tags)?;
        self.repo.save(&script).await?;
        self.drain_and_publish(&mut script);
        info!(script_id = %script.id, version = script.version, "script updated");
        Ok(script)
    }

    // -----------------------------------------------------------------------
    // delete
    // -----------------------------------------------------------------------

    /// Soft-delete a script. Verifies tenant + ownership. Idempotent at
    /// the aggregate level; the repository's partial unique index
    /// re-opens the name for future reuse.
    #[instrument(skip(self), fields(script_id = %id))]
    pub async fn delete(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<(), ScriptServiceError> {
        let mut script = self.get(id, tenant_id, created_by).await?;
        script.soft_delete();
        self.repo.save(&script).await?;
        // `save` won't append to version_history for a delete-marked
        // row, but we still need the soft_delete row write. Trigger the
        // soft_delete repo method too so the `deleted_at` column is
        // written even if save ordering ever changes.
        self.repo.soft_delete(id, tenant_id).await?;
        self.drain_and_publish(&mut script);
        info!(script_id = %id, "script soft-deleted");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn drain_and_publish(&self, script: &mut Script) {
        for event in script.take_events() {
            self.event_bus.publish_script_event(event);
        }
    }
}
