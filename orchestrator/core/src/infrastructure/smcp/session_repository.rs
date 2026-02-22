// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::domain::agent::AgentId;
use crate::domain::smcp_session::{SessionId, SmcpSession};
use crate::domain::smcp_session_repository::SmcpSessionRepository;

pub struct InMemorySmcpSessionRepository {
    // Maps SessionId -> SmcpSession
    sessions: Arc<RwLock<HashMap<SessionId, SmcpSession>>>,
}

impl InMemorySmcpSessionRepository {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl SmcpSessionRepository for InMemorySmcpSessionRepository {
    async fn save(&self, session: SmcpSession) -> Result<()> {
        let mut guard = self.sessions.write().await;
        guard.insert(session.id, session);
        Ok(())
    }

    async fn find_by_id(&self, id: &SessionId) -> Result<Option<SmcpSession>> {
        let guard = self.sessions.read().await;
        Ok(guard.get(id).cloned())
    }

    async fn find_active_by_agent(&self, agent_id: &AgentId) -> Result<Option<SmcpSession>> {
        let guard = self.sessions.read().await;
        // Returns the most recently created active session for the agent
        let mut active_sessions: Vec<_> = guard.values()
            .filter(|s| s.agent_id == *agent_id && s.status == crate::domain::smcp_session::SessionStatus::Active)
            .collect();
        
        active_sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        
        Ok(active_sessions.first().map(|s| (*s).clone()))
    }

    async fn revoke_for_agent(&self, agent_id: &AgentId, reason: String) -> Result<()> {
        let mut guard = self.sessions.write().await;
        for session in guard.values_mut() {
            if session.agent_id == *agent_id && session.status == crate::domain::smcp_session::SessionStatus::Active {
                session.revoke(reason.clone());
            }
        }
        Ok(())
    }
}
