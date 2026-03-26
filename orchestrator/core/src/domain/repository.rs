// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Domain Repository Interfaces (AGENTS.md §Repository Patterns)
//!
//! Persistence contracts for each aggregate root, following the DDD Repository
//! pattern: one repository per aggregate, interface defined in the domain layer,
//! implemented in `crate::infrastructure::repositories`.
//!
//! | Trait | Aggregate | Implementations |
//! |-------|-----------|----------------|
//! | `AgentRepository` | `Agent` | `InMemoryAgentRepository`, `PostgresAgentRepository` |
//! | `ExecutionRepository` | `Execution` | `InMemoryExecutionRepository` |
//! | `WorkflowRepository` | `Workflow` | `InMemoryWorkflowRepository` |
//! | `VolumeRepository` | `Volume` | `InMemoryVolumeRepository`, `PostgresVolumeRepository` |
//!
//! ## Storage Backend Abstraction
//!
//! Concrete implementations are selected at orchestrator startup based on
//! configuration (`aegis-config.yaml`). In-memory implementations are used
//! for development and testing; PostgreSQL implementations (ADR-025) for
//! production.
//!
//! See AGENTS.md §Repository Patterns, ADR-025 (PostgreSQL Schema).
//!
//! # Code Quality Principles
//!
//! - Keep repository contracts in the domain layer and implementations in infrastructure.
//! - Return explicit errors for missing or inconsistent persistence state.
//! - Preserve aggregate boundaries and tenant scoping in repository methods.

// Repository Pattern - Storage Backend Abstraction
//
// Defines pluggable storage backend for repositories, enabling:
// - In-memory storage for development/testing
// - PostgreSQL for production persistence
// - Future storage backends (SQLite, etc.)
//
// Follows DDD Repository Pattern from AGENTS.md

use crate::domain::agent::{Agent, AgentId};
use crate::domain::execution::{Execution, ExecutionId};
use crate::domain::tenancy::Tenant;
use crate::domain::tenant::TenantId;
use crate::domain::volume::{Volume, VolumeId, VolumeOwnership};
use crate::domain::workflow::{Workflow, WorkflowId};
use async_trait::async_trait;

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
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent: &Agent,
    ) -> Result<(), RepositoryError>;

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> Result<Option<Agent>, RepositoryError>;

    async fn find_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Agent>, RepositoryError>;

    async fn list_all_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Agent>, RepositoryError>;

    async fn delete_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> Result<(), RepositoryError>;

    /// Save agent (create or update)
    async fn save(&self, agent: &Agent) -> Result<(), RepositoryError> {
        self.save_for_tenant(&TenantId::local_default(), agent)
            .await
    }

    /// Find agent by ID
    async fn find_by_id(&self, id: AgentId) -> Result<Option<Agent>, RepositoryError> {
        self.find_by_id_for_tenant(&TenantId::local_default(), id)
            .await
    }

    /// Find agent by name
    async fn find_by_name(&self, name: &str) -> Result<Option<Agent>, RepositoryError> {
        self.find_by_name_for_tenant(&TenantId::local_default(), name)
            .await
    }

    /// List all agents
    async fn list_all(&self) -> Result<Vec<Agent>, RepositoryError> {
        self.list_all_for_tenant(&TenantId::local_default()).await
    }

    /// Delete agent by ID
    async fn delete(&self, id: AgentId) -> Result<(), RepositoryError> {
        self.delete_for_tenant(&TenantId::local_default(), id).await
    }
}

/// Repository interface for Execution aggregates
/// One repository per aggregate root (Execution Context)
#[async_trait]
pub trait ExecutionRepository: Send + Sync {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution: &Execution,
    ) -> Result<(), RepositoryError>;

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Option<Execution>, RepositoryError>;

    async fn find_by_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
        limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError>;

    async fn find_recent_for_tenant(
        &self,
        tenant_id: &TenantId,
        limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError>;

    async fn delete_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<(), RepositoryError>;

    /// Save execution (create or update)
    async fn save(&self, execution: &Execution) -> Result<(), RepositoryError> {
        self.save_for_tenant(&TenantId::local_default(), execution)
            .await
    }

    /// Find execution by ID
    async fn find_by_id(&self, id: ExecutionId) -> Result<Option<Execution>, RepositoryError> {
        self.find_by_id_for_tenant(&TenantId::local_default(), id)
            .await
    }

    /// Find executions by agent ID (newest first, capped at `limit`)
    async fn find_by_agent(
        &self,
        agent_id: AgentId,
        limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError> {
        self.find_by_agent_for_tenant(&TenantId::local_default(), agent_id, limit)
            .await
    }

    /// Find recent executions (limit results)
    async fn find_recent(&self, limit: usize) -> Result<Vec<Execution>, RepositoryError> {
        self.find_recent_for_tenant(&TenantId::local_default(), limit)
            .await
    }

    /// Delete execution by ID
    async fn delete(&self, id: ExecutionId) -> Result<(), RepositoryError> {
        self.delete_for_tenant(&TenantId::local_default(), id).await
    }
}

/// Repository interface for Workflow aggregates
/// One repository per aggregate root (Workflow Context)
#[async_trait]
pub trait WorkflowRepository: Send + Sync {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow: &Workflow,
    ) -> Result<(), RepositoryError>;

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
    ) -> Result<Option<Workflow>, RepositoryError>;

    async fn find_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError>;

    async fn list_all_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Workflow>, RepositoryError>;

    async fn delete_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
    ) -> Result<(), RepositoryError>;

    /// Save workflow (create or update)
    async fn save(&self, workflow: &Workflow) -> Result<(), RepositoryError> {
        self.save_for_tenant(&TenantId::local_default(), workflow)
            .await
    }

    /// Find workflow by ID
    async fn find_by_id(&self, id: WorkflowId) -> Result<Option<Workflow>, RepositoryError> {
        self.find_by_id_for_tenant(&TenantId::local_default(), id)
            .await
    }

    /// Find workflow by name
    async fn find_by_name(&self, name: &str) -> Result<Option<Workflow>, RepositoryError> {
        self.find_by_name_for_tenant(&TenantId::local_default(), name)
            .await
    }

    /// List all workflows
    async fn list_all(&self) -> Result<Vec<Workflow>, RepositoryError> {
        self.list_all_for_tenant(&TenantId::local_default()).await
    }

    /// Delete workflow by ID
    async fn delete(&self, id: WorkflowId) -> Result<(), RepositoryError> {
        self.delete_for_tenant(&TenantId::local_default(), id).await
    }
}

/// Repository interface for WorkflowExecution aggregates
/// One repository per aggregate root (Workflow Execution Context)
#[async_trait]
pub trait WorkflowExecutionRepository: Send + Sync {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution: &crate::domain::workflow::WorkflowExecution,
    ) -> Result<(), RepositoryError>;

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Option<crate::domain::workflow::WorkflowExecution>, RepositoryError>;

    async fn find_active_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError>;

    async fn find_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: crate::domain::workflow::WorkflowId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError>;

    async fn list_paginated_for_tenant(
        &self,
        tenant_id: &TenantId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError>;

    /// Save workflow execution (create or update)
    async fn save(
        &self,
        execution: &crate::domain::workflow::WorkflowExecution,
    ) -> Result<(), RepositoryError> {
        self.save_for_tenant(&TenantId::local_default(), execution)
            .await
    }

    /// Find workflow execution by ID
    async fn find_by_id(
        &self,
        id: ExecutionId,
    ) -> Result<Option<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        self.find_by_id_for_tenant(&TenantId::local_default(), id)
            .await
    }

    /// Resolve the owning tenant for a workflow execution by ID.
    async fn find_tenant_id_by_execution(
        &self,
        id: ExecutionId,
    ) -> Result<Option<TenantId>, RepositoryError> {
        Ok(self.find_by_id(id).await?.map(|we| we.tenant_id))
    }

    /// Find active workflow executions
    async fn find_active(
        &self,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        self.find_active_for_tenant(&TenantId::local_default())
            .await
    }

    async fn find_by_workflow(
        &self,
        workflow_id: crate::domain::workflow::WorkflowId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        self.find_by_workflow_for_tenant(&TenantId::local_default(), workflow_id, limit, offset)
            .await
    }

    /// Persist Temporal workflow/run linkage for an already-created workflow execution.
    async fn update_temporal_linkage_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution_id: ExecutionId,
        temporal_workflow_id: &str,
        temporal_run_id: &str,
    ) -> Result<(), RepositoryError>;

    async fn update_temporal_linkage(
        &self,
        execution_id: ExecutionId,
        temporal_workflow_id: &str,
        temporal_run_id: &str,
    ) -> Result<(), RepositoryError> {
        self.update_temporal_linkage_for_tenant(
            &TenantId::local_default(),
            execution_id,
            temporal_workflow_id,
            temporal_run_id,
        )
        .await
    }

    /// Append an execution event to the event sourcing audit trail (idempotent)
    async fn append_event(
        &self,
        execution_id: ExecutionId,
        temporal_sequence_number: i64,
        event_type: String,
        payload: serde_json::Value,
        iteration_number: Option<u8>,
    ) -> Result<(), RepositoryError>;

    /// Retrieve the persisted audit-trail events for a workflow execution in
    /// sequence order, with pagination support.
    ///
    /// Returns an empty `Vec` if no events have been persisted yet (e.g. the
    /// in-memory reference implementation).
    async fn find_events_by_execution(
        &self,
        id: ExecutionId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecutionEventRecord>, RepositoryError>;

    /// List workflow executions paginated (newest first)
    async fn list_paginated(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
        self.list_paginated_for_tenant(&TenantId::local_default(), limit, offset)
            .await
    }
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
    async fn find_by_ownership(
        &self,
        ownership: &VolumeOwnership,
    ) -> Result<Vec<Volume>, RepositoryError>;

    /// Delete volume by ID
    async fn delete(&self, id: VolumeId) -> Result<(), RepositoryError>;
}

/// Repository interface for StorageEvent audit trail (ADR-036)
/// Persists file-level operations for forensic analysis
#[async_trait]
pub trait StorageEventRepository: Send + Sync {
    /// Save storage event to audit log
    async fn save(
        &self,
        event: &crate::domain::events::StorageEvent,
    ) -> Result<(), RepositoryError>;

    /// Find events by execution ID (ordered by timestamp descending)
    async fn find_by_execution(
        &self,
        execution_id: ExecutionId,
        limit: Option<usize>,
    ) -> Result<Vec<crate::domain::events::StorageEvent>, RepositoryError>;

    /// Find events by volume ID
    async fn find_by_volume(
        &self,
        volume_id: VolumeId,
        limit: Option<usize>,
    ) -> Result<Vec<crate::domain::events::StorageEvent>, RepositoryError>;

    /// Find security violation events (forensic analysis)
    async fn find_violations(
        &self,
        execution_id: Option<ExecutionId>,
    ) -> Result<Vec<crate::domain::events::StorageEvent>, RepositoryError>;
}

/// Repository interface for Tenant aggregates (ADR-056)
#[async_trait]
pub trait TenantRepository: Send + Sync {
    /// Find an active tenant by slug
    async fn find_by_slug(&self, slug: &TenantId) -> Result<Option<Tenant>, RepositoryError>;

    /// Find all active tenants
    async fn find_all_active(&self) -> Result<Vec<Tenant>, RepositoryError>;

    /// Insert a new tenant
    async fn insert(&self, tenant: &Tenant) -> Result<(), RepositoryError>;

    /// Update tenant status (active/suspended/deleted)
    async fn update_status(
        &self,
        slug: &TenantId,
        status: &crate::domain::tenancy::TenantStatus,
    ) -> Result<(), RepositoryError>;

    /// Update tenant quotas
    async fn update_quotas(
        &self,
        slug: &TenantId,
        quotas: &crate::domain::tenancy::TenantQuotas,
    ) -> Result<(), RepositoryError>;
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
