// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Cortex Pattern Pruner - Background task for time-decay and consolidation
//! 
//! Implements the "sleep cycle" from ADR-018 (Weighted Cortex Memory)
//! Periodically prunes low-weight patterns that haven't been verified recently
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for cortex pruner
//! - **Related ADRs:** ADR-029: Cortex Time-Decay Parameters

use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use tokio::time::interval;
use chrono::Utc;
use tracing::{info, warn, debug};

use crate::application::CortexService;
use crate::domain::CortexEvent;
use crate::application::EventBus;

/// Configuration for the Cortex pruner
#[derive(Debug, Clone)]
pub struct CortexPrunerConfig {
    /// Minimum weight threshold - patterns below this are candidates for pruning
    pub min_weight: f64,
    
    /// Maximum age in days - patterns older than this are candidates for pruning
    pub max_age_days: i64,
    
    /// How often to run the pruner (in seconds)
    pub interval_seconds: u64,
    
    /// Whether pruning is enabled
    pub enabled: bool,
}

impl Default for CortexPrunerConfig {
    fn default() -> Self {
        Self {
            min_weight: 0.5,         // Prune patterns with weight < 0.5
            max_age_days: 90,        // Prune patterns older than 90 days
            interval_seconds: 3600,  // Run every hour
            enabled: true,
        }
    }
}

/// Cortex Pattern Pruner - Background task
pub struct CortexPruner {
    cortex_service: Arc<dyn CortexService>,
    event_bus: Arc<dyn EventBus>,
    config: CortexPrunerConfig,
    shutdown_token: tokio_util::sync::CancellationToken,
}

impl CortexPruner {
    pub fn new(
        cortex_service: Arc<dyn CortexService>,
        event_bus: Arc<dyn EventBus>,
        config: CortexPrunerConfig,
    ) -> Self {
        Self {
            cortex_service,
            event_bus,
            config,
            shutdown_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Get a handle to trigger shutdown
    pub fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown_token.clone()
    }

    /// Start the pruner background task
    /// Returns a handle that can be used to stop the task
    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    /// Run the pruner loop with graceful shutdown support
    async fn run(&self) {
        if !self.config.enabled {
            info!("Cortex pruner is disabled");
            return;
        }

        info!(
            interval_seconds = self.config.interval_seconds,
            min_weight = self.config.min_weight,
            max_age_days = self.config.max_age_days,
            "Starting Cortex pruner background task"
        );

        let mut tick = interval(Duration::from_secs(self.config.interval_seconds));

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    debug!("Running Cortex pruner cycle");

                    match self.prune_cycle().await {
                        Ok(pruned_count) => {
                            info!(
                                pruned_count,
                                "Cortex pruner cycle completed successfully"
                            );
                        }
                        Err(e) => {
                            warn!("Cortex pruner cycle failed: {}", e);
                        }
                    }
                }
                _ = self.shutdown_token.cancelled() => {
                    info!("Shutdown signal received, stopping Cortex pruner");
                    break;
                }
            }
        }

        info!("Cortex pruner background task stopped");
    }

    /// Execute a single pruning cycle
    async fn prune_cycle(&self) -> Result<usize> {
        let start_time = Utc::now();

        // Prune old and low-weight patterns
        let pruned_count = self.cortex_service
            .prune_patterns(self.config.min_weight, self.config.max_age_days)
            .await?;

        // Publish pruning event
        let event = CortexEvent::PatternsPruned {
            count: pruned_count,
            min_weight: self.config.min_weight,
            max_age_days: self.config.max_age_days,
            pruned_at: start_time,
        };

        self.event_bus.publish(event).await?;

        Ok(pruned_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CortexPattern, PatternId, ErrorSignature};
    use async_trait::async_trait;
    use uuid::Uuid;

    struct MockCortexService {
        prune_count: Arc<tokio::sync::Mutex<usize>>,
    }

    impl MockCortexService {
        fn new(prune_count: usize) -> Self {
            Self {
                prune_count: Arc::new(tokio::sync::Mutex::new(prune_count)),
            }
        }
    }

    #[async_trait]
    impl CortexService for MockCortexService {
        async fn store_pattern(
            &self,
            _execution_id: Option<Uuid>,
            _error_signature: ErrorSignature,
            _solution_code: String,
            _task_category: String,
            _embedding: Vec<f32>,
        ) -> Result<PatternId> {
            Ok(PatternId::new())
        }

        async fn search_patterns(
            &self,
            _query_embedding: Vec<f32>,
            _limit: usize,
        ) -> Result<Vec<CortexPattern>> {
            Ok(vec![])
        }

        async fn update_pattern_success(
            &self,
            _pattern_id: PatternId,
            _execution_id: Option<Uuid>,
            _success_score: f64,
        ) -> Result<()> {
            Ok(())
        }

        async fn apply_dopamine(
            &self,
            _pattern_id: PatternId,
            _execution_id: Option<Uuid>,
            _amount: f64,
        ) -> Result<()> {
            Ok(())
        }

        async fn apply_cortisol(&self, _pattern_id: PatternId, _penalty: f64) -> Result<()> {
            Ok(())
        }

        async fn get_pattern(&self, _pattern_id: PatternId) -> Result<Option<CortexPattern>> {
            Ok(None)
        }

        async fn prune_patterns(&self, _min_weight: f64, _max_age_days: i64) -> Result<usize> {
            Ok(*self.prune_count.lock().await)
        }
    }

    struct MockEventBus;

    #[async_trait]
    impl EventBus for MockEventBus {
        async fn publish(&self, _event: CortexEvent) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_pruner_configuration() {
        let config = CortexPrunerConfig::default();
        assert_eq!(config.min_weight, 0.5);
        assert_eq!(config.max_age_days, 90);
        assert_eq!(config.interval_seconds, 3600);
        assert!(config.enabled);
    }

    #[tokio::test]
    async fn test_prune_cycle() {
        let cortex = Arc::new(MockCortexService::new(42)) as Arc<dyn CortexService>;
        let event_bus = Arc::new(MockEventBus) as Arc<dyn EventBus>;
        let config = CortexPrunerConfig::default();

        let pruner = CortexPruner::new(cortex, event_bus, config);

        let result = pruner.prune_cycle().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_pruner_disabled() {
        let cortex = Arc::new(MockCortexService::new(0)) as Arc<dyn CortexService>;
        let event_bus = Arc::new(MockEventBus) as Arc<dyn EventBus>;
        let mut config = CortexPrunerConfig::default();
        config.enabled = false;

        let pruner = Arc::new(CortexPruner::new(cortex, event_bus, config));

        // Start the pruner in a background task
        let handle = pruner.start();

        // Wait a bit to ensure it doesn't do any work
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Cancel the task
        handle.abort();
    }

    #[tokio::test]
    async fn test_custom_config() {
        let mut config = CortexPrunerConfig::default();
        config.min_weight = 1.0;
        config.max_age_days = 30;
        config.interval_seconds = 60;

        assert_eq!(config.min_weight, 1.0);
        assert_eq!(config.max_age_days, 30);
        assert_eq!(config.interval_seconds, 60);
    }
}
