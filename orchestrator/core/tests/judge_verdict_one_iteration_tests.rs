// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! AEGIS ADR-131 U38: the goal judge's complete verdict, "Nothing is missing.",
//! fenced as the judge's model writes it, completes the judge's execution in
//! one iteration through the supervisor, with the validation the judge's own
//! manifest declares. Before U38 its schema refused that reasoning as shorter
//! than 20 characters and the supervisor refined the same verdict to the cap
//! (execution `52b4ae5f`, 2026-10-07: three iterations, 88 s, for an 11 s
//! verdict).

use std::collections::HashMap;
use std::sync::Arc;

use aegis_orchestrator_core::domain::agent::{AgentManifest, ImagePullPolicy, ValidatorSpec};
use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionInput};
use aegis_orchestrator_core::domain::runtime::{
    AgentRuntime, InstanceId, InstanceStatus, ResourceLimits, RuntimeConfig, RuntimeError,
    TaskInput, TaskOutput,
};
use aegis_orchestrator_core::domain::supervisor::{Supervisor, SupervisorObserver};
use aegis_orchestrator_core::domain::validation::{
    OutputGradientValidator, ValidationPipeline, ValidationResults, ValidatorEntry, ValidatorKind,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const GOAL_JUDGE_YAML: &str = include_str!("../../../cli/templates/agents/goal-judge.yaml");

/// A judge whose model answers the same fenced verdict on every iteration.
struct SameVerdict {
    verdict: String,
}

#[async_trait::async_trait]
impl AgentRuntime for SameVerdict {
    async fn spawn(&self, _config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
        Ok(InstanceId::new("judge".to_string()))
    }

    async fn execute(
        &self,
        _id: &InstanceId,
        _input: TaskInput,
    ) -> Result<TaskOutput, RuntimeError> {
        Ok(TaskOutput {
            result: serde_json::Value::String(self.verdict.clone()),
            logs: vec![],
            tool_calls: vec![],
            exit_code: 0,
            trajectory: vec![],
        })
    }

    async fn terminate(&self, _id: &InstanceId) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus, RuntimeError> {
        Ok(InstanceStatus {
            id: id.clone(),
            state: "running".to_string(),
            uptime_seconds: 0,
            memory_usage_mb: 0,
            cpu_usage_percent: 0.0,
        })
    }
}

/// Counts the iterations started and the validation verdict of each.
#[derive(Default)]
struct Iterations {
    started: Mutex<Vec<u8>>,
    validated: Mutex<Vec<bool>>,
}

#[async_trait::async_trait]
impl SupervisorObserver for Iterations {
    async fn on_iteration_start(&self, iteration: u8, _prompt: &str) {
        self.started.lock().await.push(iteration);
    }
    async fn on_console_output(&self, _iteration: u8, _stream: &str, _content: &str) {}
    async fn on_iteration_complete(&self, _iteration: u8, _result: &str, _exit_code: i64) {}
    async fn on_iteration_fail(&self, _iteration: u8, _error: &str) {}
    async fn on_instance_spawned(&self, _iteration: u8, _instance_id: &InstanceId) {}
    async fn on_instance_terminated(&self, _iteration: u8, _instance_id: &InstanceId) {}
    async fn on_validation_complete(
        &self,
        _iteration: u8,
        _results: &ValidationResults,
        passed: bool,
    ) {
        self.validated.lock().await.push(passed);
    }
}

/// The goal judge's manifest, its execution strategy, and the validation
/// pipeline its `validation` list builds: each `json_schema` step is the
/// output validator on format `json`, at its `min_score`, as the
/// orchestrator's pipeline factory builds it.
fn goal_judge() -> (
    aegis_orchestrator_core::domain::agent::ExecutionStrategy,
    ValidationPipeline,
) {
    let manifest: AgentManifest = serde_yaml::from_str(GOAL_JUDGE_YAML).expect("goal-judge parses");
    let strategy = manifest
        .spec
        .execution
        .expect("goal-judge declares its execution");
    let entries = strategy
        .validation
        .clone()
        .expect("goal-judge declares its validation")
        .into_iter()
        .map(|spec| match spec {
            ValidatorSpec::JsonSchema { schema, min_score } => ValidatorEntry {
                kind: ValidatorKind::Output,
                validator: Box::new(OutputGradientValidator::new(
                    "json".to_string(),
                    Some(schema),
                    None,
                )),
                min_score,
                min_confidence: 0.0,
            },
            other => panic!("goal-judge declares a validator this test does not build: {other:?}"),
        })
        .collect();
    (strategy, ValidationPipeline::new(entries))
}

fn runtime_config(
    execution: aegis_orchestrator_core::domain::agent::ExecutionStrategy,
) -> RuntimeConfig {
    RuntimeConfig {
        language: "python".to_string(),
        version: "3.11".to_string(),
        isolation: "inherit".to_string(),
        env: HashMap::new(),
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        container_uid: 1000,
        container_gid: 1000,
        resources: ResourceLimits {
            cpu_millis: None,
            memory_bytes: None,
            disk_bytes: None,
            timeout_seconds: None,
        },
        execution,
        volumes: Vec::new(),
        keep_container_on_failure: false,
        image: "python:3.11".to_string(),
        bootstrap_path: None,
        execution_id: ExecutionId::new(),
        workflow_execution_id: None,
        program_files: Vec::new(),
        program_input: None,
    }
}

#[tokio::test]
async fn the_goal_judges_complete_fenced_verdict_completes_in_one_iteration() {
    let verdict = serde_json::json!({
        "score": 1.0,
        "confidence": 1.0,
        "reasoning": "Nothing is missing.",
        "signals": [
            {"category": "delivered", "score": 1.0, "message": "Shown."},
            {"category": "evidence", "score": 1.0, "message": "Borne out."},
            {"category": "chain", "score": 1.0, "message": "All ran."},
            {"category": "feasibility", "score": 1.0, "message": "Met."},
            {"category": "alignment", "score": 1.0, "message": "As asked."},
        ],
    });
    let output = format!(
        "```json\n{}\n```",
        serde_json::to_string_pretty(&verdict).unwrap()
    );
    let (strategy, pipeline) = goal_judge();
    let max_retries = strategy.max_retries;
    let iterations = Arc::new(Iterations::default());

    let result = Supervisor::new(Arc::new(SameVerdict {
        verdict: output.clone(),
    }))
    .run_loop(
        runtime_config(strategy),
        ExecutionInput {
            intent: Some("Judge the goal.".to_string()),
            input: serde_json::json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        },
        max_retries,
        iterations.clone(),
        CancellationToken::new(),
        Some(Arc::new(pipeline)),
    )
    .await;

    let started = iterations.started.lock().await.clone();
    let validated = iterations.validated.lock().await.clone();
    assert!(
        result.is_ok() && started == vec![1],
        "the judge's complete verdict took {} iterations of {max_retries} (validations {validated:?}): {result:?}",
        started.len()
    );
    assert_eq!(result.unwrap(), output);
}
