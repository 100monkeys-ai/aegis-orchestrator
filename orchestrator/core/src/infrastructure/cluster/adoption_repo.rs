// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A dead node's executions, adopted in PostgreSQL (ADR-152 D5.7)
//!
//! The `execution-adoption` holder ends each unfinished execution of a dead
//! node in one fenced statement: it increments `owner_epoch`, names itself
//! the owner and writes the ending, only while the dead node still owns the
//! unfinished row and the duty is at the holder's epoch. A late write by the
//! dead owner then affects zero rows (D5.2). Every statement carries a
//! WHERE clause (pg_strict) and needs no session (PgBouncer, transaction
//! mode).

use async_trait::async_trait;
use sqlx::PgPool;

use crate::domain::cluster::NodeId;
use crate::domain::cluster_membership::{DutyFence, ExecutionAdoptionRepository, NodeProcess};
use crate::domain::execution::{Execution, ExecutionId, ExecutionStatus};
use crate::domain::repository::ExecutionRepository;
use crate::infrastructure::cluster::membership_repo::under_duty_epoch;
use crate::infrastructure::repositories::postgres_execution::PostgresExecutionRepository;

/// A node's unfinished executions, with whether a cancel was asked for.
pub const UNFINISHED_OF_SQL: &str = r#"
    SELECT id, cancel_requested_at IS NOT NULL AS cancel_requested
    FROM executions
    WHERE owner_node_id = $1 AND status IN ('pending', 'running')
    ORDER BY started_at
"#;

/// The adoption of one execution, fenced by the duty's epoch ($9, $10).
pub fn adopt_sql() -> String {
    format!(
        r#"
    UPDATE executions SET
        status = $2,
        iterations = $3,
        current_iteration = $4,
        error_message = $5,
        completed_at = $6,
        owner_node_id = $7,
        owner_incarnation = $8,
        owner_epoch = owner_epoch + 1
    WHERE id = $1
      AND owner_node_id = $11
      AND status IN ('pending', 'running')
      AND {}
"#,
        under_duty_epoch(9, 10)
    )
}

pub struct PgExecutionAdoptionRepository {
    pool: PgPool,
    executions: PostgresExecutionRepository,
}

impl PgExecutionAdoptionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self {
            executions: PostgresExecutionRepository::new(pool.clone()),
            pool,
        }
    }
}

#[async_trait]
impl ExecutionAdoptionRepository for PgExecutionAdoptionRepository {
    async fn unfinished_of(&self, node: NodeId) -> anyhow::Result<Vec<(Execution, bool)>> {
        let rows: Vec<(uuid::Uuid, bool)> = sqlx::query_as(UNFINISHED_OF_SQL)
            .bind(node.0)
            .fetch_all(&self.pool)
            .await?;
        let mut unfinished = Vec::with_capacity(rows.len());
        for (id, cancel_requested) in rows {
            if let Some(execution) = self.executions.find_by_id_unscoped(ExecutionId(id)).await? {
                unfinished.push((execution, cancel_requested));
            }
        }
        Ok(unfinished)
    }

    async fn adopt(
        &self,
        ended: &Execution,
        from: NodeId,
        adopter: NodeProcess,
        fence: DutyFence,
    ) -> anyhow::Result<bool> {
        // Adoption takes a run from another node; a node running its duties
        // alone has no peer to take from.
        let DutyFence::Leased { duty, epoch } = fence else {
            return Ok(false);
        };
        let status = match ended.status {
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::Cancelled => "cancelled",
            ref other => {
                anyhow::bail!("an adopted execution ends failed or cancelled, not {other:?}")
            }
        };
        let taken = sqlx::query(&adopt_sql())
            .bind(ended.id.0)
            .bind(status)
            .bind(serde_json::to_value(ended.iterations())?)
            .bind(ended.iterations().len() as i32)
            .bind(&ended.error)
            .bind(ended.ended_at)
            .bind(adopter.node_id.0)
            .bind(adopter.incarnation.0)
            .bind(duty.as_str())
            .bind(epoch.0)
            .bind(from.0)
            .execute(&self.pool)
            .await?;
        Ok(taken.rows_affected() == 1)
    }
}
