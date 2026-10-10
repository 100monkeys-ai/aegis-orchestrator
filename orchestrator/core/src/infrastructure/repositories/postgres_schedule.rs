// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Schedule repositories (AEGIS ADR-139 N1, N6, N7)
//!
//! [`PostgresScheduleRepository`] stores schedules and their fires in the
//! `schedules` and `schedule_fires` tables of migration `048_schedules.sql`
//! (with the `profile_id` column of `050_profile_scoping.sql`, and the
//! nullable `scheduled_time` of `051_schedule_run_now.sql`),
//! and records a started run's schedule by the `schedule_id` column that
//! migration adds to `executions` and `workflow_executions` (as a goal's id
//! is recorded). [`InMemoryScheduleRepository`] keeps them in process, for
//! tests and for a daemon run without a database.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::domain::execution::ExecutionId;
use crate::domain::repository::RepositoryError;
use crate::domain::schedule::{
    FireClaim, FireOutcome, OwnerKind, Recurrence, Schedule, ScheduleFire, ScheduleId,
    ScheduleOwner, ScheduleRepository, ScheduleState, TargetKind, Timing,
};
use crate::domain::tenant::TenantId;

const SCHEDULE_COLUMNS: &str = "id, tenant_id, owner_sub, owner_realm, owner_kind, \
     owner_zaru_tier, name, target_kind, target, target_version, intent, input, attachments, \
     repositories, contexts, run_at, cron, timezone, jitter_seconds, state, paused_reason, \
     temporal_schedule_id, created_at, updated_at, deleted_at, profile_id";

const FIRE_COLUMNS: &str =
    "id, schedule_id, scheduled_time, fired_at, outcome, execution_id, detail";

pub struct PostgresScheduleRepository {
    pool: PgPool,
}

impl PostgresScheduleRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn column<'r, T>(row: &'r PgRow, name: &str) -> Result<T, RepositoryError>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(name)
        .map_err(|e| RepositoryError::Serialization(format!("{name}: {e}")))
}

fn unknown(name: &str, value: &str) -> RepositoryError {
    RepositoryError::Serialization(format!("unknown {name}: {value}"))
}

fn hydrate_schedule(row: &PgRow) -> Result<Schedule, RepositoryError> {
    let tenant: String = column(row, "tenant_id")?;
    let owner_kind: String = column(row, "owner_kind")?;
    let target_kind: String = column(row, "target_kind")?;
    let state: String = column(row, "state")?;
    let run_at: Option<DateTime<Utc>> = column(row, "run_at")?;
    let cron: Option<String> = column(row, "cron")?;
    let timezone: Option<String> = column(row, "timezone")?;
    let jitter: Option<i32> = column(row, "jitter_seconds")?;
    let timing = match (run_at, cron, timezone, jitter) {
        (Some(at), None, None, None) => Timing::Once { at },
        (None, Some(cron), Some(timezone), Some(jitter)) => Timing::Recurrence(Recurrence {
            cron,
            timezone,
            jitter_seconds: u32::try_from(jitter)
                .map_err(|_| unknown("jitter_seconds", &jitter.to_string()))?,
        }),
        _ => return Err(RepositoryError::Serialization("schedule timing".into())),
    };
    let attachments: serde_json::Value = column(row, "attachments")?;
    Ok(Schedule {
        id: ScheduleId(column(row, "id")?),
        tenant_id: TenantId::new(tenant)
            .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?,
        owner: ScheduleOwner {
            sub: column(row, "owner_sub")?,
            realm: column(row, "owner_realm")?,
            kind: OwnerKind::parse(&owner_kind)
                .ok_or_else(|| unknown("owner_kind", &owner_kind))?,
            zaru_tier: column(row, "owner_zaru_tier")?,
        },
        name: column(row, "name")?,
        target_kind: TargetKind::parse(&target_kind)
            .ok_or_else(|| unknown("target_kind", &target_kind))?,
        target: column(row, "target")?,
        target_version: column(row, "target_version")?,
        intent: column(row, "intent")?,
        input: column(row, "input")?,
        attachments: serde_json::from_value(attachments)
            .map_err(|e| RepositoryError::Serialization(format!("attachments: {e}")))?,
        repositories: column(row, "repositories")?,
        contexts: column(row, "contexts")?,
        profile_id: column(row, "profile_id")?,
        timing,
        state: ScheduleState::parse(&state).ok_or_else(|| unknown("state", &state))?,
        paused_reason: column(row, "paused_reason")?,
        temporal_schedule_id: column(row, "temporal_schedule_id")?,
        created_at: column(row, "created_at")?,
        updated_at: column(row, "updated_at")?,
        deleted_at: column(row, "deleted_at")?,
    })
}

fn hydrate_fire(row: &PgRow) -> Result<ScheduleFire, RepositoryError> {
    let outcome: String = column(row, "outcome")?;
    let execution: Option<Uuid> = column(row, "execution_id")?;
    Ok(ScheduleFire {
        id: column(row, "id")?,
        schedule_id: ScheduleId(column(row, "schedule_id")?),
        scheduled_time: column(row, "scheduled_time")?,
        fired_at: column(row, "fired_at")?,
        outcome: FireOutcome::parse(&outcome).ok_or_else(|| unknown("outcome", &outcome))?,
        execution_id: execution.map(ExecutionId),
        detail: column(row, "detail")?,
    })
}

type TimingColumns = (
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
    Option<i32>,
);

fn timing_columns(timing: &Timing) -> TimingColumns {
    match timing {
        Timing::Once { at } => (Some(*at), None, None, None),
        Timing::Recurrence(r) => (
            None,
            Some(r.cron.clone()),
            Some(r.timezone.clone()),
            Some(i32::try_from(r.jitter_seconds).unwrap_or(i32::MAX)),
        ),
    }
}

fn attachments_json(schedule: &Schedule) -> Result<serde_json::Value, RepositoryError> {
    serde_json::to_value(&schedule.attachments)
        .map_err(|e| RepositoryError::Serialization(format!("attachments: {e}")))
}

#[async_trait]
impl ScheduleRepository for PostgresScheduleRepository {
    async fn insert(&self, s: &Schedule) -> Result<(), RepositoryError> {
        let (run_at, cron, timezone, jitter) = timing_columns(&s.timing);
        sqlx::query(&format!(
            "INSERT INTO schedules ({SCHEDULE_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
             $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26)"
        ))
        .bind(s.id.0)
        .bind(s.tenant_id.as_str())
        .bind(&s.owner.sub)
        .bind(&s.owner.realm)
        .bind(s.owner.kind.as_str())
        .bind(&s.owner.zaru_tier)
        .bind(&s.name)
        .bind(s.target_kind.as_str())
        .bind(&s.target)
        .bind(&s.target_version)
        .bind(&s.intent)
        .bind(&s.input)
        .bind(attachments_json(s)?)
        .bind(&s.repositories)
        .bind(&s.contexts)
        .bind(run_at)
        .bind(cron)
        .bind(timezone)
        .bind(jitter)
        .bind(s.state.as_str())
        .bind(&s.paused_reason)
        .bind(&s.temporal_schedule_id)
        .bind(s.created_at)
        .bind(s.updated_at)
        .bind(s.deleted_at)
        .bind(s.profile_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update(&self, s: &Schedule) -> Result<(), RepositoryError> {
        let (run_at, cron, timezone, jitter) = timing_columns(&s.timing);
        let done = sqlx::query(
            "UPDATE schedules SET owner_sub = $2, owner_realm = $3, owner_kind = $4, \
             owner_zaru_tier = $5, name = $6, target_kind = $7, target = $8, target_version = $9, \
             intent = $10, input = $11, attachments = $12, repositories = $13, contexts = $14, \
             run_at = $15, cron = $16, timezone = $17, jitter_seconds = $18, state = $19, \
             paused_reason = $20, updated_at = $21, deleted_at = $22, profile_id = $23 \
             WHERE id = $1",
        )
        .bind(s.id.0)
        .bind(&s.owner.sub)
        .bind(&s.owner.realm)
        .bind(s.owner.kind.as_str())
        .bind(&s.owner.zaru_tier)
        .bind(&s.name)
        .bind(s.target_kind.as_str())
        .bind(&s.target)
        .bind(&s.target_version)
        .bind(&s.intent)
        .bind(&s.input)
        .bind(attachments_json(s)?)
        .bind(&s.repositories)
        .bind(&s.contexts)
        .bind(run_at)
        .bind(cron)
        .bind(timezone)
        .bind(jitter)
        .bind(s.state.as_str())
        .bind(&s.paused_reason)
        .bind(s.updated_at)
        .bind(s.deleted_at)
        .bind(s.profile_id)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!("schedule {}", s.id)));
        }
        Ok(())
    }

    async fn remove(&self, id: ScheduleId) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM schedules WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get(&self, id: ScheduleId) -> Result<Option<Schedule>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE id = $1"
        ))
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(hydrate_schedule).transpose()
    }

    async fn list_for_owner(&self, owner_sub: &str) -> Result<Vec<Schedule>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE owner_sub = $1 \
             AND deleted_at IS NULL ORDER BY created_at DESC"
        ))
        .bind(owner_sub)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_schedule).collect()
    }

    async fn list_for_tenant(&self, tenant: &TenantId) -> Result<Vec<Schedule>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE tenant_id = $1 \
             AND deleted_at IS NULL ORDER BY created_at DESC"
        ))
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_schedule).collect()
    }

    async fn list_live(&self) -> Result<Vec<Schedule>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE deleted_at IS NULL \
             AND state IN ('active', 'paused') ORDER BY created_at"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_schedule).collect()
    }

    async fn claim_fire(
        &self,
        schedule_id: ScheduleId,
        scheduled_time: Option<DateTime<Utc>>,
        fired_at: DateTime<Utc>,
    ) -> Result<FireClaim, RepositoryError> {
        // A null scheduled time never conflicts (NULLs are distinct in the
        // unique constraint): a run asked for now is always inserted.
        let inserted = sqlx::query(&format!(
            "INSERT INTO schedule_fires ({FIRE_COLUMNS}) VALUES ($1, $2, $3, $4, 'starting', NULL, NULL) \
             ON CONFLICT (schedule_id, scheduled_time) DO NOTHING RETURNING {FIRE_COLUMNS}"
        ))
        .bind(Uuid::new_v4())
        .bind(schedule_id.0)
        .bind(scheduled_time)
        .bind(fired_at)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = inserted {
            return Ok(FireClaim::Claimed(hydrate_fire(&row)?));
        }
        let row = sqlx::query(&format!(
            "SELECT {FIRE_COLUMNS} FROM schedule_fires WHERE schedule_id = $1 AND scheduled_time = $2"
        ))
        .bind(schedule_id.0)
        .bind(scheduled_time)
        .fetch_one(&self.pool)
        .await?;
        Ok(FireClaim::Repeated(hydrate_fire(&row)?))
    }

    async fn finish_fire(&self, fire: &ScheduleFire) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE schedule_fires SET outcome = $2, execution_id = $3, detail = $4 WHERE id = $1",
        )
        .bind(fire.id)
        .bind(fire.outcome.as_str())
        .bind(fire.execution_id.map(|e| e.0))
        .bind(&fire.detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn fires(
        &self,
        schedule_id: ScheduleId,
        limit: usize,
    ) -> Result<Vec<ScheduleFire>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {FIRE_COLUMNS} FROM schedule_fires WHERE schedule_id = $1 \
             ORDER BY COALESCE(scheduled_time, fired_at) DESC, fired_at DESC LIMIT $2"
        ))
        .bind(schedule_id.0)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_fire).collect()
    }

    async fn bind_execution(
        &self,
        kind: TargetKind,
        execution_id: ExecutionId,
        schedule_id: ScheduleId,
    ) -> Result<(), RepositoryError> {
        let sql = match kind {
            TargetKind::Agent => "UPDATE executions SET schedule_id = $1 WHERE id = $2",
            TargetKind::Workflow => "UPDATE workflow_executions SET schedule_id = $1 WHERE id = $2",
        };
        sqlx::query(sql)
            .bind(schedule_id.0)
            .bind(execution_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Schedules and fires in process.
#[derive(Default)]
pub struct InMemoryScheduleRepository {
    schedules: RwLock<HashMap<ScheduleId, Schedule>>,
    fires: RwLock<Vec<ScheduleFire>>,
    bound: RwLock<HashMap<ExecutionId, (TargetKind, ScheduleId)>>,
}

impl InMemoryScheduleRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// The schedule a run was bound to, and the kind of run it is.
    pub async fn bound_schedule(
        &self,
        execution_id: ExecutionId,
    ) -> Option<(TargetKind, ScheduleId)> {
        self.bound.read().await.get(&execution_id).copied()
    }
}

fn newest_first(mut schedules: Vec<Schedule>) -> Vec<Schedule> {
    schedules.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    schedules
}

#[async_trait]
impl ScheduleRepository for InMemoryScheduleRepository {
    async fn insert(&self, schedule: &Schedule) -> Result<(), RepositoryError> {
        self.schedules
            .write()
            .await
            .insert(schedule.id, schedule.clone());
        Ok(())
    }

    async fn update(&self, schedule: &Schedule) -> Result<(), RepositoryError> {
        let mut schedules = self.schedules.write().await;
        if !schedules.contains_key(&schedule.id) {
            return Err(RepositoryError::NotFound(format!(
                "schedule {}",
                schedule.id
            )));
        }
        schedules.insert(schedule.id, schedule.clone());
        Ok(())
    }

    async fn remove(&self, id: ScheduleId) -> Result<(), RepositoryError> {
        self.schedules.write().await.remove(&id);
        Ok(())
    }

    async fn get(&self, id: ScheduleId) -> Result<Option<Schedule>, RepositoryError> {
        Ok(self.schedules.read().await.get(&id).cloned())
    }

    async fn list_for_owner(&self, owner_sub: &str) -> Result<Vec<Schedule>, RepositoryError> {
        Ok(newest_first(
            self.schedules
                .read()
                .await
                .values()
                .filter(|s| s.owner.sub == owner_sub && s.deleted_at.is_none())
                .cloned()
                .collect(),
        ))
    }

    async fn list_for_tenant(&self, tenant: &TenantId) -> Result<Vec<Schedule>, RepositoryError> {
        Ok(newest_first(
            self.schedules
                .read()
                .await
                .values()
                .filter(|s| &s.tenant_id == tenant && s.deleted_at.is_none())
                .cloned()
                .collect(),
        ))
    }

    async fn list_live(&self) -> Result<Vec<Schedule>, RepositoryError> {
        Ok(self
            .schedules
            .read()
            .await
            .values()
            .filter(|s| s.deleted_at.is_none() && s.state != ScheduleState::Completed)
            .cloned()
            .collect())
    }

    async fn claim_fire(
        &self,
        schedule_id: ScheduleId,
        scheduled_time: Option<DateTime<Utc>>,
        fired_at: DateTime<Utc>,
    ) -> Result<FireClaim, RepositoryError> {
        let mut fires = self.fires.write().await;
        if let Some(first) = fires.iter().find(|f| {
            scheduled_time.is_some()
                && f.schedule_id == schedule_id
                && f.scheduled_time == scheduled_time
        }) {
            return Ok(FireClaim::Repeated(first.clone()));
        }
        let fire = ScheduleFire {
            id: Uuid::new_v4(),
            schedule_id,
            scheduled_time,
            fired_at,
            outcome: FireOutcome::Starting,
            execution_id: None,
            detail: None,
        };
        fires.push(fire.clone());
        Ok(FireClaim::Claimed(fire))
    }

    async fn finish_fire(&self, fire: &ScheduleFire) -> Result<(), RepositoryError> {
        let mut fires = self.fires.write().await;
        if let Some(row) = fires.iter_mut().find(|f| f.id == fire.id) {
            *row = fire.clone();
        }
        Ok(())
    }

    async fn fires(
        &self,
        schedule_id: ScheduleId,
        limit: usize,
    ) -> Result<Vec<ScheduleFire>, RepositoryError> {
        let mut fires: Vec<ScheduleFire> = self
            .fires
            .read()
            .await
            .iter()
            .filter(|f| f.schedule_id == schedule_id)
            .cloned()
            .collect();
        fires.sort_by_key(|f| {
            std::cmp::Reverse((f.scheduled_time.unwrap_or(f.fired_at), f.fired_at))
        });
        fires.truncate(limit);
        Ok(fires)
    }

    async fn bind_execution(
        &self,
        kind: TargetKind,
        execution_id: ExecutionId,
        schedule_id: ScheduleId,
    ) -> Result<(), RepositoryError> {
        self.bound
            .write()
            .await
            .insert(execution_id, (kind, schedule_id));
        Ok(())
    }
}
