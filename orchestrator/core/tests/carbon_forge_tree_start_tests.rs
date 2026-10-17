// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A carbon-forge run names its tree (AEGIS ADR-147 C2, C4)
//!
//! AEGIS ADR-147 (Proposed, page revision 55541) C2: a run names one tree
//! by the dispatch key `trees`, `[{workspace, path, revision}]`, and its
//! Nuclear Notes binding by `contexts`. Like `repositories` and `contexts`,
//! `trees` is the platform's: a start checks its shape before anything is
//! held, and the tree is materialized into the run's volume before its
//! first agent (C4), so a node that cannot materialize a tree refuses the
//! run rather than start it on nothing.
//!
//! | Clause | Test |
//! |---|---|
//! | C2, C4: no tree service, no run | `a_start_naming_a_tree_on_a_node_that_cannot_materialize_it_is_refused` |
//! | C2: the shape | `a_trees_entry_without_its_revision_is_refused_by_the_shape` |

use aegis_orchestrator_core::application::ports::{StartWorkflowParams, WorkflowEnginePort};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;
use aegis_orchestrator_core::domain::repository::WorkflowRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::Workflow;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const BINDING: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
const WORKSPACE: &str = "c8b0fadf-176c-418a-a8a4-a148dcf9fd1d";
const TREE_PATH: &str = "code/aegis-platform-deployment";

/// Records the input each workflow start hands Temporal.
#[derive(Default)]
struct RecordingEngine {
    inputs: Mutex<Vec<HashMap<String, Value>>>,
}

#[async_trait]
impl WorkflowEnginePort for RecordingEngine {
    async fn register_workflow(&self, _def: &TemporalWorkflowDefinition) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_workflow(&self, params: StartWorkflowParams<'_>) -> anyhow::Result<String> {
        self.inputs.lock().unwrap().push(params.input.clone());
        Ok("recorded-run-id".to_string())
    }
}

/// A one-state workflow standing for carbon-forge's start.
fn workflow() -> Workflow {
    WorkflowParser::parse_yaml(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: carbon-forge-start
spec:
  initial_state: ANALYZE
  states:
    ANALYZE:
      kind: System
      command: "true"
      transitions: []
"#,
    )
    .unwrap()
}

/// Start `workflow()` with `input` on a node with no tree service: the
/// start's answer and what Temporal was handed.
async fn start(input: Value) -> (anyhow::Result<String>, Vec<HashMap<String, Value>>) {
    let workflow = workflow();
    let tenant = TenantId::consumer();
    let workflows = Arc::new(InMemoryWorkflowRepository::new());
    workflows.save_for_tenant(&tenant, &workflow).await.unwrap();
    let engine = Arc::new(RecordingEngine::default());
    let use_case = StandardStartWorkflowExecutionUseCase::new(
        workflows,
        Arc::new(InMemoryWorkflowExecutionRepository::new()),
        Arc::new(tokio::sync::RwLock::new(Some(
            engine.clone() as Arc<dyn WorkflowEnginePort>
        ))),
        Arc::new(EventBus::new(8)),
    );
    let started = use_case
        .start_execution(StartWorkflowExecutionRequest {
            workflow_id: workflow.metadata.name.clone(),
            input,
            blackboard: None,
            version: None,
            tenant_id: Some(tenant),
            security_context_name: None,
            intent: None,
        })
        .await
        .map(|s| s.execution_id);
    let inputs = engine.inputs.lock().unwrap().clone();
    (started, inputs)
}

#[tokio::test]
async fn a_start_naming_a_tree_on_a_node_that_cannot_materialize_it_is_refused() {
    let (started, temporal) = start(json!({
        "task": "the task",
        "trees": [{ "workspace": WORKSPACE, "path": TREE_PATH, "revision": 4242 }],
        "contexts": { "nuclear-notes": BINDING }
    }))
    .await;
    let Err(refusal) = started else {
        panic!(
            "AEGIS ADR-147 C2, C4: a run naming a tree started with nothing to materialize it; Temporal was handed {temporal:?}"
        );
    };
    assert!(
        refusal.to_string().contains("trees"),
        "AEGIS ADR-147 C2: the refusal does not name the run's trees: {refusal:#}"
    );
    assert!(temporal.is_empty(), "a refused start reached Temporal");
}

#[tokio::test]
async fn a_trees_entry_without_its_revision_is_refused_by_the_shape() {
    let (started, temporal) = start(json!({
        "task": "the task",
        "trees": [{ "workspace": WORKSPACE, "path": TREE_PATH }],
        "contexts": { "nuclear-notes": BINDING }
    }))
    .await;
    let Err(refusal) = started else {
        panic!(
            "AEGIS ADR-147 C2: a trees entry naming no revision was not refused; Temporal was handed {temporal:?}"
        );
    };
    assert!(
        refusal
            .to_string()
            .contains("[{workspace, path, revision}]"),
        "AEGIS ADR-147 C2: the refusal does not give the shape [{{workspace, path, revision}}]: {refusal:#}"
    );
    assert!(temporal.is_empty(), "a refused start reached Temporal");
}
