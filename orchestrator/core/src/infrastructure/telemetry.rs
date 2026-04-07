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
    node_id: &str,
    node_name: &str,
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
        "node_id" => node_id.to_string(),
        "name" => node_name.to_string(),
        "region" => region.unwrap_or("unknown").to_string(),
        "version" => version.to_string()
    )
    .set(1.0);

    // Register intent pipeline metric descriptors (ADR-087 §Observability)
    register_intent_pipeline_metrics();

    tracing::info!("Metrics exporter listening on {}", addr);
    Ok(())
}

/// Registers the four intent execution pipeline metric descriptors defined in
/// ADR-087 §Observability. Calling this pre-populates the Prometheus registry
/// so the metrics appear at scrape time even before the first pipeline run.
///
/// This function is idempotent — subsequent calls are no-ops because the
/// `metrics` crate deduplicates registrations by name.
pub fn register_intent_pipeline_metrics() {
    // Counter: pipeline start events, labelled by tenant and language.
    metrics::counter!(
        "zaru_intent_pipeline_starts_total",
        "tenant_id" => "",
        "language" => ""
    )
    .absolute(0);

    // Histogram: end-to-end pipeline duration, labelled by tenant, language, and outcome.
    // Buckets are picked up from the `_duration_seconds` suffix matcher set in `init_metrics`.
    metrics::histogram!(
        "zaru_intent_pipeline_duration_seconds",
        "tenant_id" => "",
        "language" => "",
        "outcome" => ""
    )
    .record(0.0);

    // Counter: Cortex / agent cache hits, labelled by tenant.
    metrics::counter!(
        "zaru_intent_pipeline_agent_cache_hits_total",
        "tenant_id" => ""
    )
    .absolute(0);

    // Counter: container exit codes observed by the step runner.
    metrics::counter!(
        "zaru_intent_pipeline_container_exit_code_total",
        "exit_code" => ""
    )
    .absolute(0);
}

#[cfg(test)]
mod tests {
    use super::register_intent_pipeline_metrics;

    /// Regression: all four ADR-087 metric descriptors must register without panicking.
    /// The `metrics` crate panics if a metric is registered with conflicting types.
    #[test]
    fn test_intent_pipeline_metrics_register_without_panic() {
        // Calling twice proves idempotency (second call must not panic either).
        register_intent_pipeline_metrics();
        register_intent_pipeline_metrics();
    }

    /// Regression: counter increments with valid label values must not panic.
    #[test]
    fn test_intent_pipeline_counter_increment_valid_labels() {
        metrics::counter!(
            "zaru_intent_pipeline_starts_total",
            "tenant_id" => "tenant-abc",
            "language" => "python"
        )
        .increment(1);

        metrics::counter!(
            "zaru_intent_pipeline_agent_cache_hits_total",
            "tenant_id" => "tenant-abc"
        )
        .increment(1);

        metrics::counter!(
            "zaru_intent_pipeline_container_exit_code_total",
            "exit_code" => "0"
        )
        .increment(1);

        metrics::histogram!(
            "zaru_intent_pipeline_duration_seconds",
            "tenant_id" => "tenant-abc",
            "language" => "python",
            "outcome" => "success"
        )
        .record(1.5);
    }
}
