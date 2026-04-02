// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL Session Repository Trait (BC-12, ADR-035)
//!
//! Repository interface for persisting and querying [`SealSession`] aggregates.
//! The concrete implementation lives in
//! [`crate::infrastructure::seal::session_repository`].
//!
//! ## Invariant Enforcement
//!
//! [`SealSessionRepository::find_active_by_agent`] is the enforcement point for the
//! "one active session per agent" invariant: the attestation service must check this
//! before creating a new session to avoid duplicate sessions.

use anyhow::Result;
use async_trait::async_trait;

use crate::domain::agent::AgentId;
use crate::domain::seal_session::{SealSession, SessionId};

/// Repository for [`SealSession`] aggregates (BC-12 SEAL Protocol, ADR-035).
///
/// One repository per aggregate root, following the DDD repository pattern.
/// Infrastructure implementation: [`crate::infrastructure::seal::session_repository`].
#[async_trait]
pub trait SealSessionRepository: Send + Sync {
    /// Persist a new or updated session.
    ///
    /// If a session with the same [`SessionId`] already exists, it is overwritten
    /// (used for status updates on revocation/expiry).
    ///
    /// # Errors
    ///
    /// Returns an error on database write failure.
    async fn save(&self, session: SealSession) -> Result<()>;

    /// Retrieve a session by its [`SessionId`].
    ///
    /// Returns `Ok(None)` if the session does not exist (not an error).
    async fn find_by_id(&self, id: &SessionId) -> Result<Option<SealSession>>;

    /// Return the single `Active` session whose `security_token_raw` matches `token`, or `None`.
    ///
    /// Used by the tool invocation service to resolve a session from the opaque token string
    /// without requiring prior JWT decode; claim extraction then happens from the loaded session.
    async fn find_active_by_security_token(&self, token: &str) -> Result<Option<SealSession>>;

    /// Return the single `Active` session for `agent_id`, or `None` if no active session exists.
    ///
    /// Used by the attestation service to enforce the one-active-session-per-agent invariant
    /// and by the SEAL middleware to resolve sessions from incoming envelopes.
    async fn find_active_by_agent(&self, agent_id: &AgentId) -> Result<Option<SealSession>>;

    /// Transition all `Active` sessions for `agent_id` to `Revoked { reason }`.
    ///
    /// Called when an execution terminates so that dangling sessions cannot be replayed.
    ///
    /// # Errors
    ///
    /// Returns an error on database write failure.
    async fn revoke_for_agent(&self, agent_id: &AgentId, reason: String) -> Result<()>;
}
