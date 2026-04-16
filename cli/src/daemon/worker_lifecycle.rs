// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Worker node lifecycle: attestation, registration, heartbeat loop, and graceful shutdown.
//!
//! Spawned as a background task when cluster mode is enabled and the node's
//! `spec.cluster.role` is `Worker` or `Hybrid`.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Drives the worker-side cluster protocol (ADR-059) as a
//!   long-running background task within the daemon process.
//!
//! # Lifecycle Stages
//!
//! 1. **Connect** — Establish a gRPC channel to the controller endpoint.
//! 2. **Attest** — Perform the two-step Ed25519 challenge handshake; receive
//!    a `NodeSecurityToken` JWT.
//! 3. **Register** — Advertise `NodeCapabilityAdvertisement` to the controller.
//! 4. **Heartbeat loop** — Periodically send status; process any pending
//!    `NodeCommand`s returned in the response.
//! 5. **Deregister** — Gracefully leave the cluster on shutdown.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tracing;

use aegis_orchestrator_core::domain::cluster::NodeId;
use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    node_command::Command, NodeCapabilities, NodeCommand,
};
use aegis_orchestrator_core::infrastructure::cluster::NodeClusterClient;

/// Manages the full worker-side cluster lifecycle.
///
/// Constructed by the daemon server and spawned as a `tokio::spawn` background
/// task. The caller provides a `tokio::sync::watch::Receiver<bool>` that
/// signals graceful shutdown when the value changes.
pub struct WorkerLifecycle {
    client: NodeClusterClient,
    node_id: NodeId,
    role: i32,
    capabilities: NodeCapabilities,
    grpc_address: String,
    heartbeat_interval: Duration,
    token_refresh_margin: Duration,
    signing_key: Arc<ed25519_dalek::SigningKey>,
}

impl WorkerLifecycle {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: NodeClusterClient,
        node_id: NodeId,
        role: i32,
        capabilities: NodeCapabilities,
        grpc_address: String,
        heartbeat_interval: Duration,
        token_refresh_margin: Duration,
        signing_key: Arc<ed25519_dalek::SigningKey>,
    ) -> Self {
        Self {
            client,
            node_id,
            role,
            capabilities,
            grpc_address,
            heartbeat_interval,
            token_refresh_margin,
            signing_key,
        }
    }

    /// Run the full worker lifecycle until shutdown is signalled.
    ///
    /// This method consumes `self` because the lifecycle owns the gRPC client
    /// and should not be restarted without re-attestation.
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        // Step 1: Connect to the controller
        tracing::info!("Worker connecting to cluster controller");
        self.client
            .connect()
            .await
            .context("Failed to connect to cluster controller")?;

        // Step 2: Attest and challenge (obtain NodeSecurityToken JWT)
        let public_key = self.signing_key.verifying_key().to_bytes().to_vec();
        tracing::info!(node_id = %self.node_id, "Worker performing attestation handshake");
        self.client
            .attest_and_challenge(
                self.role,
                public_key,
                self.capabilities.clone(),
                self.grpc_address.clone(),
            )
            .await
            .context("Attestation handshake failed")?;
        tracing::info!(node_id = %self.node_id, "Worker attestation succeeded");

        // Step 3: Register capabilities with the controller
        tracing::info!(node_id = %self.node_id, "Worker registering with controller");
        let cluster_id = self
            .client
            .register(self.capabilities.clone(), self.grpc_address.clone())
            .await
            .context("RegisterNode RPC failed")?;
        tracing::info!(
            node_id = %self.node_id,
            cluster_id = %cluster_id,
            "Worker registered successfully"
        );

        // Step 4: Enter the heartbeat loop
        let mut interval = tokio::time::interval(self.heartbeat_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Check if token needs refresh (ADR-059 §3.2)
                    if let Some(expires_at) = self.client.token_expires_at().await {
                        let margin = chrono::Duration::from_std(self.token_refresh_margin)
                            .unwrap_or_else(|_| chrono::Duration::minutes(5));
                        let refresh_at = expires_at - margin;
                        if Utc::now() >= refresh_at {
                            tracing::info!(
                                node_id = %self.node_id,
                                expires_at = %expires_at,
                                "Token approaching expiry, re-attesting"
                            );
                            let public_key = self.signing_key.verifying_key().to_bytes().to_vec();
                            match self.client.attest_and_challenge(
                                self.role,
                                public_key,
                                self.capabilities.clone(),
                                self.grpc_address.clone(),
                            ).await {
                                Ok(_) => tracing::info!(node_id = %self.node_id, "Token refreshed successfully"),
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    node_id = %self.node_id,
                                    "Token refresh failed, will retry on next interval"
                                ),
                            }
                        }
                    }

                    match self.client.heartbeat(0.0, 0).await {
                        Ok(commands) => {
                            for cmd in commands {
                                self.process_command(cmd).await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                node_id = %self.node_id,
                                "Heartbeat failed, will retry on next interval"
                            );
                        }
                    }
                }
                _ = shutdown.changed() => {
                    tracing::info!(
                        node_id = %self.node_id,
                        "Worker lifecycle received shutdown signal"
                    );
                    break;
                }
            }
        }

        // Step 5: Graceful deregistration
        tracing::info!(node_id = %self.node_id, "Worker deregistering from cluster");
        if let Err(e) = self
            .client
            .deregister("graceful shutdown".to_string())
            .await
        {
            tracing::warn!(
                error = %e,
                node_id = %self.node_id,
                "Failed to deregister from cluster"
            );
        }

        Ok(())
    }

    /// Handle commands received from the controller via heartbeat responses.
    async fn process_command(&mut self, command: NodeCommand) {
        match command.command {
            Some(Command::Drain(drain_cmd)) => {
                tracing::info!(
                    node_id = %self.node_id,
                    drain = drain_cmd.drain,
                    "Received Drain command from controller"
                );
                // TODO: Signal the daemon to stop accepting new executions.
            }
            Some(Command::PushConfig(config_cmd)) => {
                tracing::info!(
                    node_id = %self.node_id,
                    config_version = %config_cmd.config_version,
                    "Received PushConfig command from controller"
                );
                // TODO: Apply the pushed configuration delta and acknowledge
                // via SyncConfig RPC.
            }
            Some(Command::Shutdown(shutdown_cmd)) => {
                tracing::info!(
                    node_id = %self.node_id,
                    reason = %shutdown_cmd.reason,
                    "Received Shutdown command from controller"
                );
                // TODO: Initiate graceful process shutdown after draining.
            }
            None => {
                tracing::debug!(
                    node_id = %self.node_id,
                    "Received empty NodeCommand (no inner command set)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::cluster::NodeId;
    use aegis_orchestrator_core::infrastructure::cluster::NodeClusterClient;
    use ed25519_dalek::SigningKey;
    use std::time::Duration;
    use uuid::Uuid;

    /// Regression test for gap 059-14: token_refresh_margin must be stored as a
    /// live (non-underscore-prefixed) field so the heartbeat loop can reference it.
    /// Previously the field was `_token_refresh_margin` which silently discarded
    /// the configured margin.
    #[test]
    fn token_refresh_margin_is_stored_and_accessible() {
        let signing_key = Arc::new(SigningKey::generate(&mut rand_core::OsRng));
        let node_id = NodeId(Uuid::new_v4());
        let client = NodeClusterClient::new(
            "http://localhost:50051".to_string(),
            signing_key.clone(),
            node_id,
        );
        let margin = Duration::from_secs(300);

        let lifecycle = WorkerLifecycle::new(
            client,
            node_id,
            1, // role
            NodeCapabilities::default(),
            "127.0.0.1:50052".to_string(),
            Duration::from_secs(30),
            margin,
            signing_key,
        );

        // The field is accessible without an underscore prefix — this would fail
        // to compile if the field were still named `_token_refresh_margin`.
        assert_eq!(lifecycle.token_refresh_margin, Duration::from_secs(300));
    }
}
