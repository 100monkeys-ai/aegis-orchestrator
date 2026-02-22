// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use async_trait::async_trait;

use crate::domain::agent::AgentId;
use crate::domain::smcp_session::{SessionId, SmcpSession};

/// Protocol layer SMCP Session repository
#[async_trait]
pub trait SmcpSessionRepository: Send + Sync {
    /// Save a new or updated session
    async fn save(&self, session: SmcpSession) -> Result<()>;
    
    /// Find a session by its ID
    async fn find_by_id(&self, id: &SessionId) -> Result<Option<SmcpSession>>;
    
    /// Find the currently active session for an agent execution
    async fn find_active_by_agent(&self, agent_id: &AgentId) -> Result<Option<SmcpSession>>;
    
    /// Revoke all active sessions for an agent
    async fn revoke_for_agent(&self, agent_id: &AgentId, reason: String) -> Result<()>;
}
