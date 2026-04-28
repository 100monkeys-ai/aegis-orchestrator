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

use crate::domain::agent::{Agent, AgentId, AgentManifest, AgentScope};
// AgentScope retained: used in `deploy_agent_for_tenant` signature.
use crate::domain::iam::UserIdentity;
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
        scope: AgentScope,
        caller_identity: Option<&UserIdentity>,
    ) -> Result<AgentId>;

    async fn get_agent_for_tenant(&self, tenant_id: &TenantId, id: AgentId) -> Result<Agent>;

    /// Fetch an agent by ID, checking the requesting tenant first then falling through to aegis-system.
    ///
    /// The default implementation delegates to [`Self::get_agent_for_tenant`] — concrete repository-backed
    /// implementations override this to perform an efficient single-pass fallthrough query.
    async fn get_agent_visible(&self, tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
        self.get_agent_for_tenant(tenant_id, id).await
    }

    async fn update_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
        manifest: AgentManifest,
    ) -> Result<()>;

    async fn delete_agent_for_tenant(&self, tenant_id: &TenantId, id: AgentId) -> Result<()>;

    async fn list_agents_for_tenant(&self, tenant_id: &TenantId) -> Result<Vec<Agent>>;

    async fn list_agents_visible_for_tenant(&self, tenant_id: &TenantId) -> Result<Vec<Agent>>;

    async fn lookup_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>>;

    async fn lookup_agent_visible_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>>;

    async fn list_versions_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> Result<Vec<AgentVersion>>;

    /// Resolve an agent name + version to its [`AgentId`], or `None` if not found.
    async fn lookup_agent_for_tenant_with_version(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<AgentId>>;
}
