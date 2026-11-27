// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::domain::agent::AgentId;
use crate::domain::seal_session::{SealSession, SessionId};
use crate::domain::seal_session_repository::SealSessionRepository;
use crate::domain::secrets::SensitiveString;

pub struct InMemorySealSessionRepository {
    // Maps SessionId -> SealSession
    sessions: Arc<RwLock<HashMap<SessionId, SealSession>>>,
}

impl InMemorySealSessionRepository {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemorySealSessionRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SealSessionRepository for InMemorySealSessionRepository {
    async fn save(&self, session: SealSession) -> Result<()> {
        let mut guard = self.sessions.write().await;
        guard.insert(session.id, session);
        Ok(())
    }

    async fn find_by_id(&self, id: &SessionId) -> Result<Option<SealSession>> {
        let guard = self.sessions.read().await;
        Ok(guard.get(id).cloned())
    }

    async fn find_active_by_security_token(
        &self,
        token: &SensitiveString,
    ) -> Result<Option<SealSession>> {
        let guard = self.sessions.read().await;
        Ok(guard
            .values()
            .filter(|s| {
                // `SensitiveString`'s equality is constant-time.
                s.security_token_raw == *token
                    && s.status == crate::domain::seal_session::SessionStatus::Active
            })
            .max_by_key(|s| s.created_at)
            .cloned())
    }

    async fn find_active_by_agent(&self, agent_id: &AgentId) -> Result<Option<SealSession>> {
        let guard = self.sessions.read().await;
        // Returns the most recently created active session for the agent
        Ok(guard
            .values()
            .filter(|s| {
                s.agent_id == *agent_id
                    && s.status == crate::domain::seal_session::SessionStatus::Active
            })
            .max_by_key(|s| s.created_at)
            .cloned())
    }

    async fn revoke_for_agent(&self, agent_id: &AgentId, reason: String) -> Result<()> {
        let mut guard = self.sessions.write().await;
        for session in guard.values_mut() {
            if session.agent_id == *agent_id
                && session.status == crate::domain::seal_session::SessionStatus::Active
            {
                session.revoke(reason.clone());
                // ADR-058 BC-4: decrement active session gauge on revocation.
                metrics::gauge!("aegis_seal_sessions_active").decrement(1.0);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL (AEGIS ADR-152 D4.1)
// ---------------------------------------------------------------------------

/// A session as `seal_sessions.body` holds it: everything but its bearer
/// token, which is stored only as `token_hash`.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredSession {
    agent_public_key: Vec<u8>,
    security_context: crate::domain::security_context::SecurityContext,
    principal_subject: Option<String>,
    user_id: Option<String>,
    workload_id: Option<String>,
    zaru_tier: Option<String>,
    tenant_id: String,
    operator_escalation: Option<(uuid::Uuid, crate::domain::iam::AegisRole)>,
    attested_by: crate::domain::seal_session::AttestingCredential,
    status: crate::domain::seal_session::SessionStatus,
}

/// The SHA-256 of a session's bearer token, by which it is found.
pub fn token_hash(token: &SensitiveString) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(token.expose().as_bytes()).to_vec()
}

fn status_name(status: &crate::domain::seal_session::SessionStatus) -> &'static str {
    use crate::domain::seal_session::SessionStatus;
    match status {
        SessionStatus::Active => "active",
        SessionStatus::Expired => "expired",
        SessionStatus::Revoked { .. } => "revoked",
    }
}

/// Inserts a session, or updates its state; the token's hash, set at
/// attestation, never changes.
pub const SAVE_SESSION_SQL: &str = r#"
INSERT INTO seal_sessions (id, agent_id, execution_id, token_hash, status, body, created_at, expires_at)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status, body = EXCLUDED.body, expires_at = EXCLUDED.expires_at
"#;

const SESSION_COLUMNS: &str = "id, agent_id, execution_id, body, created_at, expires_at";

/// The SEAL sessions every member of the cluster shares (ADR-152 D4.1).
pub struct PostgresSealSessionRepository {
    pool: sqlx::PgPool,
}

impl PostgresSealSessionRepository {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }

    /// Delete sessions that ended or expired more than a day ago; how many.
    pub async fn delete_stale(pool: &sqlx::PgPool) -> Result<u64> {
        Ok(sqlx::query(DELETE_STALE_SESSIONS_SQL)
            .execute(pool)
            .await?
            .rows_affected())
    }

    fn from_row(
        row: (
            uuid::Uuid,
            uuid::Uuid,
            uuid::Uuid,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        ),
        token: SensitiveString,
    ) -> Result<SealSession> {
        let (id, agent_id, execution_id, body, created_at, expires_at) = row;
        let stored: StoredSession = serde_json::from_value(body)?;
        Ok(SealSession {
            id: SessionId(id),
            agent_id: AgentId(agent_id),
            execution_id: crate::domain::execution::ExecutionId(execution_id),
            agent_public_key: stored.agent_public_key,
            security_token_raw: token,
            security_context: stored.security_context,
            principal_subject: stored.principal_subject,
            user_id: stored.user_id,
            workload_id: stored.workload_id,
            zaru_tier: stored.zaru_tier,
            tenant_id: crate::domain::tenant::TenantId::from_string(&stored.tenant_id)
                .map_err(|e| anyhow::anyhow!("a stored SEAL session names no tenant: {e}"))?,
            operator_escalation: stored
                .operator_escalation
                .map(|(escalation_id, aegis_role)| {
                    crate::domain::seal_session::SealOperatorEscalation {
                        escalation_id,
                        aegis_role,
                    }
                }),
            attested_by: stored.attested_by,
            status: stored.status,
            created_at,
            expires_at,
        })
    }
}

/// The GC's statement: sessions over a day past their end.
pub const DELETE_STALE_SESSIONS_SQL: &str =
    "DELETE FROM seal_sessions WHERE expires_at < now() - interval '1 day'";

#[async_trait]
impl SealSessionRepository for PostgresSealSessionRepository {
    async fn save(&self, session: SealSession) -> Result<()> {
        let body = serde_json::to_value(StoredSession {
            agent_public_key: session.agent_public_key.clone(),
            security_context: session.security_context.clone(),
            principal_subject: session.principal_subject.clone(),
            user_id: session.user_id.clone(),
            workload_id: session.workload_id.clone(),
            zaru_tier: session.zaru_tier.clone(),
            tenant_id: session.tenant_id.as_str().to_string(),
            operator_escalation: session
                .operator_escalation
                .as_ref()
                .map(|e| (e.escalation_id, e.aegis_role.clone())),
            attested_by: session.attested_by,
            status: session.status.clone(),
        })?;
        sqlx::query(SAVE_SESSION_SQL)
            .bind(session.id.0)
            .bind(session.agent_id.0)
            .bind(session.execution_id.0)
            .bind(token_hash(&session.security_token_raw))
            .bind(status_name(&session.status))
            .bind(body)
            .bind(session.created_at)
            .bind(session.expires_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn find_by_id(&self, id: &SessionId) -> Result<Option<SealSession>> {
        let row = sqlx::query_as(&format!(
            "SELECT {SESSION_COLUMNS} FROM seal_sessions WHERE id = $1"
        ))
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await?;
        // Found by id the session carries no token: only a lookup by the
        // presented token gives it back the token it was presented with.
        row.map(|r| Self::from_row(r, SensitiveString::new(String::new())))
            .transpose()
    }

    async fn find_active_by_security_token(
        &self,
        token: &SensitiveString,
    ) -> Result<Option<SealSession>> {
        let row = sqlx::query_as(&format!(
            "SELECT {SESSION_COLUMNS} FROM seal_sessions WHERE token_hash = $1 AND status = 'active' ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(token_hash(token))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Self::from_row(r, token.clone())).transpose()
    }

    async fn find_active_by_agent(&self, agent_id: &AgentId) -> Result<Option<SealSession>> {
        let row = sqlx::query_as(&format!(
            "SELECT {SESSION_COLUMNS} FROM seal_sessions WHERE agent_id = $1 AND status = 'active' ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(agent_id.0)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Self::from_row(r, SensitiveString::new(String::new())))
            .transpose()
    }

    async fn revoke_for_agent(&self, agent_id: &AgentId, reason: String) -> Result<()> {
        let rows: Vec<(
            uuid::Uuid,
            uuid::Uuid,
            uuid::Uuid,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        )> = sqlx::query_as(&format!(
            "SELECT {SESSION_COLUMNS} FROM seal_sessions WHERE agent_id = $1 AND status = 'active'"
        ))
        .bind(agent_id.0)
        .fetch_all(&self.pool)
        .await?;
        for row in rows {
            let mut session = Self::from_row(row, SensitiveString::new(String::new()))?;
            session.revoke(reason.clone());
            // The update leaves token_hash as attestation set it.
            self.save(session).await?;
            metrics::gauge!("aegis_seal_sessions_active").decrement(1.0);
        }
        Ok(())
    }
}
