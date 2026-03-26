// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Swarm Domain Aggregates (BC-6)
//!
//! Defines the core types for multi-agent coordination:
//!
//! - [`Swarm`] — aggregate root tracking agent membership.
//! - [`SwarmId`] — unique identifier (UUID newtype).
//! - [`ResourceLock`] — value object representing an acquired resource lock.
//!
//! See AGENTS.md §Swarm Coordination Context.

use aegis_orchestrator_core::domain::shared_kernel::AgentId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Unique identifier for a [`Swarm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SwarmId(pub Uuid);

impl SwarmId {
    /// Generate a new random `SwarmId`.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SwarmId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwarmStatus {
    Active,
    Dissolving,
    Dissolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancellationReason {
    ParentCancelled,
    Manual,
    AllChildrenComplete,
    SecurityViolation,
}

/// Aggregate root for a group of coordinated agents (BC-6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swarm {
    pub id: SwarmId,
    pub parent_agent_id: AgentId,
    /// Tracks the current members keyed by their synthetic child slot.
    pub members: HashMap<Uuid, AgentId>,
    pub status: SwarmStatus,
    pub created_at: DateTime<Utc>,
}

impl Swarm {
    pub fn new(parent_agent_id: AgentId) -> Self {
        Self {
            id: SwarmId::new(),
            parent_agent_id,
            members: HashMap::new(),
            status: SwarmStatus::Active,
            created_at: Utc::now(),
        }
    }

    pub fn add_member(&mut self, agent_id: AgentId) {
        self.members.insert(Uuid::new_v4(), agent_id);
    }

    pub fn contains(&self, agent_id: AgentId) -> bool {
        agent_id == self.parent_agent_id || self.members.values().any(|member| *member == agent_id)
    }

    pub fn member_ids(&self) -> Vec<AgentId> {
        let mut ids = Vec::with_capacity(self.members.len() + 1);
        ids.push(self.parent_agent_id);
        ids.extend(self.members.values().copied());
        ids
    }

    pub fn dissolve(&mut self) {
        self.status = SwarmStatus::Dissolved;
    }
}

/// Optimistic resource lock held by an agent within a swarm.
///
/// Only one agent may hold a `ResourceLock` for a given `resource_id` at a time.
/// Locks must be explicitly released; they are not automatically released on agent
/// completion in Phase 1 (see AGENTS.md §Swarm Coordination Context).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceLock {
    pub resource_id: String,
    pub held_by: AgentId,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEnvelope {
    pub from: AgentId,
    pub to: AgentId,
    pub payload: Vec<u8>,
    pub sent_at: DateTime<Utc>,
}

/// Specification for spawning a child agent within a swarm.
///
/// This is the Swarm context's local representation of a child agent request,
/// forming an Anti-Corruption Layer between BC-6 (Swarm Coordination) and
/// BC-1 (Agent Lifecycle). The caller converts an `AgentManifest` into this
/// type before invoking swarm operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmChildSpec {
    /// Human-readable name for the child agent
    pub name: String,
    /// Runtime language (e.g., "python", "node")
    pub language: String,
    /// Runtime language version (e.g., "3.11")
    pub version: String,
    /// Optional resource constraints
    pub resource_limits: Option<SwarmResourceLimits>,
}

/// Resource limits for a swarm child agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmResourceLimits {
    /// CPU quota in millicores
    pub cpu: u32,
    /// Memory limit string (e.g., "512Mi")
    pub memory: String,
}
