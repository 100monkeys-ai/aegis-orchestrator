// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Peer probing over vRack (ADR-152 D1.3, D1.4)
//!
//! Every second each node probes every peer it knows from `cluster_nodes`
//! with the `Probe` RPC over the private network, and counts each peer's
//! consecutive failures. The answer of a peer that does answer carries its
//! incarnation, its state and its last renewal as it saw it.
//!
//! The probe is the arbiter of *death*, and only together with the lease:
//! a peer is a death suspect here when it has failed
//! `probe_failures_to_suspect` (default 5) consecutive probes **and** its
//! lease has run out by the database clock. A peer whose lease ran out but
//! which answers is DB-partitioned, not dead. A PlanetScale fail-over
//! expires every lease at once and leaves every probe green, so nobody is a
//! suspect; an isolated vRack link leaves leases green, so nobody is a
//! suspect either. What is done with a suspect (adoption, D5.7) is not this
//! module's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::application::cluster::membership::NodeMembership;
use crate::domain::cluster::NodeId;
use crate::domain::cluster_membership::{
    MemberState, NodeIncarnation, NodeLease, NodeLeaseRepository, NodeProcess,
};

/// Default probe interval (ADR-152 D1.3).
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Default `probe_failures_to_suspect` (ADR-152 D1.4).
pub const DEFAULT_PROBE_FAILURES_TO_SUSPECT: u32 = 5;
/// The peer list is re-read from `cluster_nodes` every this many rounds,
/// so the database is not on the probe's per-second path.
const ROUNDS_PER_PEER_REFRESH: u32 = 10;

/// What a peer answered.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeAnswer {
    pub node_id: NodeId,
    pub incarnation: NodeIncarnation,
    pub state: MemberState,
    pub last_renewed_at: Option<DateTime<Utc>>,
}

/// What a peer answered a fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FenceAnswer {
    pub fenced: bool,
    pub incarnation: NodeIncarnation,
}

/// The wire to a peer's `NodeClusterService`.
#[async_trait::async_trait]
pub trait PeerTransport: Send + Sync {
    /// Probe the peer at `address` (`host:port`) as `from`.
    async fn probe(&self, address: &str, from: NodeProcess) -> anyhow::Result<ProbeAnswer>;

    /// Fence `target` at `address`.
    async fn fence(
        &self,
        address: &str,
        target: NodeProcess,
        reason: &str,
    ) -> anyhow::Result<FenceAnswer>;
}

/// This node's view of one peer.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerView {
    pub address: String,
    pub consecutive_failures: u32,
    pub last_answer: Option<ProbeAnswer>,
    /// The peer's lease by the database clock, as last read.
    pub lease_live: bool,
}

/// Probes the peers and keeps the view.
pub struct PeerProber {
    membership: Arc<NodeMembership>,
    leases: Arc<dyn NodeLeaseRepository>,
    transport: Arc<dyn PeerTransport>,
    interval: Duration,
    failures_to_suspect: u32,
    view: Mutex<HashMap<NodeId, PeerView>>,
}

impl PeerProber {
    pub fn new(
        membership: Arc<NodeMembership>,
        leases: Arc<dyn NodeLeaseRepository>,
        transport: Arc<dyn PeerTransport>,
        interval: Duration,
        failures_to_suspect: u32,
    ) -> Self {
        Self {
            membership,
            leases,
            transport,
            interval,
            failures_to_suspect: failures_to_suspect.max(1),
            view: Mutex::new(HashMap::new()),
        }
    }

    /// Re-read the peers from `cluster_nodes`: new peers join the view with
    /// no failures, departed ones leave it, and every lease is refreshed.
    pub async fn refresh_peers(&self) -> anyhow::Result<()> {
        let me = self.membership.process().node_id;
        let leases: Vec<NodeLease> = self.leases.leases().await?;
        let mut view = self.lock();
        view.retain(|id, _| leases.iter().any(|l| l.node_id == *id));
        for lease in leases.into_iter().filter(|l| l.node_id != me) {
            let entry = view.entry(lease.node_id).or_insert_with(|| PeerView {
                address: lease.grpc_address.clone(),
                consecutive_failures: 0,
                last_answer: None,
                lease_live: lease.live,
            });
            entry.address = lease.grpc_address;
            entry.lease_live = lease.live;
        }
        Ok(())
    }

    /// Probe every peer once, at once, each bounded by the probe interval.
    pub async fn probe_round(&self) {
        let me = self.membership.process();
        let peers: Vec<(NodeId, String)> = self
            .lock()
            .iter()
            .map(|(id, v)| (*id, v.address.clone()))
            .collect();
        let probes = peers.into_iter().map(|(id, address)| {
            let transport = self.transport.clone();
            let interval = self.interval;
            async move {
                let answer =
                    match tokio::time::timeout(interval, transport.probe(&address, me)).await {
                        Ok(Ok(answer)) if answer.node_id == id => Ok(answer),
                        Ok(Ok(answer)) => Err(anyhow::anyhow!(
                            "{address} answered as node {}, not {id}",
                            answer.node_id
                        )),
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(anyhow::anyhow!("no answer within {interval:?}")),
                    };
                (id, answer)
            }
        });
        let results = futures::future::join_all(probes).await;
        let mut view = self.lock();
        for (id, result) in results {
            let Some(peer) = view.get_mut(&id) else {
                continue;
            };
            match result {
                Ok(answer) => {
                    peer.consecutive_failures = 0;
                    peer.last_answer = Some(answer);
                }
                Err(e) => {
                    peer.consecutive_failures = peer.consecutive_failures.saturating_add(1);
                    metrics::counter!("aegis_cluster_probe_failures_total").increment(1);
                    if peer.consecutive_failures == self.failures_to_suspect {
                        tracing::warn!(peer = %id, error = %e, failures = peer.consecutive_failures, "Peer stopped answering probes");
                    }
                }
            }
        }
    }

    /// The peers this node would judge dead: their lease has run out by the
    /// database clock and they have failed enough consecutive probes.
    pub fn suspects(&self) -> Vec<NodeId> {
        let mut suspects: Vec<NodeId> = self
            .lock()
            .iter()
            .filter(|(_, v)| !v.lease_live && v.consecutive_failures >= self.failures_to_suspect)
            .map(|(id, _)| *id)
            .collect();
        suspects.sort_by_key(|id| id.0);
        suspects
    }

    /// The peers whose lease has run out but which still answer: cut off
    /// from the database, alive on vRack (ADR-152 D6).
    pub fn db_partitioned(&self) -> Vec<NodeId> {
        let mut found: Vec<NodeId> = self
            .lock()
            .iter()
            .filter(|(_, v)| {
                !v.lease_live && v.consecutive_failures == 0 && v.last_answer.is_some()
            })
            .map(|(id, _)| *id)
            .collect();
        found.sort_by_key(|id| id.0);
        found
    }

    pub fn view(&self) -> HashMap<NodeId, PeerView> {
        self.lock().clone()
    }

    /// Probe every interval for the life of the process, re-reading the
    /// peers every ten rounds.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut round = 0u32;
            loop {
                ticker.tick().await;
                if round.is_multiple_of(ROUNDS_PER_PEER_REFRESH) {
                    if let Err(e) = self.refresh_peers().await {
                        tracing::warn!(error = %e, "Could not re-read the cluster's peers");
                    }
                }
                round = round.wrapping_add(1);
                self.probe_round().await;
            }
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<NodeId, PeerView>> {
        self.view.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::cluster::membership::tests::{join_request, FakeLeases};
    use crate::application::cluster::membership::LeaseTiming;
    use crate::domain::cluster_membership::{LeaseSuperseded, NodeJoin};

    /// Peers with leases as the test sets them.
    struct Leases(Mutex<Vec<NodeLease>>);

    #[async_trait::async_trait]
    impl NodeLeaseRepository for Leases {
        async fn join(&self, _: &NodeJoin) -> anyhow::Result<NodeIncarnation> {
            unreachable!()
        }
        async fn renew(
            &self,
            _: NodeProcess,
            _: Duration,
        ) -> anyhow::Result<Result<DateTime<Utc>, LeaseSuperseded>> {
            unreachable!()
        }
        async fn leases(&self) -> anyhow::Result<Vec<NodeLease>> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    /// Peers that answer unless they are down.
    struct Wire {
        down: Mutex<Vec<String>>,
        ids: HashMap<String, NodeId>,
    }

    #[async_trait::async_trait]
    impl PeerTransport for Wire {
        async fn probe(&self, address: &str, _: NodeProcess) -> anyhow::Result<ProbeAnswer> {
            if self.down.lock().unwrap().iter().any(|a| a == address) {
                anyhow::bail!("connection refused");
            }
            Ok(ProbeAnswer {
                node_id: self.ids[address],
                incarnation: NodeIncarnation(1),
                state: MemberState::Active,
                last_renewed_at: None,
            })
        }
        async fn fence(&self, _: &str, t: NodeProcess, _: &str) -> anyhow::Result<FenceAnswer> {
            Ok(FenceAnswer {
                fenced: true,
                incarnation: t.incarnation,
            })
        }
    }

    fn lease(id: NodeId, address: &str, live: bool) -> NodeLease {
        NodeLease {
            node_id: id,
            incarnation: NodeIncarnation(1),
            live,
            lease_expires_at: None,
            last_renewed_at: None,
            vrack_address: None,
            grpc_address: address.to_string(),
        }
    }

    async fn setup() -> (PeerProber, Arc<Leases>, Arc<Wire>, NodeId, NodeId) {
        let me = NodeMembership::join(
            Arc::new(FakeLeases::default()),
            join_request(NodeId::new()),
            LeaseTiming::default(),
        )
        .await
        .unwrap();
        let (b, c) = (NodeId::new(), NodeId::new());
        let leases = Arc::new(Leases(Mutex::new(vec![
            lease(me.process().node_id, "a:50056", true),
            lease(b, "b:50056", true),
            lease(c, "c:50056", true),
        ])));
        let wire = Arc::new(Wire {
            down: Mutex::new(vec![]),
            ids: HashMap::from([("b:50056".to_string(), b), ("c:50056".to_string(), c)]),
        });
        let prober = PeerProber::new(
            me,
            leases.clone(),
            wire.clone(),
            DEFAULT_PROBE_INTERVAL,
            DEFAULT_PROBE_FAILURES_TO_SUSPECT,
        );
        prober.refresh_peers().await.unwrap();
        (prober, leases, wire, b, c)
    }

    #[tokio::test]
    async fn a_node_never_probes_itself() {
        let (prober, ..) = setup().await;
        assert_eq!(prober.view().len(), 2);
    }

    /// Death needs both signals: probes failing alone, or a lease expired
    /// alone, suspects nobody.
    #[tokio::test]
    async fn a_peer_is_a_suspect_only_when_its_lease_expired_and_its_probes_fail() {
        let (prober, leases, wire, b, _) = setup().await;
        wire.down.lock().unwrap().push("b:50056".to_string());
        for _ in 0..DEFAULT_PROBE_FAILURES_TO_SUSPECT {
            prober.probe_round().await;
        }
        assert!(
            prober.suspects().is_empty(),
            "b's lease is live: a vRack partition"
        );

        leases.0.lock().unwrap()[1].live = false;
        prober.refresh_peers().await.unwrap();
        assert_eq!(prober.suspects(), vec![b]);

        wire.down.lock().unwrap().clear();
        prober.probe_round().await;
        assert!(prober.suspects().is_empty(), "one answer clears the count");
        assert_eq!(
            prober.db_partitioned(),
            vec![b],
            "alive on vRack, cut off from the database"
        );
    }

    /// Every lease expires at once (a database fail-over) while every
    /// probe answers: nobody is a suspect.
    #[tokio::test]
    async fn a_database_fail_over_suspects_nobody() {
        let (prober, leases, ..) = setup().await;
        for l in leases.0.lock().unwrap().iter_mut() {
            l.live = false;
        }
        prober.refresh_peers().await.unwrap();
        for _ in 0..10 {
            prober.probe_round().await;
        }
        assert!(prober.suspects().is_empty());
    }

    #[tokio::test]
    async fn a_departed_peer_leaves_the_view() {
        let (prober, leases, _, b, c) = setup().await;
        leases.0.lock().unwrap().retain(|l| l.node_id != c);
        prober.refresh_peers().await.unwrap();
        assert_eq!(prober.view().keys().copied().collect::<Vec<_>>(), vec![b]);
    }
}
