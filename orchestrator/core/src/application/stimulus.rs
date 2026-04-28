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
use crate::domain::cluster::StimulusIdempotencyRepository;
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

    /// No tenant has registered a route for this webhook source.
    ///
    /// Webhooks are unauthenticated; the only way to discover the owning
    /// tenant is via the per-tenant `(TenantId, source_name)` registration
    /// table. If no registration matches the request MUST be rejected — the
    /// system never falls back to the global consumer tenant (ADR-097).
    #[error("no tenant has registered a route for webhook source '{source_name}'")]
    UnregisteredWebhookSource { source_name: String },

    /// Multiple tenants have registered the same webhook source name and
    /// the webhook URL alone cannot disambiguate them. The operator must
    /// either deduplicate the registrations or migrate to a tenant-scoped
    /// webhook URL (e.g. `/v1/webhooks/{tenant}/{source}`).
    #[error(
        "webhook source '{source_name}' is registered by {tenant_count} tenants; \
         route is ambiguous"
    )]
    AmbiguousWebhookRoute {
        source_name: String,
        tenant_count: usize,
    },
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
            // No registered route for an unauthenticated webhook → 404.
            // The request reached us, signature was valid, but no tenant
            // owns the source — looks like the resource doesn't exist.
            StimulusError::UnregisteredWebhookSource { .. } => 404,
            // Ambiguous registration → 409 Conflict (operator must
            // disambiguate by deduplicating registrations).
            StimulusError::AmbiguousWebhookRoute { .. } => 409,
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
            StimulusError::UnregisteredWebhookSource { .. } => "unregistered_webhook_source",
            StimulusError::AmbiguousWebhookRoute { .. } => "ambiguous_webhook_route",
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
    /// `identity` should be `Some` for requests authenticated via IAM/OIDC JWT so that
    /// user-scoped rate limit counters are written in addition to tenant-scoped ones.
    /// Pass `None` for webhook/sensor/gRPC callers that carry no user identity.
    ///
    /// # Errors
    ///
    /// - `LowConfidence` — RouterAgent returned confidence below threshold
    /// - `NoRouterConfigured` — No direct route and no RouterAgent wired
    /// - `IdempotentDuplicate` — Identical `(source, idempotency_key)` already processed
    /// - `RouterAgentFailed` — The RouterAgent execution itself failed
    async fn ingest(
        &self,
        stimulus: Stimulus,
        identity: Option<crate::domain::iam::UserIdentity>,
    ) -> Result<StimulusIngestResponse, StimulusError>;

    /// Register a direct route: `(tenant_id, source_name) → workflow_id`.
    ///
    /// Routes are tenant-scoped so the same source name can be registered by
    /// different tenants without collision (gap 056-13).
    async fn register_route(
        &self,
        tenant_id: &crate::domain::tenant::TenantId,
        source_name: &str,
        workflow_id: crate::domain::workflow::WorkflowId,
    ) -> anyhow::Result<()>;

    /// Remove a direct route for a tenant by source name.
    async fn remove_route(
        &self,
        tenant_id: &crate::domain::tenant::TenantId,
        source_name: &str,
    ) -> bool;
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
    /// Used as write-through cache when `idempotency_repo` is present, or standalone fallback.
    idempotency_cache: Arc<DashMap<(String, String), IdempotencyEntry>>,
    /// Optional Postgres-backed idempotency repository (ADR-021 Phase 2).
    /// When present, Postgres is the authoritative store and DashMap acts as
    /// a write-through cache. When absent, DashMap is the sole store.
    idempotency_repo: Option<Arc<dyn StimulusIdempotencyRepository>>,
    /// TTL for idempotency entries (default: 24 hours).
    idempotency_ttl: Duration,
    /// Timeout for RouterAgent classification (default: 30 seconds).
    classification_timeout: Duration,
}

impl StandardStimulusService {
    /// Resolve the owning tenant of an incoming stimulus.
    ///
    /// Per ADR-097 / ADR-021 the stimulus tenant is derived from one of two
    /// trusted sources, in order:
    ///
    /// 1. The authenticated caller's identity (`/v1/stimuli` path — the JWT
    ///    is the source of truth).
    /// 2. The webhook's per-tenant registration in [`WorkflowRegistry`]
    ///    (`/v1/webhooks/{source}` path — the request is HMAC-verified but
    ///    unauthenticated, so the tenant comes from the registration table,
    ///    NOT from caller-supplied headers).
    ///
    /// Caller-supplied tenant headers (`x-aegis-tenant`, `x-tenant-id`) are
    /// **never** trusted on the unauthenticated webhook path. Trusting them
    /// would allow an external system to spoof any tenant.
    ///
    /// # Errors
    ///
    /// - [`StimulusError::UnregisteredWebhookSource`] — webhook with no
    ///   matching registration (404).
    /// - [`StimulusError::AmbiguousWebhookRoute`] — webhook source registered
    ///   by multiple tenants and the URL alone cannot disambiguate them
    ///   (409). The operator must deduplicate registrations or migrate to a
    ///   tenant-scoped URL.
    async fn resolve_stimulus_tenant(
        &self,
        stimulus: &Stimulus,
        identity: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<TenantId, StimulusError> {
        use crate::domain::stimulus::StimulusSource;

        // Authenticated path: trust the JWT-derived identity.
        if let Some(id) = identity {
            return crate::domain::iam::derive_tenant_id_strict(id).map_err(|e| {
                StimulusError::WorkflowError(anyhow::anyhow!(
                    "failed to derive tenant from authenticated stimulus identity: {e}"
                ))
            });
        }

        // Unauthenticated webhook path: derive tenant from the registration
        // table. There is no other trustworthy source — caller-supplied
        // headers MUST NOT be used.
        match &stimulus.source {
            StimulusSource::Webhook { source_name } => {
                let registry = self.registry.read().await;
                let routes = registry.find_routes_by_source(source_name);
                match routes.len() {
                    0 => Err(StimulusError::UnregisteredWebhookSource {
                        source_name: source_name.clone(),
                    }),
                    1 => Ok(routes.into_iter().next().expect("len == 1").0),
                    n => Err(StimulusError::AmbiguousWebhookRoute {
                        source_name: source_name.clone(),
                        tenant_count: n,
                    }),
                }
            }
            // Other unauthenticated sources (e.g. internal sensors / cron
            // dispatch) are explicitly system-initiated. There is no caller
            // tenant to derive — the stimulus belongs to the system.
            _ => Ok(TenantId::system()),
        }
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
            idempotency_repo: None,
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

    /// Wire a Postgres-backed idempotency repository (ADR-021 Phase 2).
    ///
    /// When present, Postgres is checked first (authoritative) and the DashMap
    /// acts as a write-through cache. When absent, falls back to in-memory only.
    pub fn with_idempotency_repo(mut self, repo: Arc<dyn StimulusIdempotencyRepository>) -> Self {
        self.idempotency_repo = Some(repo);
        self
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    async fn check_idempotency(&self, stimulus: &Stimulus) -> Option<StimulusId> {
        stimulus.idempotency_key.as_ref()?;

        // When a Postgres repo is wired, it is the authoritative store.
        // Check it FIRST; the DashMap is only a write-through cache.
        if let Some(repo) = &self.idempotency_repo {
            match repo.check_and_insert(&stimulus.id).await {
                Ok(true) => {
                    // Newly inserted — not a duplicate. Populate cache.
                    if let Some(key) = &stimulus.idempotency_key {
                        let cache_key = (stimulus.source.name(), key.clone());
                        self.idempotency_cache
                            .insert(cache_key, (stimulus.id, Utc::now()));
                    }
                    return None;
                }
                Ok(false) => {
                    // Already exists in Postgres — duplicate.
                    return Some(stimulus.id);
                }
                Err(e) => {
                    warn!(
                        stimulus_id = %stimulus.id,
                        error = %e,
                        "Postgres idempotency check failed; falling through to in-memory cache"
                    );
                    // Fall through to in-memory check below
                }
            }
        }

        // In-memory fallback (or sole store when no repo is wired)
        if let Some(key) = &stimulus.idempotency_key {
            let cache_key = (stimulus.source.name(), key.clone());
            if let Some(entry) = self.idempotency_cache.get(&cache_key) {
                let (original_id, recorded_at) = entry.value();
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

    async fn route(
        &self,
        stimulus: &Stimulus,
        tenant_id: &TenantId,
    ) -> Result<RoutingDecision, StimulusError> {
        let registry = self.registry.read().await;

        // Stage 1: Deterministic direct-route lookup (tenant-scoped, gap 056-13).
        let source_key = stimulus.source.name();
        if let Some(workflow_id) = registry.lookup_direct(tenant_id, &source_key) {
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
            input: json!({
                "stimulus": stimulus.content,
                "tenant_id": tenant_id.to_string(),
            }),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        };

        // Run the RouterAgent
        // ADR-083: operator context for system-initiated RouterAgent classification
        let exec_id = self
            .execution_service
            .start_execution(agent_id, input, "aegis-system-operator".to_string(), None)
            .await
            .map_err(|e| StimulusError::RouterAgentFailed(e.to_string()))?;

        // Poll for completion (RouterAgent is a single-iteration agent)
        let result = self
            .await_execution_output(tenant_id, exec_id)
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
        let workflow_id = registry
            .lookup_direct(tenant_id, &workflow_name)
            .ok_or_else(|| StimulusError::UnknownWorkflow {
                workflow: workflow_name.clone(),
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
        tenant_id: &crate::domain::shared_kernel::TenantId,
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

            let execution = self
                .execution_service
                .get_execution_for_tenant(tenant_id, exec_id)
                .await?;

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
    async fn ingest(
        &self,
        stimulus: Stimulus,
        identity: Option<crate::domain::iam::UserIdentity>,
    ) -> Result<StimulusIngestResponse, StimulusError> {
        let start = std::time::Instant::now();

        // ── 1. Idempotency check ──────────────────────────────────────────────
        if let Some(original_id) = self.check_idempotency(&stimulus).await {
            debug!(
                original_id = %original_id,
                idempotency_key = ?stimulus.idempotency_key,
                "Duplicate stimulus rejected by idempotency cache"
            );
            metrics::counter!("aegis_stimuli_rejected_total", "reason" => "idempotent_duplicate")
                .increment(1);
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
        //
        // ADR-097 footgun #3: tenant resolution is fail-closed. The
        // authenticated path uses the JWT identity; the webhook path
        // looks the source up in the tenant-scoped registration table
        // and rejects on miss/ambiguity. Caller-supplied tenant headers
        // are NEVER trusted.
        let stimulus_tenant_id = match self
            .resolve_stimulus_tenant(&stimulus, identity.as_ref())
            .await
        {
            Ok(t) => t,
            Err(e) => {
                let reason = match &e {
                    StimulusError::UnregisteredWebhookSource { .. } => {
                        "unregistered_webhook_source"
                    }
                    StimulusError::AmbiguousWebhookRoute { .. } => "ambiguous_webhook_route",
                    _ => "tenant_resolution_failed",
                };
                metrics::counter!("aegis_stimuli_rejected_total", "reason" => reason).increment(1);
                self.event_bus
                    .publish_stimulus_event(StimulusEvent::StimulusRejected {
                        stimulus_id: stimulus.id,
                        reason: e.to_string(),
                        rejected_at: Utc::now(),
                    });
                return Err(e);
            }
        };
        let decision = match self.route(&stimulus, &stimulus_tenant_id).await {
            Ok(d) => d,
            Err(StimulusError::LowConfidence {
                confidence,
                threshold,
            }) => {
                metrics::counter!("aegis_stimuli_rejected_total", "reason" => "low_confidence")
                    .increment(1);
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
                let reason = match &e {
                    StimulusError::NoRouterConfigured { .. } => "no_router_configured",
                    StimulusError::RouterAgentFailed(_) => "router_agent_failed",
                    StimulusError::ClassificationParseError(_) => "classification_parse_error",
                    StimulusError::UnknownWorkflow { .. } => "unknown_workflow",
                    StimulusError::WorkflowError(_) => "workflow_error",
                    // Already handled above, but be exhaustive
                    StimulusError::LowConfidence { .. } => "low_confidence",
                    StimulusError::IdempotentDuplicate { .. } => "idempotent_duplicate",
                    // Resolved before reaching this match; listed for exhaustiveness.
                    StimulusError::UnregisteredWebhookSource { .. } => {
                        "unregistered_webhook_source"
                    }
                    StimulusError::AmbiguousWebhookRoute { .. } => "ambiguous_webhook_route",
                };
                metrics::counter!("aegis_stimuli_rejected_total", "reason" => reason).increment(1);
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
            version: None,
            tenant_id: Some(stimulus_tenant_id.clone()),
            // Stimulus-triggered workflows inherit the security context from the
            // route's bound context once ADR-083 Phase 2 lands.
            security_context_name: None,
            intent: None,
        };

        let started = self
            .start_workflow_use_case
            .start_execution_for_tenant(&stimulus_tenant_id, workflow_request, identity.as_ref())
            .await
            .map_err(|e| {
                metrics::counter!("aegis_stimuli_rejected_total", "reason" => "workflow_error")
                    .increment(1);
                StimulusError::WorkflowError(e)
            })?;

        // ── Prometheus metrics (ADR-058, BC-8) ───────────────────────────────
        let source_str = stimulus.source.name();
        let mode_str = match decision.mode {
            RoutingMode::Deterministic => "deterministic",
            RoutingMode::LlmClassified => "llm_classified",
        };
        metrics::counter!("aegis_stimuli_ingested_total", "source" => source_str, "routing_mode" => mode_str)
            .increment(1);
        metrics::histogram!("aegis_stimulus_routing_duration_seconds", "routing_mode" => mode_str)
            .record(start.elapsed().as_secs_f64());

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
        tenant_id: &crate::domain::tenant::TenantId,
        source_name: &str,
        workflow_id: crate::domain::workflow::WorkflowId,
    ) -> anyhow::Result<()> {
        let mut registry = self.registry.write().await;
        registry.register_route(tenant_id, source_name, workflow_id)
    }

    async fn remove_route(
        &self,
        tenant_id: &crate::domain::tenant::TenantId,
        source_name: &str,
    ) -> bool {
        let mut registry = self.registry.write().await;
        registry.remove_route(tenant_id, source_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::execution::ExecutionService;
    use crate::application::start_workflow_execution::StartedWorkflowExecution;
    use crate::domain::agent::AgentId;
    use crate::domain::events::StimulusEvent;
    use crate::domain::execution::{Execution, ExecutionId};
    use crate::domain::workflow::WorkflowId;
    use crate::infrastructure::event_bus::{DomainEvent, EventBusError};
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::Stream;
    use serde_json::json;
    use std::pin::Pin;
    use std::sync::Mutex;
    use uuid::Uuid;

    #[derive(Default)]
    struct RecordingExecutionService {
        start_calls: Mutex<Vec<(AgentId, ExecutionInput)>>,
        execution: Mutex<Option<Execution>>,
        exec_id: ExecutionId,
    }

    impl RecordingExecutionService {
        fn with_completed_output(output: &str) -> Self {
            let mut execution = Execution::new(
                AgentId::new(),
                ExecutionInput {
                    intent: None,
                    input: json!({}),
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: Vec::new(),
                },
                1,
                "aegis-system-operator".to_string(),
            );
            execution.start();
            execution.start_iteration("classify".to_string()).unwrap();
            execution.complete_iteration(output.to_string());
            execution.complete();

            Self {
                start_calls: Mutex::new(Vec::new()),
                execution: Mutex::new(Some(execution)),
                exec_id: ExecutionId::new(),
            }
        }

        fn start_calls(&self) -> Vec<(AgentId, ExecutionInput)> {
            self.start_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ExecutionService for RecordingExecutionService {
        async fn start_execution(
            &self,
            agent_id: AgentId,
            input: ExecutionInput,
            _security_context_name: String,
            _identity: Option<&crate::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            self.start_calls.lock().unwrap().push((agent_id, input));
            Ok(self.exec_id)
        }

        async fn start_execution_with_id(
            &self,
            execution_id: ExecutionId,
            agent_id: AgentId,
            input: ExecutionInput,
            _security_context_name: String,
            _identity: Option<&crate::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            self.start_calls.lock().unwrap().push((agent_id, input));
            Ok(execution_id)
        }

        async fn start_child_execution(
            &self,
            _agent_id: AgentId,
            _input: ExecutionInput,
            _parent_execution_id: ExecutionId,
        ) -> Result<ExecutionId> {
            anyhow::bail!("start_child_execution not used in stimulus tests")
        }

        async fn get_execution_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: ExecutionId,
        ) -> Result<Execution> {
            self.execution
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("execution not configured"))
        }

        async fn get_execution_unscoped(&self, _id: ExecutionId) -> Result<Execution> {
            self.execution
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("execution not configured"))
        }

        async fn get_iterations_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _exec_id: ExecutionId,
        ) -> Result<Vec<crate::domain::execution::Iteration>> {
            anyhow::bail!("get_iterations_for_tenant not used in stimulus tests")
        }

        async fn cancel_execution_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: ExecutionId,
        ) -> Result<()> {
            anyhow::bail!("cancel_execution_for_tenant not used in stimulus tests")
        }

        async fn stream_execution(
            &self,
            _id: ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<crate::domain::events::ExecutionEvent>> + Send>>>
        {
            anyhow::bail!("stream_execution not used in stimulus tests")
        }

        async fn stream_agent_events(
            &self,
            _id: AgentId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
            anyhow::bail!("stream_agent_events not used in stimulus tests")
        }

        async fn list_executions_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _agent_id: Option<AgentId>,
            _workflow_id: Option<crate::domain::workflow::WorkflowId>,
            _limit: usize,
        ) -> Result<Vec<Execution>> {
            anyhow::bail!("list_executions_for_tenant not used in stimulus tests")
        }

        async fn delete_execution_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: ExecutionId,
        ) -> Result<()> {
            anyhow::bail!("delete_execution_for_tenant not used in stimulus tests")
        }

        async fn record_llm_interaction(
            &self,
            _execution_id: ExecutionId,
            _iteration: u8,
            _interaction: crate::domain::execution::LlmInteraction,
        ) -> Result<()> {
            anyhow::bail!("record_llm_interaction not used in stimulus tests")
        }

        async fn store_iteration_trajectory(
            &self,
            _execution_id: ExecutionId,
            _iteration: u8,
            _trajectory: Vec<crate::domain::execution::TrajectoryStep>,
        ) -> Result<()> {
            anyhow::bail!("store_iteration_trajectory not used in stimulus tests")
        }
    }

    #[derive(Default)]
    struct RecordingStartWorkflowUseCase {
        calls: Mutex<Vec<StartWorkflowExecutionRequest>>,
        identities: Mutex<Vec<Option<crate::domain::iam::UserIdentity>>>,
        response_execution_id: String,
    }

    impl RecordingStartWorkflowUseCase {
        fn new(response_execution_id: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                identities: Mutex::new(Vec::new()),
                response_execution_id: response_execution_id.to_string(),
            }
        }

        fn calls(&self) -> Vec<StartWorkflowExecutionRequest> {
            self.calls.lock().unwrap().clone()
        }

        fn recorded_identities(&self) -> Vec<Option<crate::domain::iam::UserIdentity>> {
            self.identities.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl StartWorkflowExecutionUseCase for RecordingStartWorkflowUseCase {
        async fn start_execution_for_tenant(
            &self,
            _tenant_id: &TenantId,
            request: StartWorkflowExecutionRequest,
            identity: Option<&crate::domain::iam::UserIdentity>,
        ) -> Result<StartedWorkflowExecution> {
            self.calls.lock().unwrap().push(request.clone());
            self.identities.lock().unwrap().push(identity.cloned());
            Ok(StartedWorkflowExecution {
                execution_id: self.response_execution_id.clone(),
                workflow_id: request.workflow_id.clone(),
                temporal_run_id: "temporal-run".to_string(),
                status: "running".to_string(),
                started_at: Utc::now(),
            })
        }
    }

    fn build_service(
        registry: WorkflowRegistry,
        execution_service: Arc<dyn ExecutionService>,
        starter: Arc<dyn StartWorkflowExecutionUseCase>,
        event_bus: EventBus,
    ) -> StandardStimulusService {
        StandardStimulusService::new(
            Arc::new(RwLock::new(registry)),
            execution_service,
            starter,
            event_bus,
        )
    }

    fn make_workflow_id() -> WorkflowId {
        WorkflowId(Uuid::new_v4())
    }

    fn make_stimulus(source_name: &str, content: &str) -> Stimulus {
        Stimulus::new(
            crate::domain::stimulus::StimulusSource::Webhook {
                source_name: source_name.to_string(),
            },
            content,
        )
    }

    async fn recv_stimulus_event(
        receiver: &mut crate::infrastructure::event_bus::EventReceiver,
    ) -> StimulusEvent {
        match receiver.recv().await.unwrap() {
            DomainEvent::Stimulus(event) => event,
            other => panic!("expected stimulus event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deterministic_route_bypasses_router_and_starts_workflow() {
        let workflow_id = make_workflow_id();
        let mut registry = WorkflowRegistry::new(None);
        registry
            .register_route(&TenantId::consumer(), "github", workflow_id)
            .unwrap();

        let execution_service = Arc::new(RecordingExecutionService::default());
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("wf-exec-123"));
        let event_bus = EventBus::new(8);
        let mut receiver = event_bus.subscribe();
        let service = build_service(
            registry,
            execution_service.clone(),
            starter.clone(),
            event_bus,
        );

        let stimulus = make_stimulus("github", "{\"action\":\"opened\"}").with_headers(
            std::collections::HashMap::from([("x-github-event".to_string(), "issues".to_string())]),
        );

        let response = service.ingest(stimulus.clone(), None).await.unwrap();

        assert_eq!(response.workflow_execution_id, "wf-exec-123");
        assert!(execution_service.start_calls().is_empty());

        let calls = starter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workflow_id, workflow_id.0.to_string());
        assert_eq!(
            calls[0].tenant_id.as_ref().unwrap().as_str(),
            crate::domain::tenant::CONSUMER_SLUG
        );
        assert_eq!(calls[0].input["stimulus_content"], stimulus.content);
        assert_eq!(calls[0].input["stimulus_source"], "github");
        assert_eq!(calls[0].input["headers"]["x-github-event"], "issues");

        assert!(matches!(
            recv_stimulus_event(&mut receiver).await,
            StimulusEvent::StimulusReceived { stimulus_id, ref source, .. }
                if stimulus_id == response.stimulus_id && source == "github"
        ));
        assert!(matches!(
            recv_stimulus_event(&mut receiver).await,
            StimulusEvent::StimulusClassified {
                stimulus_id,
                workflow_id: ref routed_workflow_id,
                confidence,
                ref routing_mode,
                ..
            }
                if stimulus_id == response.stimulus_id
                    && routed_workflow_id == &workflow_id.0.to_string()
                    && (confidence - 1.0).abs() < f64::EPSILON
                    && routing_mode == "Deterministic"
        ));
    }

    #[tokio::test]
    async fn no_router_configured_returns_error_and_publishes_classification_failed() {
        let execution_service = Arc::new(RecordingExecutionService::default());
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("wf-exec-123"));
        let event_bus = EventBus::new(8);
        let mut receiver = event_bus.subscribe();
        let service = build_service(
            WorkflowRegistry::new(None),
            execution_service.clone(),
            starter.clone(),
            event_bus,
        );

        let stimulus = make_stimulus("github", "payload");
        let error = service.ingest(stimulus.clone(), None).await.unwrap_err();

        assert!(matches!(
            error,
            StimulusError::NoRouterConfigured { ref source_name } if source_name == "github"
        ));
        assert!(execution_service.start_calls().is_empty());
        assert!(starter.calls().is_empty());

        assert!(matches!(
            recv_stimulus_event(&mut receiver).await,
            StimulusEvent::StimulusReceived { stimulus_id, .. } if stimulus_id == stimulus.id
        ));
        assert!(matches!(
            recv_stimulus_event(&mut receiver).await,
            StimulusEvent::ClassificationFailed { stimulus_id, ref error, .. }
                if stimulus_id == stimulus.id && error.contains("no router agent configured")
        ));
    }

    #[tokio::test]
    async fn low_confidence_router_result_rejects_and_uses_tenant_header() {
        let router_agent_id = AgentId::new();
        let mut registry = WorkflowRegistry::new(Some(router_agent_id));
        registry.set_confidence_threshold(0.8).unwrap();

        let execution_service = Arc::new(RecordingExecutionService::with_completed_output(
            r#"{"workflow":"deploy-workflow","confidence":0.41}"#,
        ));
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("wf-exec-123"));
        let event_bus = EventBus::new(8);
        let mut receiver = event_bus.subscribe();
        let service = build_service(
            registry,
            execution_service.clone(),
            starter.clone(),
            event_bus,
        );

        let stimulus = make_stimulus("github", "{\"event\":\"push\"}").with_headers(
            std::collections::HashMap::from([(
                "x-aegis-tenant".to_string(),
                "tenant-42".to_string(),
            )]),
        );

        let error = service.ingest(stimulus.clone(), None).await.unwrap_err();

        assert!(matches!(
            error,
            StimulusError::LowConfidence { confidence, threshold }
                if (confidence - 0.41).abs() < f64::EPSILON && (threshold - 0.8).abs() < f64::EPSILON
        ));
        assert!(starter.calls().is_empty());

        let calls = execution_service.start_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, router_agent_id);
        assert_eq!(calls[0].1.input["stimulus"], stimulus.content);
        assert_eq!(calls[0].1.input["tenant_id"], "tenant-42");

        assert!(matches!(
            recv_stimulus_event(&mut receiver).await,
            StimulusEvent::StimulusReceived { stimulus_id, .. } if stimulus_id == stimulus.id
        ));
        assert!(matches!(
            recv_stimulus_event(&mut receiver).await,
            StimulusEvent::StimulusRejected { stimulus_id, ref reason, .. }
                if stimulus_id == stimulus.id && reason.contains("low_confidence")
        ));
    }

    #[tokio::test]
    async fn valid_router_result_resolves_workflow_and_starts_execution() {
        let router_agent_id = AgentId::new();
        let workflow_id = make_workflow_id();
        let mut registry = WorkflowRegistry::new(Some(router_agent_id));
        registry
            .register_route(
                &TenantId::from_string("tenant-99").unwrap(),
                "deploy-workflow",
                workflow_id,
            )
            .unwrap();

        let execution_service = Arc::new(RecordingExecutionService::with_completed_output(
            r#"{"workflow":"deploy-workflow","confidence":0.93}"#,
        ));
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("wf-exec-456"));
        let event_bus = EventBus::new(8);
        let mut receiver = event_bus.subscribe();
        let service = build_service(
            registry,
            execution_service.clone(),
            starter.clone(),
            event_bus,
        );

        let stimulus = make_stimulus("github", "{\"event\":\"deploy\"}").with_headers(
            std::collections::HashMap::from([("x-tenant-id".to_string(), "tenant-99".to_string())]),
        );

        let response = service.ingest(stimulus.clone(), None).await.unwrap();

        assert_eq!(response.workflow_execution_id, "wf-exec-456");
        let execution_calls = execution_service.start_calls();
        assert_eq!(execution_calls.len(), 1);
        assert_eq!(execution_calls[0].0, router_agent_id);
        assert_eq!(execution_calls[0].1.input["tenant_id"], "tenant-99");

        let calls = starter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workflow_id, workflow_id.0.to_string());
        assert_eq!(calls[0].tenant_id.as_ref().unwrap().as_str(), "tenant-99");
        assert_eq!(calls[0].input["stimulus_content"], stimulus.content);

        assert!(matches!(
            recv_stimulus_event(&mut receiver).await,
            StimulusEvent::StimulusReceived { stimulus_id, .. } if stimulus_id == response.stimulus_id
        ));
        assert!(matches!(
            recv_stimulus_event(&mut receiver).await,
            StimulusEvent::StimulusClassified {
                stimulus_id,
                workflow_id: ref routed_workflow_id,
                confidence,
                ref routing_mode,
                ..
            }
                if stimulus_id == response.stimulus_id
                    && routed_workflow_id == &workflow_id.0.to_string()
                    && (confidence - 0.93).abs() < f64::EPSILON
                    && routing_mode == "LlmClassified"
        ));
    }

    #[tokio::test]
    async fn duplicate_idempotency_key_does_not_start_second_workflow() {
        let workflow_id = make_workflow_id();
        let mut registry = WorkflowRegistry::new(None);
        registry
            .register_route(&TenantId::consumer(), "github", workflow_id)
            .unwrap();

        let execution_service = Arc::new(RecordingExecutionService::default());
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("wf-exec-789"));
        let event_bus = EventBus::new(8);
        let mut receiver = event_bus.subscribe();
        let service = build_service(registry, execution_service, starter.clone(), event_bus)
            .with_idempotency_ttl(Duration::from_secs(300));

        let stimulus = make_stimulus("github", "payload").with_idempotency_key("dup-1");
        let first = service.ingest(stimulus.clone(), None).await.unwrap();
        let second = service.ingest(stimulus, None).await.unwrap_err();

        assert_eq!(starter.calls().len(), 1);
        assert!(matches!(
            second,
            StimulusError::IdempotentDuplicate { original_id } if original_id == first.stimulus_id
        ));

        let mut event_count = 0;
        while let Ok(event) = receiver.try_recv() {
            if matches!(event, DomainEvent::Stimulus(_)) {
                event_count += 1;
            }
        }
        assert_eq!(event_count, 2);
        assert!(matches!(receiver.try_recv(), Err(EventBusError::Empty)));
    }

    /// Regression test: identity passed to `ingest()` must be forwarded to
    /// `start_execution_for_tenant` so that user-scoped rate limit counters are
    /// written.  Previously `ingest()` accepted no identity parameter and called
    /// the identity-free `start_execution()` convenience method, causing
    /// `scope_type="user"` rows to never be written.
    #[tokio::test]
    async fn ingest_with_identity_forwards_identity_to_workflow_use_case() {
        use crate::domain::iam::{IdentityKind, UserIdentity, ZaruTier};

        let workflow_id = make_workflow_id();
        let mut registry = WorkflowRegistry::new(None);
        registry
            .register_route(&TenantId::consumer(), "api", workflow_id)
            .unwrap();

        let execution_service = Arc::new(RecordingExecutionService::default());
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("wf-exec-id-threaded"));
        let event_bus = EventBus::new(8);
        let service = build_service(registry, execution_service, starter.clone(), event_bus);

        let identity = UserIdentity {
            sub: "user-sub-abc123".to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: Some("user@example.com".to_string()),
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Pro,
                tenant_id: TenantId::consumer(),
            },
        };

        let stimulus = make_stimulus("api", "{\"event\":\"test\"}");
        let response = service
            .ingest(stimulus, Some(identity.clone()))
            .await
            .unwrap();

        assert_eq!(response.workflow_execution_id, "wf-exec-id-threaded");

        let identities = starter.recorded_identities();
        assert_eq!(identities.len(), 1);
        let forwarded = identities[0].as_ref().expect("identity must be forwarded");
        assert_eq!(forwarded.sub, "user-sub-abc123");
    }

    // ── ADR-097 footgun #3 regression tests ──────────────────────────────
    //
    // Webhooks are unauthenticated and MUST NOT trust caller-supplied
    // tenant headers. The owning tenant is derived exclusively from the
    // tenant-scoped registration table:
    //   * 0 registrations  → reject (404 / UnregisteredWebhookSource)
    //   * 1 registration   → use that tenant
    //   * 2+ registrations → reject (409 / AmbiguousWebhookRoute)

    #[tokio::test]
    async fn unauthenticated_webhook_with_no_registration_is_rejected() {
        let registry = WorkflowRegistry::new(None);
        let execution_service = Arc::new(RecordingExecutionService::default());
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("unused"));
        let event_bus = EventBus::new(8);
        let service = build_service(registry, execution_service, starter, event_bus);

        let stimulus = make_stimulus("ghost-source", "{}");
        let err = service
            .ingest(stimulus, None)
            .await
            .expect_err("must reject webhook with no registration");
        assert!(
            matches!(err, StimulusError::UnregisteredWebhookSource { .. }),
            "expected UnregisteredWebhookSource, got {err:?}"
        );
    }

    #[tokio::test]
    async fn unauthenticated_webhook_ignores_caller_supplied_tenant_header() {
        // The webhook header `x-aegis-tenant` MUST be ignored. Even if
        // the caller injects a header pointing at a real tenant, the
        // ingest path must look up the registration; if there is no
        // registration for the source the webhook is rejected.
        let other_tenant = TenantId::from_realm_slug("other-tenant").unwrap();
        let mut registry = WorkflowRegistry::new(None);
        registry
            .register_route(&other_tenant, "different-source", make_workflow_id())
            .unwrap();

        let execution_service = Arc::new(RecordingExecutionService::default());
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("unused"));
        let event_bus = EventBus::new(8);
        let service = build_service(registry, execution_service, starter, event_bus);

        let stimulus = make_stimulus("api", "{}").with_headers(std::collections::HashMap::from([
            ("x-aegis-tenant".to_string(), "other-tenant".to_string()),
            ("x-tenant-id".to_string(), "other-tenant".to_string()),
        ]));
        let err = service
            .ingest(stimulus, None)
            .await
            .expect_err("must reject — header MUST NOT be trusted");
        assert!(
            matches!(err, StimulusError::UnregisteredWebhookSource { .. }),
            "expected UnregisteredWebhookSource (header ignored), got {err:?}"
        );
    }

    #[tokio::test]
    async fn unauthenticated_webhook_with_ambiguous_registration_is_rejected() {
        let tenant_a = TenantId::from_realm_slug("tenant-a").unwrap();
        let tenant_b = TenantId::from_realm_slug("tenant-b").unwrap();
        let mut registry = WorkflowRegistry::new(None);
        registry
            .register_route(&tenant_a, "shared-source", make_workflow_id())
            .unwrap();
        registry
            .register_route(&tenant_b, "shared-source", make_workflow_id())
            .unwrap();

        let execution_service = Arc::new(RecordingExecutionService::default());
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("unused"));
        let event_bus = EventBus::new(8);
        let service = build_service(registry, execution_service, starter, event_bus);

        let stimulus = make_stimulus("shared-source", "{}");
        let err = service
            .ingest(stimulus, None)
            .await
            .expect_err("must reject ambiguous webhook");
        assert!(
            matches!(
                err,
                StimulusError::AmbiguousWebhookRoute {
                    tenant_count: 2,
                    ..
                }
            ),
            "expected AmbiguousWebhookRoute(2), got {err:?}"
        );
    }

    #[tokio::test]
    async fn unauthenticated_webhook_with_unique_registration_uses_that_tenant() {
        let owning_tenant = TenantId::from_realm_slug("u-abc123").unwrap();
        let workflow_id = make_workflow_id();
        let mut registry = WorkflowRegistry::new(None);
        registry
            .register_route(&owning_tenant, "stripe-events", workflow_id)
            .unwrap();

        let execution_service = Arc::new(RecordingExecutionService::default());
        let starter = Arc::new(RecordingStartWorkflowUseCase::new("wf-id"));
        let event_bus = EventBus::new(8);
        let service = build_service(registry, execution_service, starter.clone(), event_bus);

        let stimulus = make_stimulus("stripe-events", "{}");
        let _resp = service.ingest(stimulus, None).await.unwrap();

        let calls = starter.calls();
        assert_eq!(calls.len(), 1);
        // Workflow request must carry the registration's tenant, NEVER consumer.
        let req_tenant = calls[0]
            .tenant_id
            .as_ref()
            .expect("tenant must be set on workflow request");
        assert_eq!(req_tenant, &owning_tenant);
        assert_ne!(req_tenant, &TenantId::consumer());
    }
}
