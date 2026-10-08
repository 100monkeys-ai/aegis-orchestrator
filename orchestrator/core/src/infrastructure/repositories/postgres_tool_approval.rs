// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tool approval repositories (AEGIS ADR-126 D3)
//!
//! [`PostgresToolApprovalRepository`] stores requests and policies in the
//! `tool_approval_requests` and `tool_approval_policies` tables of migration
//! `036_tool_approvals.sql`, with the `conversation_id` column of
//! `044_tool_approval_conversation.sql`; [`InMemoryToolApprovalRepository`] keeps them in
//! process, for tests and for a daemon run without a database.

use std::collections::HashMap;

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
    ToolApprovalId, ToolApprovalPolicy, ToolApprovalPolicyId, ToolApprovalRepository,
    ToolApprovalRequest, ToolApprovalStatus,
};

const REQUEST_COLUMNS: &str = "id, tenant_id, user_sub, execution_id, agent_id, tool_name, \
     arguments, summary, binding_id, security_context_name, policy_id, status, created_at, \
     decided_at, decided_by, result, error, conversation_id";

const POLICY_COLUMNS: &str =
    "id, tenant_id, user_sub, tool_name, binding_id, created_at, created_by, revoked_at";

pub struct PostgresToolApprovalRepository {
    pool: PgPool,
}

impl PostgresToolApprovalRepository {
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

fn tenant(value: String) -> Result<TenantId, RepositoryError> {
    TenantId::new(value).map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))
}

fn hydrate_request(row: &PgRow) -> Result<ToolApprovalRequest, RepositoryError> {
    let status_text: String = column(row, "status")?;
    let status = ToolApprovalStatus::parse(&status_text).ok_or_else(|| {
        RepositoryError::Serialization(format!("unknown approval status: {status_text}"))
    })?;
    let policy_id: Option<Uuid> = column(row, "policy_id")?;
    Ok(ToolApprovalRequest {
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
        policy_id: policy_id.map(ToolApprovalPolicyId),
        status,
        created_at: column(row, "created_at")?,
        decided_at: column(row, "decided_at")?,
        decided_by: column(row, "decided_by")?,
        result: column(row, "result")?,
        error: column(row, "error")?,
    })
}

fn hydrate_policy(row: &PgRow) -> Result<ToolApprovalPolicy, RepositoryError> {
    Ok(ToolApprovalPolicy {
        id: ToolApprovalPolicyId(column(row, "id")?),
        tenant_id: tenant(column(row, "tenant_id")?)?,
        user_sub: column(row, "user_sub")?,
        tool_name: column(row, "tool_name")?,
        binding_id: column(row, "binding_id")?,
        created_at: column(row, "created_at")?,
        created_by: column(row, "created_by")?,
        revoked_at: column(row, "revoked_at")?,
    })
}

#[async_trait]
impl ToolApprovalRepository for PostgresToolApprovalRepository {
    async fn insert_request(&self, request: &ToolApprovalRequest) -> Result<(), RepositoryError> {
        sqlx::query(&format!(
            "INSERT INTO tool_approval_requests ({REQUEST_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)"
        ))
        .bind(request.id.0)
        .bind(request.tenant_id.as_str())
        .bind(&request.user_sub)
        .bind(request.execution_id.0)
        .bind(request.agent_id.0)
        .bind(&request.tool_name)
        .bind(&request.arguments)
        .bind(&request.summary)
        .bind(&request.binding_id)
        .bind(&request.security_context_name)
        .bind(request.policy_id.map(|p| p.0))
        .bind(request.status.as_str())
        .bind(request.created_at)
        .bind(request.decided_at)
        .bind(&request.decided_by)
        .bind(&request.result)
        .bind(&request.error)
        .bind(&request.conversation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_request(
        &self,
        id: ToolApprovalId,
    ) -> Result<Option<ToolApprovalRequest>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {REQUEST_COLUMNS} FROM tool_approval_requests WHERE id = $1"
        ))
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(hydrate_request).transpose()
    }

    async fn list_requests_for_user(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {REQUEST_COLUMNS} FROM tool_approval_requests \
             WHERE tenant_id = $1 AND user_sub = $2 AND ($3::TEXT IS NULL OR status = $3) \
             ORDER BY created_at DESC"
        ))
        .bind(tenant_id.as_str())
        .bind(user_sub)
        .bind(status.map(|s| s.as_str()))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_request).collect()
    }

    async fn list_requests(
        &self,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {REQUEST_COLUMNS} FROM tool_approval_requests \
             WHERE ($1::TEXT IS NULL OR status = $1) ORDER BY created_at DESC"
        ))
        .bind(status.map(|s| s.as_str()))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_request).collect()
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
             WHERE id = $1 AND status = 'pending' RETURNING {REQUEST_COLUMNS}"
        ))
        .bind(id.0)
        .bind(status.as_str())
        .bind(decided_by)
        .bind(decided_at)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(hydrate_request).transpose()
    }

    async fn record_outcome(
        &self,
        id: ToolApprovalId,
        result: Option<&Value>,
        error: Option<&str>,
    ) -> Result<(), RepositoryError> {
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
             WHERE status = 'pending' AND created_at <= $1 RETURNING {REQUEST_COLUMNS}"
        ))
        .bind(cutoff)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(hydrate_request).collect()
    }

    async fn insert_policy(&self, policy: &ToolApprovalPolicy) -> Result<(), RepositoryError> {
        sqlx::query(&format!(
            "INSERT INTO tool_approval_policies ({POLICY_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8)"
        ))
        .bind(policy.id.0)
        .bind(policy.tenant_id.as_str())
        .bind(&policy.user_sub)
        .bind(&policy.tool_name)
        .bind(&policy.binding_id)
        .bind(policy.created_at)
        .bind(&policy.created_by)
        .bind(policy.revoked_at)
        .execute(&self.pool)
        .await?;
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
}

impl InMemoryToolApprovalRepository {
    pub fn new() -> Self {
        Self::default()
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
        requests.insert(request.id, request.clone());
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
        self.policies
            .write()
            .await
            .insert(policy.id, policy.clone());
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
