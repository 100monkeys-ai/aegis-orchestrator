// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Health Sweeper (BC-16, ADR-062)
//!
//! Background task that periodically scans active peers and transitions
//! stale nodes (no heartbeat within the configured threshold) to `Unhealthy`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::domain::cluster::{ClusterEvent, NodeClusterRepository, NodePeerStatus};
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
        Ok(())
    }
}
