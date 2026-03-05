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

use crate::application::agent::AgentLifecycleService;
use crate::domain::agent::{Agent, AgentId, AgentManifest};
use crate::domain::repository::AgentRepository;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

pub struct StandardAgentLifecycleService {
    repository: Arc<dyn AgentRepository>,
}

impl StandardAgentLifecycleService {
    pub fn new(repository: Arc<dyn AgentRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait]
impl AgentLifecycleService for StandardAgentLifecycleService {
    async fn deploy_agent(&self, manifest: AgentManifest, force: bool) -> Result<AgentId> {
        // Validate manifest before deploying
        manifest.validate().map_err(|e| anyhow::anyhow!(e))?;

        // Check if an agent with the same name already exists
        if let Some(existing) = self
            .repository
            .find_by_name(&manifest.metadata.name)
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
                let mut updated = existing.clone();
                updated.update_manifest(manifest);
                self.repository.save(&updated).await?;
                return Ok(updated.id);
            }

            // Different version — treat as an in-place update (new version replaces old).
            let mut updated = existing.clone();
            updated.update_manifest(manifest);
            self.repository.save(&updated).await?;
            return Ok(updated.id);
        }

        // No existing agent with this name — create a fresh one
        let agent = Agent::new(manifest);
        self.repository.save(&agent).await?;
        Ok(agent.id)
    }

    async fn get_agent(&self, id: AgentId) -> Result<Agent> {
        self.repository
            .find_by_id(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Agent not found"))
    }

    async fn update_agent(&self, id: AgentId, manifest: AgentManifest) -> Result<()> {
        let mut agent = self.get_agent(id).await?;
        agent.update_manifest(manifest);
        self.repository.save(&agent).await?;
        Ok(())
    }

    async fn delete_agent(&self, id: AgentId) -> Result<()> {
        self.repository.delete(id).await?;
        Ok(())
    }

    async fn list_agents(&self) -> Result<Vec<Agent>> {
        self.repository
            .list_all()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agents: {}", e))
    }

    async fn lookup_agent(&self, name: &str) -> Result<Option<AgentId>> {
        let agent = self
            .repository
            .find_by_name(name)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {}", e))?;
        Ok(agent.map(|a| a.id))
    }
}
