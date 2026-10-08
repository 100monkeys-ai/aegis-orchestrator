// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tool approval repositories (AEGIS ADR-126 D3)
//!
//! [`PostgresToolApprovalRepository`] stores requests and policies in the
//! `tool_approval_requests` and `tool_approval_policies` tables of migration
//! `036_tool_approvals.sql`, with the `conversation_id` column of
//! `044_tool_approval_conversation.sql` and the policies' `effect` of
//! `046_tool_approval_policy_effect.sql`; [`InMemoryToolApprovalRepository`] keeps them in
//! process, for tests and for a daemon run without a database.
//!
//! ## One standing choice per key (ADR-126, Update of 2026-10-08 (2))
//!
//! [`ToolApprovalRepository::insert_policy`] revokes every unrevoked policy of
//! the same tenant, user, tool and binding and inserts the new one in one
//! transaction, holding a transaction-scoped advisory lock on that key, so
//! two writers of one key run one after the other and an allow and a deny
//! never coexist.
//!
//! ## Sealing (ADR-126, Updates of 2026-10-04 clause 2 and 2026-10-08)
//!
//! A request's `arguments`, `summary`, `result` and `error` are stored as
//! OpenBao Transit ciphertext under the tenant's key ([`transit_key`]), and the
//! row says so in `sealed` (migration `045_tool_approval_sealed.sql`): the
//! ciphertext is a JSON string in the JSONB columns `arguments` and `result`
//! and the text itself in the TEXT columns `summary` and `error`. A row
//! written before the migration has `sealed = false` and is read as stored.
//!
//! The reads that answer the request's own user or run its stored call
//! ([`ToolApprovalRepository::find_request`],
//! [`ToolApprovalRepository::list_requests_for_user`],
//! [`ToolApprovalRepository::decide_pending`]) unseal; the operator's list
//! ([`ToolApprovalRepository::list_requests`]) and the expiry sweep
//! ([`ToolApprovalRepository::expire_pending_before`]) answer the four fields
//! still sealed and decrypt nothing. A refused seal stores nothing, and a
//! refused unseal fails the read.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;
use crate::domain::tool_approval::{
    ToolApprovalId, ToolApprovalPolicy, ToolApprovalPolicyEffect, ToolApprovalPolicyId,
    ToolApprovalRepository, ToolApprovalRequest, ToolApprovalStatus,
};
use crate::infrastructure::secrets_manager::SecretsManager;

const REQUEST_COLUMNS: &str = "id, tenant_id, user_sub, execution_id, agent_id, tool_name, \
     arguments, summary, binding_id, security_context_name, policy_id, status, created_at, \
     decided_at, decided_by, result, error, conversation_id, schedule_id, sealed";

/// What a read answers: the stored columns and the name of the schedule
/// whose run made the call (AEGIS ADR-139 N9). A deleted schedule keeps its
/// row, so its name is still answered.
const READ_COLUMNS: &str = "id, tenant_id, user_sub, execution_id, agent_id, tool_name, \
     arguments, summary, binding_id, security_context_name, policy_id, status, created_at, \
     decided_at, decided_by, result, error, conversation_id, schedule_id, sealed, \
     (SELECT s.name FROM schedules s WHERE s.id = tool_approval_requests.schedule_id) \
     AS schedule_name";

const POLICY_COLUMNS: &str =
    "id, tenant_id, user_sub, tool_name, binding_id, effect, created_at, created_by, revoked_at";

/// The text a policy's advisory lock is keyed on: its tenant, user, tool and
/// binding (`=<binding>`, or `-` for none), separated by a byte none of them
/// holds.
fn policy_lock_key(policy: &ToolApprovalPolicy) -> String {
    let binding = match &policy.binding_id {
        Some(binding) => format!("={binding}"),
        None => "-".to_string(),
    };
    format!(
        "tool_approval_policy\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{binding}",
        policy.tenant_id.as_str(),
        policy.user_sub,
        policy.tool_name,
    )
}

/// The Transit key a tenant's approval requests are sealed under.
pub fn transit_key(tenant_id: &TenantId) -> String {
    format!("tool-approvals-{}", tenant_id.as_str())
}

pub struct PostgresToolApprovalRepository {
    pool: PgPool,
    secrets: Arc<SecretsManager>,
}

/// A request as its row holds it, before any unsealing.
struct StoredRequest {
    request: ToolApprovalRequest,
    sealed: bool,
}

impl PostgresToolApprovalRepository {
    pub fn new(pool: PgPool, secrets: Arc<SecretsManager>) -> Self {
        Self { pool, secrets }
    }

    async fn seal(
        &self,
        tenant_id: &TenantId,
        plaintext: &[u8],
    ) -> Result<String, RepositoryError> {
        self.secrets
            .encrypt(&transit_key(tenant_id), plaintext)
            .await
            .map_err(|e| RepositoryError::Database(format!("seal approval request: {e}")))
    }

    async fn unseal(
        &self,
        tenant_id: &TenantId,
        name: &str,
        ciphertext: &str,
    ) -> Result<Vec<u8>, RepositoryError> {
        self.secrets
            .decrypt(&transit_key(tenant_id), ciphertext)
            .await
            .map_err(|e| RepositoryError::Database(format!("unseal approval request {name}: {e}")))
    }

    async fn seal_json(
        &self,
        tenant_id: &TenantId,
        value: &Value,
    ) -> Result<Value, RepositoryError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|e| RepositoryError::Serialization(format!("seal approval request: {e}")))?;
        Ok(Value::String(self.seal(tenant_id, &bytes).await?))
    }

    async fn unseal_json(
        &self,
        tenant_id: &TenantId,
        name: &str,
        value: &Value,
    ) -> Result<Value, RepositoryError> {
        let ciphertext = value.as_str().ok_or_else(|| {
            RepositoryError::Serialization(format!("sealed {name} is not a ciphertext string"))
        })?;
        let bytes = self.unseal(tenant_id, name, ciphertext).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| RepositoryError::Serialization(format!("unsealed {name}: {e}")))
    }

    async fn unseal_text(
        &self,
        tenant_id: &TenantId,
        name: &str,
        ciphertext: &str,
    ) -> Result<String, RepositoryError> {
        let bytes = self.unseal(tenant_id, name, ciphertext).await?;
        String::from_utf8(bytes)
            .map_err(|e| RepositoryError::Serialization(format!("unsealed {name}: {e}")))
    }

    /// The request with its four sealed fields opened; a row written before
    /// the migration is answered as stored.
    async fn open(&self, stored: StoredRequest) -> Result<ToolApprovalRequest, RepositoryError> {
        let mut request = stored.request;
        if !stored.sealed {
            return Ok(request);
        }
        let tenant_id = request.tenant_id.clone();
        request.arguments = self
            .unseal_json(&tenant_id, "arguments", &request.arguments)
            .await?;
        request.summary = self
            .unseal_text(&tenant_id, "summary", &request.summary)
            .await?;
        if let Some(result) = request.result.take() {
            request.result = Some(self.unseal_json(&tenant_id, "result", &result).await?);
        }
        if let Some(error) = request.error.take() {
            request.error = Some(self.unseal_text(&tenant_id, "error", &error).await?);
        }
        Ok(request)
    }

    async fn open_all(
        &self,
        stored: Vec<StoredRequest>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError> {
        let mut opened = Vec::with_capacity(stored.len());
        for row in stored {
            opened.push(self.open(row).await?);
        }
        Ok(opened)
    }
}

/// The requests as stored, their sealed fields left sealed.
fn still_sealed(stored: Vec<StoredRequest>) -> Vec<ToolApprovalRequest> {
    stored.into_iter().map(|s| s.request).collect()
}

fn column<'r, T>(row: &'r PgRow, name: &str) -> Result<T, RepositoryError>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(name)
        .map_err(|e| RepositoryError::Serialization(format!("{name}: {e}")))
}

fn tenant(value: String) -> Result<TenantId, RepositoryError> {
    TenantId::new(value).map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))
}

fn hydrate_request(row: &PgRow) -> Result<StoredRequest, RepositoryError> {
    let status_text: String = column(row, "status")?;
    let status = ToolApprovalStatus::parse(&status_text).ok_or_else(|| {
        RepositoryError::Serialization(format!("unknown approval status: {status_text}"))
    })?;
    let policy_id: Option<Uuid> = column(row, "policy_id")?;
    let request = ToolApprovalRequest {
        id: ToolApprovalId(column(row, "id")?),
        tenant_id: tenant(column(row, "tenant_id")?)?,
        user_sub: column(row, "user_sub")?,
        execution_id: ExecutionId(column(row, "execution_id")?),
        agent_id: AgentId(column(row, "agent_id")?),
        tool_name: column(row, "tool_name")?,
        arguments: column(row, "arguments")?,
        summary: column(row, "summary")?,
        binding_id: column(row, "binding_id")?,
        security_context_name: column(row, "security_context_name")?,
        conversation_id: column(row, "conversation_id")?,
        schedule_id: column(row, "schedule_id")?,
        schedule_name: column(row, "schedule_name")?,
        policy_id: policy_id.map(ToolApprovalPolicyId),
        status,
        created_at: column(row, "created_at")?,
        decided_at: column(row, "decided_at")?,
        decided_by: column(row, "decided_by")?,
        result: column(row, "result")?,
        error: column(row, "error")?,
    };
    Ok(StoredRequest {
        request,
        sealed: column(row, "sealed")?,
    })
}

fn hydrate_policy(row: &PgRow) -> Result<ToolApprovalPolicy, RepositoryError> {
    let effect_text: String = column(row, "effect")?;
    let effect = ToolApprovalPolicyEffect::parse(&effect_text).ok_or_else(|| {
        RepositoryError::Serialization(format!("unknown approval policy effect: {effect_text}"))
    })?;
    Ok(ToolApprovalPolicy {
        id: ToolApprovalPolicyId(column(row, "id")?),
        tenant_id: tenant(column(row, "tenant_id")?)?,
        user_sub: column(row, "user_sub")?,
        tool_name: column(row, "tool_name")?,
        binding_id: column(row, "binding_id")?,
        effect,
        created_at: column(row, "created_at")?,
        created_by: column(row, "created_by")?,
        revoked_at: column(row, "revoked_at")?,
    })
}

#[async_trait]
impl ToolApprovalRepository for PostgresToolApprovalRepository {
    async fn insert_request(&self, request: &ToolApprovalRequest) -> Result<(), RepositoryError> {
        let tenant_id = &request.tenant_id;
        let arguments = self.seal_json(tenant_id, &request.arguments).await?;
        let summary = self.seal(tenant_id, request.summary.as_bytes()).await?;
        let result = match &request.result {
            Some(result) => Some(self.seal_json(tenant_id, result).await?),
            None => None,
        };
        let error = match &request.error {
            Some(error) => Some(self.seal(tenant_id, error.as_bytes()).await?),
            None => None,
        };
        sqlx::query(&format!(
            "INSERT INTO tool_approval_requests ({REQUEST_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, TRUE)"
        ))
        .bind(request.id.0)
        .bind(request.tenant_id.as_str())
        .bind(&request.user_sub)
        .bind(request.execution_id.0)
        .bind(request.agent_id.0)
        .bind(&request.tool_name)
        .bind(&arguments)
        .bind(&summary)
        .bind(&request.binding_id)
        .bind(&request.security_context_name)
        .bind(request.policy_id.map(|p| p.0))
        .bind(request.status.as_str())
        .bind(request.created_at)
        .bind(request.decided_at)
        .bind(&request.decided_by)
        .bind(&result)
        .bind(&error)
        .bind(&request.conversation_id)
        .bind(request.schedule_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_request(
        &self,
        id: ToolApprovalId,
    ) -> Result<Option<ToolApprovalRequest>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {READ_COLUMNS} FROM tool_approval_requests WHERE id = $1"
        ))
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;
        match row.as_ref().map(hydrate_request).transpose()? {
            Some(stored) => Ok(Some(self.open(stored).await?)),
            None => Ok(None),
        }
    }

    async fn list_requests_for_user(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {READ_COLUMNS} FROM tool_approval_requests \
             WHERE tenant_id = $1 AND user_sub = $2 AND ($3::TEXT IS NULL OR status = $3) \
             ORDER BY created_at DESC"
        ))
        .bind(tenant_id.as_str())
        .bind(user_sub)
        .bind(status.map(|s| s.as_str()))
        .fetch_all(&self.pool)
        .await?;
        let stored = rows.iter().map(hydrate_request).collect::<Result<_, _>>()?;
        self.open_all(stored).await
    }

    async fn list_requests(
        &self,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {READ_COLUMNS} FROM tool_approval_requests \
             WHERE ($1::TEXT IS NULL OR status = $1) ORDER BY created_at DESC"
        ))
        .bind(status.map(|s| s.as_str()))
        .fetch_all(&self.pool)
        .await?;
        // The operator's read: answered sealed, nothing decrypted.
        Ok(still_sealed(
            rows.iter().map(hydrate_request).collect::<Result<_, _>>()?,
        ))
    }

    async fn decide_pending(
        &self,
        id: ToolApprovalId,
        status: ToolApprovalStatus,
        decided_by: Option<&str>,
        decided_at: DateTime<Utc>,
    ) -> Result<Option<ToolApprovalRequest>, RepositoryError> {
        let row = sqlx::query(&format!(
            "UPDATE tool_approval_requests SET status = $2, decided_by = $3, decided_at = $4 \
             WHERE id = $1 AND status = 'pending' RETURNING {READ_COLUMNS}"
        ))
        .bind(id.0)
        .bind(status.as_str())
        .bind(decided_by)
        .bind(decided_at)
        .fetch_optional(&self.pool)
        .await?;
        match row.as_ref().map(hydrate_request).transpose()? {
            Some(stored) => Ok(Some(self.open(stored).await?)),
            None => Ok(None),
        }
    }

    async fn record_outcome(
        &self,
        id: ToolApprovalId,
        result: Option<&Value>,
        error: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let Some(row) =
            sqlx::query("SELECT tenant_id, sealed FROM tool_approval_requests WHERE id = $1")
                .bind(id.0)
                .fetch_optional(&self.pool)
                .await?
        else {
            return Ok(());
        };
        let tenant_id = tenant(column(&row, "tenant_id")?)?;
        let sealed: bool = column(&row, "sealed")?;
        // A row written before the migration keeps its fields as stored.
        let (result, error) = if sealed {
            let result = match result {
                Some(result) => Some(self.seal_json(&tenant_id, result).await?),
                None => None,
            };
            let error = match error {
                Some(error) => Some(self.seal(&tenant_id, error.as_bytes()).await?),
                None => None,
            };
            (result, error)
        } else {
            (result.cloned(), error.map(str::to_string))
        };
        sqlx::query("UPDATE tool_approval_requests SET result = $2, error = $3 WHERE id = $1")
            .bind(id.0)
            .bind(result)
            .bind(error)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn expire_pending_before(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "UPDATE tool_approval_requests SET status = 'expired', decided_at = $2 \
             WHERE status = 'pending' AND created_at <= $1 RETURNING {READ_COLUMNS}"
        ))
        .bind(cutoff)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        // The sweep publishes only ids and statuses: nothing is decrypted.
        Ok(still_sealed(
            rows.iter().map(hydrate_request).collect::<Result<_, _>>()?,
        ))
    }

    async fn insert_policy(&self, policy: &ToolApprovalPolicy) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(policy_lock_key(policy))
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE tool_approval_policies SET revoked_at = $5 \
             WHERE tenant_id = $1 AND user_sub = $2 AND tool_name = $3 \
             AND binding_id IS NOT DISTINCT FROM $4 AND revoked_at IS NULL",
        )
        .bind(policy.tenant_id.as_str())
        .bind(&policy.user_sub)
        .bind(&policy.tool_name)
        .bind(&policy.binding_id)
        .bind(policy.created_at)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "INSERT INTO tool_approval_policies ({POLICY_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
        ))
        .bind(policy.id.0)
        .bind(policy.tenant_id.as_str())
        .bind(&policy.user_sub)
        .bind(&policy.tool_name)
        .bind(&policy.binding_id)
        .bind(policy.effect.as_str())
        .bind(policy.created_at)
        .bind(&policy.created_by)
        .bind(policy.revoked_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn find_active_policy(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        tool_name: &str,
        binding_id: Option<&str>,
    ) -> Result<Option<ToolApprovalPolicy>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM tool_approval_policies \
             WHERE tenant_id = $1 AND user_sub = $2 AND tool_name = $3 \
             AND binding_id IS NOT DISTINCT FROM $4 AND revoked_at IS NULL \
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(tenant_id.as_str())
        .bind(user_sub)
        .bind(tool_name)
        .bind(binding_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(hydrate_policy).transpose()
    }

    async fn schedule_of_run(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Option<Uuid>, RepositoryError> {
        // The agent run's own schedule, else the schedule of the workflow
        // run it is a state of (a workflow's agent states carry none).
        let row = sqlx::query(
            "SELECT COALESCE(e.schedule_id, we.schedule_id) AS schedule_id \
             FROM executions e \
             LEFT JOIN workflow_executions we ON we.id = e.workflow_execution_id \
             WHERE e.id = $1",
        )
        .bind(execution_id.0)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => column(&row, "schedule_id"),
            None => Ok(None),
        }
    }

    async fn list_active_policies(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> Result<Vec<ToolApprovalPolicy>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM tool_approval_policies \
             WHERE tenant_id = $1 AND user_sub = $2 AND revoked_at IS NULL \
             ORDER BY created_at DESC"
        ))
        .bind(tenant_id.as_str())
        .bind(user_sub)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_policy).collect()
    }

    async fn revoke_policy(
        &self,
        id: ToolApprovalPolicyId,
        tenant_id: &TenantId,
        user_sub: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        let done = sqlx::query(
            "UPDATE tool_approval_policies SET revoked_at = $4 \
             WHERE id = $1 AND tenant_id = $2 AND user_sub = $3 AND revoked_at IS NULL",
        )
        .bind(id.0)
        .bind(tenant_id.as_str())
        .bind(user_sub)
        .bind(revoked_at)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
}

/// In-process form of [`ToolApprovalRepository`], with the same rules as
/// the PostgreSQL form.
#[derive(Default)]
pub struct InMemoryToolApprovalRepository {
    requests: RwLock<HashMap<ToolApprovalId, ToolApprovalRequest>>,
    policies: RwLock<HashMap<ToolApprovalPolicyId, ToolApprovalPolicy>>,
    /// The runs schedules started, with each schedule's name: what the
    /// PostgreSQL form reads from the execution records and `schedules`.
    runs: RwLock<HashMap<ExecutionId, (Uuid, String)>>,
}

impl InMemoryToolApprovalRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `execution_id` is a run of the schedule `schedule_id`
    /// named `name`: [`ToolApprovalRepository::schedule_of_run`] answers it,
    /// and a request stored with that schedule is read with its name.
    pub async fn bind_run_to_schedule(
        &self,
        execution_id: ExecutionId,
        schedule_id: Uuid,
        name: &str,
    ) {
        self.runs
            .write()
            .await
            .insert(execution_id, (schedule_id, name.to_string()));
    }
}

fn newest_first(mut requests: Vec<ToolApprovalRequest>) -> Vec<ToolApprovalRequest> {
    requests.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    requests
}

#[async_trait]
impl ToolApprovalRepository for InMemoryToolApprovalRepository {
    async fn insert_request(&self, request: &ToolApprovalRequest) -> Result<(), RepositoryError> {
        let mut requests = self.requests.write().await;
        if requests.contains_key(&request.id) {
            return Err(RepositoryError::Database(format!(
                "approval request {} already exists",
                request.id
            )));
        }
        let mut stored = request.clone();
        stored.schedule_name = match stored.schedule_id {
            Some(schedule_id) => self
                .runs
                .read()
                .await
                .values()
                .find(|(id, _)| *id == schedule_id)
                .map(|(_, name)| name.clone()),
            None => None,
        };
        requests.insert(request.id, stored);
        Ok(())
    }

    async fn find_request(
        &self,
        id: ToolApprovalId,
    ) -> Result<Option<ToolApprovalRequest>, RepositoryError> {
        Ok(self.requests.read().await.get(&id).cloned())
    }

    async fn list_requests_for_user(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError> {
        Ok(newest_first(
            self.requests
                .read()
                .await
                .values()
                .filter(|r| r.belongs_to(tenant_id, user_sub))
                .filter(|r| status.is_none_or(|s| r.status == s))
                .cloned()
                .collect(),
        ))
    }

    async fn list_requests(
        &self,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError> {
        Ok(newest_first(
            self.requests
                .read()
                .await
                .values()
                .filter(|r| status.is_none_or(|s| r.status == s))
                .cloned()
                .collect(),
        ))
    }

    async fn decide_pending(
        &self,
        id: ToolApprovalId,
        status: ToolApprovalStatus,
        decided_by: Option<&str>,
        decided_at: DateTime<Utc>,
    ) -> Result<Option<ToolApprovalRequest>, RepositoryError> {
        let mut requests = self.requests.write().await;
        match requests.get_mut(&id) {
            Some(request) if request.status == ToolApprovalStatus::Pending => {
                request.status = status;
                request.decided_by = decided_by.map(str::to_string);
                request.decided_at = Some(decided_at);
                Ok(Some(request.clone()))
            }
            _ => Ok(None),
        }
    }

    async fn record_outcome(
        &self,
        id: ToolApprovalId,
        result: Option<&Value>,
        error: Option<&str>,
    ) -> Result<(), RepositoryError> {
        if let Some(request) = self.requests.write().await.get_mut(&id) {
            request.result = result.cloned();
            request.error = error.map(str::to_string);
        }
        Ok(())
    }

    async fn expire_pending_before(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError> {
        let mut expired = Vec::new();
        for request in self.requests.write().await.values_mut() {
            if request.status == ToolApprovalStatus::Pending && request.created_at <= cutoff {
                request.status = ToolApprovalStatus::Expired;
                request.decided_at = Some(now);
                expired.push(request.clone());
            }
        }
        Ok(expired)
    }

    async fn insert_policy(&self, policy: &ToolApprovalPolicy) -> Result<(), RepositoryError> {
        let mut policies = self.policies.write().await;
        for other in policies.values_mut() {
            if other.tenant_id == policy.tenant_id
                && other.user_sub == policy.user_sub
                && other.tool_name == policy.tool_name
                && other.binding_id == policy.binding_id
                && other.revoked_at.is_none()
            {
                other.revoked_at = Some(policy.created_at);
            }
        }
        policies.insert(policy.id, policy.clone());
        Ok(())
    }

    async fn find_active_policy(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        tool_name: &str,
        binding_id: Option<&str>,
    ) -> Result<Option<ToolApprovalPolicy>, RepositoryError> {
        Ok(self
            .policies
            .read()
            .await
            .values()
            .filter(|p| {
                &p.tenant_id == tenant_id
                    && p.user_sub == user_sub
                    && p.tool_name == tool_name
                    && p.binding_id.as_deref() == binding_id
                    && p.revoked_at.is_none()
            })
            .max_by_key(|p| p.created_at)
            .cloned())
    }

    async fn schedule_of_run(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Option<Uuid>, RepositoryError> {
        Ok(self
            .runs
            .read()
            .await
            .get(&execution_id)
            .map(|(schedule_id, _)| *schedule_id))
    }

    async fn list_active_policies(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> Result<Vec<ToolApprovalPolicy>, RepositoryError> {
        let mut policies: Vec<ToolApprovalPolicy> = self
            .policies
            .read()
            .await
            .values()
            .filter(|p| &p.tenant_id == tenant_id && p.user_sub == user_sub)
            .filter(|p| p.revoked_at.is_none())
            .cloned()
            .collect();
        policies.sort_by_key(|p| std::cmp::Reverse(p.created_at));
        Ok(policies)
    }

    async fn revoke_policy(
        &self,
        id: ToolApprovalPolicyId,
        tenant_id: &TenantId,
        user_sub: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError> {
        match self.policies.write().await.get_mut(&id) {
            Some(policy)
                if &policy.tenant_id == tenant_id
                    && policy.user_sub == user_sub
                    && policy.revoked_at.is_none() =>
            {
                policy.revoked_at = Some(revoked_at);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
