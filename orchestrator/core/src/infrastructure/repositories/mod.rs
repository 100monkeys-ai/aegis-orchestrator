// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Repository Implementations
//!
//! This module provides infrastructure implementations of repository abstractions
//! defined in the domain layer, following the Repository pattern from DDD.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Persist and retrieve domain aggregates
//! - **Pattern:** Repository (DDD), Adapter (Hexagonal Architecture)
//!
//! # Available Implementations
//!
//! ## PostgreSQL Repositories
//!
//! Production-ready implementations backed by PostgreSQL:
//! - **PostgresAgentRepository** - Agent manifest persistence
//! - **PostgresExecutionRepository** - Execution state and history
//! - **PostgresWorkflowRepository** - Workflow definitions and versions
//! - **PostgresWorkflowExecutionRepository** - Workflow execution state
//!
//! ## In-Memory Repositories
//!
//! Lightweight implementations for testing and development:
//! - **InMemoryAgentRepository** - Thread-safe HashMap-backed storage
//! - **InMemoryExecutionRepository** - Ephemeral execution tracking
//! - **InMemoryWorkflowRepository** - Workflow definition cache
//!
//! # Usage
//!
//! ```ignore
//! use sqlx::PgPool;
//! use repositories::PostgresAgentRepository;
//!
//! let pool = PgPool::connect(&database_url).await?;
//! let repo = PostgresAgentRepository::new(pool);
//!
//! // Repository implements AgentRepository trait
//! let agent = repo.find_by_id(agent_id).await?;
//! ```
//!
//! # Design Principles
//!
//! 1. **Technology Agnostic**: Domain layer has no knowledge of persistence
//! 2. **Transactional Consistency**: Operations are atomic where possible
//! 3. **Error Mapping**: Infrastructure errors mapped to domain RepositoryError
//! 4. **Connection Pooling**: Efficient database connection management

pub mod postgres_agent;
pub mod postgres_execution;
pub mod postgres_storage_event;
pub mod postgres_tenant;
pub mod postgres_volume;
pub mod postgres_workflow;
pub mod postgres_workflow_execution;

use crate::domain::agent::{Agent, AgentId, AgentScope};
use crate::domain::execution::{Execution, ExecutionId};
use crate::domain::repository::{
    AgentRepository, ExecutionRepository, RepositoryError, StorageEventRepository,
    WorkflowRepository,
};
use crate::domain::tenant::TenantId;
use crate::domain::workflow::{Workflow, WorkflowId, WorkflowScope};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct InMemoryAgentRepository {
    agents: Arc<RwLock<HashMap<TenantId, HashMap<AgentId, Agent>>>>,
}

impl InMemoryAgentRepository {
    pub fn new() -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryAgentRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentRepository for InMemoryAgentRepository {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent: &Agent,
    ) -> Result<(), RepositoryError> {
        let mut agents = self.agents.write().unwrap();
        agents
            .entry(tenant_id.clone())
            .or_default()
            .insert(agent.id, agent.clone());
        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> Result<Option<Agent>, RepositoryError> {
        let agents = self.agents.read().unwrap();
        Ok(agents
            .get(tenant_id)
            .and_then(|tenant_agents| tenant_agents.get(&id))
            .cloned())
    }

    async fn find_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Agent>, RepositoryError> {
        let agents = self.agents.read().unwrap();
        Ok(agents.get(tenant_id).and_then(|tenant_agents| {
            tenant_agents
                .values()
                .filter(|a| a.name == name)
                .max_by(|a, b| {
                    a.manifest
                        .metadata
                        .version
                        .cmp(&b.manifest.metadata.version)
                })
                .cloned()
        }))
    }

    async fn find_by_name_and_version_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<Agent>, RepositoryError> {
        let agents = self.agents.read().unwrap();
        Ok(agents.get(tenant_id).and_then(|tenant_agents| {
            tenant_agents
                .values()
                .find(|a| a.name == name && a.manifest.metadata.version == version)
                .cloned()
        }))
    }

    async fn list_all_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Agent>, RepositoryError> {
        let agents = self.agents.read().unwrap();
        Ok(agents
            .get(tenant_id)
            .map(|tenant_agents| tenant_agents.values().cloned().collect())
            .unwrap_or_default())
    }

    async fn delete_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> Result<(), RepositoryError> {
        let mut agents = self.agents.write().unwrap();
        if let Some(tenant_agents) = agents.get_mut(tenant_id) {
            tenant_agents.remove(&id);
        }
        Ok(())
    }

    async fn list_versions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: AgentId,
    ) -> Result<Vec<crate::domain::repository::AgentVersion>, RepositoryError> {
        // In-memory repo does not track version history
        Ok(Vec::new())
    }

    async fn list_visible_for_tenant(
        &self,
        tenant_id: &TenantId,
        user_id: Option<&str>,
    ) -> Result<Vec<Agent>, RepositoryError> {
        let agents = self.agents.read().unwrap();
        let system_tid = TenantId::system();
        let mut result: Vec<Agent> = Vec::new();
        for (tid, tenant_agents) in agents.iter() {
            for a in tenant_agents.values() {
                let visible = match &a.scope {
                    AgentScope::User { owner_user_id } => {
                        tid == tenant_id
                            && user_id
                                .map(|u| u == owner_user_id.as_str())
                                .unwrap_or(false)
                    }
                    AgentScope::Tenant => tid == tenant_id,
                    AgentScope::Global => tid == &system_tid,
                };
                if visible {
                    result.push(a.clone());
                }
            }
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }

    async fn update_scope(
        &self,
        id: AgentId,
        new_scope: AgentScope,
        new_tenant_id: &TenantId,
    ) -> Result<(), RepositoryError> {
        let mut agents = self.agents.write().unwrap();
        // Find and remove the agent from its current tenant bucket
        let mut found: Option<Agent> = None;
        for tenant_agents in agents.values_mut() {
            if let Some(agent) = tenant_agents.remove(&id) {
                found = Some(agent);
                break;
            }
        }
        if let Some(mut agent) = found {
            agent.scope = new_scope;
            agent.tenant_id = new_tenant_id.clone();
            agents
                .entry(new_tenant_id.clone())
                .or_default()
                .insert(id, agent);
        }
        Ok(())
    }

    async fn resolve_by_name(
        &self,
        tenant_id: &TenantId,
        user_id: Option<&str>,
        name: &str,
    ) -> Result<Option<Agent>, RepositoryError> {
        let agents = self.agents.read().unwrap();
        let system_tid = TenantId::system();

        // Priority: user-scoped > tenant-scoped > global
        let mut user_match: Option<Agent> = None;
        let mut tenant_match: Option<Agent> = None;
        let mut global_match: Option<Agent> = None;

        for (tid, tenant_agents) in agents.iter() {
            for agent in tenant_agents.values().filter(|a| a.name == name) {
                match &agent.scope {
                    AgentScope::User { owner_user_id } => {
                        if tid == tenant_id
                            && user_id
                                .map(|u| u == owner_user_id.as_str())
                                .unwrap_or(false)
                            && user_match.is_none()
                        {
                            user_match = Some(agent.clone());
                        }
                    }
                    AgentScope::Tenant => {
                        if tid == tenant_id && tenant_match.is_none() {
                            tenant_match = Some(agent.clone());
                        }
                    }
                    AgentScope::Global => {
                        if tid == &system_tid && global_match.is_none() {
                            global_match = Some(agent.clone());
                        }
                    }
                }
            }
        }

        Ok(user_match.or(tenant_match).or(global_match))
    }
}

// AgentLifecycleService implementation for in-memory use
use crate::application::agent::AgentLifecycleService;
use crate::domain::agent::AgentManifest;

#[async_trait]
impl AgentLifecycleService for InMemoryAgentRepository {
    async fn deploy_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        manifest: AgentManifest,
        force: bool,
        scope: AgentScope,
    ) -> anyhow::Result<AgentId> {
        if let Some(existing) = self
            .find_by_name_for_tenant(tenant_id, &manifest.metadata.name)
            .await?
        {
            let existing_version = &existing.manifest.metadata.version;
            let incoming_version = &manifest.metadata.version;

            if existing_version == incoming_version {
                if !force {
                    anyhow::bail!(
                        "Agent '{}' version '{}' is already deployed (ID: {}). \
                         Use --force to overwrite it.",
                        existing.name,
                        existing_version,
                        existing.id.0
                    );
                }
                let mut updated = existing.clone();
                updated.update_manifest(manifest);
                self.save_for_tenant(tenant_id, &updated)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to save agent: {e}"))?;
                return Ok(updated.id);
            }

            // Different version — update in place (preserve existing scope).
            let mut updated = existing.clone();
            updated.update_manifest(manifest);
            self.save_for_tenant(tenant_id, &updated)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to save agent: {e}"))?;
            return Ok(updated.id);
        }

        let mut agent = Agent::new(manifest);
        agent.scope = scope;
        agent.tenant_id = tenant_id.clone();
        let id = agent.id;
        self.save_for_tenant(tenant_id, &agent)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to save agent: {e}"))?;
        Ok(id)
    }

    async fn get_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> anyhow::Result<Agent> {
        self.find_by_id_for_tenant(tenant_id, id)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("Agent not found"))
    }

    async fn update_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
        manifest: AgentManifest,
    ) -> anyhow::Result<()> {
        let mut agent = self.get_agent_for_tenant(tenant_id, id).await?;
        agent.update_manifest(manifest);
        self.save_for_tenant(tenant_id, &agent)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to update agent: {e}"))
    }

    async fn delete_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> anyhow::Result<()> {
        self.delete_for_tenant(tenant_id, id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to delete agent: {e}"))
    }

    async fn list_agents_for_tenant(&self, tenant_id: &TenantId) -> anyhow::Result<Vec<Agent>> {
        self.list_all_for_tenant(tenant_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agents: {e}"))
    }

    async fn list_agents_visible_for_tenant(
        &self,
        tenant_id: &TenantId,
        user_id: Option<&str>,
    ) -> anyhow::Result<Vec<Agent>> {
        self.list_visible_for_tenant(tenant_id, user_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list visible agents: {e}"))
    }

    async fn lookup_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        let agent = self
            .find_by_name_for_tenant(tenant_id, name)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?;
        Ok(agent.map(|a| a.id))
    }

    async fn lookup_agent_for_tenant_with_version(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        let agents = self.agents.read().unwrap();
        let tenant_agents = match agents.get(tenant_id) {
            Some(m) => m,
            None => return Ok(None),
        };
        Ok(tenant_agents
            .values()
            .find(|a| a.name == name && a.manifest.metadata.version == version)
            .map(|a| a.id))
    }

    async fn list_versions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: AgentId,
    ) -> anyhow::Result<Vec<crate::domain::repository::AgentVersion>> {
        // In-memory repo does not track version history
        Ok(Vec::new())
    }
}

#[derive(Clone)]
pub struct InMemoryExecutionRepository {
    executions: Arc<RwLock<HashMap<TenantId, HashMap<ExecutionId, Execution>>>>,
}

impl InMemoryExecutionRepository {
    pub fn new() -> Self {
        Self {
            executions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryExecutionRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutionRepository for InMemoryExecutionRepository {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution: &Execution,
    ) -> Result<(), RepositoryError> {
        let mut executions = self.executions.write().unwrap();
        executions
            .entry(tenant_id.clone())
            .or_default()
            .insert(execution.id, execution.clone());
        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Option<Execution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        Ok(executions
            .get(tenant_id)
            .and_then(|tenant_execs| tenant_execs.get(&id))
            .cloned())
    }

    async fn find_by_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
        limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        let mut results: Vec<Execution> = executions
            .get(tenant_id)
            .into_iter()
            .flat_map(|tenant_execs| tenant_execs.values())
            .filter(|e| e.agent_id == agent_id)
            .cloned()
            .collect();
        results.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(results.into_iter().take(limit).collect())
    }

    async fn find_by_workflow_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_id: crate::domain::workflow::WorkflowId,
        _limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError> {
        // In-memory repo does not track workflow-to-execution relationships
        Ok(Vec::new())
    }

    async fn find_recent_for_tenant(
        &self,
        tenant_id: &TenantId,
        limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        let mut execution_list: Vec<Execution> = executions
            .get(tenant_id)
            .map(|tenant_execs| tenant_execs.values().cloned().collect())
            .unwrap_or_default();
        // Sort by started_at desc
        execution_list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(execution_list.into_iter().take(limit).collect())
    }

    async fn delete_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<(), RepositoryError> {
        let mut executions = self.executions.write().unwrap();
        if let Some(tenant_execs) = executions.get_mut(tenant_id) {
            tenant_execs.remove(&id);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct InMemoryWorkflowRepository {
    workflows: Arc<RwLock<HashMap<TenantId, HashMap<WorkflowId, Workflow>>>>,
}

impl InMemoryWorkflowRepository {
    pub fn new() -> Self {
        Self {
            workflows: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryWorkflowRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WorkflowRepository for InMemoryWorkflowRepository {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow: &Workflow,
    ) -> Result<(), RepositoryError> {
        let mut workflows = self.workflows.write().unwrap();
        workflows
            .entry(tenant_id.clone())
            .or_default()
            .insert(workflow.id, workflow.clone());
        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        Ok(workflows
            .get(tenant_id)
            .and_then(|tenant_workflows| tenant_workflows.get(&id))
            .cloned())
    }

    async fn find_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        Ok(workflows.get(tenant_id).and_then(|tenant_workflows| {
            tenant_workflows
                .values()
                .filter(|w| w.metadata.name == name)
                .max_by(|a, b| a.metadata.version.cmp(&b.metadata.version))
                .cloned()
        }))
    }

    async fn find_by_name_and_version_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        Ok(workflows.get(tenant_id).and_then(|tenant_workflows| {
            tenant_workflows
                .values()
                .find(|w| w.metadata.name == name && w.metadata.version.as_deref() == Some(version))
                .cloned()
        }))
    }

    async fn list_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        let mut results: Vec<Workflow> = workflows
            .get(tenant_id)
            .into_iter()
            .flat_map(|tenant_workflows| tenant_workflows.values())
            .filter(|w| w.metadata.name == name)
            .cloned()
            .collect();
        results.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(results)
    }

    async fn list_all_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        Ok(workflows
            .get(tenant_id)
            .map(|tenant_workflows| tenant_workflows.values().cloned().collect())
            .unwrap_or_default())
    }

    async fn resolve_by_name(
        &self,
        tenant_id: &TenantId,
        user_id: Option<&str>,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        let system_tenant = TenantId::system();

        // Scope priority: user=0 (best), tenant=1, global=2 (worst).
        let scope_priority = |w: &Workflow| -> u8 {
            match &w.scope {
                WorkflowScope::User { .. } => 0,
                WorkflowScope::Tenant => 1,
                WorkflowScope::Global => 2,
            }
        };

        let mut candidates: Vec<&Workflow> = Vec::new();

        // User scope
        if let Some(uid) = user_id {
            if let Some(tenant_wfs) = workflows.get(tenant_id) {
                for w in tenant_wfs.values() {
                    if w.metadata.name == name {
                        if let WorkflowScope::User { owner_user_id } = &w.scope {
                            if owner_user_id == uid {
                                candidates.push(w);
                            }
                        }
                    }
                }
            }
        }

        // Tenant scope
        if let Some(tenant_wfs) = workflows.get(tenant_id) {
            for w in tenant_wfs.values() {
                if w.metadata.name == name && w.scope == WorkflowScope::Tenant {
                    candidates.push(w);
                }
            }
        }

        // Global scope
        if let Some(global_wfs) = workflows.get(&system_tenant) {
            for w in global_wfs.values() {
                if w.metadata.name == name && w.scope == WorkflowScope::Global {
                    candidates.push(w);
                }
            }
        }

        // Sort by priority (ascending), then by version (descending)
        candidates.sort_by(|a, b| {
            scope_priority(a)
                .cmp(&scope_priority(b))
                .then_with(|| b.metadata.version.cmp(&a.metadata.version))
        });

        Ok(candidates.first().cloned().cloned())
    }

    async fn resolve_by_name_and_version(
        &self,
        tenant_id: &TenantId,
        user_id: Option<&str>,
        name: &str,
        version: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        let system_tenant = TenantId::system();

        // User scope (highest priority)
        if let Some(uid) = user_id {
            if let Some(tenant_wfs) = workflows.get(tenant_id) {
                for w in tenant_wfs.values() {
                    if w.metadata.name == name && w.metadata.version.as_deref() == Some(version) {
                        if let WorkflowScope::User { owner_user_id } = &w.scope {
                            if owner_user_id == uid {
                                return Ok(Some(w.clone()));
                            }
                        }
                    }
                }
            }
        }

        // Tenant scope
        if let Some(tenant_wfs) = workflows.get(tenant_id) {
            for w in tenant_wfs.values() {
                if w.metadata.name == name
                    && w.metadata.version.as_deref() == Some(version)
                    && w.scope == WorkflowScope::Tenant
                {
                    return Ok(Some(w.clone()));
                }
            }
        }

        // Global scope
        if let Some(global_wfs) = workflows.get(&system_tenant) {
            for w in global_wfs.values() {
                if w.metadata.name == name
                    && w.metadata.version.as_deref() == Some(version)
                    && w.scope == WorkflowScope::Global
                {
                    return Ok(Some(w.clone()));
                }
            }
        }

        Ok(None)
    }

    async fn list_visible(
        &self,
        tenant_id: &TenantId,
        user_id: Option<&str>,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        let system_tenant = TenantId::system();
        let mut result = Vec::new();

        // User-scoped workflows
        if let Some(uid) = user_id {
            if let Some(tenant_wfs) = workflows.get(tenant_id) {
                for w in tenant_wfs.values() {
                    if let WorkflowScope::User { owner_user_id } = &w.scope {
                        if owner_user_id == uid {
                            result.push(w.clone());
                        }
                    }
                }
            }
        }

        // Tenant-scoped workflows
        if let Some(tenant_wfs) = workflows.get(tenant_id) {
            for w in tenant_wfs.values() {
                if w.scope == WorkflowScope::Tenant {
                    result.push(w.clone());
                }
            }
        }

        // Global-scoped workflows
        if let Some(global_wfs) = workflows.get(&system_tenant) {
            for w in global_wfs.values() {
                if w.scope == WorkflowScope::Global {
                    result.push(w.clone());
                }
            }
        }

        result.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
        Ok(result)
    }

    async fn list_global(&self) -> Result<Vec<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        let system_tenant = TenantId::system();
        let mut result = Vec::new();

        if let Some(global_wfs) = workflows.get(&system_tenant) {
            for w in global_wfs.values() {
                if w.scope == WorkflowScope::Global {
                    result.push(w.clone());
                }
            }
        }

        result.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
        Ok(result)
    }

    async fn update_scope(
        &self,
        id: WorkflowId,
        new_scope: WorkflowScope,
        new_tenant_id: &TenantId,
    ) -> Result<(), RepositoryError> {
        let mut workflows = self.workflows.write().unwrap();

        // Find the workflow across all tenants
        let mut found = None;
        for (tid, tenant_wfs) in workflows.iter() {
            if let Some(w) = tenant_wfs.get(&id) {
                found = Some((tid.clone(), w.clone()));
                break;
            }
        }

        let (old_tenant_id, mut workflow) =
            found.ok_or_else(|| RepositoryError::NotFound(format!("Workflow {id} not found")))?;

        // Remove from old tenant
        if let Some(tenant_wfs) = workflows.get_mut(&old_tenant_id) {
            tenant_wfs.remove(&id);
        }

        // Update scope and tenant
        workflow.scope = new_scope;
        workflow.tenant_id = new_tenant_id.clone();

        // Insert under new tenant
        workflows
            .entry(new_tenant_id.clone())
            .or_default()
            .insert(id, workflow);

        Ok(())
    }

    async fn delete_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
    ) -> Result<(), RepositoryError> {
        let mut workflows = self.workflows.write().unwrap();
        if let Some(tenant_workflows) = workflows.get_mut(tenant_id) {
            tenant_workflows.remove(&id);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct InMemoryWorkflowExecutionRepository {
    executions: Arc<
        RwLock<
            HashMap<
                TenantId,
                HashMap<
                    crate::domain::execution::ExecutionId,
                    crate::domain::workflow::WorkflowExecution,
                >,
            >,
        >,
    >,
}

impl InMemoryWorkflowExecutionRepository {
    pub fn new() -> Self {
        Self {
            executions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryWorkflowExecutionRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::domain::repository::WorkflowExecutionRepository
    for InMemoryWorkflowExecutionRepository
{
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution: &crate::domain::workflow::WorkflowExecution,
    ) -> Result<(), RepositoryError> {
        let mut executions = self.executions.write().unwrap();
        executions
            .entry(tenant_id.clone())
            .or_default()
            .insert(execution.id, execution.clone());
        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: crate::domain::execution::ExecutionId,
    ) -> Result<Option<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        Ok(executions
            .get(tenant_id)
            .and_then(|tenant_execs| tenant_execs.get(&id))
            .cloned())
    }

    async fn find_tenant_id_by_execution(
        &self,
        id: crate::domain::execution::ExecutionId,
    ) -> Result<Option<TenantId>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        Ok(executions.iter().find_map(|(tenant_id, tenant_execs)| {
            tenant_execs.contains_key(&id).then(|| tenant_id.clone())
        }))
    }

    async fn find_active_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        Ok(executions
            .get(tenant_id)
            .into_iter()
            .flat_map(|tenant_execs| tenant_execs.values())
            .filter(|e| e.status == crate::domain::execution::ExecutionStatus::Running)
            .cloned()
            .collect())
    }

    async fn find_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: crate::domain::workflow::WorkflowId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        let mut list: Vec<_> = executions
            .get(tenant_id)
            .map(|tenant_execs| {
                tenant_execs
                    .values()
                    .filter(|execution| execution.workflow_id == workflow_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(list.into_iter().skip(offset).take(limit).collect())
    }

    async fn update_temporal_linkage_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution_id: ExecutionId,
        _temporal_workflow_id: &str,
        _temporal_run_id: &str,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn append_event(
        &self,
        _execution_id: ExecutionId,
        _temporal_sequence_number: i64,
        _event_type: String,
        _payload: serde_json::Value,
        _iteration_number: Option<u8>,
    ) -> Result<(), RepositoryError> {
        // No-op for in-memory repositories.
        Ok(())
    }

    async fn find_events_by_execution(
        &self,
        _id: ExecutionId,
        _limit: usize,
        _offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecutionEventRecord>, RepositoryError> {
        Ok(vec![])
    }

    async fn list_paginated_for_tenant(
        &self,
        tenant_id: &TenantId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        let mut list: Vec<_> = executions
            .get(tenant_id)
            .map(|tenant_execs| tenant_execs.values().cloned().collect())
            .unwrap_or_default();
        list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(list.into_iter().skip(offset).take(limit).collect())
    }
}

// ============================================================================
// In-Memory StorageEventRepository (for testing)
// ============================================================================

#[derive(Clone)]
pub struct InMemoryStorageEventRepository {
    events: Arc<RwLock<Vec<crate::domain::events::StorageEvent>>>,
}

impl InMemoryStorageEventRepository {
    pub fn new() -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl Default for InMemoryStorageEventRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StorageEventRepository for InMemoryStorageEventRepository {
    async fn save(
        &self,
        event: &crate::domain::events::StorageEvent,
    ) -> Result<(), RepositoryError> {
        let mut events = self.events.write().unwrap();
        events.push(event.clone());
        Ok(())
    }

    async fn find_by_execution(
        &self,
        execution_id: ExecutionId,
        limit: Option<usize>,
    ) -> Result<Vec<crate::domain::events::StorageEvent>, RepositoryError> {
        let events = self.events.read().unwrap();
        let mut results: Vec<_> = events
            .iter()
            .filter(|e| {
                use crate::domain::events::StorageEvent;
                match e {
                    StorageEvent::FileOpened {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::FileRead {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::FileWritten {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::FileClosed {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::DirectoryListed {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::FileCreated {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::FileDeleted {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::PathTraversalBlocked {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::FilesystemPolicyViolation {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::QuotaExceeded {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                    StorageEvent::UnauthorizedVolumeAccess {
                        execution_id: eid, ..
                    } => *eid == execution_id,
                }
            })
            .cloned()
            .collect();

        // Apply limit if specified
        if let Some(limit) = limit {
            results.truncate(limit);
        }

        Ok(results)
    }

    async fn find_by_volume(
        &self,
        volume_id: crate::domain::volume::VolumeId,
        limit: Option<usize>,
    ) -> Result<Vec<crate::domain::events::StorageEvent>, RepositoryError> {
        let events = self.events.read().unwrap();
        let mut results: Vec<_> = events
            .iter()
            .filter(|e| {
                use crate::domain::events::StorageEvent;
                match e {
                    StorageEvent::FileOpened { volume_id: vid, .. } => *vid == volume_id,
                    StorageEvent::FileRead { volume_id: vid, .. } => *vid == volume_id,
                    StorageEvent::FileWritten { volume_id: vid, .. } => *vid == volume_id,
                    StorageEvent::FileClosed { volume_id: vid, .. } => *vid == volume_id,
                    StorageEvent::DirectoryListed { volume_id: vid, .. } => *vid == volume_id,
                    StorageEvent::FileCreated { volume_id: vid, .. } => *vid == volume_id,
                    StorageEvent::FileDeleted { volume_id: vid, .. } => *vid == volume_id,
                    StorageEvent::FilesystemPolicyViolation { volume_id: vid, .. } => {
                        *vid == volume_id
                    }
                    StorageEvent::QuotaExceeded { volume_id: vid, .. } => *vid == volume_id,
                    StorageEvent::UnauthorizedVolumeAccess { volume_id: vid, .. } => {
                        *vid == volume_id
                    }
                    // PathTraversalBlocked doesn't have volume_id
                    StorageEvent::PathTraversalBlocked { .. } => false,
                }
            })
            .cloned()
            .collect();

        // Apply limit if specified
        if let Some(limit) = limit {
            results.truncate(limit);
        }

        Ok(results)
    }

    async fn find_violations(
        &self,
        execution_id: Option<ExecutionId>,
    ) -> Result<Vec<crate::domain::events::StorageEvent>, RepositoryError> {
        let events = self.events.read().unwrap();
        let violations: Vec<_> = events
            .iter()
            .filter(|e| {
                use crate::domain::events::StorageEvent;
                let is_violation = matches!(
                    e,
                    StorageEvent::PathTraversalBlocked { .. }
                        | StorageEvent::FilesystemPolicyViolation { .. }
                        | StorageEvent::QuotaExceeded { .. }
                        | StorageEvent::UnauthorizedVolumeAccess { .. }
                );

                if !is_violation {
                    return false;
                }

                // If execution_id filter is specified, only include matching events
                if let Some(eid) = execution_id {
                    match e {
                        StorageEvent::PathTraversalBlocked {
                            execution_id: e_eid,
                            ..
                        } => *e_eid == eid,
                        StorageEvent::FilesystemPolicyViolation {
                            execution_id: e_eid,
                            ..
                        } => *e_eid == eid,
                        StorageEvent::QuotaExceeded {
                            execution_id: e_eid,
                            ..
                        } => *e_eid == eid,
                        StorageEvent::UnauthorizedVolumeAccess {
                            execution_id: e_eid,
                            ..
                        } => *e_eid == eid,
                        _ => false,
                    }
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        Ok(violations)
    }
}

// ============================================================================
// In-Memory VolumeRepository (for testing)
// ============================================================================

#[derive(Clone)]
pub struct InMemoryVolumeRepository {
    volumes: Arc<RwLock<HashMap<crate::domain::volume::VolumeId, crate::domain::volume::Volume>>>,
}

impl InMemoryVolumeRepository {
    pub fn new() -> Self {
        Self {
            volumes: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryVolumeRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::domain::repository::VolumeRepository for InMemoryVolumeRepository {
    async fn save(&self, volume: &crate::domain::volume::Volume) -> Result<(), RepositoryError> {
        let mut volumes = self.volumes.write().unwrap();
        volumes.insert(volume.id, volume.clone());
        Ok(())
    }

    async fn find_by_id(
        &self,
        id: crate::domain::volume::VolumeId,
    ) -> Result<Option<crate::domain::volume::Volume>, RepositoryError> {
        let volumes = self.volumes.read().unwrap();
        Ok(volumes.get(&id).cloned())
    }

    async fn find_by_tenant(
        &self,
        tenant_id: crate::domain::volume::TenantId,
    ) -> Result<Vec<crate::domain::volume::Volume>, RepositoryError> {
        let volumes = self.volumes.read().unwrap();
        Ok(volumes
            .values()
            .filter(|v| v.tenant_id == tenant_id)
            .cloned()
            .collect())
    }

    async fn find_expired(&self) -> Result<Vec<crate::domain::volume::Volume>, RepositoryError> {
        let volumes = self.volumes.read().unwrap();
        let now = chrono::Utc::now();
        Ok(volumes
            .values()
            .filter(|v| {
                // Check if volume has an expiration time and it has passed
                if let Some(expires_at) = v.expires_at {
                    expires_at < now
                } else {
                    false
                }
            })
            .cloned()
            .collect())
    }

    async fn find_by_ownership(
        &self,
        ownership: &crate::domain::volume::VolumeOwnership,
    ) -> Result<Vec<crate::domain::volume::Volume>, RepositoryError> {
        let volumes = self.volumes.read().unwrap();
        Ok(volumes
            .values()
            .filter(|v| v.ownership == *ownership)
            .cloned()
            .collect())
    }

    async fn delete(&self, id: crate::domain::volume::VolumeId) -> Result<(), RepositoryError> {
        let mut volumes = self.volumes.write().unwrap();
        volumes.remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_in_memory_agent_repository_basic() {
        let repo = InMemoryAgentRepository::new();

        // Test with empty repository
        let all_agents = repo.list_all().await.unwrap();
        assert_eq!(all_agents.len(), 0);

        // Test finding non-existent agent
        let non_existent_id = AgentId::new();
        let result = repo.find_by_id(non_existent_id).await.unwrap();
        assert!(result.is_none());

        // Test finding by non-existent name
        let result = repo.find_by_name("non-existent").await.unwrap();
        assert!(result.is_none());

        // Test deleting non-existent agent (should not error)
        let delete_result = repo.delete(non_existent_id).await;
        assert!(delete_result.is_ok());
    }

    #[tokio::test]
    async fn test_in_memory_execution_repository_basic() {
        let repo = InMemoryExecutionRepository::new();

        // Test with empty repository
        let agent_id = AgentId::new();
        let agent_executions = repo.find_by_agent(agent_id, 100).await.unwrap();
        assert_eq!(agent_executions.len(), 0);

        // Test finding non-existent execution
        let execution_id = ExecutionId::new();
        let result = repo.find_by_id(execution_id).await.unwrap();
        assert!(result.is_none());

        // Test deleting non-existent execution (should not error)
        let delete_result = repo.delete(execution_id).await;
        assert!(delete_result.is_ok());
    }

    #[tokio::test]
    async fn test_in_memory_workflow_repository_basic() {
        let repo = InMemoryWorkflowRepository::new();

        // Test with empty repository
        let all_workflows = repo.list_all().await.unwrap();
        assert_eq!(all_workflows.len(), 0);

        // Test finding non-existent workflow
        let workflow_id = WorkflowId::new();
        let result = repo.find_by_id(workflow_id).await.unwrap();
        assert!(result.is_none());

        // Test finding by non-existent name
        let result = repo.find_by_name("non-existent").await.unwrap();
        assert!(result.is_none());
    }
}
