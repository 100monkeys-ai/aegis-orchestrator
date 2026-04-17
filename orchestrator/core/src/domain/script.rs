// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Script Domain (BC-7 Storage Gateway, ADR-110 §D7)
//!
//! Domain model for [`Script`] — a persisted TypeScript program authored in
//! Zaru Live Mode / Code Mode and executed client-side via the QuickJS WASM
//! sandbox.
//!
//! ## Shape
//!
//! Scripts are per-tenant records with monotonically increasing version
//! numbers. Updates bump the version and append a [`ScriptVersionSummary`]
//! to the history surface. Deletions are soft — the row sticks around
//! marked `deleted_at` so the version-history audit trail survives.
//!
//! ## Type Map
//!
//! | Type | Role |
//! |------|------|
//! | [`ScriptId`] | UUID newtype — aggregate root identity |
//! | [`Visibility`] | `Private` / `Tenant` / `Public` — only `Private` is honoured today |
//! | [`ScriptEvent`] | Domain events published to the event bus |
//! | [`Script`] | Aggregate root |
//! | [`ScriptRepository`] | Repository trait (Postgres impl in infrastructure) |
//! | [`ScriptVersionSummary`] | Value object summarising a past version |
//!
//! See [`crate::domain::script_tier_limits`] for per-[`crate::domain::iam::ZaruTier`] gating.

use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::TenantId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ============================================================================
// Limits — exposed to handlers + tests for validation parity.
// ============================================================================

/// Maximum [`Script::name`] length in bytes.
pub const SCRIPT_NAME_MAX_BYTES: usize = 128;
/// Maximum [`Script::description`] length in bytes (2 KiB).
pub const SCRIPT_DESCRIPTION_MAX_BYTES: usize = 2 * 1024;
/// Maximum [`Script::code`] length in bytes (256 KiB — a generous upper
/// bound for a hand-authored TypeScript program).
pub const SCRIPT_CODE_MAX_BYTES: usize = 256 * 1024;
/// Maximum number of tags permitted per script.
pub const SCRIPT_MAX_TAGS: usize = 16;
/// Maximum length of a single tag.
pub const SCRIPT_TAG_MAX_BYTES: usize = 32;

// ============================================================================
// Value Object — Identity
// ============================================================================

/// Unique identifier for a [`Script`] aggregate root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScriptId(pub Uuid);

impl ScriptId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ScriptId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ScriptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Value Object — Visibility
// ============================================================================

/// Script visibility scope (ADR-110 §D7).
///
/// Only [`Visibility::Private`] is currently honoured by the service layer;
/// [`Visibility::Tenant`] and [`Visibility::Public`] are accepted by the
/// enum (so the schema is marketplace-ready) but rejected at construction
/// with [`ScriptError::VisibilityNotSupported`] until the marketplace ADR
/// defines the product surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Private,
    Tenant,
    Public,
}

impl Visibility {
    /// Stable text encoding used by the Postgres `visibility` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Tenant => "tenant",
            Visibility::Public => "public",
        }
    }

    /// Parse the text encoding produced by [`Visibility::as_str`].
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s {
            "private" => Some(Visibility::Private),
            "tenant" => Some(Visibility::Tenant),
            "public" => Some(Visibility::Public),
            _ => None,
        }
    }
}

// ============================================================================
// Errors
// ============================================================================

/// Validation failures produced by the [`Script`] aggregate constructors
/// and mutators. The application service wraps these in its own error
/// envelope so the HTTP layer can map them onto `400 Bad Request`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptError {
    #[error("script name must not be empty")]
    EmptyName,

    #[error("script name exceeds {SCRIPT_NAME_MAX_BYTES}-byte maximum")]
    NameTooLong,

    #[error("script name may not contain path separators")]
    NameContainsPathSeparator,

    #[error("script description exceeds {SCRIPT_DESCRIPTION_MAX_BYTES}-byte maximum")]
    DescriptionTooLong,

    #[error("script code exceeds {SCRIPT_CODE_MAX_BYTES}-byte maximum")]
    CodeTooLarge,

    #[error("scripts may have at most {SCRIPT_MAX_TAGS} tags")]
    TooManyTags,

    #[error("invalid tag: {0}")]
    TagInvalid(String),

    #[error("script visibility other than `private` is not supported yet")]
    VisibilityNotSupported,
}

// ============================================================================
// Domain Events
// ============================================================================

/// Domain events published by [`Script`] state transitions (ADR-110 §D7).
///
/// Emitted by the aggregate during state changes and drained by the
/// application service via [`Script::take_events`] for publication to the
/// event bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScriptEvent {
    /// A script was created at version 1.
    Created {
        id: ScriptId,
        tenant_id: TenantId,
        name: String,
        version: u32,
        created_at: DateTime<Utc>,
    },
    /// A script's contents were updated; `new_version` is the freshly
    /// assigned monotonically-increasing version number.
    Updated {
        id: ScriptId,
        name: String,
        new_version: u32,
        updated_at: DateTime<Utc>,
    },
    /// A script was soft-deleted. The row remains in the database so the
    /// version history survives for audit; it no longer appears in `list`
    /// or `find_by_id` responses.
    Deleted {
        id: ScriptId,
        deleted_at: DateTime<Utc>,
    },
}

// ============================================================================
// Value Object — ScriptVersionSummary
// ============================================================================

/// Lightweight summary of a past version, returned by
/// [`ScriptRepository::list_versions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptVersionSummary {
    pub version: u32,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

// ============================================================================
// Aggregate Root — Script
// ============================================================================

/// Aggregate root for a persisted TypeScript program (ADR-110 §D7).
///
/// ## Invariants
///
/// - `tenant_id` and `created_by` are immutable after construction.
/// - `version` is monotonically increasing and bumped only by
///   [`Script::update`].
/// - `deleted_at` is `None` for active scripts; calling
///   [`Script::soft_delete`] sets it to `Utc::now()`.
/// - Only tenant-owned scripts appear in [`ScriptRepository`] queries; the
///   service layer enforces per-user ownership on top of tenant isolation.
#[derive(Debug, Clone)]
pub struct Script {
    pub id: ScriptId,
    pub tenant_id: TenantId,
    pub created_by: String,
    pub name: String,
    pub description: String,
    pub code: String,
    pub tags: Vec<String>,
    pub visibility: Visibility,
    pub version: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// Event buffer. Drained by [`take_events`](Self::take_events) at the
    /// aggregate boundary and published to the event bus.
    pub domain_events: Vec<ScriptEvent>,
}

impl Script {
    /// Construct a new [`Script`] at version 1 with
    /// [`Visibility::Private`]. Buffers a [`ScriptEvent::Created`].
    ///
    /// Runs the full validation suite ([`validate_name`],
    /// [`validate_description`], [`validate_code`], [`validate_tags`]).
    pub fn new(
        tenant_id: TenantId,
        created_by: String,
        name: String,
        description: String,
        code: String,
        tags: Vec<String>,
    ) -> Result<Self, ScriptError> {
        validate_name(&name)?;
        validate_description(&description)?;
        validate_code(&code)?;
        validate_tags(&tags)?;

        let id = ScriptId::new();
        let now = Utc::now();
        let mut script = Self {
            id,
            tenant_id: tenant_id.clone(),
            created_by,
            name: name.clone(),
            description,
            code,
            tags,
            visibility: Visibility::Private,
            version: 1,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            domain_events: Vec::new(),
        };
        script.domain_events.push(ScriptEvent::Created {
            id,
            tenant_id,
            name,
            version: 1,
            created_at: now,
        });
        Ok(script)
    }

    /// Update the script's mutable fields and bump the version counter.
    ///
    /// Emits [`ScriptEvent::Updated`]. Does **not** change
    /// `created_by` / `created_at` / `tenant_id`. Returns
    /// [`ScriptError::VisibilityNotSupported`] if called on a
    /// soft-deleted script (the service layer should refuse before
    /// reaching here, but this is a defence-in-depth guard).
    pub fn update(
        &mut self,
        name: String,
        description: String,
        code: String,
        tags: Vec<String>,
    ) -> Result<(), ScriptError> {
        validate_name(&name)?;
        validate_description(&description)?;
        validate_code(&code)?;
        validate_tags(&tags)?;

        let now = Utc::now();
        self.name = name.clone();
        self.description = description;
        self.code = code;
        self.tags = tags;
        self.version = self.version.saturating_add(1);
        self.updated_at = now;
        self.domain_events.push(ScriptEvent::Updated {
            id: self.id,
            name,
            new_version: self.version,
            updated_at: now,
        });
        Ok(())
    }

    /// Mark the script as soft-deleted. Idempotent — a second call is a
    /// no-op. Emits [`ScriptEvent::Deleted`] on the first successful
    /// invocation.
    pub fn soft_delete(&mut self) {
        if self.deleted_at.is_some() {
            return;
        }
        let now = Utc::now();
        self.deleted_at = Some(now);
        self.updated_at = now;
        self.domain_events.push(ScriptEvent::Deleted {
            id: self.id,
            deleted_at: now,
        });
    }

    /// `true` when the script has been soft-deleted.
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    /// Drain and return the buffered [`ScriptEvent`]s. The application
    /// service calls this at the aggregate boundary to publish them.
    pub fn take_events(&mut self) -> Vec<ScriptEvent> {
        std::mem::take(&mut self.domain_events)
    }
}

// ============================================================================
// Validation helpers
// ============================================================================

/// Validate a script name: non-empty, <= 128 bytes, no path separators.
pub fn validate_name(name: &str) -> Result<(), ScriptError> {
    if name.is_empty() {
        return Err(ScriptError::EmptyName);
    }
    if name.len() > SCRIPT_NAME_MAX_BYTES {
        return Err(ScriptError::NameTooLong);
    }
    if name.contains('/') || name.contains('\\') {
        return Err(ScriptError::NameContainsPathSeparator);
    }
    Ok(())
}

/// Validate a script description: no length lower bound, <= 2 KiB.
pub fn validate_description(description: &str) -> Result<(), ScriptError> {
    if description.len() > SCRIPT_DESCRIPTION_MAX_BYTES {
        return Err(ScriptError::DescriptionTooLong);
    }
    Ok(())
}

/// Validate TypeScript code: <= 256 KiB. Parsing is performed by the
/// browser runtime; the server only enforces an upper bound.
pub fn validate_code(code: &str) -> Result<(), ScriptError> {
    if code.len() > SCRIPT_CODE_MAX_BYTES {
        return Err(ScriptError::CodeTooLarge);
    }
    Ok(())
}

/// Validate a tag list: at most 16 tags, each at most 32 bytes, lowercase
/// ASCII letters / digits / `-` / `_` only.
pub fn validate_tags(tags: &[String]) -> Result<(), ScriptError> {
    if tags.len() > SCRIPT_MAX_TAGS {
        return Err(ScriptError::TooManyTags);
    }
    for tag in tags {
        if tag.is_empty() || tag.len() > SCRIPT_TAG_MAX_BYTES {
            return Err(ScriptError::TagInvalid(tag.clone()));
        }
        if !tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(ScriptError::TagInvalid(tag.clone()));
        }
    }
    Ok(())
}

// ============================================================================
// Repository Trait
// ============================================================================

/// Persistence interface for the [`Script`] aggregate root (ADR-110 §D7).
///
/// All queries are tenant-scoped to enforce the multi-tenant isolation
/// boundary. Soft-deleted rows are filtered out of every read method
/// except [`ScriptRepository::list_versions`], which retains history for
/// audit.
#[async_trait]
pub trait ScriptRepository: Send + Sync {
    /// Upsert a [`Script`] by primary key. Also records a row in the
    /// `script_versions` history table when the write corresponds to a
    /// new version of an existing script.
    async fn save(&self, script: &Script) -> Result<(), RepositoryError>;

    /// Load a script by its aggregate id, scoped to `tenant_id`. Returns
    /// `None` for soft-deleted scripts and for tenant mismatches.
    async fn find_by_id(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
    ) -> Result<Option<Script>, RepositoryError>;

    /// Return all active scripts created by `created_by` in `tenant_id`.
    /// Excludes soft-deleted records.
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<Vec<Script>, RepositoryError>;

    /// Lookup an active script by unique `(tenant_id, name)`. Returns
    /// `None` for soft-deleted or missing rows.
    async fn find_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Script>, RepositoryError>;

    /// Return the full version-history audit surface for the script.
    /// Rows are ordered by version ascending. Survives soft-deletion.
    async fn list_versions(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
    ) -> Result<Vec<ScriptVersionSummary>, RepositoryError>;

    /// Reconstruct a past version of the script from the history table.
    /// Returns `None` when the version does not exist. Scoped by
    /// `tenant_id` for isolation.
    async fn get_version(
        &self,
        id: &ScriptId,
        tenant_id: &TenantId,
        version: u32,
    ) -> Result<Option<Script>, RepositoryError>;

    /// Soft-delete the script (sets `deleted_at = NOW()`). Idempotent on
    /// already-deleted rows. The version-history table is preserved.
    async fn soft_delete(&self, id: &ScriptId, tenant_id: &TenantId)
        -> Result<(), RepositoryError>;

    /// Count the number of active scripts created by `created_by` in
    /// `tenant_id` — used by the tier-limit gate in
    /// [`crate::domain::script_tier_limits::ScriptTierLimits`].
    async fn count_by_owner(
        &self,
        tenant_id: &TenantId,
        created_by: &str,
    ) -> Result<u32, RepositoryError>;
}

// ============================================================================
// Tests — constructor / validation / event emission.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_script() -> Script {
        Script::new(
            TenantId::consumer(),
            "user-1".to_string(),
            "demo".to_string(),
            "a script".to_string(),
            "return 1;".to_string(),
            vec!["alpha".to_string()],
        )
        .expect("valid sample")
    }

    #[test]
    fn new_script_is_version_one_and_private() {
        let mut s = sample_script();
        assert_eq!(s.version, 1);
        assert_eq!(s.visibility, Visibility::Private);
        assert!(s.deleted_at.is_none());
        let events = s.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ScriptEvent::Created { version: 1, .. }));
    }

    #[test]
    fn update_bumps_version_and_emits_event() {
        let mut s = sample_script();
        let _ = s.take_events();
        s.update(
            "demo-v2".to_string(),
            "updated".to_string(),
            "return 2;".to_string(),
            vec!["beta".to_string()],
        )
        .unwrap();
        assert_eq!(s.version, 2);
        assert_eq!(s.name, "demo-v2");
        let events = s.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            ScriptEvent::Updated { new_version: 2, .. }
        ));
    }

    #[test]
    fn soft_delete_is_idempotent() {
        let mut s = sample_script();
        let _ = s.take_events();
        s.soft_delete();
        assert!(s.is_deleted());
        let events_first = s.take_events();
        assert_eq!(events_first.len(), 1);
        s.soft_delete();
        let events_second = s.take_events();
        assert!(events_second.is_empty());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert_eq!(validate_name(""), Err(ScriptError::EmptyName));
    }

    #[test]
    fn validate_name_rejects_path_separators() {
        assert_eq!(
            validate_name("foo/bar"),
            Err(ScriptError::NameContainsPathSeparator)
        );
        assert_eq!(
            validate_name("foo\\bar"),
            Err(ScriptError::NameContainsPathSeparator)
        );
    }

    #[test]
    fn validate_name_rejects_too_long() {
        let too_long = "a".repeat(SCRIPT_NAME_MAX_BYTES + 1);
        assert_eq!(validate_name(&too_long), Err(ScriptError::NameTooLong));
    }

    #[test]
    fn validate_tags_rejects_invalid_chars() {
        let bad = vec!["ALPHA".to_string()]; // uppercase not allowed
        assert!(matches!(
            validate_tags(&bad),
            Err(ScriptError::TagInvalid(_))
        ));
        let spaced = vec!["hello world".to_string()];
        assert!(matches!(
            validate_tags(&spaced),
            Err(ScriptError::TagInvalid(_))
        ));
    }

    #[test]
    fn visibility_roundtrip() {
        for v in [Visibility::Private, Visibility::Tenant, Visibility::Public] {
            assert_eq!(Visibility::from_str_ci(v.as_str()), Some(v));
        }
    }

    /// Regression: the derived `Default` impl (clippy::derivable_impls) must
    /// preserve the original behaviour of a hand-written `impl Default` that
    /// returned `Visibility::Private`. If the `#[default]` attribute is moved
    /// to another variant, new scripts would silently adopt the wrong
    /// visibility scope.
    #[test]
    fn visibility_default_is_private() {
        assert_eq!(Visibility::default(), Visibility::Private);
    }
}
