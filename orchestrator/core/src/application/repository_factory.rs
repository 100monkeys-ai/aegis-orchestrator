// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Repository Factory - Application Layer
//!
//! Creates concrete repository implementations based on storage backend configuration.
//! This module belongs to the Application Layer per AGENTS.md Section 9, keeping
//! the Domain Layer pure and free of infrastructure dependencies.
//!
//! Per DDD best practices and the AEGIS architecture (AGENTS.md):
//! - Domain layer: Defines repository traits (pure interfaces)
//! - Application layer: Implements factories that create repository instances
//! - Infrastructure layer: Provides concrete implementations
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for repository factory

use std::sync::Arc;
use sqlx::PgPool;

use crate::domain::repository::{
    StorageBackend, AgentRepository, ExecutionRepository, WorkflowExecutionRepository,
    VolumeRepository, StorageEventRepository,
};
use crate::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryWorkflowExecutionRepository,
    InMemoryVolumeRepository, InMemoryStorageEventRepository,
};
use crate::infrastructure::repositories::postgres_agent::PostgresAgentRepository;
use crate::infrastructure::repositories::postgres_execution::PostgresExecutionRepository;
use crate::infrastructure::repositories::postgres_workflow_execution::PostgresWorkflowExecutionRepository;
use crate::infrastructure::repositories::postgres_volume::PostgresVolumeRepository;
use crate::infrastructure::repositories::postgres_storage_event::PostgresStorageEventRepository;

/// Creates an AgentRepository implementation based on the configured backend
pub fn create_agent_repository(backend: &StorageBackend, pool: PgPool) -> Arc<dyn AgentRepository> {
    match backend {
        StorageBackend::InMemory => Arc::new(InMemoryAgentRepository::new()),
        StorageBackend::PostgreSQL(_) => Arc::new(PostgresAgentRepository::new(pool)),
    }
}

/// Creates an ExecutionRepository implementation based on the configured backend
pub fn create_execution_repository(backend: &StorageBackend, pool: PgPool) -> Arc<dyn ExecutionRepository> {
    match backend {
        StorageBackend::InMemory => Arc::new(InMemoryExecutionRepository::new()),
        StorageBackend::PostgreSQL(_) => Arc::new(PostgresExecutionRepository::new(pool)),
    }
}

/// Creates a WorkflowExecutionRepository implementation based on the configured backend
pub fn create_workflow_execution_repository(
    backend: &StorageBackend,
    pool: PgPool,
) -> Arc<dyn WorkflowExecutionRepository> {
    match backend {
        StorageBackend::InMemory => Arc::new(InMemoryWorkflowExecutionRepository::new()),
        StorageBackend::PostgreSQL(_) => Arc::new(PostgresWorkflowExecutionRepository::new(pool)),
    }
}

/// Creates a VolumeRepository implementation based on the configured backend
pub fn create_volume_repository(backend: &StorageBackend, pool: PgPool) -> Arc<dyn VolumeRepository> {
    match backend {
        StorageBackend::InMemory => Arc::new(InMemoryVolumeRepository::new()),
        StorageBackend::PostgreSQL(_) => Arc::new(PostgresVolumeRepository::new(pool)),
    }
}

/// Creates a StorageEventRepository implementation based on the configured backend
pub fn create_storage_event_repository(
    backend: &StorageBackend,
    pool: PgPool,
) -> Arc<dyn StorageEventRepository> {
    match backend {
        StorageBackend::InMemory => Arc::new(InMemoryStorageEventRepository::new()),
        StorageBackend::PostgreSQL(_) => Arc::new(PostgresStorageEventRepository::new(pool)),
    }
}
