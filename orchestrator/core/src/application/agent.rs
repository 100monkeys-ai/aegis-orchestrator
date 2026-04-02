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
//! - The Zaru client (BC-12) calls `deploy_agent` / `list_agents` via the gRPC API.
//! - Agent manifests drive the `SecurityContext` name used to initialise SEAL sessions.
//!
//! # Code Quality Principles
//!
//! - Treat manifest validation as a hard boundary before persistence or execution.
//! - Keep the service interface transport-agnostic and side-effect free beyond its repository.
//! - Fail closed on malformed or incomplete agent definitions.

use crate::domain::agent::{Agent, AgentId, AgentManifest};
use crate::domain::repository::AgentVersion;
use crate::domain::tenant::TenantId;
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
    async fn deploy_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        manifest: AgentManifest,
        force: bool,
    ) -> Result<AgentId>;

    async fn get_agent_for_tenant(&self, tenant_id: &TenantId, id: AgentId) -> Result<Agent>;

    async fn update_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
        manifest: AgentManifest,
    ) -> Result<()>;

    async fn delete_agent_for_tenant(&self, tenant_id: &TenantId, id: AgentId) -> Result<()>;

    async fn list_agents_for_tenant(&self, tenant_id: &TenantId) -> Result<Vec<Agent>>;

    async fn lookup_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>>;

    async fn list_versions_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> Result<Vec<AgentVersion>>;

    /// Parse, validate, and persist a new agent manifest, returning the assigned [`AgentId`].
    ///
    /// When `force` is `false` the call fails if an agent with the same `metadata.name` **and**
    /// `metadata.version` already exists.  Set `force = true` to silently overwrite that agent
    /// in place (same [`AgentId`] is preserved).
    ///
    /// A different `metadata.version` will always update the existing agent regardless of `force`.
    ///
    /// # Errors
    ///
    /// - Manifest schema validation failures
    /// - Same name + same version already deployed and `force = false`
    /// - Database write errors
    async fn deploy_agent(&self, manifest: AgentManifest, force: bool) -> Result<AgentId> {
        self.deploy_agent_for_tenant(&TenantId::local_default(), manifest, force)
            .await
    }

    /// Retrieve a fully-hydrated [`Agent`] by its UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if `id` does not exist in the registry.
    async fn get_agent(&self, id: AgentId) -> Result<Agent> {
        self.get_agent_for_tenant(&TenantId::local_default(), id)
            .await
    }

    /// Replace the manifest of an existing agent in-place.
    ///
    /// The agent's [`AgentId`] is unchanged. Running executions are not affected —
    /// the new manifest takes effect on the next execution start.
    ///
    /// # Errors
    ///
    /// - `id` not found
    /// - New manifest fails schema validation
    async fn update_agent(&self, id: AgentId, manifest: AgentManifest) -> Result<()> {
        self.update_agent_for_tenant(&TenantId::local_default(), id, manifest)
            .await
    }

    /// Permanently remove an agent manifest from the registry.
    ///
    /// Does **not** cancel in-progress executions. Callers must cancel executions
    /// before deleting the agent if that behaviour is required.
    ///
    /// # Errors
    ///
    /// Returns an error if `id` does not exist.
    async fn delete_agent(&self, id: AgentId) -> Result<()> {
        self.delete_agent_for_tenant(&TenantId::local_default(), id)
            .await
    }

    /// Return all registered agents in unspecified order.
    async fn list_agents(&self) -> Result<Vec<Agent>> {
        self.list_agents_for_tenant(&TenantId::local_default())
            .await
    }

    /// Resolve an agent name string to its [`AgentId`], or `None` if not found.
    ///
    /// Used by workflow execution to dereference agent name references in workflow
    /// manifests (e.g. `agent: "code-reviewer"`).
    async fn lookup_agent(&self, name: &str) -> Result<Option<AgentId>> {
        self.lookup_agent_for_tenant(&TenantId::local_default(), name)
            .await
    }

    /// Resolve an agent name + version to its [`AgentId`], or `None` if not found.
    async fn lookup_agent_for_tenant_with_version(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<AgentId>>;

    /// Convenience wrapper calling [`Self::lookup_agent_for_tenant_with_version`] with the local default tenant.
    async fn lookup_agent_with_version(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<AgentId>> {
        self.lookup_agent_for_tenant_with_version(&TenantId::local_default(), name, version)
            .await
    }
}
