// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Swarm Application Layer (BC-6)
//!
//! Use-case interfaces for multi-agent coordination.
//!
//! | Symbol | Purpose |
//! |--------|---------|
//! | [`LockToken`] | Handle returned by `acquire_lock`; must be passed to `release_lock` |
//! | [`SpawnedChild`] | Value object returned by `spawn_child` with agent, execution, and swarm IDs |
//! | [`SwarmService`] | Application service trait for swarm lifecycle and coordination |

use crate::domain::swarm::SwarmChildSpec;
use crate::domain::{CancellationReason, Swarm, SwarmId};
use aegis_orchestrator_core::domain::shared_kernel::{AgentId, ExecutionId};
use aegis_orchestrator_core::domain::tenant::TenantId;
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

/// Opaque token returned by [`SwarmService::acquire_lock`].
///
/// Pass this token to [`SwarmService::release_lock`] to release the lock.
/// Dropping the token without releasing it does **not** auto-release in Phase 1.
#[derive(Debug, Clone)]
pub struct LockToken(pub String);

/// Value object returned when a child agent is successfully spawned within a swarm.
#[derive(Debug, Clone)]
pub struct SpawnedChild {
    pub agent_id: AgentId,
    pub execution_id: ExecutionId,
    pub swarm_id: SwarmId,
}

/// Application service for multi-agent swarm coordination (BC-6).
///
/// Implemented by `StandardSwarmService` in `crate::infrastructure`.
/// Injected into the orchestrator core via `Arc<dyn SwarmService>`.
#[async_trait]
pub trait SwarmService: Send + Sync {
    /// Create a new swarm with `parent_execution_id` as the root execution.
    ///
    /// Should be called before spawning the first child agent.
    async fn create_swarm(
        &self,
        parent_execution_id: ExecutionId,
        tenant_id: TenantId,
    ) -> Result<SwarmId>;

    /// Spawn a child agent within an existing swarm.
    ///
    /// Child agents inherit the parent's security context unless explicitly
    /// overridden in `spec`.
    async fn spawn_child(
        &self,
        swarm_id: SwarmId,
        spec: SwarmChildSpec,
        parent_security_context: Option<String>,
    ) -> Result<SpawnedChild>;

    /// Send an opaque `payload` from one agent to another.
    ///
    /// Both agents must be members of the same swarm. Payload encoding is
    /// convention-based (e.g. JSON, msgpack).
    async fn send_message(&self, from: AgentId, to: AgentId, payload: Vec<u8>) -> Result<()>;

    /// Acquire an exclusive lock on `resource` within a swarm.
    ///
    /// Returns a [`LockToken`] that must be passed to [`Self::release_lock`]
    /// to free the resource.
    async fn acquire_lock(
        &self,
        swarm_id: SwarmId,
        resource: &str,
        holder: AgentId,
        execution_id: ExecutionId,
        ttl: Duration,
    ) -> Result<LockToken>;

    /// Release a previously acquired lock.
    async fn release_lock(&self, token: LockToken) -> Result<()>;

    /// Cancel a swarm, dissolving it and cleaning up resources.
    async fn cancel_swarm(&self, swarm_id: SwarmId, reason: CancellationReason) -> Result<()>;

    /// Retrieve a snapshot of a swarm by ID.
    async fn get_swarm(&self, swarm_id: SwarmId) -> Result<Option<Swarm>>;

    /// List all child execution IDs within a swarm.
    async fn list_child_executions(&self, swarm_id: SwarmId) -> Result<Vec<ExecutionId>>;

    /// Broadcast a message to all members of a swarm except `from`.
    async fn broadcast_message(
        &self,
        swarm_id: SwarmId,
        from: AgentId,
        payload: Vec<u8>,
    ) -> Result<()>;
}
