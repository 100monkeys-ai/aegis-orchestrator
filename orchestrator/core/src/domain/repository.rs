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

use crate::domain::agent::{Agent, AgentId, AgentScope};
use crate::domain::execution::{Execution, ExecutionId};
use crate::domain::tenancy::Tenant;
use crate::domain::tenant::TenantId;
use crate::domain::volume::{Volume, VolumeId, VolumeOwnership};
use crate::domain::workflow::{Workflow, WorkflowId, WorkflowScope};
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

/// A snapshot of a specific agent version from the append-only version history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentVersion {
    pub id: uuid::Uuid,
    pub version: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub manifest_yaml: String,
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

    async fn find_by_name_and_version_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
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

    async fn list_versions_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> Result<Vec<AgentVersion>, RepositoryError>;

    async fn list_visible_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Agent>, RepositoryError>;

    async fn update_scope(
        &self,
        id: AgentId,
        new_scope: AgentScope,
        new_tenant_id: &TenantId,
    ) -> Result<(), RepositoryError>;

    async fn resolve_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Agent>, RepositoryError>;

    /// Fetch an agent by ID, falling through to the global tenant if not found in the requesting tenant.
    async fn find_by_id_visible(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> Result<Option<Agent>, RepositoryError>;
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

    async fn find_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: WorkflowId,
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

    async fn count_by_agent_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> Result<i64, RepositoryError>;

    /// Look up an execution by ID without a tenant filter.
    ///
    /// This is an internal service-to-service method. It intentionally bypasses tenant
    /// isolation because the caller already holds a trusted `ExecutionId` (orchestrator-
    /// provisioned, not user-supplied). The returned execution carries its own `tenant_id`
    /// field which the caller MUST use for all downstream tenant-scoped operations.
    async fn find_by_id_unscoped(
        &self,
        id: ExecutionId,
    ) -> Result<Option<Execution>, RepositoryError>;
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

    async fn find_by_name_and_version_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<Workflow>, RepositoryError>;

    /// List all versions of a workflow by name for a tenant (newest first).
    async fn list_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Vec<Workflow>, RepositoryError>;

    async fn list_all_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Workflow>, RepositoryError>;

    async fn delete_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
    ) -> Result<(), RepositoryError>;

    // --- Scope-aware methods (ADR-076) ---

    /// Resolve a workflow by name using the two-level scope hierarchy.
    /// Priority: Tenant > Global (narrowest wins).
    async fn resolve_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError>;

    /// Resolve a workflow by name and version using the scope hierarchy.
    async fn resolve_by_name_and_version(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<Workflow>, RepositoryError>;

    /// List all workflows visible within a tenant (tenant + global).
    async fn list_visible(&self, tenant_id: &TenantId) -> Result<Vec<Workflow>, RepositoryError>;

    /// List only global-scope workflows.
    async fn list_global(&self) -> Result<Vec<Workflow>, RepositoryError>;

    /// Fetch a workflow by name, falling through to the global tenant if not found in the requesting tenant.
    async fn find_by_name_visible(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError>;

    /// Update the scope of an existing workflow (for promote/demote).
    async fn update_scope(
        &self,
        id: WorkflowId,
        new_scope: WorkflowScope,
        new_tenant_id: &TenantId,
    ) -> Result<(), RepositoryError>;

    /// Fetch a workflow by ID, falling through to the system tenant if not found in the requesting tenant.
    ///
    /// The default implementation performs two sequential `find_by_id_for_tenant` calls.
    /// Repository implementations may override this with a single-pass query if desired.
    async fn find_by_id_visible(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
    ) -> Result<Option<Workflow>, RepositoryError> {
        if let Some(workflow) = self.find_by_id_for_tenant(tenant_id, id).await? {
            return Ok(Some(workflow));
        }
        self.find_by_id_for_tenant(&TenantId::system(), id).await
    }

    /// Fetch a workflow by ID, checking the requesting tenant first then falling through to the
    /// system tenant. Returns `Ok(Some(...))` if found in either tenant, `Ok(None)` if absent
    /// in both. Mirrors the agent visibility pattern.
    async fn get_workflow_visible(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
    ) -> anyhow::Result<crate::domain::workflow::Workflow> {
        self.find_by_id_visible(tenant_id, id)
            .await
            .map_err(|e| anyhow::anyhow!("Repository error: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("Workflow not found"))
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

    async fn count_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: crate::domain::workflow::WorkflowId,
    ) -> Result<i64, RepositoryError>;

    /// Persist Temporal workflow/run linkage for an already-created workflow execution.
    async fn update_temporal_linkage_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution_id: ExecutionId,
        temporal_workflow_id: &str,
        temporal_run_id: &str,
    ) -> Result<(), RepositoryError>;

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

    /// Resolve the owning tenant for a workflow execution by its ID.
    ///
    /// Returns `None` if the execution does not exist. Used by the Temporal event listener
    /// to route events to the correct tenant scope without requiring a known tenant upfront.
    async fn find_tenant_id_by_execution(
        &self,
        id: ExecutionId,
    ) -> Result<Option<TenantId>, RepositoryError>;
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
