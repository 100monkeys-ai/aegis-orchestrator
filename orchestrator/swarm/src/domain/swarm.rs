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

#[cfg(test)]
mod tests {
    use super::*;

    // ═══════════════════════════════════════════════════════════════════════════
    // Swarm::new() — creation with parent agent.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn new_swarm_has_active_status() {
        let parent = AgentId::new();
        let swarm = Swarm::new(parent);
        assert_eq!(swarm.status, SwarmStatus::Active);
        assert_eq!(swarm.parent_agent_id, parent);
        assert!(swarm.members.is_empty());
    }

    #[test]
    fn new_swarm_contains_parent() {
        let parent = AgentId::new();
        let swarm = Swarm::new(parent);
        assert!(swarm.contains(parent));
    }

    #[test]
    fn new_swarm_does_not_contain_arbitrary_agent() {
        let parent = AgentId::new();
        let swarm = Swarm::new(parent);
        let other = AgentId::new();
        assert!(!swarm.contains(other));
    }

    #[test]
    fn new_swarm_member_ids_includes_parent_only() {
        let parent = AgentId::new();
        let swarm = Swarm::new(parent);
        let ids = swarm.member_ids();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], parent);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // add_member() / contains() / member_ids().
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn add_member_makes_agent_contained() {
        let parent = AgentId::new();
        let mut swarm = Swarm::new(parent);
        let child = AgentId::new();
        swarm.add_member(child);
        assert!(swarm.contains(child));
    }

    #[test]
    fn add_member_increments_member_count() {
        let parent = AgentId::new();
        let mut swarm = Swarm::new(parent);
        assert_eq!(swarm.members.len(), 0);
        swarm.add_member(AgentId::new());
        assert_eq!(swarm.members.len(), 1);
        swarm.add_member(AgentId::new());
        assert_eq!(swarm.members.len(), 2);
    }

    #[test]
    fn member_ids_includes_parent_and_all_children() {
        let parent = AgentId::new();
        let mut swarm = Swarm::new(parent);
        let child1 = AgentId::new();
        let child2 = AgentId::new();
        let child3 = AgentId::new();
        swarm.add_member(child1);
        swarm.add_member(child2);
        swarm.add_member(child3);

        let ids = swarm.member_ids();
        assert_eq!(ids.len(), 4); // parent + 3 children
        assert_eq!(ids[0], parent);
        assert!(ids.contains(&child1));
        assert!(ids.contains(&child2));
        assert!(ids.contains(&child3));
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Multiple members — add several, verify all present.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn multiple_members_all_contained() {
        let parent = AgentId::new();
        let mut swarm = Swarm::new(parent);
        let children: Vec<AgentId> = (0..10).map(|_| AgentId::new()).collect();
        for &c in &children {
            swarm.add_member(c);
        }
        for &c in &children {
            assert!(swarm.contains(c), "child {c} should be in swarm");
        }
        assert!(swarm.contains(parent));
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Duplicate member handling — adding the same AgentId twice creates two
    // member slots (each with a unique synthetic child UUID key).
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn duplicate_member_creates_separate_slots() {
        let parent = AgentId::new();
        let mut swarm = Swarm::new(parent);
        let child = AgentId::new();
        swarm.add_member(child);
        swarm.add_member(child);
        // HashMap keys are random UUIDs, so both insertions create distinct entries.
        assert_eq!(swarm.members.len(), 2);
        assert!(swarm.contains(child));
        // member_ids will list the child twice (plus parent).
        let ids = swarm.member_ids();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids.iter().filter(|&&id| id == child).count(), 2);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // dissolve() — swarm dissolution.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn dissolve_sets_status_to_dissolved() {
        let parent = AgentId::new();
        let mut swarm = Swarm::new(parent);
        swarm.add_member(AgentId::new());
        assert_eq!(swarm.status, SwarmStatus::Active);
        swarm.dissolve();
        assert_eq!(swarm.status, SwarmStatus::Dissolved);
    }

    #[test]
    fn dissolve_preserves_members() {
        let parent = AgentId::new();
        let mut swarm = Swarm::new(parent);
        let child = AgentId::new();
        swarm.add_member(child);
        swarm.dissolve();
        // Members are still tracked even after dissolution.
        assert!(swarm.contains(child));
        assert_eq!(swarm.members.len(), 1);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // SwarmId — construction and uniqueness.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn swarm_id_new_generates_unique_ids() {
        let id1 = SwarmId::new();
        let id2 = SwarmId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn swarm_id_default_generates_unique_id() {
        let id1 = SwarmId::default();
        let id2 = SwarmId::default();
        assert_ne!(id1, id2);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // SwarmStatus — enum variant construction.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn swarm_status_variants_are_distinct() {
        assert_ne!(SwarmStatus::Active, SwarmStatus::Dissolving);
        assert_ne!(SwarmStatus::Dissolving, SwarmStatus::Dissolved);
        assert_ne!(SwarmStatus::Active, SwarmStatus::Dissolved);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // ResourceLock — construction and properties.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn resource_lock_construction() {
        let holder = AgentId::new();
        let now = Utc::now();
        let expires = now + chrono::Duration::minutes(5);
        let lock = ResourceLock {
            resource_id: "shared-db".to_string(),
            held_by: holder,
            acquired_at: now,
            expires_at: expires,
        };
        assert_eq!(lock.resource_id, "shared-db");
        assert_eq!(lock.held_by, holder);
        assert!(lock.expires_at > lock.acquired_at);
    }

    #[test]
    fn resource_lock_serde_roundtrip() {
        let holder = AgentId::new();
        let now = Utc::now();
        let expires = now + chrono::Duration::minutes(10);
        let lock = ResourceLock {
            resource_id: "file-lock".to_string(),
            held_by: holder,
            acquired_at: now,
            expires_at: expires,
        };
        let json = serde_json::to_string(&lock).unwrap();
        let deser: ResourceLock = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.resource_id, "file-lock");
        assert_eq!(deser.held_by, holder);
    }

    #[test]
    fn resource_lock_equality() {
        let holder = AgentId::new();
        let now = Utc::now();
        let expires = now + chrono::Duration::minutes(5);
        let lock1 = ResourceLock {
            resource_id: "res-1".to_string(),
            held_by: holder,
            acquired_at: now,
            expires_at: expires,
        };
        let lock2 = lock1.clone();
        assert_eq!(lock1, lock2);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // SwarmChildSpec — Anti-Corruption Layer construction.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn swarm_child_spec_construction_without_limits() {
        let spec = SwarmChildSpec {
            name: "worker-1".to_string(),
            language: "python".to_string(),
            version: "3.11".to_string(),
            resource_limits: None,
        };
        assert_eq!(spec.name, "worker-1");
        assert_eq!(spec.language, "python");
        assert_eq!(spec.version, "3.11");
        assert!(spec.resource_limits.is_none());
    }

    #[test]
    fn swarm_child_spec_construction_with_limits() {
        let spec = SwarmChildSpec {
            name: "gpu-worker".to_string(),
            language: "node".to_string(),
            version: "20".to_string(),
            resource_limits: Some(SwarmResourceLimits {
                cpu: 2000,
                memory: "1Gi".to_string(),
            }),
        };
        assert!(spec.resource_limits.is_some());
        let limits = spec.resource_limits.unwrap();
        assert_eq!(limits.cpu, 2000);
        assert_eq!(limits.memory, "1Gi");
    }

    #[test]
    fn swarm_child_spec_serde_roundtrip() {
        let spec = SwarmChildSpec {
            name: "analyzer".to_string(),
            language: "python".to_string(),
            version: "3.12".to_string(),
            resource_limits: Some(SwarmResourceLimits {
                cpu: 500,
                memory: "256Mi".to_string(),
            }),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let deser: SwarmChildSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.name, "analyzer");
        assert_eq!(deser.language, "python");
        assert_eq!(deser.version, "3.12");
        let limits = deser.resource_limits.unwrap();
        assert_eq!(limits.cpu, 500);
        assert_eq!(limits.memory, "256Mi");
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // MessageEnvelope — construction and serde.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn message_envelope_serde_roundtrip() {
        let from = AgentId::new();
        let to = AgentId::new();
        let envelope = MessageEnvelope {
            from,
            to,
            payload: b"hello swarm".to_vec(),
            sent_at: Utc::now(),
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let deser: MessageEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.from, from);
        assert_eq!(deser.to, to);
        assert_eq!(deser.payload, b"hello swarm");
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // CancellationReason — variant coverage.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn cancellation_reason_serde_roundtrip() {
        let reasons = vec![
            CancellationReason::ParentCancelled,
            CancellationReason::Manual,
            CancellationReason::AllChildrenComplete,
            CancellationReason::SecurityViolation,
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let deser: CancellationReason = serde_json::from_str(&json).unwrap();
            assert_eq!(deser, reason);
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Swarm serde round-trip (full aggregate).
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn swarm_serde_roundtrip() {
        let parent = AgentId::new();
        let mut swarm = Swarm::new(parent);
        let child1 = AgentId::new();
        let child2 = AgentId::new();
        swarm.add_member(child1);
        swarm.add_member(child2);

        let json = serde_json::to_string(&swarm).unwrap();
        let deser: Swarm = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.id, swarm.id);
        assert_eq!(deser.parent_agent_id, parent);
        assert_eq!(deser.status, SwarmStatus::Active);
        assert_eq!(deser.members.len(), 2);
        assert!(deser.contains(child1));
        assert!(deser.contains(child2));
        assert!(deser.contains(parent));
    }
}
