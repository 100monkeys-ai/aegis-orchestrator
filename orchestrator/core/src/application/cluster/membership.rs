// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Node membership: join, lease renewal, fencing (ADR-152 D1, D6)
//!
//! At start a node joins: its row in `cluster_nodes` gets a new
//! incarnation, which identifies this process, and a first lease. From then
//! on [`NodeMembership::spawn_renewal`] renews the lease every
//! `lease_renew` (default 3 s) for `lease_ttl` (default 15 s), by the
//! database clock.
//!
//! The node judges its own lease only by its own monotonic clock: a lease
//! is live locally until `lease_ttl` after the moment the last successful
//! renewal was *sent*, which is never later than the database's own expiry.
//! Its state, reported to peers by `Probe`, follows:
//!
//! - **Fenced** when a peer fenced this incarnation, when a newer process of
//!   the same node has joined (the renewal finds no row for this
//!   incarnation), or while the local lease has run out (the database has
//!   been unreachable for a whole lease; a renewal that succeeds again
//!   inside the grace makes it active again). A fenced node holds no duty.
//! - **Draining** once the node has begun to drain.
//! - **Active** otherwise.
//!
//! Fenced by a peer or superseded are final for the incarnation: it stops
//! renewing, so its lease runs out and what it held can move.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::domain::cluster_membership::{
    MemberState, NodeIncarnation, NodeJoin, NodeLeaseRepository, NodeProcess,
};

/// Default `lease_renew_secs` (ADR-152 D1.2).
pub const DEFAULT_LEASE_RENEW: Duration = Duration::from_secs(3);
/// Default `lease_ttl_secs` (ADR-152 D1.2): five renewals.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(15);

/// How often a lease is renewed, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTiming {
    pub renew_every: Duration,
    pub ttl: Duration,
}

impl Default for LeaseTiming {
    fn default() -> Self {
        Self {
            renew_every: DEFAULT_LEASE_RENEW,
            ttl: DEFAULT_LEASE_TTL,
        }
    }
}

impl LeaseTiming {
    /// The timing a configuration asks for, refused when a lease would
    /// outlive fewer than two renewals: one late renewal must not lose it.
    pub fn new(renew_every: Duration, ttl: Duration) -> anyhow::Result<Self> {
        if renew_every.is_zero() {
            anyhow::bail!("cluster.lease_renew_secs must be greater than 0");
        }
        if ttl < renew_every * 2 {
            anyhow::bail!(
                "cluster.lease_ttl_secs ({}s) must be at least twice cluster.lease_renew_secs ({}s)",
                ttl.as_secs_f64(),
                renew_every.as_secs_f64()
            );
        }
        Ok(Self { renew_every, ttl })
    }
}

/// What one renewal attempt found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewOutcome {
    Renewed,
    /// The database did not answer; the local lease lives until its
    /// deadline.
    Unreachable,
    /// A newer process of this node joined: this one holds nothing.
    Superseded,
    /// Fenced by a peer or superseded earlier; nothing was sent.
    Stopped,
}

#[derive(Debug)]
struct Inner {
    lease_deadline: Instant,
    last_renewed_at: Option<DateTime<Utc>>,
    fenced_by_peer: bool,
    superseded: bool,
    draining: bool,
}

impl Inner {
    fn state(&self, now: Instant) -> MemberState {
        if self.fenced_by_peer || self.superseded || now >= self.lease_deadline {
            MemberState::Fenced
        } else if self.draining {
            MemberState::Draining
        } else {
            MemberState::Active
        }
    }
}

/// This process's membership in the cluster.
pub struct NodeMembership {
    process: NodeProcess,
    repo: Arc<dyn NodeLeaseRepository>,
    timing: LeaseTiming,
    inner: Mutex<Inner>,
    state: watch::Sender<MemberState>,
}

impl NodeMembership {
    /// Join the cluster: one statement gives this process its incarnation
    /// and its first lease.
    pub async fn join(
        repo: Arc<dyn NodeLeaseRepository>,
        join: NodeJoin,
        timing: LeaseTiming,
    ) -> anyhow::Result<Arc<Self>> {
        let sent = Instant::now();
        let join = NodeJoin {
            lease_ttl: timing.ttl,
            ..join
        };
        let incarnation = repo.join(&join).await?;
        let (state, _) = watch::channel(MemberState::Active);
        tracing::info!(
            node_id = %join.node_id,
            incarnation = %incarnation,
            lease_ttl_secs = timing.ttl.as_secs_f64(),
            "Joined the cluster"
        );
        Ok(Arc::new(Self {
            process: NodeProcess {
                node_id: join.node_id,
                incarnation,
            },
            repo,
            timing,
            inner: Mutex::new(Inner {
                lease_deadline: sent + timing.ttl,
                last_renewed_at: Some(Utc::now()),
                fenced_by_peer: false,
                superseded: false,
                draining: false,
            }),
            state,
        }))
    }

    pub fn process(&self) -> NodeProcess {
        self.process
    }

    pub fn incarnation(&self) -> NodeIncarnation {
        self.process.incarnation
    }

    pub fn timing(&self) -> LeaseTiming {
        self.timing
    }

    /// This process's state now.
    pub fn state(&self) -> MemberState {
        let state = self.lock().state(Instant::now());
        self.publish(state);
        state
    }

    /// Follow the state as it changes (a duty runner stops when it leaves
    /// `Active`).
    pub fn subscribe(&self) -> watch::Receiver<MemberState> {
        self.state.subscribe()
    }

    /// When this process last renewed, as the database answered.
    pub fn last_renewed_at(&self) -> Option<DateTime<Utc>> {
        self.lock().last_renewed_at
    }

    /// A peer fences this node (ADR-152 D6). Only the named incarnation is
    /// fenced: `false`, and nothing changes, when this process is another.
    pub fn fence(&self, incarnation: NodeIncarnation, reason: &str) -> bool {
        if incarnation != self.process.incarnation {
            tracing::warn!(
                asked = %incarnation,
                current = %self.process.incarnation,
                reason,
                "Refused a fence for another incarnation of this node"
            );
            return false;
        }
        self.lock().fenced_by_peer = true;
        tracing::error!(incarnation = %incarnation, reason, "This node was fenced by a peer");
        self.state();
        true
    }

    /// Begin draining: the node takes no new work (ADR-152 D5.9).
    pub fn start_drain(&self) {
        self.lock().draining = true;
        self.state();
    }

    /// Send one renewal and record what it found.
    pub async fn renew_once(&self) -> RenewOutcome {
        {
            let inner = self.lock();
            if inner.fenced_by_peer || inner.superseded {
                return RenewOutcome::Stopped;
            }
        }
        let sent = Instant::now();
        let outcome = match self.repo.renew(self.process, self.timing.ttl).await {
            Ok(Ok(renewed_at)) => {
                let mut inner = self.lock();
                inner.lease_deadline = sent + self.timing.ttl;
                inner.last_renewed_at = Some(renewed_at);
                metrics::histogram!("aegis_cluster_lease_renewal_seconds")
                    .record(sent.elapsed().as_secs_f64());
                RenewOutcome::Renewed
            }
            Ok(Err(superseded)) => {
                tracing::error!(error = %superseded, "Lease renewal found a newer incarnation");
                self.lock().superseded = true;
                RenewOutcome::Superseded
            }
            Err(e) => {
                metrics::counter!("aegis_cluster_lease_renewal_failures_total").increment(1);
                tracing::warn!(error = %e, "Lease renewal failed; the database did not answer");
                RenewOutcome::Unreachable
            }
        };
        self.state();
        outcome
    }

    /// Renew every `renew_every` until this incarnation is fenced or
    /// superseded.
    pub fn spawn_renewal(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.timing.renew_every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // the join granted the first lease
            loop {
                ticker.tick().await;
                match self.renew_once().await {
                    RenewOutcome::Superseded | RenewOutcome::Stopped => break,
                    RenewOutcome::Renewed | RenewOutcome::Unreachable => {}
                }
            }
            tracing::warn!(
                incarnation = %self.process.incarnation,
                "Lease renewal stopped: this incarnation holds nothing"
            );
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn publish(&self, state: MemberState) {
        self.state.send_if_modified(|current| {
            if *current == state {
                false
            } else {
                metrics::gauge!("aegis_cluster_member_fenced").set(
                    if state == MemberState::Fenced {
                        1.0
                    } else {
                        0.0
                    },
                );
                tracing::info!(
                    from = current.as_str(),
                    to = state.as_str(),
                    "Membership state changed"
                );
                *current = state;
                true
            }
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::cluster::NodeId;
    use crate::domain::cluster_membership::{LeaseSuperseded, NodeLease};
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

    /// A lease store whose database can be made unreachable, and whose row
    /// can be taken by a newer incarnation.
    #[derive(Default)]
    pub(crate) struct FakeLeases {
        pub(crate) incarnation: AtomicI64,
        pub(crate) unreachable: AtomicBool,
    }

    #[async_trait::async_trait]
    impl NodeLeaseRepository for FakeLeases {
        async fn join(&self, _: &NodeJoin) -> anyhow::Result<NodeIncarnation> {
            Ok(NodeIncarnation(
                self.incarnation.fetch_add(1, Ordering::SeqCst) + 1,
            ))
        }
        async fn renew(
            &self,
            process: NodeProcess,
            _: Duration,
        ) -> anyhow::Result<Result<DateTime<Utc>, LeaseSuperseded>> {
            if self.unreachable.load(Ordering::SeqCst) {
                anyhow::bail!("connection refused");
            }
            if self.incarnation.load(Ordering::SeqCst) != process.incarnation.0 {
                return Ok(Err(LeaseSuperseded {
                    node_id: process.node_id,
                    incarnation: process.incarnation,
                }));
            }
            Ok(Ok(Utc::now()))
        }
        async fn leases(&self) -> anyhow::Result<Vec<NodeLease>> {
            Ok(vec![])
        }
    }

    pub(crate) fn join_request(node_id: NodeId) -> NodeJoin {
        NodeJoin {
            node_id,
            role: "hybrid".to_string(),
            public_key: vec![7; 32],
            grpc_address: "10.120.0.1:50056".to_string(),
            vrack_address: Some("10.120.0.1".to_string()),
            lease_ttl: Duration::ZERO,
            max_executions: None,
            display_name: None,
        }
    }

    fn timing() -> LeaseTiming {
        LeaseTiming::new(Duration::from_secs(3), Duration::from_secs(15)).unwrap()
    }

    #[test]
    fn a_lease_must_outlive_two_renewals() {
        assert!(LeaseTiming::new(Duration::from_secs(3), Duration::from_secs(5)).is_err());
        assert!(LeaseTiming::new(Duration::ZERO, Duration::from_secs(5)).is_err());
        assert_eq!(LeaseTiming::default().ttl, Duration::from_secs(15));
    }

    #[tokio::test(start_paused = true)]
    async fn a_node_that_cannot_renew_for_a_whole_lease_is_fenced_and_recovers_on_renewal() {
        let leases = Arc::new(FakeLeases::default());
        let me = NodeMembership::join(leases.clone(), join_request(NodeId::new()), timing())
            .await
            .unwrap();
        assert_eq!(me.state(), MemberState::Active);

        leases.unreachable.store(true, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(14)).await;
        assert_eq!(me.renew_once().await, RenewOutcome::Unreachable);
        assert_eq!(me.state(), MemberState::Active, "the lease still lives");

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(me.state(), MemberState::Fenced, "15 s without a renewal");

        leases.unreachable.store(false, Ordering::SeqCst);
        assert_eq!(me.renew_once().await, RenewOutcome::Renewed);
        assert_eq!(me.state(), MemberState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn a_superseded_incarnation_stops_for_good() {
        let leases = Arc::new(FakeLeases::default());
        let old = NodeMembership::join(leases.clone(), join_request(NodeId::new()), timing())
            .await
            .unwrap();
        // The same node starts again: incarnation 2.
        leases.join(&join_request(NodeId::new())).await.unwrap();

        assert_eq!(old.renew_once().await, RenewOutcome::Superseded);
        assert_eq!(old.state(), MemberState::Fenced);
        assert_eq!(old.renew_once().await, RenewOutcome::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn a_fence_names_its_incarnation() {
        let leases = Arc::new(FakeLeases::default());
        let me = NodeMembership::join(leases, join_request(NodeId::new()), timing())
            .await
            .unwrap();
        let mut watch = me.subscribe();

        assert!(!me.fence(NodeIncarnation(99), "test"));
        assert_eq!(me.state(), MemberState::Active);

        assert!(me.fence(me.incarnation(), "db_partition_grace_elapsed"));
        assert_eq!(me.state(), MemberState::Fenced);
        assert_eq!(me.renew_once().await, RenewOutcome::Stopped);
        watch.changed().await.unwrap();
        assert_eq!(*watch.borrow(), MemberState::Fenced);
    }

    #[tokio::test(start_paused = true)]
    async fn a_draining_node_reports_draining() {
        let leases = Arc::new(FakeLeases::default());
        let me = NodeMembership::join(leases, join_request(NodeId::new()), timing())
            .await
            .unwrap();
        me.start_drain();
        assert_eq!(me.state(), MemberState::Draining);
    }
}
