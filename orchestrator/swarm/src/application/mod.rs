// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Swarm Application Layer (BC-6)
//!
//! Use-case interfaces for multi-agent coordination.
//!
//! | Symbol | Purpose |
//! |--------|---------|
//! | [`LockToken`] | Handle returned by `acquire_lock`; must be passed to `release_lock` |
//! | [`SwarmService`] | Application service trait for swarm lifecycle and coordination |

use crate::domain::SwarmId;
use aegis_core::domain::agent::{AgentId, AgentManifest};
use anyhow::Result;
use async_trait::async_trait;

/// Opaque token returned by [`SwarmService::acquire_lock`].
///
/// Pass this token to [`SwarmService::release_lock`] to release the lock.
/// Dropping the token without releasing it does **not** auto-release in Phase 1.
#[derive(Debug, Clone)]
pub struct LockToken(pub String);

/// Application service for multi-agent swarm coordination (BC-6).
///
/// Implemented by `StandardSwarmService` in `crate::infrastructure`.
/// Injected into the orchestrator core via `Arc<dyn SwarmService>`.
#[async_trait]
pub trait SwarmService: Send + Sync {
    /// Create a new swarm with `parent_id` as the root agent.
    ///
    /// Should be called before spawning the first child agent.
    async fn create_swarm(&self, parent_id: AgentId) -> Result<SwarmId>;

    /// Spawn a child agent within the current swarm.
    ///
    /// Child agents inherit the parent's security context unless explicitly
    /// overridden in `manifest`.
    async fn spawn_child(&self, parent_id: AgentId, manifest: AgentManifest) -> Result<AgentId>;

    /// Send an opaque `payload` from one agent to another.
    ///
    /// Both agents must be members of the same swarm. Payload encoding is
    /// convention-based (e.g. JSON, msgpack).
    async fn send_message(&self, from: AgentId, to: AgentId, payload: Vec<u8>) -> Result<()>;

    /// Acquire an exclusive lock on `resource`.
    ///
    /// Blocks until the lock is available. Returns a [`LockToken`] that
    /// must be passed to [`Self::release_lock`] to free the resource.
    async fn acquire_lock(&self, resource: &str) -> Result<LockToken>;
    async fn release_lock(&self, token: LockToken) -> Result<()>;
}
