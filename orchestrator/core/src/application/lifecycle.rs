// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Agent Lifecycle Application Service — BC-1
//!
//! Use-case implementations for the **Agent Lifecycle** bounded context:
//! deploy, update, pause, resume, and delete agent definitions.
//!
//! Orchestrates:
//! 1. Manifest validation via the `ManifestValidator` domain service
//! 2. Persistence via [`crate::domain::repository::AgentRepository`]
//! 3. Event publication (`AgentLifecycleEvent`) via the `EventBus`
//!
//! Implements the `AgentLifecycleService` trait from
//! [`crate::application::agent`].
//!
//! See AGENTS.md §BC-1 Agent Lifecycle Context.
//!
//! # Code Quality Principles
//!
//! - Keep manifest validation and lifecycle persistence in one application boundary.
//! - Fail closed on invalid manifests rather than synthesizing defaults.
//! - Publish lifecycle state changes explicitly instead of relying on implicit side effects.

use crate::application::agent::AgentLifecycleService;
use crate::domain::agent::{Agent, AgentId, AgentManifest, AgentScope};
use crate::domain::events::AgentLifecycleEvent;
use crate::domain::iam::{IdentityKind, UserIdentity};
use crate::domain::repository::AgentRepository;
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use std::sync::Arc;

pub struct StandardAgentLifecycleService {
    repository: Arc<dyn AgentRepository>,
    event_bus: Arc<EventBus>,
    security_context_repo: Arc<dyn SecurityContextRepository>,
}

impl StandardAgentLifecycleService {
    pub fn new(
        repository: Arc<dyn AgentRepository>,
        event_bus: Arc<EventBus>,
        security_context_repo: Arc<dyn SecurityContextRepository>,
    ) -> Self {
        Self {
            repository,
            event_bus,
            security_context_repo,
        }
    }
}

#[async_trait]
impl AgentLifecycleService for StandardAgentLifecycleService {
    async fn deploy_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        manifest: AgentManifest,
        force: bool,
        scope: AgentScope,
        caller_identity: Option<&UserIdentity>,
    ) -> Result<AgentId> {
        // Validate manifest before deploying
        manifest.validate().map_err(|e| anyhow::anyhow!(e))?;

        // ADR-102: Only Operators and ServiceAccounts may register agents with aegis-system-* contexts.
        if let Some(ctx) = &manifest.spec.security_context {
            if ctx.starts_with("aegis-system-") {
                let permitted = caller_identity.is_none()
                    || matches!(
                        caller_identity.map(|id| &id.identity_kind),
                        Some(IdentityKind::Operator { .. })
                            | Some(IdentityKind::ServiceAccount { .. })
                    );
                if !permitted {
                    anyhow::bail!(
                        "Forbidden: only platform operators may register agents \
                         with an aegis-system-* security context"
                    );
                }
            }
            // Validate the named security context exists.
            if self
                .security_context_repo
                .find_by_name(ctx)
                .await?
                .is_none()
            {
                anyhow::bail!("Unknown security context: '{ctx}'");
            }
        }

        // Check if an agent with the same name already exists
        if let Some(existing) = self
            .repository
            .find_by_name_for_tenant(tenant_id, &manifest.metadata.name)
            .await?
        {
            let existing_version = &existing.manifest.metadata.version;
            let incoming_version = &manifest.metadata.version;

            if existing_version == incoming_version {
                // Same name AND same version — only allowed when --force is set
                if !force {
                    anyhow::bail!(
                        "Agent '{}' version '{}' is already deployed (ID: {}). \
                         Use --force to overwrite it.",
                        existing.name,
                        existing_version,
                        existing.id.0
                    );
                }
                // --force: overwrite the existing agent's manifest in place,
                // preserving its AgentId so existing execution references remain valid.
                // Preserve the existing scope on force-overwrite.
                let mut updated = existing.clone();
                updated.update_manifest(manifest);
                self.repository.save_for_tenant(tenant_id, &updated).await?;
                return Ok(updated.id);
            }

            // Different version — treat as an in-place update (new version replaces old).
            // Preserve existing scope when updating version.
            let mut updated = existing.clone();
            updated.update_manifest(manifest);
            self.repository.save_for_tenant(tenant_id, &updated).await?;
            return Ok(updated.id);
        }

        // No existing agent with this name — create a fresh one with the requested scope
        let mut agent = Agent::new(manifest);
        agent.scope = scope;
        agent.tenant_id = tenant_id.clone();
        self.repository.save_for_tenant(tenant_id, &agent).await?;

        metrics::counter!(
            "aegis_agent_lifecycle_operations_total",
            "operation" => "deploy",
            "result" => "success"
        )
        .increment(1);

        Ok(agent.id)
    }

    async fn get_agent_for_tenant(&self, tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
        self.repository
            .find_by_id_for_tenant(tenant_id, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Agent not found"))
    }

    async fn get_agent_visible(&self, tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
        self.repository
            .find_by_id_visible(tenant_id, id)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("Agent not found"))
    }

    async fn update_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
        manifest: AgentManifest,
    ) -> Result<()> {
        let mut agent = self.get_agent_for_tenant(tenant_id, id).await?;
        agent.update_manifest(manifest);
        self.repository.save_for_tenant(tenant_id, &agent).await?;
        Ok(())
    }

    async fn delete_agent_for_tenant(&self, tenant_id: &TenantId, id: AgentId) -> Result<()> {
        self.repository.delete_for_tenant(tenant_id, id).await?;

        self.event_bus
            .publish_agent_event(AgentLifecycleEvent::AgentRemoved {
                agent_id: id,
                removed_at: Utc::now(),
            });

        metrics::counter!(
            "aegis_agent_lifecycle_operations_total",
            "operation" => "delete",
            "result" => "success"
        )
        .increment(1);

        Ok(())
    }

    async fn list_agents_for_tenant(&self, tenant_id: &TenantId) -> Result<Vec<Agent>> {
        self.repository
            .list_all_for_tenant(tenant_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agents: {e}"))
    }

    async fn list_agents_visible_for_tenant(&self, tenant_id: &TenantId) -> Result<Vec<Agent>> {
        self.repository
            .list_visible_for_tenant(tenant_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list visible agents: {e}"))
    }

    async fn lookup_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        let agent = self
            .repository
            .find_by_name_for_tenant(tenant_id, name)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?;
        Ok(agent.map(|a| a.id))
    }

    async fn lookup_agent_visible_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        let agent = self
            .repository
            .resolve_by_name(tenant_id, name)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?;
        Ok(agent.map(|a| a.id))
    }

    async fn lookup_agent_for_tenant_with_version(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<AgentId>> {
        let agent = self
            .repository
            .find_by_name_and_version_for_tenant(tenant_id, name, version)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?;
        Ok(agent.map(|a| a.id))
    }

    async fn list_versions_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> Result<Vec<crate::domain::repository::AgentVersion>> {
        self.repository
            .list_versions_for_tenant(tenant_id, agent_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agent versions: {e}"))
    }
}
