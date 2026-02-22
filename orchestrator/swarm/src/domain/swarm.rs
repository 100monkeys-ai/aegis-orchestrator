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

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use aegis_core::domain::agent::AgentId;

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

/// Aggregate root for a group of coordinated agents (BC-6).
///
/// A `Swarm` is created when an agent spawns child agents. It tracks membership
/// and provides the basis for cascade cancellation and resource lock deconfliction.
///
/// # Invariants
///
/// - An agent may only belong to one swarm at a time.
/// - Removing the last agent dissolves the swarm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swarm {
    /// Unique swarm identifier.
    pub id: SwarmId,
    /// Set of all `AgentId`s participating in this swarm.
    pub agents: HashSet<AgentId>,
    /// When the swarm was created (first child spawned).
    pub created_at: DateTime<Utc>,
}

/// Optimistic resource lock held by an agent within a swarm.
///
/// Only one agent may hold a `ResourceLock` for a given `resource_id` at a time.
/// Locks must be explicitly released; they are not automatically released on agent
/// completion in Phase 1 (see AGENTS.md §Swarm Coordination Context).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceLock {
    /// Opaque identifier for the locked resource (e.g. file path, DB row key).
    pub resource_id: String,
    /// The agent currently holding the lock.
    pub held_by: AgentId,
    /// When the lock was acquired.
    pub acquired_at: DateTime<Utc>,
}
