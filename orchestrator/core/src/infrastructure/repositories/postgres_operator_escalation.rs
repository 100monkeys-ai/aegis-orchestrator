// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Operator escalation repositories (AEGIS ADR-129)
//!
//! [`PostgresOperatorEscalationRepository`] stores codes and escalations in
//! the tables of migration `037_operator_escalations.sql` and appends audit
//! rows to `admin_audit_log` (migration 003); [`InMemoryOperatorEscalationRepository`]
//! keeps them in process, for tests and for a daemon run without a database,
//! with the same rules.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::domain::iam::AegisRole;
use crate::domain::operator_escalation::{
    AdminAuditEntry, EscalationEndReason, OperatorEscalation, OperatorEscalationCode,
    OperatorEscalationRepository,
};
use crate::domain::repository::RepositoryError;

const CODE_COLUMNS: &str = "id, code_hash, consumer_sub, system_sub, aegis_role, created_at, \
     expires_at, consumed_at, failed_attempts, invalidated_at";

const ESCALATION_COLUMNS: &str = "id, api_key_id, consumer_sub, system_sub, aegis_role, \
     code_id, started_at, expires_at, ended_at, end_reason";

pub struct PostgresOperatorEscalationRepository {
    pool: PgPool,
}

impl PostgresOperatorEscalationRepository {
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

fn role(value: String) -> Result<AegisRole, RepositoryError> {
    AegisRole::from_claim(&value)
        .ok_or_else(|| RepositoryError::Serialization(format!("unknown aegis_role: {value}")))
}

fn hydrate_code(row: &PgRow) -> Result<OperatorEscalationCode, RepositoryError> {
    let failed: i32 = column(row, "failed_attempts")?;
    Ok(OperatorEscalationCode {
        id: column(row, "id")?,
        code_hash: column(row, "code_hash")?,
        consumer_sub: column(row, "consumer_sub")?,
        system_sub: column(row, "system_sub")?,
        aegis_role: role(column(row, "aegis_role")?)?,
        created_at: column(row, "created_at")?,
        expires_at: column(row, "expires_at")?,
        consumed_at: column(row, "consumed_at")?,
        failed_attempts: u32::try_from(failed).unwrap_or(0),
        invalidated_at: column(row, "invalidated_at")?,
    })
}

fn hydrate_escalation(row: &PgRow) -> Result<OperatorEscalation, RepositoryError> {
    let end_reason: Option<String> = column(row, "end_reason")?;
    let end_reason = match end_reason {
        None => None,
        Some(text) => Some(EscalationEndReason::parse(&text).ok_or_else(|| {
            RepositoryError::Serialization(format!("unknown end_reason: {text}"))
        })?),
    };
    Ok(OperatorEscalation {
        id: column(row, "id")?,
        api_key_id: column(row, "api_key_id")?,
        consumer_sub: column(row, "consumer_sub")?,
        system_sub: column(row, "system_sub")?,
        aegis_role: role(column(row, "aegis_role")?)?,
        code_id: column(row, "code_id")?,
        started_at: column(row, "started_at")?,
        expires_at: column(row, "expires_at")?,
        ended_at: column(row, "ended_at")?,
        end_reason,
    })
}

#[async_trait]
impl OperatorEscalationRepository for PostgresOperatorEscalationRepository {
    async fn insert_code(&self, code: &OperatorEscalationCode) -> Result<(), RepositoryError> {
        sqlx::query(&format!(
            "INSERT INTO operator_escalation_codes ({CODE_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"
        ))
        .bind(code.id)
        .bind(&code.code_hash)
        .bind(&code.consumer_sub)
        .bind(&code.system_sub)
        .bind(code.aegis_role.as_claim_str())
        .bind(code.created_at)
        .bind(code.expires_at)
        .bind(code.consumed_at)
        .bind(i32::try_from(code.failed_attempts).unwrap_or(i32::MAX))
        .bind(code.invalidated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_code(
        &self,
        consumer_sub: &str,
        code_hash: &str,
    ) -> Result<Option<OperatorEscalationCode>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {CODE_COLUMNS} FROM operator_escalation_codes \
             WHERE consumer_sub = $1 AND code_hash = $2 ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(consumer_sub)
        .bind(code_hash)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(hydrate_code).transpose()
    }

    async fn consume_code(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, RepositoryError> {
        let done = sqlx::query(
            "UPDATE operator_escalation_codes SET consumed_at = $2 \
             WHERE id = $1 AND consumed_at IS NULL AND invalidated_at IS NULL AND expires_at > $2",
        )
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn invalidate_code(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, RepositoryError> {
        let done = sqlx::query(
            "UPDATE operator_escalation_codes SET invalidated_at = $2 \
             WHERE id = $1 AND consumed_at IS NULL AND invalidated_at IS NULL AND expires_at > $2",
        )
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn record_failure(
        &self,
        consumer_sub: &str,
        now: DateTime<Utc>,
        max_failed_attempts: u32,
    ) -> Result<Vec<OperatorEscalationCode>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "UPDATE operator_escalation_codes SET failed_attempts = failed_attempts + 1, \
             invalidated_at = CASE WHEN failed_attempts + 1 >= $3 THEN $2 ELSE NULL END \
             WHERE consumer_sub = $1 AND consumed_at IS NULL AND invalidated_at IS NULL \
             AND expires_at > $2 RETURNING {CODE_COLUMNS}"
        ))
        .bind(consumer_sub)
        .bind(now)
        .bind(i32::try_from(max_failed_attempts).unwrap_or(i32::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_code).collect()
    }

    async fn insert_escalation(
        &self,
        escalation: &OperatorEscalation,
    ) -> Result<(), RepositoryError> {
        sqlx::query(&format!(
            "INSERT INTO operator_escalations ({ESCALATION_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"
        ))
        .bind(escalation.id)
        .bind(escalation.api_key_id)
        .bind(&escalation.consumer_sub)
        .bind(&escalation.system_sub)
        .bind(escalation.aegis_role.as_claim_str())
        .bind(escalation.code_id)
        .bind(escalation.started_at)
        .bind(escalation.expires_at)
        .bind(escalation.ended_at)
        .bind(escalation.end_reason.map(|r| r.as_str()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_escalation(
        &self,
        id: Uuid,
    ) -> Result<Option<OperatorEscalation>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {ESCALATION_COLUMNS} FROM operator_escalations WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(hydrate_escalation).transpose()
    }

    async fn active_for_api_key(
        &self,
        api_key_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<OperatorEscalation>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {ESCALATION_COLUMNS} FROM operator_escalations \
             WHERE api_key_id = $1 AND ended_at IS NULL AND expires_at > $2 \
             ORDER BY expires_at DESC LIMIT 1"
        ))
        .bind(api_key_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(hydrate_escalation).transpose()
    }

    async fn active_for_system_sub(
        &self,
        system_sub: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {ESCALATION_COLUMNS} FROM operator_escalations \
             WHERE system_sub = $1 AND ended_at IS NULL AND expires_at > $2 \
             ORDER BY started_at DESC"
        ))
        .bind(system_sub)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_escalation).collect()
    }

    async fn active_system_subs(&self, now: DateTime<Utc>) -> Result<Vec<String>, RepositoryError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT system_sub FROM operator_escalations \
             WHERE ended_at IS NULL AND expires_at > $1 ORDER BY system_sub",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(s,)| s).collect())
    }

    async fn end_escalation(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        reason: EscalationEndReason,
    ) -> Result<Option<OperatorEscalation>, RepositoryError> {
        let row = sqlx::query(&format!(
            "UPDATE operator_escalations SET ended_at = $2, end_reason = $3 \
             WHERE id = $1 AND ended_at IS NULL AND expires_at > $2 \
             RETURNING {ESCALATION_COLUMNS}"
        ))
        .bind(id)
        .bind(now)
        .bind(reason.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(hydrate_escalation).transpose()
    }

    async fn end_active_for_api_key(
        &self,
        api_key_id: Uuid,
        now: DateTime<Utc>,
        reason: EscalationEndReason,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "UPDATE operator_escalations SET ended_at = $2, end_reason = $3 \
             WHERE api_key_id = $1 AND ended_at IS NULL AND expires_at > $2 \
             RETURNING {ESCALATION_COLUMNS}"
        ))
        .bind(api_key_id)
        .bind(now)
        .bind(reason.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_escalation).collect()
    }

    async fn end_active_for_system_sub(
        &self,
        system_sub: &str,
        now: DateTime<Utc>,
        reason: EscalationEndReason,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "UPDATE operator_escalations SET ended_at = $2, end_reason = $3 \
             WHERE system_sub = $1 AND ended_at IS NULL AND expires_at > $2 \
             RETURNING {ESCALATION_COLUMNS}"
        ))
        .bind(system_sub)
        .bind(now)
        .bind(reason.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_escalation).collect()
    }

    async fn end_expired(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "UPDATE operator_escalations SET ended_at = expires_at, end_reason = 'expired' \
             WHERE ended_at IS NULL AND expires_at <= $1 RETURNING {ESCALATION_COLUMNS}"
        ))
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_escalation).collect()
    }

    async fn append_audit(&self, entry: &AdminAuditEntry) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO admin_audit_log (actor_id, action, target_resource, after_state) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&entry.actor_id)
        .bind(&entry.action)
        .bind(&entry.target_resource)
        .bind(&entry.after_state)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// In-process form of [`OperatorEscalationRepository`], with the same rules
/// as the PostgreSQL form. `audit_entries` exposes the appended audit rows to
/// tests.
#[derive(Default)]
pub struct InMemoryOperatorEscalationRepository {
    codes: RwLock<HashMap<Uuid, OperatorEscalationCode>>,
    escalations: RwLock<HashMap<Uuid, OperatorEscalation>>,
    audit: RwLock<Vec<AdminAuditEntry>>,
}

impl InMemoryOperatorEscalationRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every audit row appended, oldest first.
    pub async fn audit_entries(&self) -> Vec<AdminAuditEntry> {
        self.audit.read().await.clone()
    }
}

fn end_all(
    escalations: &mut HashMap<Uuid, OperatorEscalation>,
    now: DateTime<Utc>,
    reason: EscalationEndReason,
    matches: impl Fn(&OperatorEscalation) -> bool,
) -> Vec<OperatorEscalation> {
    let mut ended = Vec::new();
    for e in escalations.values_mut() {
        if e.is_active(now) && matches(e) {
            e.ended_at = Some(now);
            e.end_reason = Some(reason);
            ended.push(e.clone());
        }
    }
    ended
}

#[async_trait]
impl OperatorEscalationRepository for InMemoryOperatorEscalationRepository {
    async fn insert_code(&self, code: &OperatorEscalationCode) -> Result<(), RepositoryError> {
        let mut codes = self.codes.write().await;
        if codes.contains_key(&code.id) {
            return Err(RepositoryError::Database(format!(
                "escalation code {} already exists",
                code.id
            )));
        }
        codes.insert(code.id, code.clone());
        Ok(())
    }

    async fn find_code(
        &self,
        consumer_sub: &str,
        code_hash: &str,
    ) -> Result<Option<OperatorEscalationCode>, RepositoryError> {
        Ok(self
            .codes
            .read()
            .await
            .values()
            .filter(|c| c.consumer_sub == consumer_sub && c.code_hash == code_hash)
            .max_by_key(|c| c.created_at)
            .cloned())
    }

    async fn consume_code(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, RepositoryError> {
        let mut codes = self.codes.write().await;
        match codes.get_mut(&id) {
            Some(c) if c.is_live(now) => {
                c.consumed_at = Some(now);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn invalidate_code(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, RepositoryError> {
        let mut codes = self.codes.write().await;
        match codes.get_mut(&id) {
            Some(c) if c.is_live(now) => {
                c.invalidated_at = Some(now);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn record_failure(
        &self,
        consumer_sub: &str,
        now: DateTime<Utc>,
        max_failed_attempts: u32,
    ) -> Result<Vec<OperatorEscalationCode>, RepositoryError> {
        let mut codes = self.codes.write().await;
        let mut counted = Vec::new();
        for c in codes.values_mut() {
            if c.consumer_sub == consumer_sub && c.is_live(now) {
                c.failed_attempts += 1;
                if c.failed_attempts >= max_failed_attempts {
                    c.invalidated_at = Some(now);
                }
                counted.push(c.clone());
            }
        }
        Ok(counted)
    }

    async fn insert_escalation(
        &self,
        escalation: &OperatorEscalation,
    ) -> Result<(), RepositoryError> {
        let mut escalations = self.escalations.write().await;
        if escalations.contains_key(&escalation.id) {
            return Err(RepositoryError::Database(format!(
                "escalation {} already exists",
                escalation.id
            )));
        }
        escalations.insert(escalation.id, escalation.clone());
        Ok(())
    }

    async fn find_escalation(
        &self,
        id: Uuid,
    ) -> Result<Option<OperatorEscalation>, RepositoryError> {
        Ok(self.escalations.read().await.get(&id).cloned())
    }

    async fn active_for_api_key(
        &self,
        api_key_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<OperatorEscalation>, RepositoryError> {
        Ok(self
            .escalations
            .read()
            .await
            .values()
            .filter(|e| e.api_key_id == api_key_id && e.is_active(now))
            .max_by_key(|e| e.expires_at)
            .cloned())
    }

    async fn active_for_system_sub(
        &self,
        system_sub: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError> {
        let mut active: Vec<OperatorEscalation> = self
            .escalations
            .read()
            .await
            .values()
            .filter(|e| e.system_sub == system_sub && e.is_active(now))
            .cloned()
            .collect();
        active.sort_by_key(|e| std::cmp::Reverse(e.started_at));
        Ok(active)
    }

    async fn active_system_subs(&self, now: DateTime<Utc>) -> Result<Vec<String>, RepositoryError> {
        let mut subs: Vec<String> = self
            .escalations
            .read()
            .await
            .values()
            .filter(|e| e.is_active(now))
            .map(|e| e.system_sub.clone())
            .collect();
        subs.sort();
        subs.dedup();
        Ok(subs)
    }

    async fn end_escalation(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        reason: EscalationEndReason,
    ) -> Result<Option<OperatorEscalation>, RepositoryError> {
        let mut escalations = self.escalations.write().await;
        Ok(end_all(&mut escalations, now, reason, |e| e.id == id)
            .into_iter()
            .next())
    }

    async fn end_active_for_api_key(
        &self,
        api_key_id: Uuid,
        now: DateTime<Utc>,
        reason: EscalationEndReason,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError> {
        let mut escalations = self.escalations.write().await;
        Ok(end_all(&mut escalations, now, reason, |e| {
            e.api_key_id == api_key_id
        }))
    }

    async fn end_active_for_system_sub(
        &self,
        system_sub: &str,
        now: DateTime<Utc>,
        reason: EscalationEndReason,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError> {
        let mut escalations = self.escalations.write().await;
        Ok(end_all(&mut escalations, now, reason, |e| {
            e.system_sub == system_sub
        }))
    }

    async fn end_expired(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<OperatorEscalation>, RepositoryError> {
        let mut escalations = self.escalations.write().await;
        let mut ended = Vec::new();
        for e in escalations.values_mut() {
            if e.ended_at.is_none() && e.expires_at <= now {
                e.ended_at = Some(e.expires_at);
                e.end_reason = Some(EscalationEndReason::Expired);
                ended.push(e.clone());
            }
        }
        Ok(ended)
    }

    async fn append_audit(&self, entry: &AdminAuditEntry) -> Result<(), RepositoryError> {
        self.audit.write().await.push(entry.clone());
        Ok(())
    }
}
