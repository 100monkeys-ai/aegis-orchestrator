// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Start Workflow Execution Use Case
//!
//! Application service for starting workflow executions with Temporal.
//!
//! # DDD Pattern: Application Service
//!
//! - **Layer:** Application
//! - **Responsibility:** Orchestrate workflow execution startup
//! - **Collaborators:**
//!   - Domain: Workflow, WorkflowExecution aggregates
//!   - Infrastructure: WorkflowRepository, WorkflowExecutionRepository, TemporalClient, EventBus
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for start workflow execution

use crate::application::ports::WorkflowEnginePort;
use crate::domain::execution::ExecutionId;
use crate::domain::iam::UserIdentity;
use crate::domain::repository::{WorkflowExecutionRepository, WorkflowRepository};
use crate::domain::tenant::TenantId;
use crate::domain::workflow::{WorkflowExecution, WorkflowId};
use crate::infrastructure::event_bus::EventBus;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

/// Workflow execution start request
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StartWorkflowExecutionRequest {
    /// Workflow ID to execute
    pub workflow_id: String,

    /// Input context/parameters for workflow
    pub input: serde_json::Value,

    /// Optional blackboard seed: key-value context forwarded to the TypeScript worker at startup.
    /// Values are included in the `StartWorkflowExecution` Temporal payload and accessible
    /// inside the worker via Handlebars templates (e.g. `{{blackboard.judges}}`).
    /// Rust does not mutate the blackboard during workflow execution.
    pub blackboard: Option<serde_json::Value>,

    /// Optional version qualifier for workflow resolution. When set, the workflow is
    /// looked up by name **and** version rather than name alone.
    #[serde(default)]
    pub version: Option<String>,

    #[serde(default)]
    pub tenant_id: Option<TenantId>,

    /// Security context name resolved from the calling agent's SEAL session (ADR-083).
    /// Propagated so the workflow engine can enforce the same security boundary on
    /// child agent executions spawned within workflow states.
    #[serde(default)]
    pub security_context_name: Option<String>,

    /// Optional intent string passed as a top-level field in the Temporal workflow
    /// input (sibling of `input`, not nested inside it).  The TypeScript worker
    /// destructures `GenericWorkflowInput` as `{ intent, ...inputFields }`.
    #[serde(default)]
    pub intent: Option<String>,
}

/// Started workflow execution response
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartedWorkflowExecution {
    pub execution_id: String,
    pub workflow_id: String,
    pub temporal_run_id: String,
    pub status: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Start Workflow Execution Use Case
#[async_trait]
pub trait StartWorkflowExecutionUseCase: Send + Sync {
    async fn start_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        request: StartWorkflowExecutionRequest,
        identity: Option<&UserIdentity>,
    ) -> Result<StartedWorkflowExecution>;

    /// Start a workflow execution
    ///
    /// # Arguments
    ///
    /// * `request` - Start execution request with workflow_id and input
    ///
    /// # Returns
    ///
    /// Started execution metadata with Temporal run_id
    ///
    /// # Errors
    ///
    /// - WorkflowNotFound: Specified workflow_id doesn't exist
    /// - TemporalError: Temporal server unavailable
    /// - PersistenceError: Database save failed
    async fn start_execution(
        &self,
        request: StartWorkflowExecutionRequest,
    ) -> Result<StartedWorkflowExecution> {
        let tenant_id = request
            .tenant_id
            .clone()
            .unwrap_or_else(TenantId::local_default);
        self.start_execution_for_tenant(&tenant_id, request, None)
            .await
    }
}

/// Standard implementation of StartWorkflowExecutionUseCase
pub struct StandardStartWorkflowExecutionUseCase {
    workflow_repository: Arc<dyn WorkflowRepository>,
    execution_repository: Arc<dyn WorkflowExecutionRepository>,
    workflow_engine: Arc<tokio::sync::RwLock<Option<Arc<dyn WorkflowEnginePort>>>>,
    event_bus: Arc<EventBus>,
    /// Optional rate limit enforcer for checking workflow execution quotas (ADR-072).
    rate_limit_enforcer: Option<Arc<dyn crate::domain::rate_limit::RateLimitEnforcer>>,
    /// Optional rate limit policy resolver (ADR-072).
    rate_limit_resolver: Option<Arc<dyn crate::domain::rate_limit::RateLimitPolicyResolver>>,
}

impl StandardStartWorkflowExecutionUseCase {
    const RESERVED_BLACKBOARD_KEYS: [&'static str; 6] = [
        "instruction",
        "input",
        "iteration_number",
        "previous_error",
        "context",
        "workflow",
    ];

    pub fn new(
        workflow_repository: Arc<dyn WorkflowRepository>,
        execution_repository: Arc<dyn WorkflowExecutionRepository>,
        workflow_engine: Arc<tokio::sync::RwLock<Option<Arc<dyn WorkflowEnginePort>>>>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            workflow_repository,
            execution_repository,
            workflow_engine,
            event_bus,
            rate_limit_enforcer: None,
            rate_limit_resolver: None,
        }
    }

    /// Attach rate limiting enforcement for workflow execution quotas (ADR-072).
    pub fn with_rate_limiting(
        mut self,
        enforcer: Arc<dyn crate::domain::rate_limit::RateLimitEnforcer>,
        resolver: Arc<dyn crate::domain::rate_limit::RateLimitPolicyResolver>,
    ) -> Self {
        self.rate_limit_enforcer = Some(enforcer);
        self.rate_limit_resolver = Some(resolver);
        self
    }

    fn normalize_blackboard(
        blackboard: Option<serde_json::Value>,
    ) -> Result<Option<HashMap<String, serde_json::Value>>> {
        match blackboard {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::Object(map)) => {
                if let Some(key) = map.keys().find(|key| {
                    Self::RESERVED_BLACKBOARD_KEYS
                        .iter()
                        .any(|reserved| reserved == key)
                }) {
                    anyhow::bail!("Workflow blackboard overrides contain reserved key '{key}'");
                }

                Ok(Some(map.into_iter().collect()))
            }
            Some(_) => Err(anyhow::anyhow!(
                "Workflow blackboard overrides must be a JSON/YAML object"
            )),
        }
    }
}

#[async_trait]
impl StartWorkflowExecutionUseCase for StandardStartWorkflowExecutionUseCase {
    async fn start_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        request: StartWorkflowExecutionRequest,
        identity: Option<&UserIdentity>,
    ) -> Result<StartedWorkflowExecution> {
        let normalized_blackboard = Self::normalize_blackboard(request.blackboard.clone())?;

        // Step 0: Rate limit check (ADR-072): enforce WorkflowExecution quota at tenant scope
        if let (Some(enforcer), Some(resolver)) =
            (&self.rate_limit_enforcer, &self.rate_limit_resolver)
        {
            use crate::domain::rate_limit::{RateLimitResourceType, RateLimitScope};

            let scope = RateLimitScope::Tenant {
                tenant_id: tenant_id.clone(),
            };

            // Build a default identity for tenant-scoped resolution (user identity not
            // available at this layer — tier defaults apply via the resolver).
            let default_identity = crate::domain::iam::UserIdentity {
                sub: "tenant-scope".to_string(),
                realm_slug: "aegis-system".to_string(),
                email: None,
                identity_kind: crate::domain::iam::IdentityKind::TenantUser {
                    tenant_slug: tenant_id.as_str().to_string(),
                },
            };

            let resource_type = RateLimitResourceType::WorkflowExecution;

            match resolver
                .resolve_policy(&default_identity, tenant_id, &resource_type)
                .await
            {
                Ok(policy) => match enforcer.check_and_increment(&scope, &policy, 1).await {
                    Ok(decision) if !decision.allowed => {
                        let retry_hint = decision
                            .retry_after_seconds
                            .map(|s| format!(", retry after {s}s"))
                            .unwrap_or_default();
                        tracing::warn!(
                            tenant_id = %tenant_id.as_str(),
                            workflow_id = %request.workflow_id,
                            bucket = ?decision.exhausted_bucket,
                            "Rate limit exceeded for WorkflowExecution"
                        );
                        anyhow::bail!(
                            "Rate limit exceeded for workflow execution (tenant: {}){retry_hint}",
                            tenant_id.as_str()
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            tenant_id = %tenant_id.as_str(),
                            error = %e,
                            "Rate limit enforcement error (allowing workflow execution)"
                        );
                    }
                    Ok(_) => {} // allowed
                },
                Err(e) => {
                    tracing::warn!(
                        tenant_id = %tenant_id.as_str(),
                        error = %e,
                        "Rate limit policy resolution failed (allowing workflow execution)"
                    );
                }
            }

            // ADR-072: dual-scope — enforce user-scoped rate limit when identity is available
            if let Some(id) = identity {
                let user_scope = RateLimitScope::User {
                    user_id: id.sub.clone(),
                };
                let resource_type = RateLimitResourceType::WorkflowExecution;

                match resolver.resolve_policy(id, tenant_id, &resource_type).await {
                    Ok(policy) => {
                        match enforcer.check_and_increment(&user_scope, &policy, 1).await {
                            Ok(decision) if !decision.allowed => {
                                let retry_hint = decision
                                    .retry_after_seconds
                                    .map(|s| format!(", retry after {s}s"))
                                    .unwrap_or_default();
                                tracing::warn!(
                                    user_id = %id.sub,
                                    tenant_id = %tenant_id.as_str(),
                                    workflow_id = %request.workflow_id,
                                    bucket = ?decision.exhausted_bucket,
                                    "User-scoped rate limit exceeded for WorkflowExecution"
                                );
                                anyhow::bail!(
                                    "Rate limit exceeded for workflow execution (user: {}){retry_hint}",
                                    id.sub
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    user_id = %id.sub,
                                    error = %e,
                                    "User-scoped rate limit enforcement error (allowing workflow execution)"
                                );
                            }
                            Ok(_) => {} // allowed
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            user_id = %id.sub,
                            error = %e,
                            "User-scoped rate limit policy resolution failed (allowing workflow execution)"
                        );
                    }
                }
            }
        }

        // Step 1: Load workflow from repository
        let workflow = if let Ok(uuid) = uuid::Uuid::parse_str(&request.workflow_id) {
            let id = WorkflowId::from_uuid(uuid);
            self.workflow_repository
                .find_by_id_for_tenant(tenant_id, id)
                .await
        } else if let Some(ref version) = request.version {
            self.workflow_repository
                .resolve_by_name_and_version(tenant_id, &request.workflow_id, version)
                .await
        } else {
            self.workflow_repository
                .resolve_by_name(tenant_id, &request.workflow_id)
                .await
        }
        .context("Failed to query workflow repository")?
        .ok_or_else(|| anyhow::anyhow!("Workflow not found: {}", request.workflow_id))?;

        // Step 2: Create workflow execution aggregate
        let execution_id = ExecutionId(uuid::Uuid::new_v4());
        let mut workflow_execution =
            WorkflowExecution::new(&workflow, execution_id, request.input.clone());

        // Step 3: Merge initial blackboard if provided
        if let Some(blackboard_map) = normalized_blackboard.clone() {
            for (key, value) in blackboard_map {
                workflow_execution.blackboard.set(key, value);
            }
        }

        // Step 4: Persist execution to repository (establishes idempotency key)
        self.execution_repository
            .save_for_tenant(tenant_id, &workflow_execution)
            .await
            .map_err(|error| {
                error!(
                    tenant_id = %tenant_id.as_str(),
                    execution_id = %workflow_execution.id.0,
                    workflow_id = %workflow_execution.workflow_id.0,
                    error = %error,
                    "Failed to persist workflow execution before Temporal start"
                );
                anyhow::Error::new(error)
            })
            .context("Failed to persist workflow execution to repository")?;

        // Step 5: Start execution in Temporal via gRPC
        let engine = {
            let lock = self.workflow_engine.read().await;
            lock.clone()
                .ok_or_else(|| anyhow::anyhow!("Workflow engine not connected yet"))?
        };

        let workflow_id = workflow.id.to_string();
        let temporal_workflow_id = execution_id.0.to_string();

        let temporal_run_id = engine
            .start_workflow(crate::application::ports::StartWorkflowParams {
                workflow_id: &workflow_id,
                execution_id,
                tenant_id: tenant_id.as_str(),
                input: match &request.input {
                    serde_json::Value::Object(map) => {
                        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                    }
                    _ => {
                        // Wrap non-object inputs
                        let mut map = HashMap::new();
                        map.insert("input".to_string(), request.input.clone());
                        map
                    }
                },
                blackboard: normalized_blackboard,
                security_context_name: request.security_context_name.clone(),
                intent: request.intent.clone(),
            })
            .await
            .context("Failed to start workflow execution in Temporal")?;

        self.execution_repository
            .update_temporal_linkage_for_tenant(
                tenant_id,
                execution_id,
                &temporal_workflow_id,
                &temporal_run_id,
            )
            .await
            .map_err(|error| {
                error!(
                    tenant_id = %tenant_id.as_str(),
                    execution_id = %execution_id.0,
                    workflow_id = %workflow.id.0,
                    temporal_workflow_id = %temporal_workflow_id,
                    temporal_run_id = %temporal_run_id,
                    error = %error,
                    "Failed to persist Temporal linkage for workflow execution"
                );
                anyhow::Error::new(error)
            })
            .context("Failed to persist Temporal linkage for workflow execution")?;

        // Step 7: Publish domain event
        self.event_bus.publish_workflow_event(
            crate::domain::events::WorkflowEvent::WorkflowExecutionStarted {
                execution_id,
                workflow_id: workflow.id,
                started_at: Utc::now(),
            },
        );

        // Step 8: Record Prometheus metrics (ADR-058, BC-3)
        metrics::counter!("aegis_workflow_executions_total", "status" => "started").increment(1);
        metrics::gauge!("aegis_workflow_executions_active").increment(1.0);

        Ok(StartedWorkflowExecution {
            execution_id: execution_id.0.to_string(),
            workflow_id,
            temporal_run_id,
            status: "running".to_string(),
            started_at: Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::WorkflowEvent;
    use crate::domain::repository::{RepositoryError, WorkflowExecutionRepository};
    use crate::domain::workflow::{
        StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowId,
        WorkflowMetadata, WorkflowSpec, WorkflowState,
    };
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::{
        InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    struct StartCall {
        workflow_id: String,
        execution_id: ExecutionId,
        input: HashMap<String, serde_json::Value>,
        blackboard: Option<HashMap<String, serde_json::Value>>,
    }

    struct RecordingWorkflowEngine {
        calls: Mutex<Vec<StartCall>>,
        run_id: String,
    }

    impl RecordingWorkflowEngine {
        fn new(run_id: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                run_id: run_id.to_string(),
            }
        }

        fn calls(&self) -> Vec<StartCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WorkflowEnginePort for RecordingWorkflowEngine {
        async fn register_workflow(
            &self,
            _definition: &crate::application::temporal_mapper::TemporalWorkflowDefinition,
        ) -> Result<()> {
            Ok(())
        }

        async fn start_workflow(
            &self,
            params: crate::application::ports::StartWorkflowParams<'_>,
        ) -> Result<String> {
            self.calls.lock().unwrap().push(StartCall {
                workflow_id: params.workflow_id.to_string(),
                execution_id: params.execution_id,
                input: params.input,
                blackboard: params.blackboard,
            });
            Ok(self.run_id.clone())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TemporalLinkageCall {
        tenant_id: TenantId,
        execution_id: ExecutionId,
        temporal_workflow_id: String,
        temporal_run_id: String,
    }

    #[derive(Default)]
    struct RecordingWorkflowExecutionRepository {
        executions: Mutex<HashMap<ExecutionId, WorkflowExecution>>,
        temporal_linkage_updates: Mutex<Vec<TemporalLinkageCall>>,
    }

    impl RecordingWorkflowExecutionRepository {
        fn temporal_linkage_updates(&self) -> Vec<TemporalLinkageCall> {
            self.temporal_linkage_updates.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WorkflowExecutionRepository for RecordingWorkflowExecutionRepository {
        async fn save_for_tenant(
            &self,
            _tenant_id: &TenantId,
            execution: &WorkflowExecution,
        ) -> Result<(), RepositoryError> {
            self.executions
                .lock()
                .unwrap()
                .insert(execution.id, execution.clone());
            Ok(())
        }

        async fn find_by_id_for_tenant(
            &self,
            _tenant_id: &TenantId,
            id: ExecutionId,
        ) -> Result<Option<WorkflowExecution>, RepositoryError> {
            Ok(self.executions.lock().unwrap().get(&id).cloned())
        }

        async fn find_active_for_tenant(
            &self,
            _tenant_id: &TenantId,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            Ok(self.executions.lock().unwrap().values().cloned().collect())
        }

        async fn find_by_workflow_for_tenant(
            &self,
            _tenant_id: &TenantId,
            workflow_id: crate::domain::workflow::WorkflowId,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            let mut matches: Vec<_> = self
                .executions
                .lock()
                .unwrap()
                .values()
                .filter(|execution| execution.workflow_id == workflow_id)
                .cloned()
                .collect();
            matches.sort_by_key(|execution| execution.started_at);
            matches.reverse();
            Ok(matches.into_iter().skip(offset).take(limit).collect())
        }

        async fn update_temporal_linkage_for_tenant(
            &self,
            tenant_id: &TenantId,
            execution_id: ExecutionId,
            temporal_workflow_id: &str,
            temporal_run_id: &str,
        ) -> Result<(), RepositoryError> {
            self.temporal_linkage_updates
                .lock()
                .unwrap()
                .push(TemporalLinkageCall {
                    tenant_id: tenant_id.clone(),
                    execution_id,
                    temporal_workflow_id: temporal_workflow_id.to_string(),
                    temporal_run_id: temporal_run_id.to_string(),
                });
            Ok(())
        }

        async fn append_event(
            &self,
            _execution_id: ExecutionId,
            _temporal_sequence_number: i64,
            _event_type: String,
            _payload: serde_json::Value,
            _iteration_number: Option<u8>,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }

        async fn find_events_by_execution(
            &self,
            _id: ExecutionId,
            _limit: usize,
            _offset: usize,
        ) -> Result<Vec<crate::domain::workflow::WorkflowExecutionEventRecord>, RepositoryError>
        {
            Ok(vec![])
        }

        async fn count_by_workflow_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _workflow_id: WorkflowId,
        ) -> Result<i64, RepositoryError> {
            Ok(0)
        }

        async fn list_paginated_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _limit: usize,
            _offset: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }
    }

    fn build_test_workflow(name: &str) -> Workflow {
        let mut states = HashMap::new();
        states.insert(
            StateName::new("START").unwrap(),
            WorkflowState {
                kind: StateKind::System {
                    command: "echo ready".to_string(),
                    env: HashMap::new(),
                    workdir: None,
                },
                transitions: vec![TransitionRule {
                    condition: TransitionCondition::Always,
                    target: StateName::new("END").unwrap(),
                    feedback: None,
                }],
                timeout: None,
            },
        );
        states.insert(
            StateName::new("END").unwrap(),
            WorkflowState {
                kind: StateKind::System {
                    command: "echo done".to_string(),
                    env: HashMap::new(),
                    workdir: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );

        Workflow::new(
            WorkflowMetadata {
                name: name.to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                labels: HashMap::new(),
                annotations: HashMap::new(),
                input_schema: None,
            },
            WorkflowSpec {
                initial_state: StateName::new("START").unwrap(),
                context: HashMap::new(),
                states,
                storage: Default::default(),
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn start_execution_wraps_scalar_input_seeds_blackboard_and_publishes_event() {
        let workflow = build_test_workflow("build-and-test");
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        workflow_repo.save(&workflow).await.unwrap();
        let execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let engine = Arc::new(RecordingWorkflowEngine::new("temporal-run-123"));
        let event_bus = Arc::new(EventBus::new(32));
        let mut receiver = event_bus.subscribe();

        let service = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo.clone(),
            execution_repo.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(engine.clone()))),
            event_bus,
        );

        let result = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: workflow.metadata.name.clone(),
                input: json!("run-ci"),
                blackboard: Some(json!({
                    "judges": ["lint-judge", "security-judge"],
                    "validation_threshold": 0.85
                })),
                version: None,
                tenant_id: Some(TenantId::local_default()),
                security_context_name: None,
                intent: None,
            })
            .await
            .unwrap();

        assert_eq!(result.status, "running");
        assert_eq!(result.temporal_run_id, "temporal-run-123");

        let calls = engine.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workflow_id, workflow.id.to_string());
        assert_eq!(calls[0].input.get("input"), Some(&json!("run-ci")));
        assert_eq!(
            calls[0]
                .blackboard
                .as_ref()
                .and_then(|blackboard| blackboard.get("validation_threshold")),
            Some(&json!(0.85))
        );
        assert_eq!(calls[0].execution_id.to_string(), result.execution_id);

        let execution_id = ExecutionId::from_string(&result.execution_id).unwrap();
        let persisted = execution_repo
            .find_by_id(execution_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.blackboard.get("validation_threshold"),
            Some(&json!(0.85))
        );
        assert_eq!(
            persisted.blackboard.get("judges"),
            Some(&json!(["lint-judge", "security-judge"]))
        );

        let event = receiver.recv().await.unwrap();
        match event {
            DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionStarted {
                execution_id,
                workflow_id,
                ..
            }) => {
                assert_eq!(execution_id.to_string(), result.execution_id);
                assert_eq!(workflow_id, workflow.id);
            }
            other => panic!("unexpected event type: {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_execution_saves_before_engine_connection_check() {
        let workflow = build_test_workflow("save-first-workflow");
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        workflow_repo.save(&workflow).await.unwrap();
        let execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());

        let service = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo,
            execution_repo.clone(),
            Arc::new(tokio::sync::RwLock::new(None)),
            Arc::new(EventBus::new(8)),
        );

        let err = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: workflow.metadata.name.clone(),
                input: json!({ "branch": "main" }),
                blackboard: None,
                version: None,
                tenant_id: Some(TenantId::local_default()),
                security_context_name: None,
                intent: None,
            })
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("Workflow engine not connected yet")
        );

        let active = execution_repo.find_active().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].workflow_id, workflow.id);
    }

    #[tokio::test]
    async fn start_execution_rejects_non_object_blackboard() {
        let workflow = build_test_workflow("invalid-blackboard");
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        workflow_repo.save(&workflow).await.unwrap();
        let execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let engine = Arc::new(RecordingWorkflowEngine::new("temporal-run-invalid"));

        let service = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo,
            execution_repo,
            Arc::new(tokio::sync::RwLock::new(Some(engine))),
            Arc::new(EventBus::new(8)),
        );

        let err = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: workflow.metadata.name.clone(),
                input: json!({ "target": "release" }),
                blackboard: Some(json!(["not", "an", "object"])),
                version: None,
                tenant_id: Some(TenantId::local_default()),
                security_context_name: None,
                intent: None,
            })
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("Workflow blackboard overrides must be a JSON/YAML object")
        );
    }

    #[tokio::test]
    async fn start_execution_rejects_reserved_blackboard_keys() {
        let workflow = build_test_workflow("reserved-blackboard");
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        workflow_repo.save(&workflow).await.unwrap();
        let execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let engine = Arc::new(RecordingWorkflowEngine::new("temporal-run-reserved"));

        let service = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo,
            execution_repo,
            Arc::new(tokio::sync::RwLock::new(Some(engine))),
            Arc::new(EventBus::new(8)),
        );

        let err = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: workflow.metadata.name.clone(),
                input: json!({ "target": "release" }),
                blackboard: Some(json!({ "input": "reserved" })),
                version: None,
                tenant_id: Some(TenantId::local_default()),
                security_context_name: None,
                intent: None,
            })
            .await
            .unwrap_err();

        assert!(err.to_string().contains("reserved key 'input'"));
    }

    #[tokio::test]
    async fn start_execution_resolves_workflow_by_uuid_identifier() {
        let workflow = build_test_workflow("uuid-resolve-workflow");
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        workflow_repo.save(&workflow).await.unwrap();
        let execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let engine = Arc::new(RecordingWorkflowEngine::new("temporal-run-xyz"));

        let service = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo,
            execution_repo,
            Arc::new(tokio::sync::RwLock::new(Some(engine.clone()))),
            Arc::new(EventBus::new(8)),
        );

        let response = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: workflow.id.to_string(),
                input: json!({ "target": "release" }),
                blackboard: None,
                version: None,
                tenant_id: Some(TenantId::local_default()),
                security_context_name: None,
                intent: None,
            })
            .await
            .unwrap();

        let calls = engine.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workflow_id, workflow.id.to_string());
        assert_eq!(calls[0].input.get("target"), Some(&json!("release")));
        assert_eq!(response.temporal_run_id, "temporal-run-xyz");
    }

    #[tokio::test]
    async fn start_execution_persists_temporal_linkage_after_temporal_start() {
        let workflow = build_test_workflow("temporal-linkage-workflow");
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        workflow_repo.save(&workflow).await.unwrap();
        let execution_repo = Arc::new(RecordingWorkflowExecutionRepository::default());
        let engine = Arc::new(RecordingWorkflowEngine::new("temporal-run-456"));

        let service = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo,
            execution_repo.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(engine))),
            Arc::new(EventBus::new(8)),
        );

        let response = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: workflow.metadata.name.clone(),
                input: json!({ "prompt": "ship it" }),
                blackboard: None,
                version: None,
                tenant_id: Some(TenantId::local_default()),
                security_context_name: None,
                intent: None,
            })
            .await
            .unwrap();

        let execution_id = ExecutionId::from_string(&response.execution_id).unwrap();
        let updates = execution_repo.temporal_linkage_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0],
            TemporalLinkageCall {
                tenant_id: TenantId::local_default(),
                execution_id,
                temporal_workflow_id: response.execution_id.clone(),
                temporal_run_id: "temporal-run-456".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn start_execution_returns_not_found_for_unknown_workflow() {
        struct EmptyWorkflowRepo;

        #[async_trait]
        impl WorkflowRepository for EmptyWorkflowRepo {
            async fn save_for_tenant(
                &self,
                _tenant_id: &TenantId,
                workflow: &Workflow,
            ) -> Result<(), RepositoryError> {
                self.save(workflow).await
            }

            async fn save(&self, _workflow: &Workflow) -> Result<(), RepositoryError> {
                Ok(())
            }

            async fn find_by_id_for_tenant(
                &self,
                _tenant_id: &TenantId,
                id: WorkflowId,
            ) -> Result<Option<Workflow>, RepositoryError> {
                self.find_by_id(id).await
            }

            async fn find_by_id(
                &self,
                _id: WorkflowId,
            ) -> Result<Option<Workflow>, RepositoryError> {
                Ok(None)
            }

            async fn find_by_name_for_tenant(
                &self,
                _tenant_id: &TenantId,
                name: &str,
            ) -> Result<Option<Workflow>, RepositoryError> {
                self.find_by_name(name).await
            }

            async fn find_by_name(&self, _name: &str) -> Result<Option<Workflow>, RepositoryError> {
                Ok(None)
            }

            async fn find_by_name_and_version_for_tenant(
                &self,
                _tenant_id: &TenantId,
                _name: &str,
                _version: &str,
            ) -> Result<Option<Workflow>, RepositoryError> {
                Ok(None)
            }

            async fn list_all_for_tenant(
                &self,
                _tenant_id: &TenantId,
            ) -> Result<Vec<Workflow>, RepositoryError> {
                self.list_all().await
            }

            async fn list_all(&self) -> Result<Vec<Workflow>, RepositoryError> {
                Ok(vec![])
            }

            async fn delete_for_tenant(
                &self,
                _tenant_id: &TenantId,
                id: WorkflowId,
            ) -> Result<(), RepositoryError> {
                self.delete(id).await
            }

            async fn delete(&self, _id: WorkflowId) -> Result<(), RepositoryError> {
                Ok(())
            }

            async fn resolve_by_name(
                &self,
                _tenant_id: &TenantId,
                _name: &str,
            ) -> Result<Option<Workflow>, RepositoryError> {
                Ok(None)
            }
            async fn resolve_by_name_and_version(
                &self,
                _tenant_id: &TenantId,
                _name: &str,
                _version: &str,
            ) -> Result<Option<Workflow>, RepositoryError> {
                Ok(None)
            }
            async fn list_visible(
                &self,
                _tenant_id: &TenantId,
            ) -> Result<Vec<Workflow>, RepositoryError> {
                Ok(vec![])
            }
            async fn list_global(&self) -> Result<Vec<Workflow>, RepositoryError> {
                Ok(vec![])
            }
            async fn update_scope(
                &self,
                _id: WorkflowId,
                _new_scope: crate::domain::workflow::WorkflowScope,
                _new_tenant_id: &TenantId,
            ) -> Result<(), RepositoryError> {
                Ok(())
            }

            async fn list_by_name_for_tenant(
                &self,
                _tenant_id: &TenantId,
                _name: &str,
            ) -> Result<Vec<Workflow>, RepositoryError> {
                Ok(vec![])
            }

            async fn find_by_name_visible(
                &self,
                _tenant_id: &TenantId,
                _name: &str,
            ) -> Result<Option<Workflow>, RepositoryError> {
                Ok(None)
            }
        }

        let service = StandardStartWorkflowExecutionUseCase::new(
            Arc::new(EmptyWorkflowRepo),
            Arc::new(InMemoryWorkflowExecutionRepository::new()),
            Arc::new(tokio::sync::RwLock::new(None)),
            Arc::new(EventBus::new(8)),
        );

        let err = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: "does-not-exist".to_string(),
                input: json!({}),
                blackboard: None,
                version: None,
                tenant_id: Some(TenantId::local_default()),
                security_context_name: None,
                intent: None,
            })
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Workflow not found"));
    }
}
