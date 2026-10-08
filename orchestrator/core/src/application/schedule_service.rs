// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Schedule service (AEGIS ADR-139 N1 to N5)
//!
//! Creates, changes, pauses, resumes and deletes a person's schedules over a
//! Temporal Schedule each ([`ScheduleEnginePort`]), and, at boot, re-creates
//! any Temporal Schedule that has gone missing.
//!
//! The row is written first and the Temporal Schedule second; when Temporal
//! refuses, the row is taken back and the caller is told nothing was saved
//! (N5).

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::application::ports::{ScheduleEnginePort, TemporalScheduleSpec};
use crate::domain::iam::UserIdentity;
use crate::domain::repository::RepositoryError;
use crate::domain::schedule::{
    count_refusal, Schedule, ScheduleDraft, ScheduleFire, ScheduleId, SchedulePatch,
    ScheduleRepository, ScheduleState, MAX_SCHEDULES_PER_OWNER, UNAVAILABLE_REFUSAL,
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

pub struct ScheduleService {
    repo: Arc<dyn ScheduleRepository>,
    engine: Arc<dyn ScheduleEnginePort>,
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

impl ScheduleService {
    pub fn new(repo: Arc<dyn ScheduleRepository>, engine: Arc<dyn ScheduleEnginePort>) -> Self {
        Self { repo, engine }
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
