// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Schedule service (AEGIS ADR-139 N1 to N8)
//!
//! Creates, changes, pauses, resumes and deletes a person's schedules over a
//! Temporal Schedule each ([`ScheduleEnginePort`]); answers a Temporal
//! Schedule's fire by starting the run as the schedule's owner
//! ([`ScheduledRunPort`]); and, at boot, re-creates any Temporal Schedule
//! that has gone missing.
//!
//! The row is written first and the Temporal Schedule second; when Temporal
//! refuses, the row is taken back and the caller is told nothing was saved
//! (N5). A fire is decided once per (schedule, scheduled time): a repeated
//! fire answers the first one's row (N6).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::application::agent::AgentLifecycleService;
use crate::application::execution::ExecutionService;
use crate::application::ports::{ScheduleEnginePort, TemporalScheduleSpec};
use crate::application::start_workflow_execution::{
    StartWorkflowExecutionRequest, StartWorkflowExecutionUseCase,
};
use crate::domain::execution::{ExecutionError, ExecutionId, ExecutionInput, ExecutionStatus};
use crate::domain::iam::UserIdentity;
use crate::domain::repository::{RepositoryError, WorkflowExecutionRepository};
use crate::domain::schedule::{
    count_refusal, paused_after_refusals, FireClaim, FireOutcome, Schedule, ScheduleDraft,
    ScheduleFire, ScheduleId, SchedulePatch, ScheduleRepository, ScheduleState, TargetKind,
    MAX_SCHEDULES_PER_OWNER, REFUSALS_BEFORE_PAUSE, SCHEDULED_RUN_SECURITY_CONTEXT,
    UNAVAILABLE_REFUSAL,
};
use crate::domain::tenant::TenantId;

/// Why a schedule call was not done.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ScheduleError {
    /// The request was refused before anything was stored: the sentence.
    #[error("{0}")]
    Refused(String),
    /// No such schedule, or not the caller's.
    #[error("Not found")]
    NotFound,
    /// The caller may read but not do this.
    #[error("{0}")]
    Forbidden(String),
    /// Temporal could not be reached; nothing was saved (N5).
    #[error("{}", UNAVAILABLE_REFUSAL)]
    Unavailable,
    #[error("Schedule store failed: {0}")]
    Repository(String),
}

impl From<RepositoryError> for ScheduleError {
    fn from(e: RepositoryError) -> Self {
        Self::Repository(e.to_string())
    }
}

/// Starts a schedule's run as its owner, and reads a run's status (N7, N8).
#[async_trait]
pub trait ScheduledRunPort: Send + Sync {
    /// Start the run exactly as the starting tool would for the owner;
    /// `Err` carries the refusal's sentence.
    async fn start(&self, schedule: &Schedule, owner: &UserIdentity)
        -> Result<ExecutionId, String>;

    /// The status of a run the schedule started; `None` when it is gone.
    async fn run_status(
        &self,
        kind: TargetKind,
        tenant: &TenantId,
        execution_id: ExecutionId,
    ) -> anyhow::Result<Option<ExecutionStatus>>;
}

/// Who reads a schedule.
#[derive(Debug, Clone)]
pub enum ScheduleReader {
    /// The owner, by `sub`.
    Owner { sub: String },
    /// An operator, reading one tenant's schedules.
    Operator { tenant: TenantId },
}

/// A schedule as its routes answer it.
#[derive(Debug, Clone)]
pub struct ScheduleView {
    pub schedule: Schedule,
    /// Temporal's next action time; `None` when paused or completed.
    pub next_run_at: Option<DateTime<Utc>>,
    /// The newest fire.
    pub last_run: Option<ScheduleFire>,
}

/// One fire with its run's status, for `GET /v1/schedules/{id}/runs`.
#[derive(Debug, Clone)]
pub struct ScheduleRunView {
    pub fire: ScheduleFire,
    pub kind: TargetKind,
    pub status: Option<ExecutionStatus>,
}

pub struct ScheduleService {
    repo: Arc<dyn ScheduleRepository>,
    engine: Arc<dyn ScheduleEnginePort>,
    runs: Arc<dyn ScheduledRunPort>,
}

fn engine_spec(schedule: &Schedule) -> TemporalScheduleSpec {
    TemporalScheduleSpec {
        temporal_schedule_id: schedule.temporal_schedule_id.clone(),
        schedule_id: schedule.id.to_string(),
        tenant_id: schedule.tenant_id.as_str().to_string(),
        timing: schedule.timing.clone(),
        paused: schedule.state != ScheduleState::Active,
    }
}

fn is_running(status: &ExecutionStatus) -> bool {
    matches!(status, ExecutionStatus::Pending | ExecutionStatus::Running)
}

impl ScheduleService {
    pub fn new(
        repo: Arc<dyn ScheduleRepository>,
        engine: Arc<dyn ScheduleEnginePort>,
        runs: Arc<dyn ScheduledRunPort>,
    ) -> Self {
        Self { repo, engine, runs }
    }

    /// The schedule if `reader` may read it.
    async fn readable(
        &self,
        reader: &ScheduleReader,
        id: ScheduleId,
    ) -> Result<Schedule, ScheduleError> {
        let schedule = self
            .repo
            .get(id)
            .await?
            .filter(|s| s.deleted_at.is_none())
            .ok_or(ScheduleError::NotFound)?;
        let allowed = match reader {
            ScheduleReader::Owner { sub } => &schedule.owner.sub == sub,
            ScheduleReader::Operator { tenant } => &schedule.tenant_id == tenant,
        };
        if !allowed {
            return Err(ScheduleError::NotFound);
        }
        Ok(schedule)
    }

    /// The schedule if `owner` owns it.
    async fn owned(&self, owner: &UserIdentity, id: ScheduleId) -> Result<Schedule, ScheduleError> {
        self.readable(
            &ScheduleReader::Owner {
                sub: owner.sub.clone(),
            },
            id,
        )
        .await
    }

    async fn view(&self, schedule: Schedule) -> Result<ScheduleView, ScheduleError> {
        let next_run_at = if schedule.state == ScheduleState::Active {
            match self
                .engine
                .describe_schedule(&schedule.temporal_schedule_id)
                .await
            {
                Ok(Some(description)) if !description.paused => {
                    description.next_action_times.first().copied()
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(schedule_id = %schedule.id, error = %e, "DescribeSchedule failed");
                    None
                }
            }
        } else {
            None
        };
        let last_run = self.repo.fires(schedule.id, 1).await?.into_iter().next();
        Ok(ScheduleView {
            schedule,
            next_run_at,
            last_run,
        })
    }

    /// `POST /v1/schedules` (N1 to N5).
    pub async fn create(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
        draft: ScheduleDraft,
    ) -> Result<ScheduleView, ScheduleError> {
        let schedule = Schedule::create(draft, owner, tenant.clone(), Utc::now())
            .map_err(ScheduleError::Refused)?;
        let held = self.repo.list_for_owner(&schedule.owner.sub).await?;
        if held.len() >= MAX_SCHEDULES_PER_OWNER {
            return Err(ScheduleError::Refused(count_refusal()));
        }
        self.repo.insert(&schedule).await?;
        if let Err(e) = self.engine.create_schedule(&engine_spec(&schedule)).await {
            tracing::error!(schedule_id = %schedule.id, error = %e, "CreateSchedule failed; the row is taken back");
            self.repo.remove(schedule.id).await?;
            return Err(ScheduleError::Unavailable);
        }
        self.view(schedule).await
    }

    /// `GET /v1/schedules`.
    pub async fn list(&self, reader: &ScheduleReader) -> Result<Vec<ScheduleView>, ScheduleError> {
        let schedules = match reader {
            ScheduleReader::Owner { sub } => self.repo.list_for_owner(sub).await?,
            ScheduleReader::Operator { tenant } => self.repo.list_for_tenant(tenant).await?,
        };
        let mut views = Vec::with_capacity(schedules.len());
        for schedule in schedules {
            views.push(self.view(schedule).await?);
        }
        Ok(views)
    }

    /// `GET /v1/schedules/{id}`.
    pub async fn get(
        &self,
        reader: &ScheduleReader,
        id: ScheduleId,
    ) -> Result<ScheduleView, ScheduleError> {
        let schedule = self.readable(reader, id).await?;
        self.view(schedule).await
    }

    /// Write `next` and its Temporal Schedule; on Temporal's refusal put
    /// `previous` back and answer that nothing was saved.
    async fn store_with_engine(
        &self,
        previous: &Schedule,
        next: &Schedule,
        call: impl std::future::Future<Output = anyhow::Result<()>>,
    ) -> Result<(), ScheduleError> {
        self.repo.update(next).await?;
        if let Err(e) = call.await {
            tracing::error!(schedule_id = %next.id, error = %e, "Temporal schedule call failed; the row is put back");
            self.repo.update(previous).await?;
            return Err(ScheduleError::Unavailable);
        }
        Ok(())
    }

    /// `PATCH /v1/schedules/{id}` (N10): the Temporal Schedule updated in
    /// the same request.
    pub async fn update(
        &self,
        owner: &UserIdentity,
        id: ScheduleId,
        patch: SchedulePatch,
    ) -> Result<ScheduleView, ScheduleError> {
        let previous = self.owned(owner, id).await?;
        let mut next = previous.clone();
        next.apply(patch, owner, Utc::now())
            .map_err(ScheduleError::Refused)?;
        let spec = engine_spec(&next);
        let engine = self.engine.clone();
        let was_completed = previous.state == ScheduleState::Completed;
        self.store_with_engine(&previous, &next, async move {
            if was_completed {
                // A completed schedule's Temporal Schedule may be gone or
                // spent: make it again.
                engine.delete_schedule(&spec.temporal_schedule_id).await?;
                engine.create_schedule(&spec).await
            } else {
                engine.update_schedule(&spec).await
            }
        })
        .await?;
        self.view(next).await
    }

    /// `POST /v1/schedules/{id}/pause`.
    pub async fn pause(
        &self,
        owner: &UserIdentity,
        id: ScheduleId,
    ) -> Result<ScheduleView, ScheduleError> {
        let previous = self.owned(owner, id).await?;
        if previous.state != ScheduleState::Active {
            return self.view(previous).await;
        }
        let mut next = previous.clone();
        next.state = ScheduleState::Paused;
        next.updated_at = Utc::now();
        let engine = self.engine.clone();
        let temporal_id = next.temporal_schedule_id.clone();
        self.store_with_engine(&previous, &next, async move {
            engine.set_schedule_paused(&temporal_id, true).await
        })
        .await?;
        self.view(next).await
    }

    /// `POST /v1/schedules/{id}/resume`: the owner's projection written
    /// again from their live token.
    pub async fn resume(
        &self,
        owner: &UserIdentity,
        id: ScheduleId,
    ) -> Result<ScheduleView, ScheduleError> {
        let previous = self.owned(owner, id).await?;
        if previous.state != ScheduleState::Paused {
            return self.view(previous).await;
        }
        let mut next = previous.clone();
        next.owner = crate::domain::schedule::ScheduleOwner::from_identity(owner)
            .map_err(ScheduleError::Refused)?;
        next.state = ScheduleState::Active;
        next.paused_reason = None;
        next.updated_at = Utc::now();
        let engine = self.engine.clone();
        let temporal_id = next.temporal_schedule_id.clone();
        self.store_with_engine(&previous, &next, async move {
            engine.set_schedule_paused(&temporal_id, false).await
        })
        .await?;
        self.view(next).await
    }

    /// `DELETE /v1/schedules/{id}`: the Temporal Schedule deleted, the row's
    /// `deleted_at` set; its runs keep their `schedule_id`.
    pub async fn delete(&self, owner: &UserIdentity, id: ScheduleId) -> Result<(), ScheduleError> {
        let mut schedule = self.owned(owner, id).await?;
        if let Err(e) = self
            .engine
            .delete_schedule(&schedule.temporal_schedule_id)
            .await
        {
            tracing::error!(schedule_id = %schedule.id, error = %e, "DeleteSchedule failed");
            return Err(ScheduleError::Unavailable);
        }
        let now = Utc::now();
        schedule.deleted_at = Some(now);
        schedule.updated_at = now;
        self.repo.update(&schedule).await?;
        Ok(())
    }

    /// `GET /v1/schedules/{id}/runs`: its fires newest first, each with its
    /// run's status.
    pub async fn runs(
        &self,
        reader: &ScheduleReader,
        id: ScheduleId,
        limit: usize,
    ) -> Result<Vec<ScheduleRunView>, ScheduleError> {
        let schedule = self.readable(reader, id).await?;
        let fires = self.repo.fires(id, limit).await?;
        let mut views = Vec::with_capacity(fires.len());
        for fire in fires {
            let status = match fire.execution_id {
                Some(execution) => self
                    .runs
                    .run_status(schedule.target_kind, &schedule.tenant_id, execution)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "a scheduled run's status could not be read");
                        None
                    }),
                None => None,
            };
            views.push(ScheduleRunView {
                fire,
                kind: schedule.target_kind,
                status,
            });
        }
        Ok(views)
    }

    /// `POST /v1/internal/schedules/{id}/fire` (N6, N7): decided once per
    /// scheduled time, in the tenant the worker names.
    pub async fn fire(
        &self,
        tenant: &TenantId,
        id: ScheduleId,
        scheduled_time: DateTime<Utc>,
    ) -> Result<ScheduleFire, ScheduleError> {
        let mut schedule = self
            .repo
            .get(id)
            .await?
            .filter(|s| s.deleted_at.is_none() && &s.tenant_id == tenant)
            .ok_or(ScheduleError::NotFound)?;
        let mut fire = match self.repo.claim_fire(id, scheduled_time, Utc::now()).await? {
            FireClaim::Repeated(first) => return Ok(first),
            FireClaim::Claimed(fire) => fire,
        };

        if schedule.state != ScheduleState::Active {
            fire.outcome = FireOutcome::SkippedPaused;
            self.repo.finish_fire(&fire).await?;
            return Ok(fire);
        }

        if self.last_run_is_running(&schedule).await {
            fire.outcome = FireOutcome::SkippedOverlap;
            self.repo.finish_fire(&fire).await?;
            return Ok(fire);
        }

        let started = match schedule.owner.to_identity(&schedule.tenant_id) {
            Ok(owner) => self.runs.start(&schedule, &owner).await,
            Err(sentence) => Err(sentence),
        };
        match started {
            Ok(execution_id) => {
                self.repo
                    .bind_execution(schedule.target_kind, execution_id, schedule.id)
                    .await?;
                fire.outcome = FireOutcome::Started;
                fire.execution_id = Some(execution_id);
                self.repo.finish_fire(&fire).await?;
            }
            Err(sentence) => {
                fire.outcome = FireOutcome::Refused;
                fire.detail = Some(sentence.clone());
                self.repo.finish_fire(&fire).await?;
                if !schedule.is_once() && self.refused_in_a_row(schedule.id).await? {
                    schedule.state = ScheduleState::Paused;
                    schedule.paused_reason = Some(paused_after_refusals(&sentence));
                    schedule.updated_at = Utc::now();
                    self.repo.update(&schedule).await?;
                    if let Err(e) = self
                        .engine
                        .set_schedule_paused(&schedule.temporal_schedule_id, true)
                        .await
                    {
                        // The row is paused, so a later fire is skipped
                        // whatever Temporal holds; the boot re-creation and
                        // the next resume bring the two back together.
                        tracing::warn!(schedule_id = %schedule.id, error = %e, "PatchSchedule (pause) failed");
                    }
                }
            }
        }
        if schedule.is_once() && schedule.state == ScheduleState::Active {
            schedule.state = ScheduleState::Completed;
            schedule.updated_at = Utc::now();
            self.repo.update(&schedule).await?;
        }
        Ok(fire)
    }

    /// Whether the run the newest started fire began is still running.
    async fn last_run_is_running(&self, schedule: &Schedule) -> bool {
        let fires = match self.repo.fires(schedule.id, 50).await {
            Ok(fires) => fires,
            Err(_) => return false,
        };
        let Some(execution_id) = fires
            .iter()
            .find(|f| f.outcome == FireOutcome::Started)
            .and_then(|f| f.execution_id)
        else {
            return false;
        };
        match self
            .runs
            .run_status(schedule.target_kind, &schedule.tenant_id, execution_id)
            .await
        {
            Ok(Some(status)) => is_running(&status),
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(schedule_id = %schedule.id, error = %e, "the last run's status could not be read");
                false
            }
        }
    }

    /// The newest fires that decided anything are all refusals, as many as
    /// the bound.
    async fn refused_in_a_row(&self, id: ScheduleId) -> Result<bool, ScheduleError> {
        let fires = self.repo.fires(id, REFUSALS_BEFORE_PAUSE).await?;
        Ok(fires.len() == REFUSALS_BEFORE_PAUSE
            && fires.iter().all(|f| f.outcome == FireOutcome::Refused))
    }

    /// At boot: describe each active or paused schedule's Temporal Schedule
    /// and create any that is missing (N5). Answers how many were made.
    pub async fn recreate_missing(&self) -> Result<usize, ScheduleError> {
        let mut made = 0;
        for schedule in self.repo.list_live().await? {
            match self
                .engine
                .describe_schedule(&schedule.temporal_schedule_id)
                .await
            {
                Ok(Some(_)) => {}
                Ok(None) => match self.engine.create_schedule(&engine_spec(&schedule)).await {
                    Ok(()) => made += 1,
                    Err(e) => {
                        tracing::error!(schedule_id = %schedule.id, error = %e, "re-creating a missing Temporal Schedule failed")
                    }
                },
                Err(e) => {
                    tracing::error!(schedule_id = %schedule.id, error = %e, "DescribeSchedule failed at boot")
                }
            }
        }
        Ok(made)
    }
}

// ── The production run starter ───────────────────────────────────────────────

/// Starts a schedule's run through the services the starting tools use,
/// with the same checks, as the owner (N7, N8).
pub struct ServiceRunStarter {
    execution_service: Arc<dyn ExecutionService>,
    agent_lifecycle: Arc<dyn AgentLifecycleService>,
    workflow_start: Option<Arc<dyn StartWorkflowExecutionUseCase>>,
    workflow_executions: Option<Arc<dyn WorkflowExecutionRepository>>,
}

impl ServiceRunStarter {
    pub fn new(
        execution_service: Arc<dyn ExecutionService>,
        agent_lifecycle: Arc<dyn AgentLifecycleService>,
        workflow_start: Option<Arc<dyn StartWorkflowExecutionUseCase>>,
        workflow_executions: Option<Arc<dyn WorkflowExecutionRepository>>,
    ) -> Self {
        Self {
            execution_service,
            agent_lifecycle,
            workflow_start,
            workflow_executions,
        }
    }

    async fn start_agent(
        &self,
        schedule: &Schedule,
        owner: &UserIdentity,
    ) -> Result<ExecutionId, String> {
        let tenant = &schedule.tenant_id;
        let target = &schedule.target;
        let agent_id = if let Ok(uuid) = uuid::Uuid::parse_str(target) {
            if schedule.target_version.is_some() {
                return Err(
                    "version parameter is only supported when identifying agents by name, not UUID"
                        .to_string(),
                );
            }
            crate::domain::agent::AgentId(uuid)
        } else if let Some(version) = &schedule.target_version {
            match self
                .agent_lifecycle
                .lookup_agent_for_tenant_with_version(tenant, target, version)
                .await
            {
                Ok(Some(id)) => id,
                _ => return Err(format!("Agent '{target}' version '{version}' not found")),
            }
        } else {
            match self
                .agent_lifecycle
                .lookup_agent_visible_for_tenant(tenant, target)
                .await
            {
                Ok(Some(id)) => id,
                _ => return Err(format!("Agent '{target}' not found")),
            }
        };
        let mut input = schedule.start_input();
        if let Some(map) = input.as_object_mut() {
            map.entry("tenant_id")
                .or_insert_with(|| serde_json::Value::String(tenant.to_string()));
        }
        self.execution_service
            .start_execution(
                agent_id,
                ExecutionInput {
                    intent: schedule.intent.clone(),
                    input,
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: schedule.attachments.clone(),
                },
                SCHEDULED_RUN_SECURITY_CONTEXT.to_string(),
                Some(owner),
            )
            .await
            .map_err(|e| match e.downcast_ref::<ExecutionError>() {
                Some(refused @ ExecutionError::Refused(_)) => refused.to_string(),
                _ => format!("Failed to start task execution: {e}"),
            })
    }

    async fn start_workflow(
        &self,
        schedule: &Schedule,
        owner: &UserIdentity,
    ) -> Result<ExecutionId, String> {
        let start = self
            .workflow_start
            .as_ref()
            .ok_or_else(|| "Workflow execution service not configured".to_string())?;
        let started = start
            .start_execution_for_tenant(
                &schedule.tenant_id,
                StartWorkflowExecutionRequest {
                    workflow_id: schedule.target.clone(),
                    input: schedule.start_input(),
                    blackboard: None,
                    version: schedule.target_version.clone(),
                    tenant_id: Some(schedule.tenant_id.clone()),
                    security_context_name: Some(SCHEDULED_RUN_SECURITY_CONTEXT.to_string()),
                    intent: schedule.intent.clone(),
                },
                Some(owner),
            )
            .await
            .map_err(|e| format!("Failed to start workflow: {e}"))?;
        uuid::Uuid::parse_str(&started.execution_id)
            .map(ExecutionId)
            .map_err(|e| format!("Failed to start workflow: {e}"))
    }
}

#[async_trait]
impl ScheduledRunPort for ServiceRunStarter {
    async fn start(
        &self,
        schedule: &Schedule,
        owner: &UserIdentity,
    ) -> Result<ExecutionId, String> {
        match schedule.target_kind {
            TargetKind::Agent => self.start_agent(schedule, owner).await,
            TargetKind::Workflow => self.start_workflow(schedule, owner).await,
        }
    }

    async fn run_status(
        &self,
        kind: TargetKind,
        tenant: &TenantId,
        execution_id: ExecutionId,
    ) -> anyhow::Result<Option<ExecutionStatus>> {
        match kind {
            TargetKind::Agent => Ok(self
                .execution_service
                .get_execution_for_tenant(tenant, execution_id)
                .await
                .ok()
                .map(|e| e.status)),
            TargetKind::Workflow => match &self.workflow_executions {
                Some(repo) => Ok(repo
                    .find_by_id_for_tenant(tenant, execution_id)
                    .await?
                    .map(|e| e.status)),
                None => Ok(None),
            },
        }
    }
}
