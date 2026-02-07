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
    async fn save(&self, agent: Agent) -> Result<(), RepositoryError>;
    
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
    async fn save(&self, execution: Execution) -> Result<(), RepositoryError>;
    
    /// Find execution by ID
    async fn find_by_id(&self, id: ExecutionId) -> Result<Option<Execution>, RepositoryError>;
    
    /// Find executions by agent ID
    async fn find_by_agent(&self, agent_id: AgentId) -> Result<Vec<Execution>, RepositoryError>;
    
    /// Find recent executions (limit results)
    async fn find_recent(&self, limit: usize) -> Result<Vec<Execution>, RepositoryError>;
    
    /// Delete execution by ID
    async fn delete(&self, id: ExecutionId) -> Result<(), RepositoryError>;
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

/// Factory for creating repositories from storage backend
pub fn create_agent_repository(backend: &StorageBackend) -> Arc<dyn AgentRepository> {
    match backend {
        StorageBackend::InMemory => {
            Arc::new(InMemoryAgentRepository::new())
        }
        StorageBackend::PostgreSQL(_config) => {
            // Future: Arc::new(PostgresAgentRepository::new(config))
            todo!("PostgreSQL repository not yet implemented")
        }
    }
}

pub fn create_execution_repository(backend: &StorageBackend) -> Arc<dyn ExecutionRepository> {
    match backend {
        StorageBackend::InMemory => {
            Arc::new(InMemoryExecutionRepository::new())
        }
        StorageBackend::PostgreSQL(_config) => {
            // Future: Arc::new(PostgresExecutionRepository::new(config))
            todo!("PostgreSQL repository not yet implemented")
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
