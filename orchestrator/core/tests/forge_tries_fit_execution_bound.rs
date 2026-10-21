// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A try starts only when it fits under the execution's bound (AEGIS ADR-141,
//! Update of 2026-10-15, clause (1)).
//!
//! In Forge run `40e66741` ARCHITECT's first try ended at its 600 s iteration
//! timeout and the second was cut by the execution's 900 s bound from inside
//! the try, the run failing with "Execution timed out after 900 seconds".
//! The first try always starts; a later try starts only when the time left
//! under the bound is at least the iteration timeout, and otherwise the
//! execution ends at once, saying how much was left and how much a try needs.

use aegis_orchestrator_core::domain::agent::{ExecutionMode, ExecutionStrategy, ImagePullPolicy};
use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionInput, ExhaustedBy};
use aegis_orchestrator_core::domain::runtime::{
    AgentRuntime, InstanceId, InstanceStatus, ResourceLimits, RuntimeConfig, RuntimeError,
    TaskInput, TaskOutput,
};
use aegis_orchestrator_core::domain::supervisor::{Supervisor, SupervisorObserver};
use aegis_orchestrator_core::domain::validation::{
    GradientResult, GradientValidator, ValidationContext, ValidationPipeline, ValidationResults,
    ValidatorEntry, ValidatorKind,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// A runtime whose every try works longer than any clock in the test.
#[derive(Default)]
struct SlowRuntime {
    spawns: Mutex<u32>,
}

#[async_trait]
impl AgentRuntime for SlowRuntime {
    async fn spawn(&self, _config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
        let mut spawns = self.spawns.lock().await;
        *spawns += 1;
        Ok(InstanceId::new(format!("instance-{spawns}")))
    }

    async fn execute(
        &self,
        _id: &InstanceId,
        _input: TaskInput,
    ) -> Result<TaskOutput, RuntimeError> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(TaskOutput {
            result: serde_json::Value::String("late".to_string()),
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

/// Records which iterations started and which were recorded exhausted.
#[derive(Default)]
struct RecordingObserver {
    starts: Mutex<Vec<u8>>,
    exhausted: Mutex<Vec<(u8, ExhaustedBy)>>,
}

#[async_trait]
impl SupervisorObserver for RecordingObserver {
    async fn on_iteration_start(&self, iteration: u8, _prompt: &str) {
        self.starts.lock().await.push(iteration);
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
        _passed: bool,
    ) {
    }
    async fn on_iteration_exhausted(&self, iteration: u8, by: ExhaustedBy) {
        self.exhausted.lock().await.push((iteration, by));
    }
}

/// An iterative agent with a 10 s iteration timeout under a 30 s bound.
fn config() -> RuntimeConfig {
    RuntimeConfig {
        language: "python".to_string(),
        version: "3.12".to_string(),
        isolation: "process".to_string(),
        env: HashMap::new(),
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        container_uid: 1000,
        container_gid: 1000,
        resources: ResourceLimits {
            cpu_millis: None,
            memory_bytes: None,
            disk_bytes: None,
            timeout_seconds: Some(30),
        },
        execution: ExecutionStrategy {
            mode: ExecutionMode::Iterative,
            iteration_timeout: Some("10s".to_string()),
            ..Default::default()
        },
        volumes: Vec::new(),
        keep_container_on_failure: false,
        image: "python:3.12".to_string(),
        bootstrap_path: None,
        execution_id: ExecutionId::new(),
        workflow_execution_id: None,
        program_files: Vec::new(),
        program_input: None,
    }
}

fn input() -> ExecutionInput {
    ExecutionInput {
        intent: Some("Design the change".to_string()),
        input: serde_json::json!({}),
        workspace_volume_id: None,
        workspace_volume_mount_path: None,
        workspace_remote_path: None,
        workflow_execution_id: None,
        attachments: Vec::new(),
    }
}

/// Try 1 ends at its 10 s clock and the wait after it runs to 25 s, leaving
/// 5 s of the 30 s bound: try 2 is not started, and the execution ends at
/// once saying so, try 1's cause first (AEGIS ADR-141, Update of 2026-10-21,
/// clause 14), with try 1 recorded exhausted by its clock.
#[tokio::test(start_paused = true)]
async fn a_try_that_cannot_fit_under_the_bound_is_not_started() {
    let runtime = Arc::new(SlowRuntime::default());
    let observer = Arc::new(RecordingObserver::default());
    let started = tokio::time::Instant::now();

    let result = Supervisor::new(runtime.clone())
        .run_loop(
            config(),
            input(),
            3,
            observer.clone(),
            CancellationToken::new(),
            None,
        )
        .await;
    let ended_after = started.elapsed();

    let mut complaints = Vec::new();
    let expected = "Execution ended after 1 try: the last attempt ran past its time limit of 10 s \
                    and was stopped; a further try does not fit: 5 s left under the 30 s bound, \
                    under the 10 s a try needs";
    match &result {
        Err(error) if error.to_string().contains(expected) => {}
        other => complaints.push(format!(
            "the execution ended {other:?}, expected \"{expected}\""
        )),
    }
    let spawns = *runtime.spawns.lock().await;
    if spawns != 1 {
        complaints.push(format!("{spawns} tries were spawned, expected 1"));
    }
    let starts = observer.starts.lock().await.clone();
    if starts != vec![1] {
        complaints.push(format!("iterations started: {starts:?}, expected [1]"));
    }
    let exhausted = observer.exhausted.lock().await.clone();
    if exhausted != vec![(1, ExhaustedBy::Clock)] {
        complaints.push(format!(
            "recorded exhausted: {exhausted:?}, expected [(1, Clock)]"
        ));
    }
    if ended_after > Duration::from_secs(26) {
        complaints.push(format!(
            "the execution ended {ended_after:?} after it started, expected at once after the 25 s mark"
        ));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// The first try always starts, even under a bound shorter than one
/// iteration timeout; the bound then cuts it as before.
#[tokio::test(start_paused = true)]
async fn the_first_try_always_starts() {
    let runtime = Arc::new(SlowRuntime::default());
    let observer = Arc::new(RecordingObserver::default());
    let mut config = config();
    config.resources.timeout_seconds = Some(5);

    let result = Supervisor::new(runtime.clone())
        .run_loop(
            config,
            input(),
            3,
            observer.clone(),
            CancellationToken::new(),
            None,
        )
        .await;

    assert_eq!(*runtime.spawns.lock().await, 1, "the first try was spawned");
    assert!(
        matches!(result, Err(RuntimeError::TimedOut(5))),
        "the bound cut the first try: {result:?}"
    );
}

/// A runtime whose every try answers at once.
#[derive(Default)]
struct AnsweringRuntime {
    spawns: Mutex<u32>,
}

#[async_trait]
impl AgentRuntime for AnsweringRuntime {
    async fn spawn(&self, _config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
        let mut spawns = self.spawns.lock().await;
        *spawns += 1;
        Ok(InstanceId::new(format!("instance-{spawns}")))
    }

    async fn execute(
        &self,
        _id: &InstanceId,
        _input: TaskInput,
    ) -> Result<TaskOutput, RuntimeError> {
        Ok(TaskOutput {
            result: serde_json::Value::String("Dear Ana, thank you for your note.".to_string()),
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

/// A judge that takes 25 s and then rejects the answer, or answers the
/// error its wait timed out with.
struct SlowJudge {
    rejects: bool,
}

#[async_trait]
impl GradientValidator for SlowJudge {
    async fn validate(&self, _ctx: &ValidationContext) -> anyhow::Result<GradientResult> {
        tokio::time::sleep(Duration::from_secs(25)).await;
        if self.rejects {
            Ok(GradientResult {
                score: 0.0,
                confidence: 0.9,
                reasoning: "the reply answers none of the questions".to_string(),
                signals: vec![],
                metadata: HashMap::new(),
            })
        } else {
            Err(anyhow::anyhow!(
                "Judge 'email-draft-judge' timed out after 180 seconds"
            ))
        }
    }
}

/// The end of an execution whose one try answered at once and whose judge
/// took 25 s of the 30 s bound, leaving 5 s, under the 10 s a try needs.
async fn ended_after_the_judge(rejects: bool) -> (Result<String, RuntimeError>, u32) {
    let runtime = Arc::new(AnsweringRuntime::default());
    let pipeline = Arc::new(ValidationPipeline::new(vec![ValidatorEntry {
        kind: ValidatorKind::Semantic,
        validator: Box::new(SlowJudge { rejects }),
        min_score: 0.7,
        min_confidence: 0.0,
    }]));
    let result = Supervisor::new(runtime.clone())
        .run_loop(
            config(),
            input(),
            3,
            Arc::new(RecordingObserver::default()),
            CancellationToken::new(),
            Some(pipeline),
        )
        .await;
    let spawns = *runtime.spawns.lock().await;
    (result, spawns)
}

/// AEGIS ADR-141, Update of 2026-10-21, Trigger (q): a judge rejects try 1,
/// the bound refuses try 2, and the execution's error reads clause 14's
/// sentence, the judge's reason before "a further try does not fit".
/// Execution `1c86c074` ended "Execution ended: 666 s left under the 900 s
/// bound, under the 780 s a try needs", the verdict dropped.
#[tokio::test(start_paused = true)]
async fn a_try_refused_after_a_rejection_names_the_judges_reason_first() {
    let (result, spawns) = ended_after_the_judge(true).await;
    let expected = "Execution ended after 1 try: the last attempt's output was rejected: \
                    Validation score 0.00 below minimum 0.70: the reply answers none of the \
                    questions; a further try does not fit: 5 s left under the 30 s bound, under \
                    the 10 s a try needs";
    let mut complaints = Vec::new();
    match &result {
        Err(RuntimeError::ExecutionFailed(reason)) if reason == expected => {}
        other => complaints.push(format!(
            "the execution ended {other:?}, expected \"{expected}\""
        )),
    }
    if spawns != 1 {
        complaints.push(format!("{spawns} tries were spawned, expected 1"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// Trigger (q), the judge's wait timed out (execution `098812cc`): the
/// error names the judge's timeout before "a further try does not fit".
#[tokio::test(start_paused = true)]
async fn a_try_refused_after_a_judges_timeout_names_the_timeout_first() {
    let (result, spawns) = ended_after_the_judge(false).await;
    let expected = "Execution ended after 1 try: the last attempt failed validation: Judge \
                    'email-draft-judge' timed out after 180 seconds; a further try does not fit: \
                    5 s left under the 30 s bound, under the 10 s a try needs";
    let mut complaints = Vec::new();
    match &result {
        Err(RuntimeError::ExecutionFailed(reason)) if reason == expected => {}
        other => complaints.push(format!(
            "the execution ended {other:?}, expected \"{expected}\""
        )),
    }
    if spawns != 1 {
        complaints.push(format!("{spawns} tries were spawned, expected 1"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
