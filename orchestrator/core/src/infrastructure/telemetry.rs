// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Telemetry and metrics infrastructure
//!
//! # Code Quality Principles
//!
//! - Keep metric names stable and low-cardinality.
//! - Avoid request-specific or identity-specific labels on production metrics.
//! - Prefer a single initialization path for exporter setup and node metadata.

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use std::net::SocketAddr;

/// Initializes the Prometheus metrics exporter.
///
/// Binds an HTTP listener to the specified port and registers initial node metadata.
pub fn init_metrics(
    port: u16,
    _node_id: &str,
    _node_name: &str,
    region: Option<&str>,
    version: &str,
) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;

    PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets_for_metric(
            Matcher::Suffix("_duration_seconds".to_string()),
            &[
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
            ],
        )?
        .set_buckets_for_metric(
            Matcher::Full("aegis_iteration_count".to_string()),
            &[1.0, 2.0, 3.0, 5.0, 8.0, 10.0],
        )?
        .install()?;

    // Register node metadata gauge
    metrics::gauge!(
        "aegis_node_info",
        "region" => region.unwrap_or("unknown").to_string(),
        "version" => version.to_string()
    )
    .set(1.0);

    tracing::info!("Metrics exporter listening on {}", addr);
    Ok(())
}
