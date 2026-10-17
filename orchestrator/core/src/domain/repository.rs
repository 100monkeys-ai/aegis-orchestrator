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

use crate::domain::agent::{Agent, AgentId, AgentScope, AgentStatus};
use crate::domain::execution::{Execution, ExecutionId};
use crate::domain::tenancy::Tenant;
use crate::domain::tenant::TenantId;
use crate::domain::volume::{Volume, VolumeId, VolumeOwnership};
use crate::domain::workflow::{Workflow, WorkflowId, WorkflowScope, WorkflowStatus};
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
    /// A connection URL, which may carry the password; prints redacted.
    pub connection_string: crate::domain::secrets::SensitiveUrl,
}

// ============================================================================
// Active and archived rows (AEGIS ADR-148)
// ============================================================================

/// Which rows a list reads by status (AEGIS ADR-148): the live lists
/// read `Active` unless `?status=archived|all` asks for the others. For an
/// agent, `Active` is every row not archived, so a paused agent stays listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusFilter {
    #[default]
    Active,
    Archived,
    All,
}

/// The sentence a `?status` of another word is refused with.
pub const STATUS_FILTER_REFUSAL: &str = "status must be one of: active, archived, all";

impl StatusFilter {
    /// The text a repository binds for the filter: `active`, `archived`
    /// or `all`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::All => "all",
        }
    }

    /// `?status`: `active` when absent, else `active`, `archived` or `all`.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("") | Some("active") => Ok(Self::Active),
            Some("archived") => Ok(Self::Archived),
            Some("all") => Ok(Self::All),
            Some(_) => Err(STATUS_FILTER_REFUSAL.to_string()),
        }
    }

    /// Whether a workflow of `status` is read.
    pub fn admits_workflow(self, status: WorkflowStatus) -> bool {
        match self {
            Self::Active => status == WorkflowStatus::Active,
            Self::Archived => status == WorkflowStatus::Archived,
            Self::All => true,
        }
    }

    /// Whether an agent of `status` is read.
    pub fn admits_agent(self, status: &AgentStatus) -> bool {
        match self {
            Self::Active => *status != AgentStatus::Archived,
            Self::Archived => *status == AgentStatus::Archived,
            Self::All => true,
        }
    }
}

/// The set of workflows a list reads, before its status filter.
#[derive(Debug, Clone, Copy)]
pub enum WorkflowListing<'a> {
    /// Every tenant's (operator only, ADR-097).
    All,
    /// The global (built-in) rows.
    Global,
    /// A tenant's own rows and the global rows.
    Visible(&'a TenantId),
    /// A tenant's own rows.
    Tenant(&'a TenantId),
}

/// The set of agents a list reads, before its status filter.
#[derive(Debug, Clone, Copy)]
pub enum AgentListing<'a> {
    /// Every tenant's (operator only, ADR-097).
    All,
    /// A tenant's own rows and the global rows.
    Visible(&'a TenantId),
    /// A tenant's own rows.
    Tenant(&'a TenantId),
}

/// The sentence a caller asking to set a built-in's status is refused with.
pub const BUILTIN_STATUS_REFUSAL: &str = "a built-in's status is set by its deploy";

/// What a status change asked for by id, or by name and version, did.
#[derive(Debug, Clone)]
pub enum StatusChange<T> {
    /// The row now has the status asked for.
    Changed(T),
    /// No row of the caller's tenant has that id or name (and version).
    NotFound,
    /// The row is a built-in: its status is set by its deploy (403).
    Global,
    /// The name has several rows and no version was named (409).
    Ambiguous { name: String, versions: usize },
}

impl<T> StatusChange<T> {
    /// The refusal's sentence for `Ambiguous`.
    pub fn ambiguous_sentence(name: &str, versions: usize) -> String {
        format!("'{name}' has {versions} versions; name the version")
    }
}

/// A new run or schedule naming an archived row by its id, or by name and
/// version, is refused with the name's active version (A10). A schedule that
/// names an archived id fails its next run with this refusal and is never
/// retargeted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind} '{name}' version '{version}' is archived; {}", active_clause(name, active_version.as_deref()))]
pub struct ArchivedTarget {
    /// `workflow` or `agent`.
    pub kind: &'static str,
    pub name: String,
    pub version: String,
    /// The version of the name a run resolves to now, if any is active.
    pub active_version: Option<String>,
}

fn active_clause(name: &str, active_version: Option<&str>) -> String {
    match active_version {
        Some(version) => format!("the active version is '{version}'"),
        None => format!("no version of '{name}' is active"),
    }
}

impl ArchivedTarget {
    /// The refusal of a run of the archived `workflow`.
    pub fn workflow(workflow: &Workflow, active_version: Option<String>) -> Self {
        Self {
            kind: "workflow",
            name: workflow.metadata.name.clone(),
            version: workflow.metadata.version.clone().unwrap_or_default(),
            active_version,
        }
    }

    /// The refusal of a run of the archived `agent`.
    pub fn agent(agent: &Agent, active_version: Option<String>) -> Self {
        Self {
            kind: "agent",
            name: agent.name.clone(),
            version: agent.manifest.metadata.version.clone(),
            active_version,
        }
    }
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

    /// List every agent across every tenant. Used by infrastructure
    /// background tasks (Cortex discovery backfill / drift reconciliation)
    /// that must observe agents under their own owning tenant rather than
    /// a single hardcoded tenant. Each returned [`Agent`] carries its own
    /// `tenant_id` field — callers MUST use it for downstream tenant-scoped
    /// operations.
    async fn list_all(&self) -> Result<Vec<Agent>, RepositoryError>;

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

    /// Count agents with `status = 'active'` for a tenant (used by quota enforcement).
    async fn count_active(&self, tenant_id: &TenantId) -> Result<u64, RepositoryError>;

    // --- Active and archived rows (AEGIS ADR-148) ---

    /// Every row of the name in the tenant, any status, newest first.
    async fn list_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Vec<Agent>, RepositoryError>;

    /// The rows of `listing` whose status `filter` admits. The live lists
    /// above read [`StatusFilter::Active`]: every row not archived.
    async fn list_by_status(
        &self,
        listing: AgentListing<'_>,
        filter: StatusFilter,
    ) -> Result<Vec<Agent>, RepositoryError>;

    /// Set the status of the tenant's row `id`; nothing else of it changes.
    async fn set_status_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
        status: AgentStatus,
    ) -> Result<(), RepositoryError> {
        let mut agent = self
            .find_by_id_for_tenant(tenant_id, id)
            .await?
            .ok_or_else(|| RepositoryError::NotFound(format!("Agent {} not found", id.0)))?;
        agent.status = status;
        self.save_for_tenant(tenant_id, &agent).await
    }

    /// A built-in name keeps one live row, the row of `keep_version`: every
    /// other global row of `name` not archived is archived, and the row of
    /// `keep_version` is made active if it was archived. A name with no
    /// global row of `keep_version` is left as it is. Nothing is deleted.
    /// Answers the ids archived.
    async fn archive_superseded_global(
        &self,
        name: &str,
        keep_version: &str,
    ) -> Result<Vec<AgentId>, RepositoryError> {
        let system = TenantId::system();
        let rows: Vec<Agent> = self
            .list_by_name_for_tenant(&system, name)
            .await?
            .into_iter()
            .filter(|a| a.scope == AgentScope::Global)
            .collect();
        let Some(keep) = rows
            .iter()
            .find(|a| a.manifest.metadata.version == keep_version)
            .map(|a| (a.id, a.status.clone()))
        else {
            return Ok(Vec::new());
        };
        let mut archived = Vec::new();
        for row in &rows {
            if row.id != keep.0 && row.status != AgentStatus::Archived {
                self.set_status_for_tenant(&system, row.id, AgentStatus::Archived)
                    .await?;
                archived.push(row.id);
            }
        }
        if keep.1 == AgentStatus::Archived {
            self.set_status_for_tenant(&system, keep.0, AgentStatus::Active)
                .await?;
        }
        Ok(archived)
    }

    /// Archive or unarchive the tenant's row named by `id_or_name`: a UUID
    /// names one row; a name names the tenant's rows of it, narrowed by
    /// `version`. A built-in's status is set by its deploy, never here.
    async fn change_status(
        &self,
        tenant_id: &TenantId,
        id_or_name: &str,
        version: Option<&str>,
        status: AgentStatus,
    ) -> Result<StatusChange<Agent>, RepositoryError> {
        let rows: Vec<Agent> = match uuid::Uuid::parse_str(id_or_name) {
            Ok(uuid) => self
                .find_by_id_visible(tenant_id, AgentId(uuid))
                .await?
                .into_iter()
                .collect(),
            Err(_) => {
                let mut rows = self.list_by_name_for_tenant(tenant_id, id_or_name).await?;
                if !tenant_id.is_system() {
                    rows.extend(
                        self.list_by_name_for_tenant(&TenantId::system(), id_or_name)
                            .await?
                            .into_iter()
                            .filter(|a| a.scope == AgentScope::Global),
                    );
                }
                rows
            }
        };
        let (own, global): (Vec<Agent>, Vec<Agent>) = rows
            .into_iter()
            .filter(|a| version.is_none_or(|v| a.manifest.metadata.version == v))
            .partition(|a| a.scope == AgentScope::Tenant && &a.tenant_id == tenant_id);
        let mut target = match own.len() {
            0 if !global.is_empty() => return Ok(StatusChange::Global),
            0 => return Ok(StatusChange::NotFound),
            1 => own.into_iter().next().expect("one row"),
            versions => {
                return Ok(StatusChange::Ambiguous {
                    name: own[0].name.clone(),
                    versions,
                })
            }
        };
        self.set_status_for_tenant(tenant_id, target.id, status.clone())
            .await?;
        target.status = status;
        Ok(StatusChange::Changed(target))
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

    async fn find_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: WorkflowId,
        limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError>;

    /// The step executions of one workflow execution, oldest first: every
    /// execution of `tenant_id` whose input names `workflow_execution_id`
    /// (AEGIS ADR-005 I9, P4).
    async fn find_by_workflow_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_execution_id: uuid::Uuid,
    ) -> Result<Vec<Execution>, RepositoryError>;

    /// The step executions of one workflow execution, oldest first, each
    /// read on its own: a step whose row cannot be read is its id with the
    /// error, never left out (AEGIS ADR-131 U32, U32a: `steps_unread`). The
    /// default reads them all at once, as
    /// [`Self::find_by_workflow_execution_for_tenant`] does; an implementation
    /// that loads its rows one by one overrides it.
    #[allow(clippy::type_complexity)]
    async fn read_steps_of_workflow_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_execution_id: uuid::Uuid,
    ) -> Result<Vec<Result<Execution, (ExecutionId, RepositoryError)>>, RepositoryError> {
        Ok(self
            .find_by_workflow_execution_for_tenant(tenant_id, workflow_execution_id)
            .await?
            .into_iter()
            .map(Ok)
            .collect())
    }

    /// The executions of any of `workflow_ids`, newest first, at most
    /// `limit` (AEGIS ADR-148: a name's executions are every version's).
    /// The default reads each id in turn; a repository that can read them
    /// at once overrides it.
    async fn find_by_workflows_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_ids: &[WorkflowId],
        limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError> {
        let mut all: Vec<Execution> = Vec::new();
        for id in workflow_ids {
            for execution in self
                .find_by_workflow_for_tenant(tenant_id, *id, limit)
                .await?
            {
                if !all.iter().any(|e| e.id == execution.id) {
                    all.push(execution);
                }
            }
        }
        all.sort_by_key(|e| std::cmp::Reverse(e.started_at));
        all.truncate(limit);
        Ok(all)
    }

    async fn find_recent_for_tenant(
        &self,
        tenant_id: &TenantId,
        limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError>;

    /// List recent executions across every tenant, newest first.
    ///
    /// Used by `list_executions_handler` and `dashboard_summary_handler`
    /// when the caller is an operator (cross-tenant aggregation, ADR-097).
    /// Each returned [`Execution`] carries its own `tenant_id`; callers
    /// MUST surface it in user-facing responses.
    async fn list_recent_all_paginated(
        &self,
        limit: usize,
        offset: usize,
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

    /// Count executions with `status IN ('running', 'pending')` for a tenant (quota enforcement).
    async fn count_running(&self, tenant_id: &TenantId) -> Result<u64, RepositoryError>;

    /// Every execution of every tenant whose status is `pending` or
    /// `running`, newest first.
    ///
    /// Read once, at start-up, by the pass that ends the executions an
    /// earlier orchestrator process left unfinished
    /// ([`crate::application::execution::fail_executions_cut_by_restart`]).
    /// Each returned [`Execution`] carries its own `tenant_id`, which the
    /// caller MUST use for the write that follows.
    ///
    /// A repository that cannot list them answers an error rather than an
    /// empty list, so the pass reports that it could not run instead of
    /// reporting that there was nothing to end.
    async fn find_unfinished_unscoped(&self) -> Result<Vec<Execution>, RepositoryError> {
        Err(RepositoryError::Unknown(
            "this execution repository does not list unfinished executions".to_string(),
        ))
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

    /// List every workflow across every tenant. Used by infrastructure
    /// background tasks (Cortex discovery backfill / drift reconciliation)
    /// that must observe workflows under their own owning tenant rather
    /// than a single hardcoded tenant. Each returned [`Workflow`] carries
    /// its own `tenant_id` field — callers MUST use it for downstream
    /// tenant-scoped operations.
    async fn list_all(&self) -> Result<Vec<Workflow>, RepositoryError>;

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

    // --- Active and archived rows (AEGIS ADR-148) ---

    /// The rows of `listing` whose status `filter` admits. The live lists
    /// above read [`StatusFilter::Active`]; a repository that keeps no
    /// archived rows answers the active ones and refuses the others.
    async fn list_by_status(
        &self,
        listing: WorkflowListing<'_>,
        filter: StatusFilter,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        if filter != StatusFilter::Active {
            return Err(RepositoryError::Unknown(
                "this workflow repository does not list archived workflows".to_string(),
            ));
        }
        match listing {
            WorkflowListing::All => self.list_all().await,
            WorkflowListing::Global => self.list_global().await,
            WorkflowListing::Visible(tenant_id) => self.list_visible(tenant_id).await,
            WorkflowListing::Tenant(tenant_id) => self.list_all_for_tenant(tenant_id).await,
        }
    }

    /// Set the status of the tenant's row `id`; nothing else of it changes.
    async fn set_status_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
        status: WorkflowStatus,
    ) -> Result<(), RepositoryError> {
        let mut workflow = self
            .find_by_id_for_tenant(tenant_id, id)
            .await?
            .ok_or_else(|| RepositoryError::NotFound(format!("Workflow {} not found", id.0)))?;
        workflow.status = status;
        self.save_for_tenant(tenant_id, &workflow).await
    }

    /// A built-in name keeps one active row, the row of `keep_version`
    /// (A9): every other global row of `name` that is active is archived,
    /// and the row of `keep_version` is made active. A name with no global
    /// row of `keep_version`, as when its deploy failed, is left as it is.
    /// Nothing is deleted. Answers the ids archived.
    async fn archive_superseded_global(
        &self,
        name: &str,
        keep_version: &str,
    ) -> Result<Vec<WorkflowId>, RepositoryError> {
        let system = TenantId::system();
        let rows: Vec<Workflow> = self
            .list_by_name_for_tenant(&system, name)
            .await?
            .into_iter()
            .filter(|w| w.scope == WorkflowScope::Global && w.tenant_id.is_system())
            .collect();
        let Some(keep) = rows
            .iter()
            .find(|w| w.metadata.version.as_deref() == Some(keep_version))
            .map(|w| (w.id, w.status))
        else {
            return Ok(Vec::new());
        };
        let mut archived = Vec::new();
        for row in &rows {
            if row.id != keep.0 && row.status == WorkflowStatus::Active {
                self.set_status_for_tenant(&system, row.id, WorkflowStatus::Archived)
                    .await?;
                archived.push(row.id);
            }
        }
        if keep.1 != WorkflowStatus::Active {
            self.set_status_for_tenant(&system, keep.0, WorkflowStatus::Active)
                .await?;
        }
        Ok(archived)
    }

    /// Archive or unarchive the tenant's row named by `id_or_name`: a UUID
    /// names one row; a name names the tenant's rows of it, narrowed by
    /// `version`. A built-in's status is set by its deploy, never here.
    async fn change_status(
        &self,
        tenant_id: &TenantId,
        id_or_name: &str,
        version: Option<&str>,
        status: WorkflowStatus,
    ) -> Result<StatusChange<Workflow>, RepositoryError> {
        let rows: Vec<Workflow> = match uuid::Uuid::parse_str(id_or_name) {
            Ok(uuid) => self
                .find_by_id_for_tenant(tenant_id, WorkflowId(uuid))
                .await?
                .into_iter()
                .collect(),
            Err(_) => self.list_by_name_for_tenant(tenant_id, id_or_name).await?,
        };
        let (own, global): (Vec<Workflow>, Vec<Workflow>) = rows
            .into_iter()
            .filter(|w| version.is_none_or(|v| w.metadata.version.as_deref() == Some(v)))
            .partition(|w| w.scope == WorkflowScope::Tenant && &w.tenant_id == tenant_id);
        let mut target = match own.len() {
            0 if !global.is_empty() => return Ok(StatusChange::Global),
            0 => return Ok(StatusChange::NotFound),
            1 => own.into_iter().next().expect("one row"),
            versions => {
                return Ok(StatusChange::Ambiguous {
                    name: own[0].metadata.name.clone(),
                    versions,
                })
            }
        };
        self.set_status_for_tenant(tenant_id, target.id, status)
            .await?;
        target.status = status;
        Ok(StatusChange::Changed(target))
    }

    /// Every version's id of the name the tenant sees, any status: its own
    /// rows and the global rows. A name's executions roll up to these.
    async fn version_ids_visible(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Vec<WorkflowId>, RepositoryError> {
        let mut rows = self.list_by_name_for_tenant(tenant_id, name).await?;
        if !tenant_id.is_system() {
            rows.extend(
                self.list_by_name_for_tenant(&TenantId::system(), name)
                    .await?
                    .into_iter()
                    .filter(|w| w.scope == WorkflowScope::Global),
            );
        }
        let mut ids: Vec<WorkflowId> = Vec::new();
        for row in rows {
            if !ids.contains(&row.id) {
                ids.push(row.id);
            }
        }
        Ok(ids)
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

    /// List workflow executions across every tenant, newest first.
    ///
    /// Operator-only cross-tenant aggregation path (ADR-097). Each returned
    /// [`WorkflowExecution`](crate::domain::workflow::WorkflowExecution)
    /// carries its own `tenant_id`; callers MUST surface it in user-facing
    /// responses.
    async fn list_paginated_all(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError>;

    async fn count_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: crate::domain::workflow::WorkflowId,
    ) -> Result<i64, RepositoryError>;

    /// The workflow executions of any of `workflow_ids` (AEGIS ADR-148: a
    /// name's count is every version's). The default sums each id's count.
    async fn count_by_workflows_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_ids: &[crate::domain::workflow::WorkflowId],
    ) -> Result<i64, RepositoryError> {
        let mut total = 0;
        for id in workflow_ids {
            total += self.count_by_workflow_for_tenant(tenant_id, *id).await?;
        }
        Ok(total)
    }

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
        sequence_number: i64,
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

    /// The tenant's running workflow executions, each with the worker's
    /// latest event of its audit trail (the highest sequence number below
    /// [`LOCAL_SEQUENCE_START`]), or `None` before its first. A workflow
    /// execution's trail also holds rows the orchestrator stores itself (a
    /// workflow agent state's repository rows), from
    /// [`LOCAL_SEQUENCE_START`] on, above every Temporal history sequence;
    /// they are not the worker's and are passed over, so a gate posted after
    /// one is still the run's latest workflow event. Read-only.
    ///
    /// The default implementation reads each running execution's trail page
    /// by page; the Postgres repository answers in two queries.
    async fn find_running_with_latest_event_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<
        Vec<(
            crate::domain::workflow::WorkflowExecution,
            Option<crate::domain::workflow::WorkflowExecutionEventRecord>,
        )>,
        RepositoryError,
    > {
        const PAGE: usize = 500;
        let mut listed = Vec::new();
        for execution in self.find_active_for_tenant(tenant_id).await? {
            let mut latest: Option<crate::domain::workflow::WorkflowExecutionEventRecord> = None;
            let mut offset = 0;
            loop {
                let page = self
                    .find_events_by_execution(execution.id, PAGE, offset)
                    .await?;
                let read = page.len();
                for event in page {
                    if event.sequence >= LOCAL_SEQUENCE_START {
                        continue;
                    }
                    if latest
                        .as_ref()
                        .is_none_or(|held| event.sequence > held.sequence)
                    {
                        latest = Some(event);
                    }
                }
                if read < PAGE {
                    break;
                }
                offset += read;
            }
            listed.push((execution, latest));
        }
        Ok(listed)
    }

    /// The tenant's workflow executions that `initiating_user_sub` started
    /// and that posted at least one Human gate, running or ended, each with
    /// its gate trail: the events of [`GATE_TRAIL_EVENT_TYPES`], which open
    /// and close its gates, in sequence order. Read-only.
    ///
    /// The default implementation pages the tenant's executions and reads
    /// each one's trail; the Postgres repository answers in two queries.
    /// Both narrow a state exit's `output` to a gate answer's fields
    /// (`response`, `feedback`, `timeout`, `answered_by`), dropping those
    /// that are absent or null.
    async fn find_gate_trails_for_initiator(
        &self,
        tenant_id: &TenantId,
        initiating_user_sub: &str,
    ) -> Result<
        Vec<(
            crate::domain::workflow::WorkflowExecution,
            Vec<crate::domain::workflow::WorkflowExecutionEventRecord>,
        )>,
        RepositoryError,
    > {
        /// A gate answer's fields: what the Postgres query keeps of a
        /// state exit's `output`.
        const GATE_ANSWER_FIELDS: [&str; 4] = ["response", "feedback", "timeout", "answered_by"];

        /// `jsonb_strip_nulls`: drops every object field whose value is
        /// null, at any depth.
        fn strip_nulls(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(fields) => {
                    fields.retain(|_, field| !field.is_null());
                    fields.values_mut().for_each(strip_nulls);
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(strip_nulls),
                _ => {}
            }
        }

        /// Narrows a state exit's `output` as the Postgres query does: the
        /// gate answer's fields that are present and not null, `{}` when
        /// none is.
        fn narrow_to_gate_answer(payload: &mut serde_json::Value) {
            let Some(event) = payload.as_object_mut() else {
                return;
            };
            let output = event.get("output");
            let mut narrowed = serde_json::Map::new();
            for field in GATE_ANSWER_FIELDS {
                if let Some(value) = output.and_then(|output| output.get(field)) {
                    narrowed.insert(field.to_string(), value.clone());
                }
            }
            let mut narrowed = serde_json::Value::Object(narrowed);
            strip_nulls(&mut narrowed);
            event.insert("output".to_string(), narrowed);
        }

        const PAGE: usize = 500;
        let mut trails = Vec::new();
        let mut offset = 0;
        loop {
            let executions = self
                .list_paginated_for_tenant(tenant_id, PAGE, offset)
                .await?;
            let read = executions.len();
            for execution in executions {
                if execution.initiating_user_sub.as_deref() != Some(initiating_user_sub) {
                    continue;
                }
                let mut trail = Vec::new();
                let mut events_offset = 0;
                loop {
                    let page = self
                        .find_events_by_execution(execution.id, PAGE, events_offset)
                        .await?;
                    let events_read = page.len();
                    trail.extend(page.into_iter().filter(|event| {
                        GATE_TRAIL_EVENT_TYPES.contains(&event.event_type.as_str())
                    }));
                    if events_read < PAGE {
                        break;
                    }
                    events_offset += events_read;
                }
                if trail
                    .iter()
                    .any(|event| event.event_type == "HumanInputRequested")
                {
                    trail.sort_by_key(|event| event.sequence);
                    for event in trail
                        .iter_mut()
                        .filter(|event| event.event_type == "WorkflowStateExited")
                    {
                        narrow_to_gate_answer(&mut event.payload);
                    }
                    trails.push((execution, trail));
                }
            }
            if read < PAGE {
                break;
            }
            offset += read;
        }
        Ok(trails)
    }
}

/// The workflow event types that open and close a Human gate: the worker's
/// post of the gate, the state exit carrying its answer, the next state
/// entered, and the events that end a run.
pub const GATE_TRAIL_EVENT_TYPES: [&str; 7] = [
    "HumanInputRequested",
    "WorkflowStateExited",
    "WorkflowStateEntered",
    "WorkflowStateVisitLimitExceeded",
    "WorkflowExecutionCompleted",
    "WorkflowExecutionFailed",
    "WorkflowExecutionCancelled",
];

/// The first sequence number an orchestrator process gives the events it
/// stores itself in an execution's audit trail (`ExecutionEventPersister`),
/// far above any Temporal history index, so the Temporal listener and the
/// persister write into the same `(execution_id, sequence_number)` space
/// without colliding. A workflow execution's row below it is the worker's.
pub const LOCAL_SEQUENCE_START: i64 = 1_000_000_000_000;

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

    /// Find all volumes owned by a specific user within a tenant.
    ///
    /// Excludes volumes in `VolumeStatus::Deleted` — that status is a transient
    /// artifact of the delete pipeline and must never appear in user-facing
    /// queries (per ADR-079).
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> Result<Vec<Volume>, RepositoryError>;

    /// Count volumes owned by a specific user within a tenant.
    ///
    /// Excludes volumes in `VolumeStatus::Deleted`, matching `find_by_owner`.
    async fn count_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> Result<u32, RepositoryError>;

    /// Sum allocated `size_limit_bytes` for all non-deleted volumes owned by a
    /// specific user within a tenant.
    ///
    /// This is the *allocation* sum (used for pre-create quota checks against
    /// the tier ceiling), NOT actual SeaweedFS consumption. For actual usage
    /// see `VolumeService::get_volume_usage` (BC-7 storage abstraction).
    async fn sum_allocated_size_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> Result<u64, RepositoryError>;
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

#[cfg(test)]
mod postgres_config_tests {
    #[test]
    fn postgres_config_debug_does_not_print_the_password() {
        let config = super::PostgresConfig {
            connection_string: "postgres://aegis:Mk7-postgres-config-marker@db.internal:5432/aegis"
                .into(),
        };
        let printed = format!("{config:?}");
        assert!(
            !printed.contains("Mk7-postgres-config-marker"),
            "PostgresConfig's Debug printed the password: {printed}"
        );
        assert!(
            printed.contains("db.internal"),
            "Debug lost the host: {printed}"
        );
    }
}
