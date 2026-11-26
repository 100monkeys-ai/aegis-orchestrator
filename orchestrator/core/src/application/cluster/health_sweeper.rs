// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Health Sweeper (BC-16, ADR-062)
//!
//! Background task that periodically scans active peers and transitions
//! stale nodes (no heartbeat within the configured threshold) to `Unhealthy`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::domain::cluster::{
    ClusterEvent, ClusterSummaryStatus, NodeClusterRepository, NodePeerStatus,
};
use crate::domain::cluster_membership::DutyFence;
use crate::infrastructure::event_bus::EventBus;

pub struct HealthSweeper {
    cluster_repo: Arc<dyn NodeClusterRepository>,
    event_bus: Arc<EventBus>,
    stale_threshold: Duration,
}

/// Default interval of the `cluster-health-sweep` duty.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// Default staleness after which a node is marked unhealthy (3x a 30 s
/// heartbeat).
pub const DEFAULT_STALE_THRESHOLD: Duration = Duration::from_secs(90);

impl HealthSweeper {
    pub fn new(
        cluster_repo: Arc<dyn NodeClusterRepository>,
        event_bus: Arc<EventBus>,
        stale_threshold: Duration,
    ) -> Self {
        Self {
            cluster_repo,
            event_bus,
            stale_threshold,
        }
    }

    pub fn stale_threshold(&self) -> Duration {
        self.stale_threshold
    }

    /// One sweep, as the `cluster-health-sweep` duty (ADR-152 D2) runs it
    /// under `fence`: a node is marked unhealthy, and the event published,
    /// only when the write lands, so a holder whose epoch was superseded
    /// marks and publishes nothing.
    pub async fn run_once(&self, fence: DutyFence) {
        if let Err(e) = self.sweep(fence).await {
            tracing::error!(error = %e, "Health sweeper failed");
        }
    }

    async fn sweep(&self, fence: DutyFence) -> anyhow::Result<()> {
        let active_peers = self
            .cluster_repo
            .list_peers_by_status(NodePeerStatus::Active)
            .await?;
        let now = Utc::now();
        let mut transitioned = 0u32;

        for peer in &active_peers {
            let staleness = now.signed_duration_since(peer.last_heartbeat_at);
            if staleness
                > chrono::Duration::from_std(self.stale_threshold)
                    .unwrap_or(chrono::Duration::seconds(90))
            {
                tracing::warn!(
                    node_id = %peer.node_id,
                    staleness_secs = staleness.num_seconds(),
                    "Marking stale node as unhealthy"
                );
                if !self
                    .cluster_repo
                    .mark_unhealthy(&peer.node_id, fence)
                    .await?
                {
                    continue;
                }
                self.event_bus
                    .publish_cluster_event(ClusterEvent::NodeUnhealthy {
                        node_id: peer.node_id,
                        last_seen: peer.last_heartbeat_at,
                        marked_at: now,
                    });
                transitioned += 1;
            }
        }

        if transitioned > 0 {
            metrics::counter!("aegis_health_sweeper_transitions_total")
                .increment(transitioned as u64);
            tracing::info!(
                count = transitioned,
                "Health sweeper transitioned stale nodes to unhealthy"
            );
        }

        // Peer count gauges by status
        let counts = self.cluster_repo.count_by_status().await?;
        let active = counts.get(&NodePeerStatus::Active).copied().unwrap_or(0);
        let draining = counts.get(&NodePeerStatus::Draining).copied().unwrap_or(0);
        let unhealthy = counts.get(&NodePeerStatus::Unhealthy).copied().unwrap_or(0);

        metrics::gauge!("aegis_cluster_peers", "status" => "active").set(active as f64);
        metrics::gauge!("aegis_cluster_peers", "status" => "draining").set(draining as f64);
        metrics::gauge!("aegis_cluster_peers", "status" => "unhealthy").set(unhealthy as f64);

        // Heartbeat freshness histogram
        let now_fresh = Utc::now();
        for peer in &active_peers {
            let age = now_fresh.signed_duration_since(peer.last_heartbeat_at);
            metrics::histogram!("aegis_cluster_heartbeat_age_seconds")
                .record(age.num_milliseconds().max(0) as f64 / 1000.0);
        }

        // Cluster summary status gauge
        let summary = ClusterSummaryStatus::from_counts(active, draining, unhealthy);
        for variant in &["healthy", "degraded", "critical"] {
            let val = match (variant, &summary) {
                (&"healthy", ClusterSummaryStatus::Healthy) => 1.0,
                (&"degraded", ClusterSummaryStatus::Degraded) => 1.0,
                (&"critical", ClusterSummaryStatus::Critical) => 1.0,
                _ => 0.0,
            };
            metrics::gauge!("aegis_cluster_health_status", "status" => variant.to_string())
                .set(val);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cluster::{NodeId, NodePeer, NodeRole, ResourceSnapshot};
    use crate::domain::cluster_membership::{ClusterDutyName, DutyEpoch};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// One stale active peer; `mark_unhealthy` lands unless the fence is
    /// superseded.
    struct OneStalePeer {
        peer: NodePeer,
        current_epoch: DutyEpoch,
        marked: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl NodeClusterRepository for OneStalePeer {
        async fn upsert_peer(&self, _: &NodePeer) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn find_peer(&self, _: &NodeId) -> anyhow::Result<Option<NodePeer>> {
            unimplemented!()
        }
        async fn list_peers_by_status(&self, _: NodePeerStatus) -> anyhow::Result<Vec<NodePeer>> {
            Ok(vec![self.peer.clone()])
        }
        async fn record_heartbeat(&self, _: &NodeId, _: ResourceSnapshot) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn mark_unhealthy(&self, _: &NodeId, fence: DutyFence) -> anyhow::Result<bool> {
            let lands = match fence {
                DutyFence::Solo => true,
                DutyFence::Leased { epoch, .. } => epoch == self.current_epoch,
            };
            if lands {
                *self.marked.lock().unwrap() += 1;
            }
            Ok(lands)
        }
        async fn start_drain(&self, _: &NodeId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn deregister(&self, _: &NodeId, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn get_config_version(&self, _: &NodeId) -> anyhow::Result<Option<String>> {
            unimplemented!()
        }
        async fn record_config_version(&self, _: &NodeId, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn list_all_peers(&self) -> anyhow::Result<Vec<NodePeer>> {
            unimplemented!()
        }
        async fn count_by_status(&self) -> anyhow::Result<HashMap<NodePeerStatus, usize>> {
            Ok(HashMap::new())
        }
    }

    fn stale_peer() -> NodePeer {
        NodePeer {
            node_id: NodeId::new(),
            role: NodeRole::Hybrid,
            public_key: vec![0; 32],
            capabilities: Default::default(),
            grpc_address: "10.0.0.2:50056".to_string(),
            status: NodePeerStatus::Active,
            last_heartbeat_at: Utc::now() - chrono::Duration::seconds(600),
            registered_at: Utc::now() - chrono::Duration::seconds(3600),
        }
    }

    fn sweeper(
        repo: Arc<OneStalePeer>,
    ) -> (
        HealthSweeper,
        crate::infrastructure::event_bus::EventReceiver,
    ) {
        let bus = Arc::new(EventBus::new(16));
        let events = bus.subscribe();
        (
            HealthSweeper::new(repo, bus, DEFAULT_STALE_THRESHOLD),
            events,
        )
    }

    #[tokio::test]
    async fn a_stale_peer_is_marked_and_announced() {
        let repo = Arc::new(OneStalePeer {
            peer: stale_peer(),
            current_epoch: DutyEpoch(4),
            marked: Mutex::new(0),
        });
        let (sweeper, mut events) = sweeper(repo.clone());
        sweeper
            .run_once(DutyFence::Leased {
                duty: ClusterDutyName::CLUSTER_HEALTH_SWEEP,
                epoch: DutyEpoch(4),
            })
            .await;
        assert_eq!(*repo.marked.lock().unwrap(), 1);
        assert!(events.try_recv().is_ok(), "NodeUnhealthy is published");
    }

    /// A holder whose epoch was superseded marks nothing and publishes
    /// nothing (ADR-152 D2.3).
    #[tokio::test]
    async fn a_superseded_holder_marks_and_announces_nothing() {
        let repo = Arc::new(OneStalePeer {
            peer: stale_peer(),
            current_epoch: DutyEpoch(5),
            marked: Mutex::new(0),
        });
        let (sweeper, mut events) = sweeper(repo.clone());
        sweeper
            .run_once(DutyFence::Leased {
                duty: ClusterDutyName::CLUSTER_HEALTH_SWEEP,
                epoch: DutyEpoch(4),
            })
            .await;
        assert_eq!(*repo.marked.lock().unwrap(), 0);
        assert!(events.try_recv().is_err(), "nothing is published");
    }
}
