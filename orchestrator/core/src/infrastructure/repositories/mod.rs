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
//! ```no_run
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
pub mod postgres_workflow;
pub mod postgres_workflow_execution;

use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use async_trait::async_trait;
use crate::domain::agent::{Agent, AgentId};
use crate::domain::execution::{Execution, ExecutionId};
use crate::domain::workflow::{Workflow, WorkflowId};
use crate::domain::repository::{AgentRepository, ExecutionRepository, WorkflowRepository, RepositoryError};

#[derive(Clone)]
pub struct InMemoryAgentRepository {
    agents: Arc<RwLock<HashMap<AgentId, Agent>>>,
}

impl InMemoryAgentRepository {
    pub fn new() -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl AgentRepository for InMemoryAgentRepository {
    async fn save(&self, agent: &Agent) -> Result<(), RepositoryError> {
        let mut agents = self.agents.write().unwrap();
        agents.insert(agent.id, agent.clone());
        Ok(())
    }

    async fn find_by_id(&self, id: AgentId) -> Result<Option<Agent>, RepositoryError> {
        let agents = self.agents.read().unwrap();
        Ok(agents.get(&id).cloned())
    }

    async fn find_by_name(&self, name: &str) -> Result<Option<Agent>, RepositoryError> {
        let agents = self.agents.read().unwrap();
        Ok(agents.values().find(|a| a.name == name).cloned())
    }

    async fn list_all(&self) -> Result<Vec<Agent>, RepositoryError> {
        let agents = self.agents.read().unwrap();
        Ok(agents.values().cloned().collect())
    }

    async fn delete(&self, id: AgentId) -> Result<(), RepositoryError> {
        let mut agents = self.agents.write().unwrap();
        agents.remove(&id);
        Ok(())
    }
}

// Keep the AgentLifecycleService implementation for backward compatibility
use crate::application::agent::AgentLifecycleService;
use crate::domain::agent::AgentManifest;

#[async_trait]
impl AgentLifecycleService for InMemoryAgentRepository {
    async fn deploy_agent(&self, manifest: AgentManifest) -> anyhow::Result<AgentId> {
        let agent = Agent::new(manifest);
        let id = agent.id;
        self.save(&agent).await
            .map_err(|e| anyhow::anyhow!("Failed to save agent: {}", e))?;
        Ok(id)
    }

    async fn get_agent(&self, id: AgentId) -> anyhow::Result<Agent> {
        self.find_by_id(id).await
            .map_err(|e| anyhow::anyhow!("Repository error: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("Agent not found"))
    }

    async fn update_agent(&self, id: AgentId, manifest: AgentManifest) -> anyhow::Result<()> {
        let mut agent = self.get_agent(id).await?;
        agent.update_manifest(manifest);
        self.save(&agent).await
            .map_err(|e| anyhow::anyhow!("Failed to update agent: {}", e))
    }

    async fn delete_agent(&self, id: AgentId) -> anyhow::Result<()> {
        self.delete(id).await
            .map_err(|e| anyhow::anyhow!("Failed to delete agent: {}", e))
    }

    async fn list_agents(&self) -> anyhow::Result<Vec<Agent>> {
        self.list_all().await
            .map_err(|e| anyhow::anyhow!("Failed to list agents: {}", e))
    }

    async fn lookup_agent(&self, name: &str) -> anyhow::Result<Option<AgentId>> {
        let agent = self.find_by_name(name).await
            .map_err(|e| anyhow::anyhow!("Repository error: {}", e))?;
        Ok(agent.map(|a| a.id))
    }
}

#[derive(Clone)]
pub struct InMemoryExecutionRepository {
    executions: Arc<RwLock<HashMap<ExecutionId, Execution>>>,
}

impl InMemoryExecutionRepository {
    pub fn new() -> Self {
        Self {
            executions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl ExecutionRepository for InMemoryExecutionRepository {
    async fn save(&self, execution: &Execution) -> Result<(), RepositoryError> {
        let mut executions = self.executions.write().unwrap();
        executions.insert(execution.id, execution.clone());
        Ok(())
    }

    async fn find_by_id(&self, id: ExecutionId) -> Result<Option<Execution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        Ok(executions.get(&id).cloned())
    }

    async fn find_by_agent(&self, agent_id: AgentId) -> Result<Vec<Execution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        Ok(executions.values()
            .filter(|e| e.agent_id == agent_id)
            .cloned()
            .collect())
    }

    async fn find_recent(&self, limit: usize) -> Result<Vec<Execution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        let mut execution_list: Vec<Execution> = executions.values().cloned().collect();
        // Sort by started_at desc
        execution_list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(execution_list.into_iter().take(limit).collect())
    }

    async fn delete(&self, id: ExecutionId) -> Result<(), RepositoryError> {
        let mut executions = self.executions.write().unwrap();
        executions.remove(&id);
        Ok(())
    }
}

#[derive(Clone)]
pub struct InMemoryWorkflowRepository {
    workflows: Arc<RwLock<HashMap<WorkflowId, Workflow>>>,
}

impl InMemoryWorkflowRepository {
    pub fn new() -> Self {
        Self {
            workflows: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl WorkflowRepository for InMemoryWorkflowRepository {
    async fn save(&self, workflow: &Workflow) -> Result<(), RepositoryError> {
        let mut workflows = self.workflows.write().unwrap();
        workflows.insert(workflow.id, workflow.clone());
        Ok(())
    }

    async fn find_by_id(&self, id: WorkflowId) -> Result<Option<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        Ok(workflows.get(&id).cloned())
    }

    async fn find_by_name(&self, name: &str) -> Result<Option<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        Ok(workflows.values().find(|w| w.metadata.name == name).cloned())
    }

    async fn list_all(&self) -> Result<Vec<Workflow>, RepositoryError> {
        let workflows = self.workflows.read().unwrap();
        Ok(workflows.values().cloned().collect())
    }

    async fn delete(&self, id: WorkflowId) -> Result<(), RepositoryError> {
        let mut workflows = self.workflows.write().unwrap();
        workflows.remove(&id);
        Ok(())
    }
}

#[derive(Clone)]
pub struct InMemoryWorkflowExecutionRepository {
    executions: Arc<RwLock<HashMap<crate::domain::execution::ExecutionId, crate::domain::workflow::WorkflowExecution>>>,
}

impl InMemoryWorkflowExecutionRepository {
    pub fn new() -> Self {
        Self {
            executions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl crate::domain::repository::WorkflowExecutionRepository for InMemoryWorkflowExecutionRepository {
    async fn save(&self, execution: &crate::domain::workflow::WorkflowExecution) -> Result<(), RepositoryError> {
        let mut executions = self.executions.write().unwrap();
        executions.insert(execution.id, execution.clone());
        Ok(())
    }

    async fn find_by_id(&self, id: crate::domain::execution::ExecutionId) -> Result<Option<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        Ok(executions.get(&id).cloned())
    }

    async fn find_active(&self) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        let executions = self.executions.read().unwrap();
        Ok(executions.values()
            .filter(|e| e.status == crate::domain::execution::ExecutionStatus::Running)
            .cloned()
            .collect())
    }
}
