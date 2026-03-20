// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # StimulusService Application Service (BC-8 — ADR-021)
//!
//! Orchestrates the two-stage hybrid routing pipeline:
//!
//! 1. **Stage 1 (Deterministic):** Look up `source_name` in `WorkflowRegistry`.
//!    If a direct route exists, skip the LLM entirely.
//! 2. **Stage 2 (LLM Classification):** Execute the RouterAgent via `ExecutionService`.
//!    Parse the JSON output, check confidence threshold, resolve `WorkflowId`.
//!
//! On success, calls [`StartWorkflowExecutionUseCase`] and publishes
//! [`StimulusEvent`]s to the [`EventBus`].
//!
//! # Code Quality Principles
//!
//! - Resolve deterministic routes before invoking any classification fallback.
//! - Fail closed on low-confidence or unauthenticated stimuli.
//! - Keep transport-specific auth and webhook verification at the boundary.

use async_trait::async_trait;
use chrono::Utc;
use dashmap::DashMap;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::application::execution::ExecutionService;
use crate::application::{StartWorkflowExecutionRequest, StartWorkflowExecutionUseCase};
use crate::domain::events::StimulusEvent;
use crate::domain::execution::{ExecutionInput, ExecutionStatus};
use crate::domain::stimulus::{RoutingDecision, RoutingMode, Stimulus, StimulusId};
use crate::domain::tenant::TenantId;
use crate::domain::workflow_registry::WorkflowRegistry;
use crate::infrastructure::event_bus::EventBus;

// ──────────────────────────────────────────────────────────────────────────────
// Response / Error types
// ──────────────────────────────────────────────────────────────────────────────

/// Successful response from [`StimulusService::ingest`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct StimulusIngestResponse {
    pub stimulus_id: StimulusId,
    /// UUID string of the `WorkflowExecution` created for this stimulus.
    pub workflow_execution_id: String,
}

/// Error returned from [`StimulusService::ingest`].
#[derive(Debug, thiserror::Error)]
pub enum StimulusError {
    #[error("classification failed: confidence {confidence:.2} below threshold {threshold:.2}")]
    LowConfidence { confidence: f64, threshold: f64 },

    #[error("no router agent configured and no direct route matched for source '{source_name}'")]
    NoRouterConfigured { source_name: String },

    #[error("idempotent duplicate: already processed as stimulus {original_id}")]
    IdempotentDuplicate { original_id: StimulusId },

    #[error("router agent execution failed: {0}")]
    RouterAgentFailed(String),

    #[error("router agent returned invalid JSON: {0}")]
    ClassificationParseError(String),

    #[error("router agent returned unknown workflow '{workflow}'")]
    UnknownWorkflow { workflow: String },

    #[error("workflow execution failed: {0}")]
    WorkflowError(#[from] anyhow::Error),
}

impl StimulusError {
    /// HTTP status code appropriate for this error.
    pub fn http_status(&self) -> u16 {
        match self {
            StimulusError::LowConfidence { .. }
            | StimulusError::NoRouterConfigured { .. }
            | StimulusError::ClassificationParseError(_)
            | StimulusError::UnknownWorkflow { .. }
            | StimulusError::RouterAgentFailed(_) => 422,
            StimulusError::IdempotentDuplicate { .. } => 409,
            StimulusError::WorkflowError(_) => 500,
        }
    }

    /// Machine-readable error code for the response body.
    pub fn error_code(&self) -> &'static str {
        match self {
            StimulusError::LowConfidence { .. } => "classification_failed",
            StimulusError::NoRouterConfigured { .. } => "no_router_configured",
            StimulusError::IdempotentDuplicate { .. } => "idempotent_duplicate",
            StimulusError::RouterAgentFailed(_) => "router_agent_failed",
            StimulusError::ClassificationParseError(_) => "classification_parse_error",
            StimulusError::UnknownWorkflow { .. } => "unknown_workflow",
            StimulusError::WorkflowError(_) => "workflow_error",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// StimulusService trait
// ──────────────────────────────────────────────────────────────────────────────

/// Application service for BC-8: ingests a stimulus, routes it, and starts a workflow.
#[async_trait]
pub trait StimulusService: Send + Sync {
    /// Ingest a stimulus, run the two-stage routing pipeline, and start a workflow execution.
    ///
    /// Returns `StimulusIngestResponse` on success containing the `stimulus_id` and
    /// the newly created `workflow_execution_id`.
    ///
    /// # Errors
    ///
    /// - `LowConfidence` — RouterAgent returned confidence below threshold
    /// - `NoRouterConfigured` — No direct route and no RouterAgent wired
    /// - `IdempotentDuplicate` — Identical `(source, idempotency_key)` already processed
    /// - `RouterAgentFailed` — The RouterAgent execution itself failed
    async fn ingest(&self, stimulus: Stimulus) -> Result<StimulusIngestResponse, StimulusError>;

    /// Register a direct route: `source_name → workflow_id`.
    ///
    /// Bypasses LLM classification entirely when matched.
    async fn register_route(
        &self,
        source_name: &str,
        workflow_id: crate::domain::workflow::WorkflowId,
    ) -> anyhow::Result<()>;

    /// Remove a direct route by source name.
    async fn remove_route(&self, source_name: &str) -> bool;
}

// ──────────────────────────────────────────────────────────────────────────────
// StandardStimulusService implementation
// ──────────────────────────────────────────────────────────────────────────────

/// Idempotency cache entry.
type IdempotencyEntry = (StimulusId, chrono::DateTime<Utc>);

/// Standard stimulus service implementing the hybrid two-stage routing pipeline.
pub struct StandardStimulusService {
    /// WorkflowRegistry aggregate (routing table + RouterAgent ref).
    registry: Arc<RwLock<WorkflowRegistry>>,
    /// ExecutionService used to run the RouterAgent in Stage 2.
    execution_service: Arc<dyn ExecutionService>,
    /// Use case for starting workflow executions (delegates to Temporal).
    start_workflow_use_case: Arc<dyn StartWorkflowExecutionUseCase>,
    /// Domain event bus for StimulusEvents.
    event_bus: EventBus,
    /// In-memory idempotency store: `(source_name, idempotency_key) → (StimulusId, received_at)`.
    /// Phase 2: move to Redis for multi-node consistency.
    idempotency_cache: Arc<DashMap<(String, String), IdempotencyEntry>>,
    /// TTL for idempotency entries (default: 24 hours).
    idempotency_ttl: Duration,
    /// Timeout for RouterAgent classification (default: 30 seconds).
    classification_timeout: Duration,
}

impl StandardStimulusService {
    fn tenant_id_for_stimulus(stimulus: &Stimulus) -> TenantId {
        stimulus
            .headers
            .get("x-aegis-tenant")
            .or_else(|| stimulus.headers.get("x-tenant-id"))
            .and_then(|value| TenantId::from_string(value).ok())
            .unwrap_or_else(TenantId::local_default)
    }

    pub fn new(
        registry: Arc<RwLock<WorkflowRegistry>>,
        execution_service: Arc<dyn ExecutionService>,
        start_workflow_use_case: Arc<dyn StartWorkflowExecutionUseCase>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            registry,
            execution_service,
            start_workflow_use_case,
            event_bus,
            idempotency_cache: Arc::new(DashMap::new()),
            idempotency_ttl: Duration::from_secs(86_400), // 24 hours
            classification_timeout: Duration::from_secs(30),
        }
    }

    /// Override the idempotency TTL (useful for testing).
    pub fn with_idempotency_ttl(mut self, ttl: Duration) -> Self {
        self.idempotency_ttl = ttl;
        self
    }

    /// Override the RouterAgent classification timeout.
    pub fn with_classification_timeout(mut self, timeout: Duration) -> Self {
        self.classification_timeout = timeout;
        self
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    async fn check_idempotency(&self, stimulus: &Stimulus) -> Option<StimulusId> {
        if let Some(key) = &stimulus.idempotency_key {
            let cache_key = (stimulus.source.name(), key.clone());
            if let Some(entry) = self.idempotency_cache.get(&cache_key) {
                let (original_id, recorded_at) = entry.value();
                // Check TTL — if entry is still fresh, treat as duplicate
                if Utc::now()
                    .signed_duration_since(*recorded_at)
                    .to_std()
                    .ok()
                    .map(|age| age < self.idempotency_ttl)
                    .unwrap_or(false)
                {
                    return Some(*original_id);
                }
                // Expired — remove stale entry
                drop(entry);
                self.idempotency_cache.remove(&cache_key);
            }
        }
        None
    }

    fn record_idempotency(&self, stimulus: &Stimulus) {
        if let Some(key) = &stimulus.idempotency_key {
            let cache_key = (stimulus.source.name(), key.clone());
            self.idempotency_cache
                .insert(cache_key, (stimulus.id, Utc::now()));
        }
    }

    async fn route(&self, stimulus: &Stimulus) -> Result<RoutingDecision, StimulusError> {
        let registry = self.registry.read().await;

        // Stage 1: Deterministic direct-route lookup
        let source_key = stimulus.source.name();
        if let Some(workflow_id) = registry.lookup_direct(&source_key) {
            debug!(
                stimulus_id = %stimulus.id,
                source = %source_key,
                workflow_id = %workflow_id.0,
                "Stimulus routed deterministically"
            );
            return Ok(RoutingDecision {
                workflow_id,
                confidence: 1.0,
                mode: RoutingMode::Deterministic,
            });
        }

        // Stage 2: LLM RouterAgent classification
        let agent_id =
            registry
                .router_agent()
                .ok_or_else(|| StimulusError::NoRouterConfigured {
                    source_name: source_key.clone(),
                })?;
        let threshold = registry.confidence_threshold();
        drop(registry); // release read lock before async I/O

        info!(
            stimulus_id = %stimulus.id,
            source = %source_key,
            agent_id = %agent_id.0,
            "No direct route found; invoking RouterAgent for LLM classification"
        );

        let input = ExecutionInput {
            intent: None,
            payload: json!({
                "stimulus": stimulus.content,
                "tenant_id": Self::tenant_id_for_stimulus(stimulus).to_string(),
            }),
        };

        // Run the RouterAgent
        let exec_id = self
            .execution_service
            .start_execution(agent_id, input)
            .await
            .map_err(|e| StimulusError::RouterAgentFailed(e.to_string()))?;

        // Poll for completion (RouterAgent is a single-iteration agent)
        let result = self
            .await_execution_output(exec_id)
            .await
            .map_err(|e| StimulusError::RouterAgentFailed(e.to_string()))?;

        // Parse classification JSON
        let classification: serde_json::Value = serde_json::from_str(&result)
            .map_err(|e| StimulusError::ClassificationParseError(e.to_string()))?;

        let confidence = classification["confidence"].as_f64().unwrap_or(0.0);
        if confidence < threshold {
            warn!(
                stimulus_id = %stimulus.id,
                confidence,
                threshold,
                "RouterAgent classification below confidence threshold"
            );
            return Err(StimulusError::LowConfidence {
                confidence,
                threshold,
            });
        }

        let workflow_name = classification["workflow"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let registry = self.registry.read().await;
        let workflow_id = registry.lookup_direct(&workflow_name).ok_or_else(|| {
            StimulusError::UnknownWorkflow {
                workflow: workflow_name.clone(),
            }
        })?;

        Ok(RoutingDecision {
            workflow_id,
            confidence,
            mode: RoutingMode::LlmClassified,
        })
    }

    /// Poll the execution until it reaches a terminal state, then return the final output.
    /// Respects `self.classification_timeout`.
    async fn await_execution_output(
        &self,
        exec_id: crate::domain::execution::ExecutionId,
    ) -> anyhow::Result<String> {
        let deadline = tokio::time::Instant::now() + self.classification_timeout;
        let poll_interval = Duration::from_millis(250);

        loop {
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!(
                    "RouterAgent classification timed out after {:?}",
                    self.classification_timeout
                );
            }

            let execution = self.execution_service.get_execution(exec_id).await?;

            match execution.status {
                ExecutionStatus::Completed => {
                    // Return the output of the last successful iteration
                    if let Some(last_iter) = execution.iterations.last() {
                        if let Some(output) = &last_iter.output {
                            return Ok(output.clone());
                        }
                    }
                    anyhow::bail!("RouterAgent execution completed with no output");
                }
                ExecutionStatus::Failed => {
                    anyhow::bail!(
                        "RouterAgent execution failed: {}",
                        execution.error.as_deref().unwrap_or("unknown error")
                    );
                }
                _ => {
                    // Still running — wait and retry
                    tokio::time::sleep(poll_interval).await;
                }
            }
        }
    }
}

#[async_trait]
impl StimulusService for StandardStimulusService {
    async fn ingest(&self, stimulus: Stimulus) -> Result<StimulusIngestResponse, StimulusError> {
        // ── 1. Idempotency check ──────────────────────────────────────────────
        if let Some(original_id) = self.check_idempotency(&stimulus).await {
            debug!(
                original_id = %original_id,
                idempotency_key = ?stimulus.idempotency_key,
                "Duplicate stimulus rejected by idempotency cache"
            );
            return Err(StimulusError::IdempotentDuplicate { original_id });
        }

        // ── 2. Publish StimulusReceived event ────────────────────────────────
        self.event_bus
            .publish_stimulus_event(StimulusEvent::StimulusReceived {
                stimulus_id: stimulus.id,
                source: stimulus.source.name(),
                received_at: stimulus.received_at,
            });

        // ── 3. Route the stimulus ─────────────────────────────────────────────
        let decision = match self.route(&stimulus).await {
            Ok(d) => d,
            Err(StimulusError::LowConfidence {
                confidence,
                threshold,
            }) => {
                self.event_bus
                    .publish_stimulus_event(StimulusEvent::StimulusRejected {
                        stimulus_id: stimulus.id,
                        reason: format!(
                            "low_confidence: {confidence:.2} (threshold: {threshold:.2})"
                        ),
                        rejected_at: Utc::now(),
                    });
                return Err(StimulusError::LowConfidence {
                    confidence,
                    threshold,
                });
            }
            Err(e) => {
                self.event_bus
                    .publish_stimulus_event(StimulusEvent::ClassificationFailed {
                        stimulus_id: stimulus.id,
                        error: e.to_string(),
                        failed_at: Utc::now(),
                    });
                return Err(e);
            }
        };

        // ── 4. Start workflow execution ───────────────────────────────────────
        let workflow_request = StartWorkflowExecutionRequest {
            workflow_id: decision.workflow_id.0.to_string(),
            input: json!({
                "stimulus_content": stimulus.content,
                "stimulus_source":  stimulus.source.name(),
                "stimulus_id":      stimulus.id.to_string(),
                "headers":          stimulus.headers,
            }),
            blackboard: None,
            tenant_id: Some(Self::tenant_id_for_stimulus(&stimulus)),
        };

        let started = self
            .start_workflow_use_case
            .start_execution(workflow_request)
            .await
            .map_err(StimulusError::WorkflowError)?;

        // ── 5. Register idempotency key ───────────────────────────────────────
        self.record_idempotency(&stimulus);

        // ── 6. Publish StimulusClassified event ───────────────────────────────
        self.event_bus
            .publish_stimulus_event(StimulusEvent::StimulusClassified {
                stimulus_id: stimulus.id,
                workflow_id: decision.workflow_id.0.to_string(),
                confidence: decision.confidence,
                routing_mode: format!("{:?}", decision.mode),
                classified_at: Utc::now(),
            });

        info!(
            stimulus_id = %stimulus.id,
            workflow_execution_id = %started.execution_id,
            routing_mode = ?decision.mode,
            confidence = decision.confidence,
            "Stimulus routed and workflow execution started"
        );

        Ok(StimulusIngestResponse {
            stimulus_id: stimulus.id,
            workflow_execution_id: started.execution_id,
        })
    }

    async fn register_route(
        &self,
        source_name: &str,
        workflow_id: crate::domain::workflow::WorkflowId,
    ) -> anyhow::Result<()> {
        let mut registry = self.registry.write().await;
        registry.register_route(source_name, workflow_id)
    }

    async fn remove_route(&self, source_name: &str) -> bool {
        let mut registry = self.registry.write().await;
        registry.remove_route(source_name)
    }
}
