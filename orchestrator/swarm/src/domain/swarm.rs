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

use aegis_orchestrator_core::domain::shared_kernel::{AgentId, ExecutionId};
use aegis_orchestrator_core::domain::tenant::TenantId;
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

/// Error returned when a child spawn violates swarm invariants.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error(
        "cross-tenant spawn forbidden: swarm tenant {swarm_tenant} != child tenant {child_tenant}"
    )]
    CrossTenantSpawnForbidden {
        swarm_tenant: String,
        child_tenant: String,
    },
}

/// Aggregate root for a group of coordinated agents (BC-6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swarm {
    pub id: SwarmId,
    pub parent_execution_id: ExecutionId,
    pub tenant_id: TenantId,
    /// Tracks the current members keyed by their execution ID.
    pub members: HashMap<ExecutionId, AgentId>,
    pub status: SwarmStatus,
    pub created_at: DateTime<Utc>,
    pub dissolved_at: Option<DateTime<Utc>>,
}

impl Swarm {
    pub fn new(parent_execution_id: ExecutionId, tenant_id: TenantId) -> Self {
        Self {
            id: SwarmId::new(),
            parent_execution_id,
            tenant_id,
            members: HashMap::new(),
            status: SwarmStatus::Active,
            created_at: Utc::now(),
            dissolved_at: None,
        }
    }

    pub fn add_member(&mut self, execution_id: ExecutionId, agent_id: AgentId) {
        self.members.insert(execution_id, agent_id);
    }

    /// Spawn a child agent into the swarm, enforcing the cross-tenant invariant.
    ///
    /// Returns `Err(SpawnError::CrossTenantSpawnForbidden)` if the child's tenant differs
    /// from the swarm's tenant. Child executions must always belong to the same tenant
    /// as their parent swarm (ADR-056).
    pub fn spawn_child(&mut self, child_spec: SwarmChildSpec) -> Result<(), SpawnError> {
        if child_spec.tenant_id != self.tenant_id {
            return Err(SpawnError::CrossTenantSpawnForbidden {
                swarm_tenant: self.tenant_id.as_str().to_string(),
                child_tenant: child_spec.tenant_id.as_str().to_string(),
            });
        }
        self.add_member(child_spec.execution_id, child_spec.agent_id);
        Ok(())
    }

    pub fn contains(&self, agent_id: AgentId) -> bool {
        self.members.values().any(|member| *member == agent_id)
    }

    pub fn contains_execution(&self, execution_id: &ExecutionId) -> bool {
        self.members.contains_key(execution_id)
    }

    pub fn member_ids(&self) -> Vec<AgentId> {
        self.members.values().copied().collect()
    }

    pub fn member_execution_ids(&self) -> Vec<ExecutionId> {
        self.members.keys().copied().collect()
    }

    pub fn dissolve(&mut self) {
        self.status = SwarmStatus::Dissolved;
        self.dissolved_at = Some(Utc::now());
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
    pub execution_id: ExecutionId,
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
    /// The execution ID assigned to the child
    pub execution_id: ExecutionId,
    /// The agent definition the child will run
    pub agent_id: AgentId,
    /// Tenant that owns this child — must match the swarm's tenant_id
    pub tenant_id: TenantId,
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
    // Swarm::new() — creation with parent execution.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn new_swarm_has_active_status() {
        let parent_exec = ExecutionId::new();
        let swarm = Swarm::new(parent_exec, TenantId::consumer());
        assert_eq!(swarm.status, SwarmStatus::Active);
        assert_eq!(swarm.parent_execution_id, parent_exec);
        assert!(swarm.members.is_empty());
        assert!(swarm.dissolved_at.is_none());
    }

    #[test]
    fn new_swarm_does_not_contain_arbitrary_agent() {
        let parent_exec = ExecutionId::new();
        let swarm = Swarm::new(parent_exec, TenantId::consumer());
        let other = AgentId::new();
        assert!(!swarm.contains(other));
    }

    #[test]
    fn new_swarm_member_ids_is_empty() {
        let parent_exec = ExecutionId::new();
        let swarm = Swarm::new(parent_exec, TenantId::consumer());
        let ids = swarm.member_ids();
        assert!(ids.is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // add_member() / contains() / member_ids().
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn add_member_makes_agent_contained() {
        let parent_exec = ExecutionId::new();
        let mut swarm = Swarm::new(parent_exec, TenantId::consumer());
        let child = AgentId::new();
        let child_exec = ExecutionId::new();
        swarm.add_member(child_exec, child);
        assert!(swarm.contains(child));
        assert!(swarm.contains_execution(&child_exec));
    }

    #[test]
    fn add_member_increments_member_count() {
        let parent_exec = ExecutionId::new();
        let mut swarm = Swarm::new(parent_exec, TenantId::consumer());
        assert_eq!(swarm.members.len(), 0);
        swarm.add_member(ExecutionId::new(), AgentId::new());
        assert_eq!(swarm.members.len(), 1);
        swarm.add_member(ExecutionId::new(), AgentId::new());
        assert_eq!(swarm.members.len(), 2);
    }

    #[test]
    fn member_ids_includes_all_children() {
        let parent_exec = ExecutionId::new();
        let mut swarm = Swarm::new(parent_exec, TenantId::consumer());
        let child1 = AgentId::new();
        let child2 = AgentId::new();
        let child3 = AgentId::new();
        swarm.add_member(ExecutionId::new(), child1);
        swarm.add_member(ExecutionId::new(), child2);
        swarm.add_member(ExecutionId::new(), child3);

        let ids = swarm.member_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&child1));
        assert!(ids.contains(&child2));
        assert!(ids.contains(&child3));
    }

    #[test]
    fn member_execution_ids_returns_all_execution_ids() {
        let parent_exec = ExecutionId::new();
        let mut swarm = Swarm::new(parent_exec, TenantId::consumer());
        let exec1 = ExecutionId::new();
        let exec2 = ExecutionId::new();
        swarm.add_member(exec1, AgentId::new());
        swarm.add_member(exec2, AgentId::new());

        let exec_ids = swarm.member_execution_ids();
        assert_eq!(exec_ids.len(), 2);
        assert!(exec_ids.contains(&exec1));
        assert!(exec_ids.contains(&exec2));
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Multiple members — add several, verify all present.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn multiple_members_all_contained() {
        let parent_exec = ExecutionId::new();
        let mut swarm = Swarm::new(parent_exec, TenantId::consumer());
        let children: Vec<(ExecutionId, AgentId)> = (0..10)
            .map(|_| (ExecutionId::new(), AgentId::new()))
            .collect();
        for &(exec_id, agent_id) in &children {
            swarm.add_member(exec_id, agent_id);
        }
        for &(_, agent_id) in &children {
            assert!(
                swarm.contains(agent_id),
                "child {agent_id} should be in swarm"
            );
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // Duplicate member handling — adding the same AgentId with different
    // ExecutionIds creates separate member entries.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn duplicate_agent_with_different_executions_creates_separate_slots() {
        let parent_exec = ExecutionId::new();
        let mut swarm = Swarm::new(parent_exec, TenantId::consumer());
        let child = AgentId::new();
        swarm.add_member(ExecutionId::new(), child);
        swarm.add_member(ExecutionId::new(), child);
        assert_eq!(swarm.members.len(), 2);
        assert!(swarm.contains(child));
        let ids = swarm.member_ids();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids.iter().filter(|&&id| id == child).count(), 2);
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // dissolve() — swarm dissolution.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn dissolve_sets_status_to_dissolved() {
        let parent_exec = ExecutionId::new();
        let mut swarm = Swarm::new(parent_exec, TenantId::consumer());
        swarm.add_member(ExecutionId::new(), AgentId::new());
        assert_eq!(swarm.status, SwarmStatus::Active);
        assert!(swarm.dissolved_at.is_none());
        swarm.dissolve();
        assert_eq!(swarm.status, SwarmStatus::Dissolved);
        assert!(swarm.dissolved_at.is_some());
    }

    #[test]
    fn dissolve_preserves_members() {
        let parent_exec = ExecutionId::new();
        let mut swarm = Swarm::new(parent_exec, TenantId::consumer());
        let child = AgentId::new();
        swarm.add_member(ExecutionId::new(), child);
        swarm.dissolve();
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
        let exec_id = ExecutionId::new();
        let now = Utc::now();
        let expires = now + chrono::Duration::minutes(5);
        let lock = ResourceLock {
            resource_id: "shared-db".to_string(),
            held_by: holder,
            execution_id: exec_id,
            acquired_at: now,
            expires_at: expires,
        };
        assert_eq!(lock.resource_id, "shared-db");
        assert_eq!(lock.held_by, holder);
        assert_eq!(lock.execution_id, exec_id);
        assert!(lock.expires_at > lock.acquired_at);
    }

    #[test]
    fn resource_lock_serde_roundtrip() {
        let holder = AgentId::new();
        let exec_id = ExecutionId::new();
        let now = Utc::now();
        let expires = now + chrono::Duration::minutes(10);
        let lock = ResourceLock {
            resource_id: "file-lock".to_string(),
            held_by: holder,
            execution_id: exec_id,
            acquired_at: now,
            expires_at: expires,
        };
        let json = serde_json::to_string(&lock).unwrap();
        let deser: ResourceLock = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.resource_id, "file-lock");
        assert_eq!(deser.held_by, holder);
        assert_eq!(deser.execution_id, exec_id);
    }

    #[test]
    fn resource_lock_equality() {
        let holder = AgentId::new();
        let exec_id = ExecutionId::new();
        let now = Utc::now();
        let expires = now + chrono::Duration::minutes(5);
        let lock1 = ResourceLock {
            resource_id: "res-1".to_string(),
            held_by: holder,
            execution_id: exec_id,
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
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tenant_id: TenantId::consumer(),
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
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tenant_id: TenantId::consumer(),
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
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tenant_id: TenantId::consumer(),
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
        let parent_exec = ExecutionId::new();
        let mut swarm = Swarm::new(parent_exec, TenantId::consumer());
        let child1 = AgentId::new();
        let child2 = AgentId::new();
        swarm.add_member(ExecutionId::new(), child1);
        swarm.add_member(ExecutionId::new(), child2);

        let json = serde_json::to_string(&swarm).unwrap();
        let deser: Swarm = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.id, swarm.id);
        assert_eq!(deser.parent_execution_id, parent_exec);
        assert_eq!(deser.status, SwarmStatus::Active);
        assert_eq!(deser.members.len(), 2);
        assert!(deser.contains(child1));
        assert!(deser.contains(child2));
        assert!(deser.dissolved_at.is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // contains_execution() — execution-based lookup.
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn contains_execution_returns_false_for_unknown() {
        let swarm = Swarm::new(ExecutionId::new(), TenantId::consumer());
        let unknown = ExecutionId::new();
        assert!(!swarm.contains_execution(&unknown));
    }

    #[test]
    fn contains_execution_returns_true_for_member() {
        let mut swarm = Swarm::new(ExecutionId::new(), TenantId::consumer());
        let exec_id = ExecutionId::new();
        swarm.add_member(exec_id, AgentId::new());
        assert!(swarm.contains_execution(&exec_id));
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // spawn_child() — cross-tenant invariant (Gap 056-2).
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn spawn_child_same_tenant_succeeds() {
        let tenant = TenantId::consumer();
        let mut swarm = Swarm::new(ExecutionId::new(), tenant.clone());
        let spec = SwarmChildSpec {
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tenant_id: tenant,
            name: "worker".to_string(),
            language: "python".to_string(),
            version: "3.11".to_string(),
            resource_limits: None,
        };
        assert!(swarm.spawn_child(spec).is_ok());
        assert_eq!(swarm.members.len(), 1);
    }

    #[test]
    fn spawn_child_cross_tenant_forbidden() {
        let swarm_tenant = TenantId::consumer();
        let child_tenant = TenantId::from_realm_slug("tenant-red").unwrap();
        let mut swarm = Swarm::new(ExecutionId::new(), swarm_tenant);
        let spec = SwarmChildSpec {
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tenant_id: child_tenant,
            name: "rogue-worker".to_string(),
            language: "python".to_string(),
            version: "3.11".to_string(),
            resource_limits: None,
        };
        let result = swarm.spawn_child(spec);
        assert!(matches!(
            result,
            Err(SpawnError::CrossTenantSpawnForbidden { .. })
        ));
        assert!(
            swarm.members.is_empty(),
            "member must not be added on cross-tenant error"
        );
    }
}
