// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SensorService — Always-On Stimulus Listeners (BC-8 — ADR-021)
//!
//! The `SensorService` manages a set of active [`Sensor`]s, each of which
//! listens for external stimuli from a configured source and forwards them to
//! [`StimulusService`] for routing.
//!
//! ## Phase 1 Sensors
//!
//! - [`StdinSensor`]: reads newline-delimited JSON from standard input (useful
//!   for local testing and CLI-driven workflows)
//!
//! ## Phase 2 Sensors (planned)
//!
//! - `HttpPollSensor`: polls an HTTP endpoint at a configured interval
//! - `ChannelSubscriber`: subscribes to an internal broadcast channel
//!
//! ## Shutdown
//!
//! `SensorService::shutdown()` signals all running sensors via a shared
//! [`tokio_util::sync::CancellationToken`].

use async_trait::async_trait;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::application::stimulus::StimulusService;
use crate::domain::stimulus::{Stimulus, StimulusSource};

// ──────────────────────────────────────────────────────────────────────────────
// Sensor trait
// ──────────────────────────────────────────────────────────────────────────────

/// A long-running component that listens for stimuli from a single source.
#[async_trait]
pub trait Sensor: Send + Sync + 'static {
    /// Descriptive name used in logs (e.g. `"stdin"`, `"http-poll:https://api.example.com/feed"`).
    fn name(&self) -> &str;

    /// Block until the next stimulus arrives or the token is cancelled.
    ///
    /// Returns `None` if the sensor has been shut down (cancellation received).
    async fn next_stimulus(&mut self, cancel: &CancellationToken) -> Option<Stimulus>;
}

// ──────────────────────────────────────────────────────────────────────────────
// StdinSensor
// ──────────────────────────────────────────────────────────────────────────────

/// Reads newline-delimited JSON lines from stdin.
///
/// Each line must be a UTF-8 JSON string. Non-JSON lines are treated as plain-text
/// content. Empty lines are skipped. Useful for local development and CLI-driven
/// workflows.
pub struct StdinSensor {
    source_name: String,
}

impl StdinSensor {
    pub fn new(source_name: impl Into<String>) -> Self {
        Self { source_name: source_name.into() }
    }
}

#[async_trait]
impl Sensor for StdinSensor {
    fn name(&self) -> &str {
        &self.source_name
    }

    async fn next_stimulus(&mut self, cancel: &CancellationToken) -> Option<Stimulus> {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        loop {
            tokio::select! {
                result = reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => {
                            // EOF
                            info!(sensor = "stdin", "EOF on stdin; sensor shutting down");
                            return None;
                        }
                        Ok(_) => {
                            let trimmed = line.trim().to_string();
                            line.clear();
                            if trimmed.is_empty() {
                                continue;
                            }
                            return Some(
                                Stimulus::new(
                                    StimulusSource::Stdin,
                                    trimmed,
                                )
                            );
                        }
                        Err(e) => {
                            error!(sensor = "stdin", error = %e, "Error reading stdin");
                            return None;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    return None;
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// SensorService
// ──────────────────────────────────────────────────────────────────────────────

/// Manages a collection of active sensors and routes their stimuli.
///
/// Call [`SensorService::start`] to spawn all sensor loops as background tasks.
/// Call [`SensorService::shutdown`] to cancel all running sensors.
pub struct SensorService {
    sensors: Vec<Box<dyn Sensor>>,
    stimulus_service: Arc<dyn StimulusService>,
    cancel: CancellationToken,
}

impl SensorService {
    pub fn new(stimulus_service: Arc<dyn StimulusService>) -> Self {
        Self {
            sensors: Vec::new(),
            stimulus_service,
            cancel: CancellationToken::new(),
        }
    }

    /// Register a sensor that will be started with [`start`].
    pub fn add_sensor(mut self, sensor: impl Sensor + 'static) -> Self {
        self.sensors.push(Box::new(sensor));
        self
    }

    /// Spawn background tasks for all registered sensors.
    ///
    /// Returns a [`JoinHandle`] that completes when all sensor loops exit
    /// (either via shutdown or natural EOF).
    pub fn start(mut self) -> JoinHandle<()> {
        let cancel = self.cancel.clone();
        let stimulus_service = self.stimulus_service.clone();
        let sensors = std::mem::take(&mut self.sensors);

        tokio::spawn(async move {
            let mut handles: Vec<JoinHandle<()>> = sensors
                .into_iter()
                .map(|sensor| {
                    let cancel_clone = cancel.clone();
                    let service_clone = stimulus_service.clone();
                    spawn_sensor_loop(sensor, service_clone, cancel_clone)
                })
                .collect();

            // Wait for all sensor loops to complete
            for handle in handles.drain(..) {
                let _ = handle.await;
            }
        })
    }

    /// Signal all sensor loops to stop.
    pub fn shutdown(&self) {
        info!("SensorService: initiating shutdown of all sensors");
        self.cancel.cancel();
    }
}

/// Spawn an async loop for a single sensor.
fn spawn_sensor_loop(
    mut sensor: Box<dyn Sensor>,
    stimulus_service: Arc<dyn StimulusService>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let sensor_name = sensor.name().to_string();
        info!(sensor = %sensor_name, "Sensor loop started");

        loop {
            match sensor.next_stimulus(&cancel).await {
                None => {
                    // Cancelled or EOF
                    info!(sensor = %sensor_name, "Sensor loop exiting");
                    break;
                }
                Some(stimulus) => {
                    let stimulus_id = stimulus.id;
                    match stimulus_service.ingest(stimulus).await {
                        Ok(resp) => {
                            info!(
                                sensor = %sensor_name,
                                stimulus_id = %stimulus_id,
                                workflow_execution_id = %resp.workflow_execution_id,
                                "Sensor stimulus routed to workflow"
                            );
                        }
                        Err(e) => {
                            error!(
                                sensor = %sensor_name,
                                stimulus_id = %stimulus_id,
                                error = %e,
                                "Sensor stimulus routing failed"
                            );
                        }
                    }
                }
            }
        }
    })
}
