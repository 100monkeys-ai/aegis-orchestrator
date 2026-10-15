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
use aegis_orchestrator_core::domain::validation::ValidationResults;
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
/// once saying so, with try 1 recorded exhausted by its clock.
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
    let expected = "Execution ended: 5 s left under the 30 s bound, under the 10 s a try needs";
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
