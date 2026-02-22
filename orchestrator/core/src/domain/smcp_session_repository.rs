// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SMCP Session Repository Trait (BC-12, ADR-035)
//!
//! Repository interface for persisting and querying [`SmcpSession`] aggregates.
//! The concrete implementation lives in
//! [`crate::infrastructure::smcp::session_repository`].
//!
//! ## Invariant Enforcement
//!
//! [`SmcpSessionRepository::find_active_by_agent`] is the enforcement point for the
//! "one active session per agent" invariant: the attestation service must check this
//! before creating a new session to avoid duplicate sessions.

use anyhow::Result;
use async_trait::async_trait;

use crate::domain::agent::AgentId;
use crate::domain::smcp_session::{SessionId, SmcpSession};

/// Repository for [`SmcpSession`] aggregates (BC-12 SMCP Protocol, ADR-035).
///
/// One repository per aggregate root, following the DDD repository pattern.
/// Infrastructure implementation: [`crate::infrastructure::smcp::session_repository`].
#[async_trait]
pub trait SmcpSessionRepository: Send + Sync {
    /// Persist a new or updated session.
    ///
    /// If a session with the same [`SessionId`] already exists, it is overwritten
    /// (used for status updates on revocation/expiry).
    ///
    /// # Errors
    ///
    /// Returns an error on database write failure.
    async fn save(&self, session: SmcpSession) -> Result<()>;

    /// Retrieve a session by its [`SessionId`].
    ///
    /// Returns `Ok(None)` if the session does not exist (not an error).
    async fn find_by_id(&self, id: &SessionId) -> Result<Option<SmcpSession>>;

    /// Return the single `Active` session for `agent_id`, or `None` if no active session exists.
    ///
    /// Used by the attestation service to enforce the one-active-session-per-agent invariant
    /// and by the SMCP middleware to resolve sessions from incoming envelopes.
    async fn find_active_by_agent(&self, agent_id: &AgentId) -> Result<Option<SmcpSession>>;

    /// Transition all `Active` sessions for `agent_id` to `Revoked { reason }`.
    ///
    /// Called when an execution terminates so that dangling sessions cannot be replayed.
    ///
    /// # Errors
    ///
    /// Returns an error on database write failure.
    async fn revoke_for_agent(&self, agent_id: &AgentId, reason: String) -> Result<()>;
}
