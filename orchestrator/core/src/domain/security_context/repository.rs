// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SecurityContext Repository Trait (BC-12, ADR-035)
//!
//! Persistence contract for [`SecurityContext`] aggregates. Follows the Repository
//! pattern from AGENTS.md §Repository Patterns — one repository per aggregate root.
//!
//! The repository is loaded once during orchestrator startup and consulted at
//! every agent attestation via [`crate::application::attestation_service::AttestationServiceImpl`].

use async_trait::async_trait;
use anyhow::Result;

use super::security_context::SecurityContext;

/// Persistence contract for [`SecurityContext`] aggregates.
///
/// # Implementations
///
/// The in-process implementation is
/// `crate::infrastructure::repositories::InMemorySecurityContextRepository`.
/// A future PostgreSQL implementation will use the `security_contexts` table
/// defined in ADR-025.
#[async_trait]
pub trait SecurityContextRepository: Send + Sync {
    /// Look up a security context by its unique name.
    ///
    /// Returns `None` if no context with that name exists. Called during
    /// agent attestation to resolve the `spec.security_context` manifest field.
    async fn find_by_name(&self, name: &str) -> Result<Option<SecurityContext>>;

    /// Persist a security context (insert or update).
    ///
    /// Upserts by name — if a context with the same name already exists, it is
    /// replaced and `metadata.version` is incremented.
    async fn save(&self, context: SecurityContext) -> Result<()>;

    /// Return all registered security contexts.
    ///
    /// Used by the Control Plane's Architect component to enumerate available
    /// contexts when generating agent manifests.
    async fn list_all(&self) -> Result<Vec<SecurityContext>>;
}
