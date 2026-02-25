// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Stimulus Domain Types (BC-8 — ADR-021)
//!
//! Value objects representing an external or internal stimulus that triggers
//! a workflow execution. Stimuli are immutable once created — all routing
//! decisions are recorded in [`RoutingDecision`].
//!
//! ## Design
//!
//! A [`Stimulus`] is a pure value object: no behaviour, no I/O. The
//! [`crate::application::stimulus::StimulusService`] owns the routing
//! pipeline and workflow dispatch.

use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use uuid::Uuid;

// ──────────────────────────────────────────────────────────────────────────────
// StimulusId
// ──────────────────────────────────────────────────────────────────────────────

/// Opaque UUID identifier for a single stimulus ingestion.
/// Used for idempotency deduplication, audit correlation, and event linking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StimulusId(pub Uuid);

impl StimulusId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_string(s: &str) -> anyhow::Result<Self> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl std::fmt::Display for StimulusId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for StimulusId {
    fn default() -> Self { Self::new() }
}

// ──────────────────────────────────────────────────────────────────────────────
// StimulusSource
// ──────────────────────────────────────────────────────────────────────────────

/// Where the stimulus originated.
///
/// The `source_name` field on [`StimulusSource::Webhook`] is the `{source}`
/// path segment from `POST /v1/webhooks/{source}`. This same string is used
/// as the routing table lookup key in [`crate::domain::workflow_registry::WorkflowRegistry`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StimulusSource {
    /// External system POSTing to `/v1/webhooks/{source_name}`.
    /// Authenticated by HMAC-SHA256; no Keycloak account required.
    Webhook { source_name: String },

    /// Internal operator or SDK calling `POST /v1/stimuli`.
    /// Authenticated by Keycloak JWT Bearer.
    HttpApi,

    /// Always-on stdin sensor (SensorService loop).
    Stdin,

    /// Temporal signal received by the TypeScript worker and forwarded over gRPC.
    TemporalSignal,
}

impl StimulusSource {
    /// Returns the canonical string key used for routing table lookups.
    pub fn name(&self) -> String {
        match self {
            StimulusSource::Webhook { source_name } => source_name.clone(),
            StimulusSource::HttpApi => "http_api".to_string(),
            StimulusSource::Stdin => "stdin".to_string(),
            StimulusSource::TemporalSignal => "temporal_signal".to_string(),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Stimulus
// ──────────────────────────────────────────────────────────────────────────────

/// Immutable value object carrying a stimulus through the routing pipeline.
///
/// Created at ingestion time and never mutated. All routing state is captured
/// in [`RoutingDecision`] and published as [`crate::domain::events::StimulusEvent`]s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stimulus {
    pub id: StimulusId,
    pub source: StimulusSource,
    /// Raw content — natural language intent, JSON webhook body, or binary-encoded string.
    pub content: String,
    /// Optional exactly-once processing key.
    /// Scope: `(source_name, idempotency_key)` tuple with 24-hour TTL.
    /// Empty `None` disables deduplication for this stimulus.
    pub idempotency_key: Option<String>,
    /// Forwarded HTTP headers from the originating request.
    /// Used by RouterAgent for event-type classification (e.g. `X-GitHub-Event`).
    pub headers: HashMap<String, String>,
    pub received_at: DateTime<Utc>,
}

impl Stimulus {
    pub fn new(source: StimulusSource, content: impl Into<String>) -> Self {
        Self {
            id: StimulusId::new(),
            source,
            content: content.into(),
            idempotency_key: None,
            headers: HashMap::new(),
            received_at: Utc::now(),
        }
    }

    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        let k = key.into();
        if !k.is_empty() {
            self.idempotency_key = Some(k);
        }
        self
    }

    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// RoutingDecision
// ──────────────────────────────────────────────────────────────────────────────

/// The outcome of the two-stage hybrid routing pipeline.
///
/// Produced by [`crate::application::stimulus::StandardStimulusService::route`]
/// after either a deterministic direct-route hit or a successful LLM classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    /// The workflow that will handle this stimulus.
    pub workflow_id: crate::domain::workflow::WorkflowId,
    /// Confidence score. Always `1.0` for deterministic routes; `[0.7, 1.0]` for LLM routes.
    pub confidence: f64,
    /// How the workflow was selected.
    pub mode: RoutingMode,
}

/// How the WorkflowId was resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RoutingMode {
    /// `source_name` matched a registered direct route — no LLM invoked.
    Deterministic,
    /// RouterAgent LLM classified the stimulus content.
    LlmClassified,
}

// ──────────────────────────────────────────────────────────────────────────────
// RejectionReason
// ──────────────────────────────────────────────────────────────────────────────

/// Structured reason for stimulus rejection, serialised into the 422 response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RejectionReason {
    /// RouterAgent returned confidence below the configured threshold.
    LowConfidence { confidence: f64, threshold: f64 },
    /// No direct route registered and no RouterAgent configured.
    NoRouterConfigured { source: String },
    /// HMAC-SHA256 signature verification failed.
    HmacInvalid,
    /// An identical `(source, idempotency_key)` was already processed within the TTL window.
    IdempotentDuplicate { original_stimulus_id: StimulusId },
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stimulus_source_name_webhook() {
        let src = StimulusSource::Webhook { source_name: "GitHub".to_string() };
        assert_eq!(src.name(), "GitHub");
    }

    #[test]
    fn stimulus_source_name_http_api() {
        assert_eq!(StimulusSource::HttpApi.name(), "http_api");
    }

    #[test]
    fn stimulus_new_has_unique_ids() {
        let a = Stimulus::new(StimulusSource::Stdin, "hello");
        let b = Stimulus::new(StimulusSource::Stdin, "hello");
        assert_ne!(a.id.0, b.id.0);
    }

    #[test]
    fn empty_idempotency_key_is_none() {
        let s = Stimulus::new(StimulusSource::HttpApi, "hi").with_idempotency_key("");
        assert!(s.idempotency_key.is_none());
    }
}
