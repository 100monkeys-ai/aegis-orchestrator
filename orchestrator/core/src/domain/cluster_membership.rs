// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Cluster membership and duties (BC-16, ADR-152 D1, D2)
//!
//! Every node of an active-active cluster is a peer. Two facts about the
//! cluster are decided in PostgreSQL, the one strongly consistent store:
//!
//! - **NodeLease** — a node's row in `cluster_nodes`, renewed by the
//!   database clock every few seconds under the **NodeIncarnation** of the
//!   process that renews it. The lease is the arbiter of *ownership*.
//! - **ClusterDuty** — a singleton background responsibility (an expiry
//!   sweep, the health sweep, a reconcile) held under a lease in
//!   `cluster_duties`. Its **epoch** is the fencing token: it changes every
//!   time the duty changes hands, so a write that carries a superseded epoch
//!   affects nothing.
//!
//! Node clocks are never compared: every lease expiry is computed and
//! checked by the database (`now()`), and a node judges its own lease only
//! against its own monotonic clock.

use chrono::{DateTime, Utc};
use std::fmt;
use std::time::Duration;

use crate::domain::cluster::NodeId;

/// The counter a node increments at each process start; identifies one
/// process of one node. Not the node id, not a boot time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeIncarnation(pub i64);

impl fmt::Display for NodeIncarnation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One process of one node: who holds a lease or a duty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeProcess {
    pub node_id: NodeId,
    pub incarnation: NodeIncarnation,
}

/// A node's membership state as the node itself reports it (the `Probe`
/// answer, ADR-152 D1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemberState {
    /// Renewing its lease and doing its work.
    Active,
    /// Finishing its work before it exits; takes no new work.
    Draining,
    /// Holds nothing: fenced by a peer, superseded by a newer incarnation,
    /// or unable to renew its lease within the lease's life.
    Fenced,
}

impl MemberState {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemberState::Active => "active",
            MemberState::Draining => "draining",
            MemberState::Fenced => "fenced",
        }
    }
}

/// What a node writes about itself when its process starts (ADR-152 D1.1).
#[derive(Debug, Clone)]
pub struct NodeJoin {
    pub node_id: NodeId,
    /// Lowercased role, as `cluster_nodes.role` stores it.
    pub role: String,
    /// Raw 32-byte Ed25519 public key of the node's keypair.
    pub public_key: Vec<u8>,
    /// `host:port` of this node's `NodeClusterService`.
    pub grpc_address: String,
    /// The node's address on the private network its peers probe it over.
    pub vrack_address: Option<String>,
    /// The lease the join grants, before the first renewal.
    pub lease_ttl: Duration,
}

/// A node's lease as the database sees it now.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeLease {
    pub node_id: NodeId,
    pub incarnation: NodeIncarnation,
    /// `lease_expires_at > now()` by the database clock.
    pub live: bool,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub last_renewed_at: Option<DateTime<Utc>>,
    pub vrack_address: Option<String>,
    pub grpc_address: String,
}

/// The renewal statement found no row for this node under this
/// incarnation: a newer process of the same node has joined, and this one
/// holds nothing from now on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("node {node_id} incarnation {incarnation} is superseded: a newer process of this node joined the cluster")]
pub struct LeaseSuperseded {
    pub node_id: NodeId,
    pub incarnation: NodeIncarnation,
}

/// Membership leases in `cluster_nodes` (ADR-152 D1).
#[async_trait::async_trait]
pub trait NodeLeaseRepository: Send + Sync {
    /// Insert this node's row if absent, otherwise increment its
    /// incarnation; either way grant a lease of `join.lease_ttl` and return
    /// the incarnation that identifies this process.
    async fn join(&self, join: &NodeJoin) -> anyhow::Result<NodeIncarnation>;

    /// Extend this process's lease to `now() + ttl` by the database clock.
    /// `Ok(Err(LeaseSuperseded))` when the row carries another incarnation.
    async fn renew(
        &self,
        process: NodeProcess,
        ttl: Duration,
    ) -> anyhow::Result<Result<DateTime<Utc>, LeaseSuperseded>>;

    /// Every node's lease, judged live or not by the database clock.
    async fn leases(&self) -> anyhow::Result<Vec<NodeLease>>;
}

/// The name of a singleton duty: the primary key of `cluster_duties`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClusterDutyName(&'static str);

impl ClusterDutyName {
    pub const VOLUME_TTL_CLEANUP: Self = Self("volume-ttl-cleanup");
    pub const RATE_LIMIT_COUNTER_CLEANUP: Self = Self("rate-limit-counter-cleanup");
    pub const DISCOVERY_BACKFILL: Self = Self("discovery-backfill");
    pub const TEMPORAL_SCHEDULES_RECONCILE: Self = Self("temporal-schedules-reconcile");
    pub const TOOL_APPROVAL_EXPIRY: Self = Self("tool-approval-expiry");
    pub const OPERATOR_ESCALATION_EXPIRY: Self = Self("operator-escalation-expiry");
    pub const GOAL_EXPIRY: Self = Self("goal-expiry");
    pub const CLUSTER_HEALTH_SWEEP: Self = Self("cluster-health-sweep");

    /// Every cluster duty this binary knows (ADR-152 D2.4). `execution-
    /// adoption` (D5) and `seal-nonce-gc` (D4.2) join with the phases that
    /// build what they sweep.
    pub const ALL: [Self; 8] = [
        Self::VOLUME_TTL_CLEANUP,
        Self::RATE_LIMIT_COUNTER_CLEANUP,
        Self::DISCOVERY_BACKFILL,
        Self::TEMPORAL_SCHEDULES_RECONCILE,
        Self::TOOL_APPROVAL_EXPIRY,
        Self::OPERATOR_ESCALATION_EXPIRY,
        Self::GOAL_EXPIRY,
        Self::CLUSTER_HEALTH_SWEEP,
    ];

    /// A name for a duty the catalogue above does not hold (tests).
    pub const fn custom(name: &'static str) -> Self {
        Self(name)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ClusterDutyName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// The fencing token of a duty: incremented every time the duty changes
/// hands, never when its holder renews.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DutyEpoch(pub i64);

impl fmt::Display for DutyEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What a duty's tick carries into its writes. A duty whose writes are not
/// idempotent conditions them on the epoch it was granted, so a holder that
/// stalled past its lease writes nothing (ADR-152 D2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DutyFence {
    /// This node runs every duty alone (clustering off, or no database):
    /// nothing to fence against.
    Solo,
    /// Held under a lease in `cluster_duties` at this epoch.
    Leased {
        duty: ClusterDutyName,
        epoch: DutyEpoch,
    },
}

/// Duty leases in `cluster_duties` (ADR-152 D2).
#[async_trait::async_trait]
pub trait ClusterDutyRepository: Send + Sync {
    /// Make sure the duty has its row (a new duty needs no migration).
    async fn ensure(&self, duty: ClusterDutyName) -> anyhow::Result<()>;

    /// Acquire the duty, or renew it when this process already holds it,
    /// for `ttl` by the database clock: one statement. `Some(epoch)` means
    /// this process holds the duty at that epoch; `None` that another live
    /// holder has it.
    async fn try_acquire(
        &self,
        duty: ClusterDutyName,
        holder: NodeProcess,
        ttl: Duration,
    ) -> anyhow::Result<Option<DutyEpoch>>;

    /// Give the duty up, so a peer takes it at its next attempt instead of
    /// waiting for the lease to run out. `false` when this process did not
    /// hold it at that epoch.
    async fn release(
        &self,
        duty: ClusterDutyName,
        holder: NodeProcess,
        epoch: DutyEpoch,
    ) -> anyhow::Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_duty_name_is_unique_and_kebab_case() {
        let mut seen = std::collections::HashSet::new();
        for duty in ClusterDutyName::ALL {
            assert!(seen.insert(duty.as_str()), "{duty} is listed twice");
            assert!(
                duty.as_str()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "{duty} is not kebab-case"
            );
        }
    }

    #[test]
    fn the_duty_names_are_the_records() {
        // ADR-152 D2.4 names these; a rename is a migration of the rows
        // already stored under the old name.
        let names: Vec<&str> = ClusterDutyName::ALL.iter().map(|d| d.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "volume-ttl-cleanup",
                "rate-limit-counter-cleanup",
                "discovery-backfill",
                "temporal-schedules-reconcile",
                "tool-approval-expiry",
                "operator-escalation-expiry",
                "goal-expiry",
                "cluster-health-sweep",
            ]
        );
    }
}

/// This process's claim on the executions it runs (ADR-152 D5.1, D5.2).
///
/// Shared by the execution repository and the execution service. Outside a
/// cluster it stays empty and the repository writes as one process always
/// has. On a cluster node the daemon sets the [`NodeProcess`] once it has
/// joined, before any work is accepted: from then on every execution this
/// process creates is claimed for it in the creating `INSERT`, and every
/// later write applies only while this process still owns the row at epoch
/// 0. A write that affects no row means the execution was adopted from
/// under this process (it was fenced); the repository reports it here, and
/// the execution service stops that execution's supervisor, which removes
/// its container.
#[derive(Default)]
pub struct ExecutionOwnership {
    process: std::sync::OnceLock<NodeProcess>,
    on_fenced:
        std::sync::OnceLock<Box<dyn Fn(crate::domain::execution::ExecutionId) + Send + Sync>>,
}

impl ExecutionOwnership {
    /// Claim executions for `process` from now on; once.
    pub fn claim_for(&self, process: NodeProcess) -> anyhow::Result<()> {
        self.process.set(process).map_err(|_| {
            anyhow::anyhow!("this process already claims executions as a cluster node")
        })
    }

    /// The process executions are claimed for, on a cluster node.
    pub fn process(&self) -> Option<&NodeProcess> {
        self.process.get()
    }

    /// What to do when a write finds an execution fenced off this process;
    /// once.
    pub fn on_fenced(
        &self,
        f: impl Fn(crate::domain::execution::ExecutionId) + Send + Sync + 'static,
    ) -> anyhow::Result<()> {
        self.on_fenced
            .set(Box::new(f))
            .map_err(|_| anyhow::anyhow!("the fenced-execution handler is already set"))
    }

    /// A write found `id` fenced off this process.
    pub fn fenced(&self, id: crate::domain::execution::ExecutionId) {
        tracing::warn!(execution_id = %id, "This node no longer owns the execution (fenced): stopping it");
        metrics::counter!("aegis_cluster_executions_fenced_total").increment(1);
        if let Some(f) = self.on_fenced.get() {
            f(id);
        }
    }
}

impl fmt::Debug for ExecutionOwnership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionOwnership")
            .field("process", &self.process.get())
            .finish_non_exhaustive()
    }
}
