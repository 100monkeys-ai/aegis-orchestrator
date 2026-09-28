// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # gRPC health checking (`grpc.health.v1.Health`)
//!
//! Every gRPC server the orchestrator starts serves the standard health
//! service, so that `grpc_health_probe` and any other standard client can ask
//! whether the server is able to serve. A server reports `SERVING` only while
//! every dependency its role needs answers, and `NOT_SERVING` otherwise.
//!
//! A dependency is expressed as a [`ReadinessCheck`]: an async predicate that
//! answers `true` when the dependency is reachable.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ::tonic_health::pb::health_server::{Health, HealthServer};
use ::tonic_health::server::HealthReporter;
use sqlx::postgres::PgPool;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

pub use ::tonic_health::ServingStatus;
/// Re-exported so callers and tests use the same `tonic-health` as the
/// server side (its `pb` module carries the client and the status enum).
pub use tonic_health;

/// The name a generated tonic server registers under
/// (e.g. `aegis.cluster.v1.NodeClusterService`).
pub fn service_name<S: tonic::server::NamedService>(_server: &S) -> &'static str {
    S::NAME
}

/// The health of one gRPC server: its overall status (the empty service
/// name, which `grpc_health_probe` asks for when given no `-service`) and the
/// status of each service it hosts. All of them move together.
#[derive(Clone)]
pub struct GrpcHealth {
    reporter: HealthReporter,
    services: Arc<[String]>,
}

impl GrpcHealth {
    /// The health service for a server hosting `services`. The overall
    /// server and every named service start `NOT_SERVING`: nothing is
    /// reported as serving before something has checked that it can.
    pub async fn new(services: &[&str]) -> (Self, HealthServer<impl Health>) {
        let (reporter, server) = ::tonic_health::server::health_reporter();
        let health = Self {
            reporter,
            services: services.iter().map(|s| s.to_string()).collect(),
        };
        health.set(ServingStatus::NotServing).await;
        (health, server)
    }

    /// Report `status` for the overall server and every named service.
    pub async fn set(&self, status: ServingStatus) {
        self.reporter.set_service_status("", status).await;
        for service in self.services.iter() {
            self.reporter.set_service_status(service, status).await;
        }
    }

    /// Evaluate `checks` now and then every `interval`, reporting `SERVING`
    /// while all of them pass and `NOT_SERVING` otherwise. With no checks
    /// the server reports `SERVING` from the first evaluation.
    pub fn spawn_monitor(self, checks: Vec<ReadinessCheck>, interval: Duration) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            let mut reported = None;
            loop {
                ticker.tick().await;
                let status = readiness(&checks).await;
                if reported != Some(status) {
                    tracing::info!(
                        status = %status,
                        services = ?self.services,
                        "gRPC health status changed"
                    );
                    self.set(status).await;
                    reported = Some(status);
                }
            }
        })
    }
}

/// `SERVING` when every check passes, `NOT_SERVING` as soon as one fails.
pub async fn readiness(checks: &[ReadinessCheck]) -> ServingStatus {
    for check in checks {
        if !check().await {
            return ServingStatus::NotServing;
        }
    }
    ServingStatus::Serving
}

/// An async predicate answering whether one dependency of a gRPC server is
/// reachable.
pub type ReadinessCheck = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Wrap an async predicate as a [`ReadinessCheck`].
pub fn readiness_check<F, Fut>(check: F) -> ReadinessCheck
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = bool> + Send + 'static,
{
    Arc::new(move || -> Pin<Box<dyn Future<Output = bool> + Send>> { Box::pin(check()) })
}

/// PostgreSQL is reachable when `SELECT 1` completes within `timeout`.
pub fn postgres_readiness(pool: PgPool, timeout: Duration) -> ReadinessCheck {
    readiness_check(move || {
        let pool = pool.clone();
        async move {
            matches!(
                tokio::time::timeout(timeout, sqlx::query("SELECT 1").execute(&pool)).await,
                Ok(Ok(_))
            )
        }
    })
}
