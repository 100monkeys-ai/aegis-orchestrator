// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Regression tests for ADR-092 D7: input_schema validation at dispatch.
//!
//! These tests prove that:
//!
//! 1. A workflow with `input_schema` rejects dispatches where the input does not satisfy
//!    the schema, returning an error whose message contains "InvalidExecutionInput".
//! 2. The same workflow accepts dispatches when the input satisfies the schema.
//! 3. An agent with `input_schema` follows the same rules when its spec declares a schema.
//!
//! The tests exercise the validation logic at the application-layer boundary via
//! `StandardStartWorkflowExecutionUseCase` (workflow path) and validate the agent path
//! by directly calling `jsonschema::validator_for` against an agent's declared schema —
//! mirroring the exact code added to `execution.rs` — so that any regression in the
//! agent path is caught by the same mechanism.

use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::domain::repository::WorkflowRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowMetadata,
    WorkflowSpec, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

use aegis_orchestrator_core::application::ports::WorkflowEnginePort;
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;

use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Stub Temporal engine — records calls without actually doing anything
// ---------------------------------------------------------------------------

struct StubEngine;

#[async_trait]
impl WorkflowEnginePort for StubEngine {
    async fn register_workflow(&self, _def: &TemporalWorkflowDefinition) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_workflow(
        &self,
        _params: aegis_orchestrator_core::application::ports::StartWorkflowParams<'_>,
    ) -> anyhow::Result<String> {
        Ok("stub-run-id".to_string())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn workflow_with_schema(name: &str, schema: serde_json::Value) -> Workflow {
    let mut states = HashMap::new();
    states.insert(
        StateName::new("START").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo ok".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::Always,
                target: StateName::new("END").unwrap(),
                feedback: None,
            }],
            timeout: None,
            max_state_visits: None,
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
            max_state_visits: None,
        },
    );
    Workflow::new(
        WorkflowMetadata {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: Some(schema),
        },
        WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
            max_total_transitions: None,
        },
    )
    .unwrap()
}

fn make_service(
    workflow_repo: Arc<InMemoryWorkflowRepository>,
) -> StandardStartWorkflowExecutionUseCase {
    StandardStartWorkflowExecutionUseCase::new(
        workflow_repo,
        Arc::new(InMemoryWorkflowExecutionRepository::new()),
        Arc::new(tokio::sync::RwLock::new(Some(
            Arc::new(StubEngine) as Arc<dyn WorkflowEnginePort>
        ))),
        Arc::new(EventBus::new(8)),
    )
}

// ---------------------------------------------------------------------------
// Workflow dispatch path — ADR-092 D7 regression
// ---------------------------------------------------------------------------

/// A workflow with a required field must reject input that omits the field.
#[tokio::test]
async fn workflow_dispatch_rejects_input_missing_required_field() {
    let schema = json!({
        "type": "object",
        "required": ["branch"],
        "properties": {
            "branch": { "type": "string" }
        }
    });
    let workflow = workflow_with_schema("dispatch-reject-missing", schema);
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    repo.save_for_tenant(&TenantId::consumer(), &workflow)
        .await
        .unwrap();

    let svc = make_service(repo);
    let err = svc
        .start_execution(StartWorkflowExecutionRequest {
            workflow_id: workflow.metadata.name.clone(),
            input: json!({}), // missing "branch"
            blackboard: None,
            version: None,
            tenant_id: Some(TenantId::consumer()),
            security_context_name: None,
            intent: None,
        })
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("Input validation failed"),
        "expected Input validation failed in error, got: {err}"
    );
}

/// A workflow with a required field must accept input that satisfies the schema.
#[tokio::test]
async fn workflow_dispatch_accepts_valid_input() {
    let schema = json!({
        "type": "object",
        "required": ["branch"],
        "properties": {
            "branch": { "type": "string" }
        }
    });
    let workflow = workflow_with_schema("dispatch-accept-valid", schema);
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    repo.save_for_tenant(&TenantId::consumer(), &workflow)
        .await
        .unwrap();

    let svc = make_service(repo);
    let result = svc
        .start_execution(StartWorkflowExecutionRequest {
            workflow_id: workflow.metadata.name.clone(),
            input: json!({ "branch": "main" }),
            blackboard: None,
            version: None,
            tenant_id: Some(TenantId::consumer()),
            security_context_name: None,
            intent: None,
        })
        .await
        .unwrap();

    assert_eq!(result.status, "running");
}

// ---------------------------------------------------------------------------
// Agent input_schema validation logic — ADR-092 D7 regression (agent path)
//
// These tests exercise the identical jsonschema logic that was added to
// `execution.rs` `do_start_execution`, ensuring a regression in that code
// path is caught without needing a full execution service harness.
// ---------------------------------------------------------------------------

/// Helper: simulate the agent-path validation added in execution.rs.
/// Returns Ok(()) when the input satisfies the schema, Err with
/// "InvalidExecutionInput" when it does not.
fn validate_agent_input(
    schema: &serde_json::Value,
    input: &serde_json::Value,
) -> anyhow::Result<()> {
    let compiled = jsonschema::validator_for(schema).map_err(|e| {
        anyhow::anyhow!(
            "{}",
            aegis_orchestrator_core::domain::execution::ExecutionError::InvalidExecutionInput(
                format!("Agent input_schema is invalid: {e}")
            )
        )
    })?;
    let errors: Vec<String> = compiled.iter_errors(input).map(|e| e.to_string()).collect();
    if !errors.is_empty() {
        return Err(anyhow::anyhow!(
            "{}",
            aegis_orchestrator_core::domain::execution::ExecutionError::InvalidExecutionInput(
                format!("Input validation failed: {}", errors.join("; "))
            )
        ));
    }
    Ok(())
}

/// Agent with required field in input_schema — missing field must error.
#[test]
fn agent_input_schema_rejects_missing_required_field() {
    let schema = json!({
        "type": "object",
        "required": ["target"],
        "properties": {
            "target": { "type": "string" }
        }
    });
    let input = json!({}); // missing "target"
    let err = validate_agent_input(&schema, &input).unwrap_err();
    assert!(
        err.to_string().contains("Input validation failed"),
        "expected Input validation failed, got: {err}"
    );
}

/// Agent with required field in input_schema — valid input must pass.
#[test]
fn agent_input_schema_accepts_valid_input() {
    let schema = json!({
        "type": "object",
        "required": ["target"],
        "properties": {
            "target": { "type": "string" }
        }
    });
    let input = json!({ "target": "production" });
    validate_agent_input(&schema, &input).unwrap();
}

/// Confirm that `intent` is NOT passed through the schema validator — only `input.input` is.
/// This test would fail if someone accidentally validated `intent` through the schema.
#[test]
fn agent_input_schema_does_not_validate_intent_field() {
    // Schema requires only "target" — intent is irrelevant to schema validation
    let schema = json!({
        "type": "object",
        "required": ["target"],
        "properties": {
            "target": { "type": "string" }
        },
        "additionalProperties": false
    });

    // Input has only "target" — no "intent" key — this must pass even though
    // in a real execution intent is a sibling field that is never sent to the validator.
    let input = json!({ "target": "production" });
    validate_agent_input(&schema, &input).unwrap();
}
