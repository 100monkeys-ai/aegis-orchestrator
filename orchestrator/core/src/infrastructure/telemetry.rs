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
/// Binds an HTTP listener to `bind_address:port` and registers initial node
/// metadata.
///
/// # Security
///
/// Per audit 002 finding 4.37.18, callers MUST pass an explicit
/// `bind_address`; the default in [`crate::domain::node_config::MetricsConfig`]
/// is `127.0.0.1` (loopback). Binding `0.0.0.0` exposes the unauthenticated
/// scrape endpoint to the network and requires an external authenticating
/// reverse proxy.
pub fn init_metrics(
    bind_address: &str,
    port: u16,
    node_id: &str,
    node_name: &str,
    region: Option<&str>,
    version: &str,
) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("{bind_address}:{port}").parse().map_err(|e| {
        anyhow::anyhow!("Invalid metrics bind_address={bind_address} port={port}: {e}")
    })?;

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

    // Register relay-coordinator metric descriptors (ADR-117 §Observability,
    // ADR-058 cardinality rules). The relay-coordinator runs in-process with
    // the orchestrator daemon and shares this exporter — there is no
    // separate metrics endpoint.
    register_relay_metrics();

    tracing::info!("Metrics exporter listening on {}", addr);
    Ok(())
}

/// Registers the four intent execution pipeline metric descriptors defined in
/// ADR-087 §Observability. Calling this pre-populates the Prometheus registry
/// so the metrics appear at scrape time even before the first pipeline run.
///
/// This function is idempotent — subsequent calls are no-ops because the
/// `metrics` crate deduplicates registrations by name.
/// Label keys used on the intent-pipeline Prometheus metrics. Centralised so
/// regression tests can assert that high-cardinality identifiers (notably
/// `tenant_id`, see audit 002 finding 4.26) are not present.
pub const INTENT_PIPELINE_STARTS_LABELS: &[&str] = &["language"];
pub const INTENT_PIPELINE_DURATION_LABELS: &[&str] = &["language", "outcome"];
pub const INTENT_PIPELINE_AGENT_CACHE_HITS_LABELS: &[&str] = &[];
pub const INTENT_PIPELINE_CONTAINER_EXIT_CODE_LABELS: &[&str] = &["exit_code"];

pub fn register_intent_pipeline_metrics() {
    // Counter: pipeline start events, labelled by language only. Per-tenant
    // breakdown is intentionally omitted to bound Prometheus cardinality
    // (audit 002, finding 4.26).
    metrics::counter!(
        "zaru_intent_pipeline_starts_total",
        "language" => ""
    )
    .absolute(0);

    // Histogram: end-to-end pipeline duration, labelled by language and
    // outcome. Buckets are picked up from the `_duration_seconds` suffix
    // matcher set in `init_metrics`.
    metrics::histogram!(
        "zaru_intent_pipeline_duration_seconds",
        "language" => "",
        "outcome" => ""
    )
    .record(0.0);

    // Counter: Cortex / agent cache hits. Aggregated across all tenants.
    metrics::counter!("zaru_intent_pipeline_agent_cache_hits_total").absolute(0);

    // Counter: container exit codes observed by the step runner.
    metrics::counter!(
        "zaru_intent_pipeline_container_exit_code_total",
        "exit_code" => ""
    )
    .absolute(0);
}

// ── Relay-Coordinator metric descriptors (ADR-117) ─────────────────────────
//
// The relay-coordinator multiplexes traffic between edge daemons and the
// controller. It runs **in-process** with the orchestrator daemon and reuses
// the single Prometheus recorder bound by `init_metrics`. These descriptors
// are pre-registered at startup so the metrics appear in `/metrics` scrapes
// even before the first edge connection.
//
// Cardinality (ADR-058): per-tenant / per-execution / per-agent /
// per-workflow / per-iteration labels are PROHIBITED. Relay metric labels
// are restricted to small bounded enums (`direction`, `result`, `reason`).

/// Allowed values for the `direction` label on relay forward counters.
pub const RELAY_DIRECTION_INBOUND: &str = "inbound";
pub const RELAY_DIRECTION_OUTBOUND: &str = "outbound";

/// Allowed values for the `result` label on relay forward counters.
pub const RELAY_RESULT_SUCCESS: &str = "success";
pub const RELAY_RESULT_ERROR: &str = "error";

/// Allowed values for the `reason` label on relay handshake-failure counters.
/// Bounded to keep cardinality fixed regardless of edge-node population.
pub const RELAY_HANDSHAKE_REASON_AUTH_FAILED: &str = "auth_failed";
pub const RELAY_HANDSHAKE_REASON_VERSION_MISMATCH: &str = "version_mismatch";
pub const RELAY_HANDSHAKE_REASON_TIMEOUT: &str = "timeout";
pub const RELAY_HANDSHAKE_REASON_UNKNOWN: &str = "unknown";

pub const RELAY_FORWARD_LABELS: &[&str] = &["direction", "result"];
pub const RELAY_HANDSHAKE_LABELS: &[&str] = &["reason"];
pub const RELAY_EDGE_NODES_LABELS: &[&str] = &[];
pub const RELAY_DURATION_LABELS: &[&str] = &[];

pub fn register_relay_metrics() {
    // Gauge: edge nodes currently connected to this relay-coordinator.
    metrics::gauge!("aegis_relay_edge_nodes_connected").set(0.0);

    // Counter: messages forwarded between edge and controller. Labelled by
    // direction (inbound/outbound) and result (success/error). Per-edge and
    // per-tenant breakdowns are intentionally omitted (ADR-058 cardinality).
    for direction in [RELAY_DIRECTION_INBOUND, RELAY_DIRECTION_OUTBOUND] {
        for result in [RELAY_RESULT_SUCCESS, RELAY_RESULT_ERROR] {
            metrics::counter!(
                "aegis_relay_messages_forwarded_total",
                "direction" => direction,
                "result" => result,
            )
            .absolute(0);
        }
    }

    // Histogram: end-to-end relay forwarding latency. Buckets come from the
    // `_duration_seconds` matcher set in `init_metrics`.
    metrics::histogram!("aegis_relay_message_duration_seconds").record(0.0);

    // Counter: relay handshake failures, labelled by bounded reason.
    for reason in [
        RELAY_HANDSHAKE_REASON_AUTH_FAILED,
        RELAY_HANDSHAKE_REASON_VERSION_MISMATCH,
        RELAY_HANDSHAKE_REASON_TIMEOUT,
        RELAY_HANDSHAKE_REASON_UNKNOWN,
    ] {
        metrics::counter!(
            "aegis_relay_handshake_failures_total",
            "reason" => reason,
        )
        .absolute(0);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        register_intent_pipeline_metrics, register_relay_metrics,
        INTENT_PIPELINE_AGENT_CACHE_HITS_LABELS, INTENT_PIPELINE_CONTAINER_EXIT_CODE_LABELS,
        INTENT_PIPELINE_DURATION_LABELS, INTENT_PIPELINE_STARTS_LABELS, RELAY_DIRECTION_INBOUND,
        RELAY_DIRECTION_OUTBOUND, RELAY_DURATION_LABELS, RELAY_EDGE_NODES_LABELS,
        RELAY_FORWARD_LABELS, RELAY_HANDSHAKE_LABELS, RELAY_HANDSHAKE_REASON_AUTH_FAILED,
        RELAY_HANDSHAKE_REASON_TIMEOUT, RELAY_HANDSHAKE_REASON_UNKNOWN,
        RELAY_HANDSHAKE_REASON_VERSION_MISMATCH, RELAY_RESULT_ERROR, RELAY_RESULT_SUCCESS,
    };

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
            "language" => "python"
        )
        .increment(1);

        metrics::counter!("zaru_intent_pipeline_agent_cache_hits_total").increment(1);

        metrics::counter!(
            "zaru_intent_pipeline_container_exit_code_total",
            "exit_code" => "0"
        )
        .increment(1);

        metrics::histogram!(
            "zaru_intent_pipeline_duration_seconds",
            "language" => "python",
            "outcome" => "success"
        )
        .record(1.5);
    }

    /// Regression for finding 4.26 (002 audit): Prometheus metrics MUST NOT
    /// carry `tenant_id` as a label. Per-tenant labels create unbounded
    /// cardinality (one series per tenant × other labels), risk DOSing
    /// Prometheus, and disclose tenant identifiers to any `/metrics` scraper.
    #[test]
    fn test_intent_pipeline_metrics_have_no_tenant_id_label() {
        for labels in [
            INTENT_PIPELINE_STARTS_LABELS,
            INTENT_PIPELINE_DURATION_LABELS,
            INTENT_PIPELINE_AGENT_CACHE_HITS_LABELS,
            INTENT_PIPELINE_CONTAINER_EXIT_CODE_LABELS,
        ] {
            assert!(
                !labels.contains(&"tenant_id"),
                "intent pipeline metrics must not include tenant_id label, got {labels:?}"
            );
        }
    }

    /// Regression: relay-coordinator descriptors must register without panic
    /// and remain idempotent across multiple invocations.
    #[test]
    fn test_relay_metrics_register_without_panic() {
        register_relay_metrics();
        register_relay_metrics();
    }

    /// Regression for ADR-058 cardinality rules: relay metrics MUST NOT
    /// include `tenant_id`, `execution_id`, `agent_id`, `workflow_id`, or
    /// `iteration_id` labels. Edge-node identity is also intentionally
    /// omitted from labels — per-edge insight is OTLP traces (ADR-057), not
    /// Prometheus metrics.
    #[test]
    fn test_relay_metrics_have_no_prohibited_labels() {
        const PROHIBITED: &[&str] = &[
            "tenant_id",
            "execution_id",
            "agent_id",
            "workflow_id",
            "iteration_id",
            "edge_node_id",
            "node_id",
        ];
        for labels in [
            RELAY_FORWARD_LABELS,
            RELAY_HANDSHAKE_LABELS,
            RELAY_EDGE_NODES_LABELS,
            RELAY_DURATION_LABELS,
        ] {
            for forbidden in PROHIBITED {
                assert!(
                    !labels.contains(forbidden),
                    "relay metric labels must not include {forbidden}, got {labels:?}"
                );
            }
        }
    }

    /// Regression: relay handshake reasons must stay in a small bounded
    /// enum so the cardinality of `aegis_relay_handshake_failures_total`
    /// remains fixed regardless of edge-node population. Adding a new
    /// reason requires updating both this enum and the dashboard panel.
    #[test]
    fn test_relay_handshake_reasons_are_bounded_enum() {
        let allowed = [
            RELAY_HANDSHAKE_REASON_AUTH_FAILED,
            RELAY_HANDSHAKE_REASON_VERSION_MISMATCH,
            RELAY_HANDSHAKE_REASON_TIMEOUT,
            RELAY_HANDSHAKE_REASON_UNKNOWN,
        ];
        assert_eq!(
            allowed.len(),
            4,
            "relay handshake reasons must stay bounded"
        );
    }

    /// Regression: relay forward direction/result label values must stay in
    /// the documented bounded enums.
    #[test]
    fn test_relay_forward_label_values_are_bounded_enum() {
        let directions = [RELAY_DIRECTION_INBOUND, RELAY_DIRECTION_OUTBOUND];
        let results = [RELAY_RESULT_SUCCESS, RELAY_RESULT_ERROR];
        assert_eq!(directions.len(), 2);
        assert_eq!(results.len(), 2);
    }
}
