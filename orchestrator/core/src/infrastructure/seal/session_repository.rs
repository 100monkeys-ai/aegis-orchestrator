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

    async fn find_active_by_security_token(&self, token: &str) -> Result<Option<SealSession>> {
        let guard = self.sessions.read().await;
        Ok(guard
            .values()
            .filter(|s| {
                s.security_token_raw == token
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
