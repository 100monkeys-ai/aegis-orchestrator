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

use sqlx::postgres::PgPool;

/// Re-exported so callers and tests use the same `tonic-health` as the
/// server side (its `pb` module carries the client and the status enum).
pub use tonic_health;

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
