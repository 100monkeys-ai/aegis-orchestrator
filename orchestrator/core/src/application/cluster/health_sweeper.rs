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
use crate::infrastructure::event_bus::EventBus;

pub struct HealthSweeper {
    cluster_repo: Arc<dyn NodeClusterRepository>,
    event_bus: Arc<EventBus>,
    stale_threshold: Duration,
    sweep_interval: Duration,
}

impl HealthSweeper {
    pub fn new(
        cluster_repo: Arc<dyn NodeClusterRepository>,
        event_bus: Arc<EventBus>,
        stale_threshold: Duration,
        sweep_interval: Duration,
    ) -> Self {
        Self {
            cluster_repo,
            event_bus,
            stale_threshold,
            sweep_interval,
        }
    }

    /// Returns the configured stale threshold.
    pub(crate) fn stale_threshold(&self) -> Duration {
        self.stale_threshold
    }

    /// Returns the configured sweep interval.
    pub(crate) fn sweep_interval(&self) -> Duration {
        self.sweep_interval
    }

    /// Default sweeper: stale after 90s (3x 30s heartbeat), sweep every 30s
    pub fn with_defaults(
        cluster_repo: Arc<dyn NodeClusterRepository>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self::new(
            cluster_repo,
            event_bus,
            Duration::from_secs(90),
            Duration::from_secs(30),
        )
    }

    /// Run the sweeper loop until shutdown signal received.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(self.sweep_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = self.sweep().await {
                        tracing::error!(error = %e, "Health sweeper failed");
                    }
                }
                _ = shutdown.changed() => {
                    tracing::info!("Health sweeper shutting down");
                    break;
                }
            }
        }
    }

    async fn sweep(&self) -> anyhow::Result<()> {
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
                self.cluster_repo.mark_unhealthy(&peer.node_id).await?;
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
    use crate::domain::cluster::{NodeId, NodePeer, RegisteredNode, ResourceSnapshot};
    use std::collections::HashMap;

    /// Stub repository that panics on every method — only used to construct
    /// a `HealthSweeper` without exercising the sweep loop.
    struct StubClusterRepo;

    #[async_trait::async_trait]
    impl NodeClusterRepository for StubClusterRepo {
        async fn upsert_peer(&self, _: &NodePeer) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn find_peer(&self, _: &NodeId) -> anyhow::Result<Option<NodePeer>> {
            unimplemented!()
        }
        async fn list_peers_by_status(&self, _: NodePeerStatus) -> anyhow::Result<Vec<NodePeer>> {
            unimplemented!()
        }
        async fn record_heartbeat(&self, _: &NodeId, _: ResourceSnapshot) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn mark_unhealthy(&self, _: &NodeId) -> anyhow::Result<()> {
            unimplemented!()
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
            unimplemented!()
        }
        async fn find_registered_node(&self, _: &NodeId) -> anyhow::Result<Option<RegisteredNode>> {
            unimplemented!()
        }
    }

    #[test]
    fn custom_thresholds_are_stored() {
        let repo = Arc::new(StubClusterRepo);
        let bus = Arc::new(EventBus::new(1));

        let sweeper =
            HealthSweeper::new(repo, bus, Duration::from_secs(120), Duration::from_secs(15));

        assert_eq!(sweeper.stale_threshold(), Duration::from_secs(120));
        assert_eq!(sweeper.sweep_interval(), Duration::from_secs(15));
    }

    #[test]
    fn with_defaults_uses_90s_stale_and_30s_sweep() {
        let repo = Arc::new(StubClusterRepo);
        let bus = Arc::new(EventBus::new(1));

        let sweeper = HealthSweeper::with_defaults(repo, bus);

        assert_eq!(sweeper.stale_threshold(), Duration::from_secs(90));
        assert_eq!(sweeper.sweep_interval(), Duration::from_secs(30));
    }
}
