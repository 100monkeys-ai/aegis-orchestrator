// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Swarm
//!
//! Provides swarm functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** Implements swarm

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use aegis_core::domain::agent::AgentId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SwarmId(pub Uuid);

impl SwarmId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SwarmId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swarm {
    pub id: SwarmId,
    pub agents: HashSet<AgentId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceLock {
    pub resource_id: String,
    pub held_by: AgentId,
    pub acquired_at: DateTime<Utc>,
}
