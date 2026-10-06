// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Goal repositories (AEGIS ADR-131 D1, Update U1)
//!
//! [`PostgresGoalRepository`] stores goals and their evaluations in the
//! `goals` and `goal_evaluations` tables of migration `039_goals.sql`, with
//! the `stopped` state and the `stop_reason` and `input_digest` columns of
//! `040_goal_stopped.sql` (U16, U17), the `cancelled` state and the
//! `closed_reason` column of `043_goal_cancelled.sql` (U33, U33a), and
//! binds an execution to its goal by the `goal_id` column that migration adds
//! to `executions` and `workflow_executions`. [`InMemoryGoalRepository`]
//! keeps them in process, for tests and for a daemon run without a database.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::domain::execution::ExecutionId;
use crate::domain::goal::{
    BoundExecution, BoundKind, Goal, GoalChannel, GoalEvaluation, GoalId, GoalOutcome,
    GoalRepository, GoalState, StopReason, WAITING_ON_EXECUTION,
};
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;

const GOAL_COLUMNS: &str =
    "id, tenant_id, user_sub, statement, client_ref, channel, state, rounds, created_at, closed_at, \
     closed_reason";

const EVALUATION_COLUMNS: &str = "id, goal_id, round, attempt, judge_execution_id, \
     companion_answer, verdict, outcome, continue, waiting_on, answer, created_at, decided_at, \
     stop_reason, input_digest";

/// The SQLSTATE of a unique violation: a second evaluation deciding a round
/// already decided (`uq_goal_evaluations_decided_round`, or
/// `uq_goal_evaluations_stopped_round` for a stopping one).
const UNIQUE_VIOLATION: &str = "23505";

pub struct PostgresGoalRepository {
    pool: PgPool,
}

impl PostgresGoalRepository {
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

fn non_negative(value: i32, name: &str) -> Result<u32, RepositoryError> {
    u32::try_from(value).map_err(|_| RepositoryError::Serialization(format!("{name}: {value}")))
}

fn hydrate_goal(row: &PgRow) -> Result<Goal, RepositoryError> {
    let channel: String = column(row, "channel")?;
    let state: String = column(row, "state")?;
    let tenant: String = column(row, "tenant_id")?;
    Ok(Goal {
        id: GoalId(column(row, "id")?),
        tenant_id: TenantId::new(tenant)
            .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?,
        user_sub: column(row, "user_sub")?,
        statement: column(row, "statement")?,
        client_ref: column(row, "client_ref")?,
        channel: GoalChannel::parse(&channel)
            .ok_or_else(|| RepositoryError::Serialization(format!("unknown channel: {channel}")))?,
        state: GoalState::parse(&state).ok_or_else(|| {
            RepositoryError::Serialization(format!("unknown goal state: {state}"))
        })?,
        rounds: non_negative(column(row, "rounds")?, "rounds")?,
        created_at: column(row, "created_at")?,
        closed_at: column(row, "closed_at")?,
        closed_reason: column(row, "closed_reason")?,
    })
}

fn hydrate_evaluation(row: &PgRow) -> Result<GoalEvaluation, RepositoryError> {
    let outcome: Option<String> = column(row, "outcome")?;
    let judge: Option<Uuid> = column(row, "judge_execution_id")?;
    let stop_reason: Option<String> = column(row, "stop_reason")?;
    Ok(GoalEvaluation {
        id: column(row, "id")?,
        goal_id: GoalId(column(row, "goal_id")?),
        round: non_negative(column(row, "round")?, "round")?,
        attempt: non_negative(column(row, "attempt")?, "attempt")?,
        judge_execution_id: judge.map(ExecutionId),
        companion_answer: column(row, "companion_answer")?,
        verdict: column(row, "verdict")?,
        outcome: outcome
            .map(|o| {
                GoalOutcome::parse(&o)
                    .ok_or_else(|| RepositoryError::Serialization(format!("unknown outcome: {o}")))
            })
            .transpose()?,
        r#continue: column(row, "continue")?,
        waiting_on: column(row, "waiting_on")?,
        answer: column(row, "answer")?,
        created_at: column(row, "created_at")?,
        decided_at: column(row, "decided_at")?,
        stop_reason: stop_reason
            .map(|r| {
                StopReason::parse(&r).ok_or_else(|| {
                    RepositoryError::Serialization(format!("unknown stop reason: {r}"))
                })
            })
            .transpose()?,
        input_digest: column(row, "input_digest")?,
    })
}

#[async_trait]
impl GoalRepository for PostgresGoalRepository {
    async fn insert_goal(&self, goal: &Goal) -> Result<(), RepositoryError> {
        sqlx::query(&format!(
            "INSERT INTO goals ({GOAL_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"
        ))
        .bind(goal.id.0)
        .bind(goal.tenant_id.as_str())
        .bind(&goal.user_sub)
        .bind(&goal.statement)
        .bind(&goal.client_ref)
        .bind(goal.channel.as_str())
        .bind(goal.state.as_str())
        .bind(goal.rounds as i32)
        .bind(goal.created_at)
        .bind(goal.closed_at)
        .bind(&goal.closed_reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_goal(&self, id: GoalId) -> Result<Option<Goal>, RepositoryError> {
        let row = sqlx::query(&format!("SELECT {GOAL_COLUMNS} FROM goals WHERE id = $1"))
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(hydrate_goal).transpose()
    }

    async fn list_open_for_client_ref(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        client_ref: &str,
    ) -> Result<Vec<Goal>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {GOAL_COLUMNS} FROM goals WHERE tenant_id = $1 AND user_sub = $2 \
             AND client_ref = $3 AND state = 'open' ORDER BY created_at"
        ))
        .bind(tenant_id.as_str())
        .bind(user_sub)
        .bind(client_ref)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_goal).collect()
    }

    async fn close_goal(
        &self,
        id: GoalId,
        state: GoalState,
        closed_at: DateTime<Utc>,
        reason: Option<&str>,
    ) -> Result<bool, RepositoryError> {
        let done = sqlx::query(
            "UPDATE goals SET state = $2, closed_at = $3, closed_reason = $4 \
             WHERE id = $1 AND state = 'open'",
        )
        .bind(id.0)
        .bind(state.as_str())
        .bind(closed_at)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn find_goal_of_execution(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Option<GoalId>, RepositoryError> {
        let row = sqlx::query(
            "SELECT goal_id FROM executions WHERE id = $1 AND goal_id IS NOT NULL \
             UNION ALL \
             SELECT goal_id FROM workflow_executions WHERE id = $1 AND goal_id IS NOT NULL \
             LIMIT 1",
        )
        .bind(execution_id.0)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(|row| column::<Uuid>(row, "goal_id").map(GoalId))
            .transpose()
    }

    async fn grant_round(&self, id: GoalId, expected_rounds: u32) -> Result<bool, RepositoryError> {
        let done = sqlx::query(
            "UPDATE goals SET rounds = rounds + 1 WHERE id = $1 AND rounds = $2 AND state = 'open'",
        )
        .bind(id.0)
        .bind(expected_rounds as i32)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn bind_execution(
        &self,
        goal_id: GoalId,
        execution_id: ExecutionId,
    ) -> Result<bool, RepositoryError> {
        let done = sqlx::query("UPDATE executions SET goal_id = $1 WHERE id = $2")
            .bind(goal_id.0)
            .bind(execution_id.0)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn bind_workflow_execution(
        &self,
        goal_id: GoalId,
        execution_id: ExecutionId,
    ) -> Result<bool, RepositoryError> {
        let done = sqlx::query("UPDATE workflow_executions SET goal_id = $1 WHERE id = $2")
            .bind(goal_id.0)
            .bind(execution_id.0)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn list_bound(&self, goal_id: GoalId) -> Result<Vec<BoundExecution>, RepositoryError> {
        let rows = sqlx::query(
            "SELECT id, started_at, 'agent' AS kind FROM executions WHERE goal_id = $1 \
             UNION ALL \
             SELECT id, started_at, 'workflow' AS kind FROM workflow_executions WHERE goal_id = $1 \
             ORDER BY started_at, id",
        )
        .bind(goal_id.0)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let kind: String = column(row, "kind")?;
                Ok(BoundExecution {
                    execution_id: ExecutionId(column(row, "id")?),
                    kind: if kind == "agent" {
                        BoundKind::Agent
                    } else {
                        BoundKind::Workflow
                    },
                    started_at: column(row, "started_at")?,
                })
            })
            .collect()
    }

    async fn insert_evaluation(&self, evaluation: &GoalEvaluation) -> Result<(), RepositoryError> {
        sqlx::query(&format!(
            "INSERT INTO goal_evaluations ({EVALUATION_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)"
        ))
        .bind(evaluation.id)
        .bind(evaluation.goal_id.0)
        .bind(evaluation.round as i32)
        .bind(evaluation.attempt as i32)
        .bind(evaluation.judge_execution_id.map(|e| e.0))
        .bind(&evaluation.companion_answer)
        .bind(&evaluation.verdict)
        .bind(evaluation.outcome.map(|o| o.as_str()))
        .bind(evaluation.r#continue)
        .bind(&evaluation.waiting_on)
        .bind(&evaluation.answer)
        .bind(evaluation.created_at)
        .bind(evaluation.decided_at)
        .bind(evaluation.stop_reason.map(|r| r.as_str()))
        .bind(&evaluation.input_digest)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn finish_evaluation(
        &self,
        evaluation: &GoalEvaluation,
    ) -> Result<bool, RepositoryError> {
        let result = sqlx::query(
            "UPDATE goal_evaluations SET verdict = $2, outcome = $3, continue = $4, \
             waiting_on = $5, answer = $6, decided_at = $7, stop_reason = $8, \
             input_digest = $9 WHERE id = $1",
        )
        .bind(evaluation.id)
        .bind(&evaluation.verdict)
        .bind(evaluation.outcome.map(|o| o.as_str()))
        .bind(evaluation.r#continue)
        .bind(&evaluation.waiting_on)
        .bind(&evaluation.answer)
        .bind(evaluation.decided_at)
        .bind(evaluation.stop_reason.map(|r| r.as_str()))
        .bind(&evaluation.input_digest)
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(true),
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNIQUE_VIOLATION) => {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn list_evaluations(
        &self,
        goal_id: GoalId,
    ) -> Result<Vec<GoalEvaluation>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {EVALUATION_COLUMNS} FROM goal_evaluations WHERE goal_id = $1 \
             ORDER BY created_at, attempt"
        ))
        .bind(goal_id.0)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_evaluation).collect()
    }

    async fn list_open_created_before(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Goal>, RepositoryError> {
        // U25: a goal whose current round holds an open wait (no judge,
        // waiting_on kind `execution`, not ended) with its wait_until after
        // `now` is not swept.
        let rows = sqlx::query(&format!(
            "SELECT {GOAL_COLUMNS} FROM goals g WHERE g.state = 'open' AND g.created_at <= $1 \
             AND NOT EXISTS (SELECT 1 FROM goal_evaluations e WHERE e.goal_id = g.id \
             AND e.round = g.rounds AND e.judge_execution_id IS NULL \
             AND e.waiting_on->>'kind' = '{WAITING_ON_EXECUTION}' AND e.decided_at IS NULL \
             AND (e.waiting_on->>'wait_until')::timestamptz > $2) \
             ORDER BY g.created_at"
        ))
        .bind(cutoff)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_goal).collect()
    }
}

/// In-process form of [`GoalRepository`], with the same rules as the
/// PostgreSQL form. It holds no executions, so a binding is always taken and
/// listed with the time it was made.
#[derive(Default)]
pub struct InMemoryGoalRepository {
    goals: RwLock<HashMap<GoalId, Goal>>,
    evaluations: RwLock<Vec<GoalEvaluation>>,
    bound: RwLock<HashMap<GoalId, Vec<BoundExecution>>>,
}

impl InMemoryGoalRepository {
    pub fn new() -> Self {
        Self::default()
    }

    async fn bind(&self, goal_id: GoalId, execution_id: ExecutionId, kind: BoundKind) {
        self.bound
            .write()
            .await
            .entry(goal_id)
            .or_default()
            .push(BoundExecution {
                execution_id,
                kind,
                started_at: Utc::now(),
            });
    }
}

#[async_trait]
impl GoalRepository for InMemoryGoalRepository {
    async fn insert_goal(&self, goal: &Goal) -> Result<(), RepositoryError> {
        let mut goals = self.goals.write().await;
        if goals.contains_key(&goal.id) {
            return Err(RepositoryError::Database(format!(
                "goal {} already exists",
                goal.id
            )));
        }
        goals.insert(goal.id, goal.clone());
        Ok(())
    }

    async fn find_goal(&self, id: GoalId) -> Result<Option<Goal>, RepositoryError> {
        Ok(self.goals.read().await.get(&id).cloned())
    }

    async fn list_open_for_client_ref(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        client_ref: &str,
    ) -> Result<Vec<Goal>, RepositoryError> {
        let mut found: Vec<Goal> = self
            .goals
            .read()
            .await
            .values()
            .filter(|g| {
                g.is_open() && g.belongs_to(tenant_id, user_sub) && g.client_ref == client_ref
            })
            .cloned()
            .collect();
        found.sort_by_key(|g| g.created_at);
        Ok(found)
    }

    async fn close_goal(
        &self,
        id: GoalId,
        state: GoalState,
        closed_at: DateTime<Utc>,
        reason: Option<&str>,
    ) -> Result<bool, RepositoryError> {
        let mut goals = self.goals.write().await;
        match goals.get_mut(&id) {
            Some(goal) if goal.is_open() => {
                goal.state = state;
                goal.closed_at = Some(closed_at);
                goal.closed_reason = reason.map(str::to_string);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn find_goal_of_execution(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Option<GoalId>, RepositoryError> {
        Ok(self
            .bound
            .read()
            .await
            .iter()
            .find(|(_, bound)| bound.iter().any(|b| b.execution_id == execution_id))
            .map(|(goal_id, _)| *goal_id))
    }

    async fn grant_round(&self, id: GoalId, expected_rounds: u32) -> Result<bool, RepositoryError> {
        let mut goals = self.goals.write().await;
        match goals.get_mut(&id) {
            Some(goal) if goal.is_open() && goal.rounds == expected_rounds => {
                goal.rounds += 1;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn bind_execution(
        &self,
        goal_id: GoalId,
        execution_id: ExecutionId,
    ) -> Result<bool, RepositoryError> {
        self.bind(goal_id, execution_id, BoundKind::Agent).await;
        Ok(true)
    }

    async fn bind_workflow_execution(
        &self,
        goal_id: GoalId,
        execution_id: ExecutionId,
    ) -> Result<bool, RepositoryError> {
        self.bind(goal_id, execution_id, BoundKind::Workflow).await;
        Ok(true)
    }

    async fn list_bound(&self, goal_id: GoalId) -> Result<Vec<BoundExecution>, RepositoryError> {
        Ok(self
            .bound
            .read()
            .await
            .get(&goal_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn insert_evaluation(&self, evaluation: &GoalEvaluation) -> Result<(), RepositoryError> {
        self.evaluations.write().await.push(evaluation.clone());
        Ok(())
    }

    async fn finish_evaluation(
        &self,
        evaluation: &GoalEvaluation,
    ) -> Result<bool, RepositoryError> {
        let mut evaluations = self.evaluations.write().await;
        if evaluation.decides_round()
            && evaluations.iter().any(|e| {
                e.id != evaluation.id
                    && e.goal_id == evaluation.goal_id
                    && e.round == evaluation.round
                    && e.decides_round()
            })
        {
            return Ok(false);
        }
        match evaluations.iter_mut().find(|e| e.id == evaluation.id) {
            Some(stored) => {
                *stored = evaluation.clone();
                Ok(true)
            }
            None => Err(RepositoryError::NotFound(format!(
                "goal evaluation {}",
                evaluation.id
            ))),
        }
    }

    async fn list_evaluations(
        &self,
        goal_id: GoalId,
    ) -> Result<Vec<GoalEvaluation>, RepositoryError> {
        Ok(self
            .evaluations
            .read()
            .await
            .iter()
            .filter(|e| e.goal_id == goal_id)
            .cloned()
            .collect())
    }

    async fn list_open_created_before(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Goal>, RepositoryError> {
        let evaluations = self.evaluations.read().await;
        let waits_ahead = |g: &Goal| {
            evaluations.iter().any(|e| {
                e.goal_id == g.id
                    && e.round == g.rounds
                    && e.is_open_wait()
                    && e.wait_until().is_some_and(|until| until > now)
            })
        };
        let mut found: Vec<Goal> = self
            .goals
            .read()
            .await
            .values()
            .filter(|g| g.is_open() && g.created_at <= cutoff && !waits_ahead(g))
            .cloned()
            .collect();
        found.sort_by_key(|g| g.created_at);
        Ok(found)
    }
}
