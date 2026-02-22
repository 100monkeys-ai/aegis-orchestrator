// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Agent Lifecycle Application Service (BC-1)
//!
//! Defines [`AgentLifecycleService`] — the primary interface for managing agent
//! manifest registration, retrieval, and removal. The concrete implementation lives
//! in [`crate::application::lifecycle::StandardAgentLifecycleService`].
//!
//! ## Relationship to other contexts
//!
//! - The Execution Context (BC-2) calls [`AgentLifecycleService::get_agent`] before
//!   every execution to resolve the manifest and security policy.
//! - The Control Plane (BC-9) calls `deploy_agent` / `list_agents` via the gRPC API.
//! - Agent manifests drive the `SecurityContext` name used to initialise SMCP sessions.

use crate::domain::agent::{Agent, AgentId, AgentManifest};
use anyhow::Result;
use async_trait::async_trait;

/// Primary interface for agent manifest management (BC-1 Agent Lifecycle Context).
///
/// All methods are async and return `anyhow::Result` to allow implementations to
/// propagate database, validation, or manifest-parsing errors upstream.
///
/// # See Also
///
/// `crate::application::lifecycle::StandardAgentLifecycleService` — the production
/// implementation backed by Postgres.
#[async_trait]
pub trait AgentLifecycleService: Send + Sync {
    /// Parse, validate, and persist a new agent manifest, returning the assigned [`AgentId`].
    ///
    /// # Errors
    ///
    /// - Manifest schema validation failures
    /// - Duplicate agent name conflicts
    /// - Database write errors
    async fn deploy_agent(&self, manifest: AgentManifest) -> Result<AgentId>;

    /// Retrieve a fully-hydrated [`Agent`] by its UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if `id` does not exist in the registry.
    async fn get_agent(&self, id: AgentId) -> Result<Agent>;

    /// Replace the manifest of an existing agent in-place.
    ///
    /// The agent's [`AgentId`] is unchanged. Running executions are not affected —
    /// the new manifest takes effect on the next execution start.
    ///
    /// # Errors
    ///
    /// - `id` not found
    /// - New manifest fails schema validation
    async fn update_agent(&self, id: AgentId, manifest: AgentManifest) -> Result<()>;

    /// Permanently remove an agent manifest from the registry.
    ///
    /// Does **not** cancel in-progress executions. Callers must cancel executions
    /// before deleting the agent if that behaviour is required.
    ///
    /// # Errors
    ///
    /// Returns an error if `id` does not exist.
    async fn delete_agent(&self, id: AgentId) -> Result<()>;

    /// Return all registered agents in unspecified order.
    async fn list_agents(&self) -> Result<Vec<Agent>>;

    /// Resolve an agent name string to its [`AgentId`], or `None` if not found.
    ///
    /// Used by workflow execution to dereference agent name references in workflow
    /// manifests (e.g. `agent: "code-reviewer"`).
    async fn lookup_agent(&self, name: &str) -> Result<Option<AgentId>>;
}
