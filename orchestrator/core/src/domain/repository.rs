// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

// Repository Pattern - Storage Backend Abstraction
//
// Defines pluggable storage backend for repositories, enabling:
// - In-memory storage for development/testing
// - PostgreSQL for production persistence
// - Future storage backends (SQLite, etc.)
//
// Follows DDD Repository Pattern from AGENTS.md

use async_trait::async_trait;
use std::sync::Arc;
use crate::domain::agent::{Agent, AgentId};
use crate::domain::execution::{Execution, ExecutionId};
use crate::domain::workflow::{Workflow, WorkflowId};
use crate::domain::volume::{Volume, VolumeId, TenantId, VolumeOwnership};

/// Storage backend enum for pluggable persistence
#[derive(Debug, Clone)]
pub enum StorageBackend {
    InMemory,
    PostgreSQL(PostgresConfig),
    // Future: SQLite(SqliteConfig),
}

#[derive(Debug, Clone)]
pub struct PostgresConfig {
    pub connection_string: String,
}

/// Repository interface for Agent aggregates
/// One repository per aggregate root (Agent Lifecycle Context)
#[async_trait]
pub trait AgentRepository: Send + Sync {
    /// Save agent (create or update)
    async fn save(&self, agent: &Agent) -> Result<(), RepositoryError>;
    
    /// Find agent by ID
    async fn find_by_id(&self, id: AgentId) -> Result<Option<Agent>, RepositoryError>;
    
    /// Find agent by name
    async fn find_by_name(&self, name: &str) -> Result<Option<Agent>, RepositoryError>;
    
    /// List all agents
    async fn list_all(&self) -> Result<Vec<Agent>, RepositoryError>;
    
    /// Delete agent by ID
    async fn delete(&self, id: AgentId) -> Result<(), RepositoryError>;
}

/// Repository interface for Execution aggregates
/// One repository per aggregate root (Execution Context)
#[async_trait]
pub trait ExecutionRepository: Send + Sync {
    /// Save execution (create or update)
    async fn save(&self, execution: &Execution) -> Result<(), RepositoryError>;
    
    /// Find execution by ID
    async fn find_by_id(&self, id: ExecutionId) -> Result<Option<Execution>, RepositoryError>;
    
    /// Find executions by agent ID
    async fn find_by_agent(&self, agent_id: AgentId) -> Result<Vec<Execution>, RepositoryError>;
    
    /// Find recent executions (limit results)
    async fn find_recent(&self, limit: usize) -> Result<Vec<Execution>, RepositoryError>;
    
    /// Delete execution by ID
    async fn delete(&self, id: ExecutionId) -> Result<(), RepositoryError>;
}

/// Repository interface for Workflow aggregates
/// One repository per aggregate root (Workflow Context)
#[async_trait]
pub trait WorkflowRepository: Send + Sync {
    /// Save workflow (create or update)
    async fn save(&self, workflow: &Workflow) -> Result<(), RepositoryError>;
    
    /// Find workflow by ID
    async fn find_by_id(&self, id: WorkflowId) -> Result<Option<Workflow>, RepositoryError>;
    
    /// Find workflow by name
    async fn find_by_name(&self, name: &str) -> Result<Option<Workflow>, RepositoryError>;
    
    /// List all workflows
    async fn list_all(&self) -> Result<Vec<Workflow>, RepositoryError>;
    
    /// Delete workflow by ID
    async fn delete(&self, id: WorkflowId) -> Result<(), RepositoryError>;
}

/// Repository interface for WorkflowExecution aggregates
/// One repository per aggregate root (Workflow Execution Context)
#[async_trait]
pub trait WorkflowExecutionRepository: Send + Sync {
    /// Save workflow execution (create or update)
    async fn save(&self, execution: &crate::domain::workflow::WorkflowExecution) -> Result<(), RepositoryError>;
    
    /// Find workflow execution by ID
    async fn find_by_id(&self, id: ExecutionId) -> Result<Option<crate::domain::workflow::WorkflowExecution>, RepositoryError>;
        
    /// Find active workflow executions
    async fn find_active(&self) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError>;
}

/// Repository interface for Volume aggregates
/// One repository per aggregate root (Infrastructure & Hosting Context)
#[async_trait]
pub trait VolumeRepository: Send + Sync {
    /// Save volume (create or update)
    async fn save(&self, volume: &Volume) -> Result<(), RepositoryError>;
    
    /// Find volume by ID
    async fn find_by_id(&self, id: VolumeId) -> Result<Option<Volume>, RepositoryError>;
    
    /// Find volumes by tenant
    async fn find_by_tenant(&self, tenant_id: TenantId) -> Result<Vec<Volume>, RepositoryError>;
    
    /// Find expired volumes (for garbage collection)
    async fn find_expired(&self) -> Result<Vec<Volume>, RepositoryError>;
    
    /// Find volumes by ownership pattern
    async fn find_by_ownership(&self, ownership: &VolumeOwnership) -> Result<Vec<Volume>, RepositoryError>;
    
    /// Delete volume by ID
    async fn delete(&self, id: VolumeId) -> Result<(), RepositoryError>;
}


/// Repository errors
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("Entity not found: {0}")]
    NotFound(String),
    
    #[error("Database error: {0}")]
    Database(String),
    
    #[error("Serialization error: {0}")]
    Serialization(String),
    
    #[error("Unknown error: {0}")]
    Unknown(String),
}

use crate::infrastructure::repositories::{InMemoryAgentRepository, InMemoryExecutionRepository};
use crate::infrastructure::repositories::postgres_agent::PostgresAgentRepository;
use crate::infrastructure::repositories::postgres_execution::PostgresExecutionRepository;
use crate::infrastructure::repositories::postgres_workflow_execution::PostgresWorkflowExecutionRepository;
use crate::infrastructure::repositories::postgres_volume::PostgresVolumeRepository;

/// Factory for creating repositories from storage backend
pub fn create_agent_repository(backend: &StorageBackend, pool: sqlx::PgPool) -> Arc<dyn AgentRepository> {
    match backend {
        StorageBackend::InMemory => {
            Arc::new(InMemoryAgentRepository::new())
        }
        StorageBackend::PostgreSQL(_) => {
            Arc::new(PostgresAgentRepository::new(pool))
        }
    }
}

pub fn create_execution_repository(backend: &StorageBackend, pool: sqlx::PgPool) -> Arc<dyn ExecutionRepository> {
    match backend {
        StorageBackend::InMemory => {
            Arc::new(InMemoryExecutionRepository::new())
        }
        StorageBackend::PostgreSQL(_) => {
            Arc::new(PostgresExecutionRepository::new(pool))
        }
    }
}

pub fn create_workflow_execution_repository(backend: &StorageBackend, pool: sqlx::PgPool) -> Arc<dyn WorkflowExecutionRepository> {
    match backend {
        StorageBackend::InMemory => {
            // Arc::new(InMemoryWorkflowExecutionRepository::new())
            todo!("InMemoryWorkflowExecutionRepository not yet implemented")
        }
        StorageBackend::PostgreSQL(_) => {
             Arc::new(PostgresWorkflowExecutionRepository::new(pool))
        }
    }
}

pub fn create_volume_repository(backend: &StorageBackend, pool: sqlx::PgPool) -> Arc<dyn VolumeRepository> {
    match backend {
        StorageBackend::InMemory => {
            // TODO: Implement InMemoryVolumeRepository for testing
            todo!("InMemoryVolumeRepository not yet implemented")
        }
        StorageBackend::PostgreSQL(_) => {
            Arc::new(PostgresVolumeRepository::new(pool))
        }
    }
}

impl From<sqlx::Error> for RepositoryError {
    fn from(err: sqlx::Error) -> Self {
        match err {
            sqlx::Error::RowNotFound => RepositoryError::NotFound("Row not found".to_string()),
            _ => RepositoryError::Database(err.to_string()),
        }
    }
}

impl From<serde_json::Error> for RepositoryError {
    fn from(err: serde_json::Error) -> Self {
        RepositoryError::Serialization(err.to_string())
    }
}
