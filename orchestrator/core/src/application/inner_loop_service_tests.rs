// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The supervisor and the inner loop as the daemon builds them, driven end to
//! end with the model and the container doubled (retry-knows-what-failed:
//! AEGIS ADR-005 "Context Injection", ADR-040's output cap, ADR-131 U16a and
//! U19).
//!
//! The container double plays the agent's bootstrap: it posts the generate
//! request to the inner loop, runs each dispatched command through the real
//! `assets/bootstrap.py` `run_dispatch` (a real shell, a real timeout), posts
//! the result back, and ends on the final answer. The model double answers
//! from a script and records every conversation it was sent.

use super::*;
use crate::application::agent::AgentLifecycleService;
use crate::domain::agent::{Agent, AgentManifest, AgentStatus};
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{Execution, ExecutionInput, Iteration, IterationError};
use crate::domain::goal::{AliasTableJudgeContext, JudgeContextSource};
use crate::domain::llm::{
    ChatResponse, ChatToolCall, FinishReason, GenerationResponse, LLMError, LLMProvider, TokenUsage,
};
use crate::domain::node_config::LLMProviderConfig;
use crate::domain::repository::AgentVersion;
use crate::domain::runtime::{
    AgentRuntime, InstanceId, InstanceStatus, ResourceLimits, RuntimeConfig, RuntimeError,
    TaskInput, TaskOutput,
};
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::security_context::SecurityContext;
use crate::domain::supervisor::{Supervisor, SupervisorObserver};
use crate::domain::validation::{
    GradientResult, GradientValidator, ValidationContext, ValidationPipeline, ValidationResults,
    ValidatorEntry, ValidatorKind,
};
use crate::infrastructure::event_bus::{DomainEvent, EventBus};
use async_trait::async_trait;
use futures::Stream;
use serde_json::json;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use tokio_util::sync::CancellationToken;

const CONTEXT: &str = "inner-loop-test-context";
/// The alias the test registry maps (`ProviderRegistry::new_for_test`).
const ALIAS: &str = "default";
/// The alias's room in the test alias table: a prompt limit of 120,000
/// bytes, so a command's output enters the conversation as at most 15,000.
const CONTEXT_WINDOW: u32 = 128_000;
const MAX_OUTPUT_TOKENS: u32 = 8_000;
const BOUND: usize = 15_000;

// ---------------------------------------------------------------------------
// The orchestrator's records
// ---------------------------------------------------------------------------

/// The executions, as the execution service holds them: the trajectory the
/// inner loop stores lands on the iteration it names; and the execution
/// events the inner loop publishes, in the order it published them.
#[derive(Default)]
struct Records(
    StdMutex<HashMap<ExecutionId, Execution>>,
    StdMutex<Vec<ExecutionEvent>>,
);

impl Records {
    fn get(&self, id: ExecutionId) -> Execution {
        self.0.lock().unwrap().get(&id).cloned().expect("execution")
    }
    fn update(&self, id: ExecutionId, f: impl FnOnce(&mut Execution)) {
        let mut map = self.0.lock().unwrap();
        f(map.get_mut(&id).expect("execution"));
    }
    /// Each published event as its variant's name and its fields, in order.
    fn events(&self) -> Vec<(String, Value)> {
        self.1
            .lock()
            .unwrap()
            .iter()
            .map(|event| {
                let value = serde_json::to_value(event).expect("an event serialises");
                let (name, fields) = value
                    .as_object()
                    .and_then(|o| o.iter().next())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .expect("an externally tagged event");
                (name, fields)
            })
            .collect()
    }
    /// The published events of one variant, in order.
    fn events_named(&self, name: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|(n, _)| n == name)
            .map(|(_, fields)| fields)
            .collect()
    }
}

#[async_trait]
impl ExecutionService for Records {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        Ok(execution_id)
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> anyhow::Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_for_tenant(
        &self,
        _: &TenantId,
        id: ExecutionId,
    ) -> anyhow::Result<Execution> {
        self.get_execution_unscoped(id).await
    }
    async fn get_execution_unscoped(&self, id: ExecutionId) -> anyhow::Result<Execution> {
        self.0
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("execution not found"))
    }
    async fn get_iterations_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> anyhow::Result<Vec<Iteration>> {
        anyhow::bail!("not exercised")
    }
    async fn cancel_execution_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> anyhow::Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn stream_execution(
        &self,
        _: ExecutionId,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<DomainEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn list_executions_for_tenant(
        &self,
        _: &TenantId,
        _: Option<AgentId>,
        _: Option<crate::domain::workflow::WorkflowId>,
        _: usize,
    ) -> anyhow::Result<Vec<Execution>> {
        anyhow::bail!("not exercised")
    }
    async fn delete_execution_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> anyhow::Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: crate::domain::execution::LlmInteraction,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn store_iteration_trajectory(
        &self,
        execution_id: ExecutionId,
        iteration: u8,
        trajectory: Vec<TrajectoryStep>,
    ) -> anyhow::Result<()> {
        let mut map = self.0.lock().unwrap();
        if let Some(exec) = map.get_mut(&execution_id) {
            let _ = exec.store_iteration_trajectory(iteration, trajectory);
        }
        Ok(())
    }
    async fn record_refinement(
        &self,
        execution_id: ExecutionId,
        iteration: u8,
        refinement: crate::domain::execution::CodeDiff,
    ) -> anyhow::Result<()> {
        let mut map = self.0.lock().unwrap();
        if let Some(exec) = map.get_mut(&execution_id) {
            let _ = exec.store_refinement(iteration, refinement);
        }
        Ok(())
    }
    async fn record_constructed_history(
        &self,
        execution_id: ExecutionId,
        iteration: u8,
        exhaustion: crate::domain::execution::Exhaustion,
    ) -> anyhow::Result<()> {
        let mut map = self.0.lock().unwrap();
        if let Some(exec) = map.get_mut(&execution_id) {
            let _ = exec.store_constructed_history(iteration, exhaustion);
        }
        Ok(())
    }
    async fn record_execution_event(&self, event: ExecutionEvent) {
        self.1.lock().unwrap().push(event);
    }
}

/// The same records as the execution repository the daemon gives the
/// supervisor (`server.rs`, `Supervisor::new(..).with_execution_repository`).
#[async_trait]
impl crate::domain::repository::ExecutionRepository for Records {
    async fn save_for_tenant(
        &self,
        _: &TenantId,
        execution: &Execution,
    ) -> Result<(), crate::domain::repository::RepositoryError> {
        self.0
            .lock()
            .unwrap()
            .insert(execution.id, execution.clone());
        Ok(())
    }
    async fn find_by_id_for_tenant(
        &self,
        _: &TenantId,
        id: ExecutionId,
    ) -> Result<Option<Execution>, crate::domain::repository::RepositoryError> {
        Ok(self.0.lock().unwrap().get(&id).cloned())
    }
    async fn find_by_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: usize,
    ) -> Result<Vec<Execution>, crate::domain::repository::RepositoryError> {
        Ok(vec![])
    }
    async fn find_by_workflow_for_tenant(
        &self,
        _: &TenantId,
        _: crate::domain::workflow::WorkflowId,
        _: usize,
    ) -> Result<Vec<Execution>, crate::domain::repository::RepositoryError> {
        Ok(vec![])
    }
    async fn find_by_workflow_execution_for_tenant(
        &self,
        _: &TenantId,
        _: uuid::Uuid,
    ) -> Result<Vec<Execution>, crate::domain::repository::RepositoryError> {
        Ok(vec![])
    }
    async fn find_recent_for_tenant(
        &self,
        _: &TenantId,
        _: usize,
    ) -> Result<Vec<Execution>, crate::domain::repository::RepositoryError> {
        Ok(vec![])
    }
    async fn list_recent_all_paginated(
        &self,
        _: usize,
        _: usize,
    ) -> Result<Vec<Execution>, crate::domain::repository::RepositoryError> {
        Ok(vec![])
    }
    async fn delete_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> Result<(), crate::domain::repository::RepositoryError> {
        Ok(())
    }
    async fn count_by_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> Result<i64, crate::domain::repository::RepositoryError> {
        Ok(0)
    }
    async fn find_by_id_unscoped(
        &self,
        id: ExecutionId,
    ) -> Result<Option<Execution>, crate::domain::repository::RepositoryError> {
        Ok(self.0.lock().unwrap().get(&id).cloned())
    }
    async fn count_running(
        &self,
        _: &TenantId,
    ) -> Result<u64, crate::domain::repository::RepositoryError> {
        Ok(0)
    }
}

/// The supervisor's observer as the execution service is to the record: an
/// iteration started, failed or completed on the execution.
struct RecordingObserver {
    records: Arc<Records>,
    execution_id: ExecutionId,
}

#[async_trait]
impl SupervisorObserver for RecordingObserver {
    async fn on_iteration_start(&self, _iteration: u8, prompt: &str) {
        self.records.update(self.execution_id, |e| {
            e.start_iteration(prompt.to_string())
                .expect("iteration starts");
        });
    }
    async fn on_console_output(&self, _iteration: u8, _stream: &str, _content: &str) {}
    async fn on_iteration_complete(&self, _iteration: u8, result: &str, _exit_code: i64) {
        self.records.update(self.execution_id, |e| {
            e.complete_iteration(result.to_string())
        });
    }
    async fn on_iteration_fail(&self, _iteration: u8, error: &str) {
        self.records.update(self.execution_id, |e| {
            e.fail_iteration(IterationError {
                message: error.to_string(),
                details: None,
            })
        });
    }
    async fn on_instance_spawned(&self, _iteration: u8, _instance_id: &InstanceId) {}
    async fn on_instance_terminated(&self, _iteration: u8, _instance_id: &InstanceId) {}
    async fn on_validation_complete(&self, _: u8, _: &ValidationResults, _: bool) {}
    /// As the execution monitor: the iteration recorded exhausted, with the
    /// history the next try is opened from.
    async fn on_iteration_exhausted(
        &self,
        iteration: u8,
        by: crate::domain::execution::ExhaustedBy,
    ) {
        self.records.update(self.execution_id, |e| {
            let constructed = crate::domain::execution::ConstructedHistory::after(
                &e.iterations,
                iteration,
                chrono::Utc::now(),
            );
            e.store_constructed_history(
                iteration,
                crate::domain::execution::Exhaustion {
                    by,
                    constructed: Some(constructed),
                },
            )
            .expect("the iteration exists");
        });
    }
}

struct OneAgent(Agent);

#[async_trait]
impl AgentLifecycleService for OneAgent {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: crate::domain::agent::AgentScope,
        _: Option<&UserIdentity>,
    ) -> anyhow::Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> anyhow::Result<Agent> {
        Ok(self.0.clone())
    }
    async fn update_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: AgentManifest,
    ) -> anyhow::Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> anyhow::Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_for_tenant(&self, _: &TenantId) -> anyhow::Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn lookup_agent_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> anyhow::Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> anyhow::Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

struct NoOpPublisher;

#[async_trait]
impl crate::domain::fsal::EventPublisher for NoOpPublisher {
    async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
}

fn agent(iteration_timeout: &str, llm_timeout_seconds: u64) -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(&format!(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: inner-loop-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: {ALIAS}
  execution:
    mode: iterative
    max_retries: 2
    iteration_timeout: "{iteration_timeout}"
    llm_timeout_seconds: {llm_timeout_seconds}
  tools: []
"#
    ))
    .unwrap();
    Agent {
        id: AgentId::new(),
        tenant_id: TenantId::default(),
        scope: crate::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn every_tool_context() -> SecurityContext {
    SecurityContext {
        name: CONTEXT.to_string(),
        description: "inner loop test".to_string(),
        capabilities: vec![crate::domain::security_context::Capability {
            tool_pattern: "*".to_string(),
            path_allowlist: None,
            command_allowlist: None,
            subcommand_allowlist: None,
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        }],
        deny_list: vec![],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

/// The alias table the daemon hands its judges, with the test alias's room.
fn alias_table() -> Arc<dyn JudgeContextSource> {
    let providers: Vec<LLMProviderConfig> = serde_yaml::from_str(&format!(
        r#"- name: test
  type: openai-compatible
  endpoint: "https://api.example.invalid/v1"
  enabled: true
  models:
    - alias: {ALIAS}
      model: test-model
      capabilities: ["chat"]
      context_window: {CONTEXT_WINDOW}
      max_output_tokens: {MAX_OUTPUT_TOKENS}
"#
    ))
    .expect("provider block parses");
    Arc::new(AliasTableJudgeContext::from_providers(&providers))
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

type Turn = Box<dyn Fn(&[ChatMessage]) -> Result<ChatResponse, LLMError> + Send + Sync>;

/// Answers the n-th call with the n-th turn; records each conversation.
struct ScriptedModel {
    turns: Vec<Turn>,
    calls: StdMutex<Vec<Vec<ChatMessage>>>,
    /// The calls (by index) that never answer, as a silent model does.
    silent: Vec<usize>,
}

impl ScriptedModel {
    fn conversation(&self, call: usize) -> Vec<ChatMessage> {
        self.calls.lock().unwrap()[call].clone()
    }
}

#[async_trait]
impl LLMProvider for ScriptedModel {
    async fn generate(
        &self,
        _prompt: &str,
        _options: &GenerationOptions,
    ) -> Result<crate::domain::llm::GenerationResponse, LLMError> {
        unimplemented!("not used by the inner loop")
    }
    async fn generate_chat(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolSchema],
        _options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(messages.to_vec());
            calls.len() - 1
        };
        if self.silent.contains(&n) {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
        let turn = self
            .turns
            .get(n)
            .unwrap_or_else(|| panic!("the model was called {} times", n + 1));
        turn(messages)
    }
    async fn health_check(&self) -> Result<(), LLMError> {
        Ok(())
    }
}

fn run_command(args: serde_json::Value) -> Turn {
    Box::new(move |_| {
        Ok(ChatResponse::ToolCalls(vec![ChatToolCall {
            id: format!("call-{}", uuid::Uuid::new_v4()),
            name: "cmd.run".to_string(),
            arguments: args.clone(),
        }]))
    })
}

/// The model calls `tool` with `args`.
fn call_tool(tool: &str, args: serde_json::Value) -> Turn {
    let tool = tool.to_string();
    Box::new(move |_| {
        Ok(ChatResponse::ToolCalls(vec![ChatToolCall {
            id: format!("call-{}", uuid::Uuid::new_v4()),
            name: tool.clone(),
            arguments: args.clone(),
        }]))
    })
}

fn answer(text: &str) -> Turn {
    let text = text.to_string();
    Box::new(move |_| {
        Ok(ChatResponse::FinalText(GenerationResponse {
            text: text.clone(),
            usage: TokenUsage::default(),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            finish_reason: FinishReason::Stop,
        }))
    })
}

fn fails(text: &str) -> Turn {
    let text = text.to_string();
    Box::new(move |_| Err(LLMError::Network(text.clone())))
}

/// The provider ends the generation at its time limit after 120.5 s, as
/// Workers AI did in execution eea195cd (HTTP 408, code 3046), in the error
/// the adapter builds for it.
fn ended_at_the_time_limit() -> Turn {
    Box::new(|_| {
        Err(
            crate::infrastructure::llm::openai::provider_time_limit_error(
                "test-model",
                120_500,
                r#"{"errors":[{"message":"AiError: AiError: Request timeout","code":3046}]}"#,
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// The container
// ---------------------------------------------------------------------------

/// Runs one dispatch message through the real bootstrap's `run_dispatch`.
async fn bootstrap_run_dispatch(message: &OrchestratorMessage, execution_id: &str) -> Value {
    let bootstrap = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/bootstrap.py")
        .canonicalize()
        .expect("assets/bootstrap.py");
    let script = format!(
        "import importlib.util, json, sys\n\
         sys.dont_write_bytecode = True\n\
         spec = importlib.util.spec_from_file_location('b', {path:?})\n\
         b = importlib.util.module_from_spec(spec); spec.loader.exec_module(b)\n\
         msg = json.loads(sys.stdin.read())\n\
         sys.stdout.write(json.dumps(b.run_dispatch(msg, sys.argv[1])))\n",
        path = bootstrap.to_string_lossy()
    );
    let mut child = tokio::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(execution_id)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("python3 runs the bootstrap");
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(serde_json::to_string(message).unwrap().as_bytes())
            .await
            .unwrap();
    }
    let out = child.wait_with_output().await.unwrap();
    assert!(
        out.status.success(),
        "run_dispatch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("a dispatch_result")
}

/// The agent's container: its bootstrap's loop against the inner loop.
struct BootstrapContainer {
    inner_loop: Arc<InnerLoopService>,
    agent_id: AgentId,
    execution_id: ExecutionId,
    iteration: StdMutex<u8>,
}

#[async_trait]
impl AgentRuntime for BootstrapContainer {
    async fn spawn(&self, config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
        let n = config
            .env
            .get("AEGIS_ITERATION")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        *self.iteration.lock().unwrap() = n;
        Ok(InstanceId::new(format!("container-{n}")))
    }

    async fn execute(
        &self,
        _id: &InstanceId,
        input: TaskInput,
    ) -> Result<TaskOutput, RuntimeError> {
        let iteration = *self.iteration.lock().unwrap();
        let execution_id = self.execution_id.to_string();
        let failed = |e: anyhow::Error| {
            RuntimeError::ExecutionFailed(format!(
                "the bootstrap exited 1: the orchestrator answered an error: {e}"
            ))
        };
        let mut message = self
            .inner_loop
            .handle_agent_message(AgentMessage::Generate {
                agent_id: self.agent_id.to_string(),
                execution_id: execution_id.clone(),
                iteration_number: iteration,
                prompt: input.prompt.clone(),
                messages: Vec::new(),
                model_alias: ALIAS.to_string(),
            })
            .await
            .map_err(failed)?;
        loop {
            match message {
                OrchestratorMessage::Dispatch { dispatch_id, .. } => {
                    let result = bootstrap_run_dispatch(&message, &execution_id).await;
                    message = self
                        .inner_loop
                        .handle_agent_message(AgentMessage::DispatchResult {
                            execution_id: execution_id.clone(),
                            dispatch_id,
                            exit_code: result["exit_code"].as_i64().unwrap() as i32,
                            stdout: result["stdout"].as_str().unwrap().to_string(),
                            stderr: result["stderr"].as_str().unwrap().to_string(),
                            duration_ms: result["duration_ms"].as_u64().unwrap(),
                            truncated: result["truncated"].as_bool().unwrap(),
                        })
                        .await
                        .map_err(failed)?;
                }
                OrchestratorMessage::Final { content, .. } => {
                    return Ok(TaskOutput {
                        result: Value::String(content),
                        logs: vec![],
                        tool_calls: vec![],
                        exit_code: 0,
                        trajectory: vec![],
                    });
                }
            }
        }
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

/// A judge that passes every output and keeps what it was given.
#[derive(Default)]
struct RecordingJudge {
    given: Arc<StdMutex<Vec<ValidationContext>>>,
}

#[async_trait]
impl GradientValidator for RecordingJudge {
    async fn validate(&self, ctx: &ValidationContext) -> anyhow::Result<GradientResult> {
        self.given.lock().unwrap().push(ctx.clone());
        Ok(GradientResult {
            score: 1.0,
            confidence: 1.0,
            reasoning: "ok".to_string(),
            signals: vec![],
            metadata: HashMap::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// The daemon's world
// ---------------------------------------------------------------------------

struct World {
    records: Arc<Records>,
    inner_loop: Arc<InnerLoopService>,
    model: Arc<ScriptedModel>,
    agent: Agent,
    execution_id: ExecutionId,
    /// The volumes the file tools find for an execution.
    volumes: crate::application::nfs_gateway::NfsVolumeRegistry,
    /// The volume records the file system layer reads.
    volume_records: Arc<crate::infrastructure::repositories::InMemoryVolumeRepository>,
}

async fn world(agent: Agent, turns: Vec<Turn>) -> World {
    world_full(agent, turns, Vec::new(), every_tool_context()).await
}

/// [`world`], with the model silent on the calls `silent` names.
async fn world_with_silent_calls(agent: Agent, turns: Vec<Turn>, silent: Vec<usize>) -> World {
    world_full(agent, turns, silent, every_tool_context()).await
}

/// A world whose execution runs under `context`.
async fn world_in(agent: Agent, turns: Vec<Turn>, context: SecurityContext) -> World {
    world_full(agent, turns, Vec::new(), context).await
}

/// A world with the model silent on the calls `silent` names, whose
/// execution runs under `context`.
async fn world_full(
    agent: Agent,
    turns: Vec<Turn>,
    silent: Vec<usize>,
    context: SecurityContext,
) -> World {
    let records = Arc::new(Records::default());
    let mut execution = Execution::new_with_id(
        ExecutionId::new(),
        agent.id,
        ExecutionInput {
            intent: Some("count the ticks".to_string()),
            input: json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        },
        5,
        CONTEXT.to_string(),
    );
    execution.tenant_id = agent.tenant_id.clone();
    let execution_id = execution.id;
    records.0.lock().unwrap().insert(execution_id, execution);

    let contexts =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    contexts.save(context).await.unwrap();
    let storage_root =
        std::env::temp_dir().join(format!("aegis-inner-loop-tests-{}", uuid::Uuid::new_v4()));
    let volume_records =
        Arc::new(crate::infrastructure::repositories::InMemoryVolumeRepository::new());
    let fsal = Arc::new(crate::domain::fsal::AegisFSAL::new(
        Arc::new(
            crate::infrastructure::storage::LocalHostStorageProvider::new(&storage_root).unwrap(),
        ),
        volume_records.clone(),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let volumes = crate::application::nfs_gateway::NfsVolumeRegistry::new();
    let tools = ToolInvocationService::new(
        Arc::new(
            crate::infrastructure::seal::session_repository::InMemorySealSessionRepository::new(),
        ),
        contexts,
        Arc::new(crate::infrastructure::seal::middleware::SealMiddleware::new()),
        Arc::new(crate::infrastructure::tool_router::ToolRouter::new(
            crate::infrastructure::tool_router::ToolRouter::builtin_dispatchers(),
        )),
        fsal,
        volumes.clone(),
        Arc::new(OneAgent(agent.clone())),
        records.clone(),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
        Arc::new(EventBus::new(64)),
        None,
    );
    let model = Arc::new(ScriptedModel {
        turns,
        calls: StdMutex::new(Vec::new()),
        silent,
    });
    let registry = ProviderRegistry::new_for_test(model.clone(), None, 1, 0, 600);
    let inner_loop = Arc::new(
        InnerLoopService::new(Arc::new(tools), records.clone(), Arc::new(registry))
            .with_context_source(alias_table()),
    );
    World {
        records,
        inner_loop,
        model,
        agent,
        execution_id,
        volumes,
        volume_records,
    }
}

impl World {
    /// The supervisor's loop over this world's container, as the execution
    /// service runs it for the agent.
    async fn run(
        &self,
        validation: Option<Arc<ValidationPipeline>>,
    ) -> Result<String, RuntimeError> {
        let container = Arc::new(BootstrapContainer {
            inner_loop: self.inner_loop.clone(),
            agent_id: self.agent.id,
            execution_id: self.execution_id,
            iteration: StdMutex::new(0),
        });
        let mut env = HashMap::new();
        env.insert(
            "AEGIS_EXECUTION_ID".to_string(),
            self.execution_id.to_string(),
        );
        let config = RuntimeConfig {
            language: "python".to_string(),
            version: "3.12".to_string(),
            isolation: "process".to_string(),
            env,
            image_pull_policy: crate::domain::agent::ImagePullPolicy::IfNotPresent,
            container_uid: 1000,
            container_gid: 1000,
            resources: ResourceLimits {
                cpu_millis: None,
                memory_bytes: None,
                disk_bytes: None,
                timeout_seconds: None,
            },
            execution: self
                .agent
                .manifest
                .spec
                .execution
                .clone()
                .unwrap_or_default(),
            volumes: Vec::new(),
            keep_container_on_failure: false,
            image: "python:3.12".to_string(),
            bootstrap_path: None,
            execution_id: self.execution_id,
            workflow_execution_id: None,
            program_files: Vec::new(),
            program_input: None,
        };
        let max_retries = config.execution.max_retries;
        Supervisor::new(container)
            .with_execution_repository(self.records.clone())
            .run_loop(
                config,
                ExecutionInput {
                    intent: Some("count the ticks".to_string()),
                    input: json!({}),
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: Vec::new(),
                },
                max_retries,
                Arc::new(RecordingObserver {
                    records: self.records.clone(),
                    execution_id: self.execution_id,
                }),
                CancellationToken::new(),
                validation,
            )
            .await
    }
}

/// The text the model was shown as the result of its last tool call.
fn last_tool_message(conversation: &[ChatMessage]) -> String {
    conversation
        .iter()
        .rev()
        .find(|m| m.role == "tool")
        .map(|m| m.content.as_text())
        .expect("a tool message")
}

fn first_user_message(conversation: &[ChatMessage]) -> String {
    conversation
        .iter()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_text())
        .expect("a user message")
}

const TICKS: &str = "for i in $(seq 1 100); do echo tick $i; sleep 1; done";

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The reproduction of 2026-10-05: a try's command left at its default
/// timeout would run to the try's own end, the container would be ended with
/// it, and the next try was told only "timed out". Now the command's timeout
/// is lowered below the time left in the try, the command ends first with
/// its own timeout result and its output so far, inside the same try, and
/// the result says the timeout was lowered; and when the try still fails,
/// the next try is told what it ran, with that result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_past_its_tries_time_ends_first_and_the_next_try_is_told_what_it_did() {
    // An 8 s try; a model call bounded at 30 s, so 4 s (half the try) are
    // kept after a command for the model to read its result.
    let w = world(
        agent("8s", 30),
        vec![
            run_command(json!({"command": TICKS, "cwd": "/tmp"})),
            fails("model call failed: 502"),
            answer("the ticks were counted"),
        ],
    )
    .await;

    let started = std::time::Instant::now();
    let outcome = w.run(None).await;
    assert_eq!(
        outcome.expect("the second try answers"),
        "the ticks were counted"
    );

    // Try 1: the command ended inside the try with its own result.
    let seen = last_tool_message(&w.model.conversation(1));
    assert!(seen.contains("tick 1"), "its output so far: {seen}");
    assert!(
        seen.contains("[AEGIS] Command timed out after"),
        "its own timeout result: {seen}"
    );
    assert!(
        seen.contains("lowered from 600 s"),
        "the cap is stated where it bit: {seen}"
    );
    let try_one = w.records.get(w.execution_id).iterations[0].clone();
    let error = try_one.error.expect("try 1 failed").message;
    assert!(
        !error.contains("timed out after 8"),
        "the command, not the try's clock, ended first: {error}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(30));

    // Try 2 is told what try 1 did.
    let told = first_user_message(&w.model.conversation(2));
    assert!(told.contains("What the previous tries did"), "{told}");
    assert!(told.contains("Try 1"), "{told}");
    assert!(told.contains("seq 1 100"), "the command it ran: {told}");
    assert!(told.contains("tick 1"), "its output so far: {told}");
    assert!(
        told.contains("[AEGIS] Command timed out after"),
        "its result: {told}"
    );
    assert!(
        told.contains("model call failed: 502"),
        "how it ended: {told}"
    );
}

/// A 300,000-byte output (the size the run of 2026-10-05 sent whole) enters
/// the model's conversation as its head and its tail under the bound, an
/// eighth of the alias's prompt limit, with the cut stated in words; the
/// output as produced is stored whole in the trajectory the judges are given
/// (U19 unchanged).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_output_enters_the_conversation_as_head_and_tail_and_is_kept_whole() {
    let w = world(
        agent("120s", 30),
        vec![
            run_command(json!({
                "command": "python3 -c \"import sys; sys.stdout.write('HEAD' + 'x' * 299992 + 'TAIL')\"",
                "cwd": "/tmp"
            })),
            answer("done"),
        ],
    )
    .await;
    assert_eq!(w.inner_loop.conversation_output_bound(ALIAS), BOUND);

    let judge = RecordingJudge::default();
    let given = judge.given.clone();
    let pipeline = Arc::new(ValidationPipeline::new(vec![ValidatorEntry {
        kind: ValidatorKind::Semantic,
        validator: Box::new(judge),
        min_score: 0.5,
        min_confidence: 0.0,
    }]));
    assert_eq!(w.run(Some(pipeline)).await.expect("answered"), "done");

    let seen = last_tool_message(&w.model.conversation(1));
    assert!(
        seen.len() <= BOUND + 2_000,
        "the conversation's copy is {} bytes against a bound of {BOUND}",
        seen.len()
    );
    let seen_json: Value = serde_json::from_str(&seen).expect("the tool message is JSON");
    let stdout = seen_json["stdout"].as_str().unwrap();
    assert!(stdout.starts_with("HEAD"), "{}", &stdout[..40]);
    assert!(stdout.ends_with("TAIL"), "{}", &stdout[stdout.len() - 40..]);
    assert!(stdout.contains("300000 bytes"), "the true size is stated");
    let notice = seen_json["context_notice"].as_str().unwrap_or_default();
    assert!(
        notice.contains("trajectory") && notice.contains("/workspace"),
        "where the whole output is: {notice}"
    );

    // The judges are given the output as it was produced, whole.
    let given = given.lock().unwrap();
    assert_eq!(given.len(), 1);
    let step = &given[0].tool_trajectory[0];
    let stored: Value = serde_json::from_str(step.result_json.as_deref().unwrap()).unwrap();
    assert_eq!(stored["stdout"].as_str().unwrap().len(), 300_000);
    assert!(stored.get("context_notice").is_none());
}

/// A world whose try started 6 s ago under an 8 s bound: 2 s left, under
/// the 4 s a try keeps after a command for the model, so the try clock
/// refuses the command the model asks for.
async fn a_try_with_no_time_left(turns: Vec<Turn>) -> World {
    let w = world(agent("8s", 30), turns).await;
    w.records.update(w.execution_id, |e| {
        e.start_iteration("count the ticks".to_string()).unwrap();
        e.iterations[0].started_at = chrono::Utc::now() - chrono::Duration::seconds(6);
    });
    w
}

impl World {
    /// The try's first generate request, as the bootstrap sends it.
    async fn generate(&self) -> anyhow::Result<OrchestratorMessage> {
        self.inner_loop
            .handle_agent_message(AgentMessage::Generate {
                agent_id: self.agent.id.to_string(),
                execution_id: self.execution_id.to_string(),
                iteration_number: 1,
                prompt: "count the ticks".to_string(),
                messages: Vec::new(),
                model_alias: ALIAS.to_string(),
            })
            .await
    }
}

/// AEGIS ADR-141, Update of 2026-10-14, Question 2's default (U37's
/// amendment of 2026-10-13 reversed): the first command the try clock
/// refuses is not run, and the try ends there, exhausted by the try clock,
/// with its refusal's sentence and no order to answer; the model is not
/// called again. The iteration records the end and the history the next try
/// is opened from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_try_clock_refusal_ends_the_try_exhausted() {
    let w = a_try_with_no_time_left(vec![
        run_command(json!({"command": "echo never", "cwd": "/tmp"})),
        answer("as far as I got"),
    ])
    .await;
    let outcome = w.generate().await;
    let mut complaints = Vec::new();
    match &outcome {
        Err(e)
            if e.to_string()
                .starts_with("[AEGIS] cmd.run was not run: this try has")
                && e.to_string().ends_with("The try ends here.") => {}
        other => complaints.push(format!(
            "the try did not end with the refusal's sentence: {other:?}"
        )),
    }
    let calls = w.model.calls.lock().unwrap().len();
    if calls != 1 {
        complaints.push(format!(
            "the model was called after the try clock refused its command: {calls} calls"
        ));
    }
    let record = w.records.get(w.execution_id);
    let steps = record.iterations[0].trajectory.clone().unwrap_or_default();
    if steps.len() != 1 || steps[0].status != "refused" {
        complaints.push(format!("the try's trajectory is {steps:?}"));
    }
    let dispatched = w.records.events_named("ToolDispatched");
    if dispatched.len() != 1
        || dispatched[0]["status"] != "refused"
        || dispatched[0]["refused_by"] != "try_time_limit"
    {
        complaints.push(format!("the dispatch events are {dispatched:?}"));
    }
    match &record.iterations[0].exhausted {
        Some(crate::domain::execution::Exhaustion {
            by: crate::domain::execution::ExhaustedBy::TryClock,
            constructed: Some(history),
        }) if history
            .facts
            .starts_with("# This task so far\nTry 1 ran 1 tool calls in ")
            && history.facts.ends_with(": cmd.run 1. Files it left: none.") => {}
        other => complaints.push(format!("the iteration's end is {other:?}")),
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// A try of 900 s whose model calls are bounded at 120 s keeps 130 s after a
/// command: one model call and the result's way back (the Forge's tester).
#[test]
fn a_900_s_try_with_120_s_model_calls_keeps_130_s_after_a_command() {
    assert_eq!(
        command_margin_secs(120, std::time::Duration::from_secs(900)),
        130
    );
}

/// AEGIS ADR-131 U34, U34a: the try clock's refusal is an event of the
/// execution's log carrying its sentence, its layer, the try and the
/// arguments as the model wrote them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_try_clock_refusal_is_an_event_with_its_sentence_and_layer() {
    let w = a_try_with_no_time_left(vec![
        run_command(json!({"command": "echo never", "cwd": "/tmp"})),
        answer("as far as I got"),
    ])
    .await;
    let _ = w.generate().await;
    let dispatched = w.records.events_named("ToolDispatched");
    let mut complaints = Vec::new();
    match dispatched.as_slice() {
        [event] => {
            if event["status"] != "refused" {
                complaints.push(format!("status {}", event["status"]));
            }
            if event["refused_by"] != "try_time_limit" {
                complaints.push(format!("refused_by {}", event["refused_by"]));
            }
            let sentence = event["sentence"].as_str().unwrap_or_default();
            if !(sentence.contains("cmd.run was not run") && sentence.contains("s left")) {
                complaints.push(format!("sentence {:?}", event["sentence"]));
            }
            if event["tool"] != "cmd.run" || event["iteration_number"] != 1 {
                complaints.push(format!(
                    "tool {} in try {}",
                    event["tool"], event["iteration_number"]
                ));
            }
            if event["arguments"] != json!({"command": "echo never", "cwd": "/tmp"}) {
                complaints.push(format!("arguments {}", event["arguments"]));
            }
            if event["stdin_given"] != false {
                complaints.push(format!("stdin_given {}", event["stdin_given"]));
            }
        }
        other => complaints.push(format!(
            "the refused command is {} ToolDispatched events: {other:?}",
            other.len()
        )),
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// AEGIS ADR-131 U34, U34a, U34b: a call the execution's security context
/// refuses is a `refused` event with the security context's sentence and
/// layer, and a `refused` step of the trajectory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_security_context_refusal_is_a_refused_event_and_a_refused_step() {
    let mut context = every_tool_context();
    context.deny_list = vec!["cmd.run".to_string()];
    let w = world_in(
        agent("60s", 30),
        vec![
            run_command(json!({"command": "rm -rf /tmp/x", "cwd": "/tmp"})),
            answer("I could not run it"),
        ],
        context,
    )
    .await;
    w.records.update(w.execution_id, |e| {
        e.start_iteration("count the ticks".to_string()).unwrap();
    });
    let answered = w.generate().await;
    let mut complaints = Vec::new();
    if !matches!(answered, Ok(OrchestratorMessage::Final { .. })) {
        complaints.push(format!("the try answered {answered:?}"));
    }
    let dispatched = w.records.events_named("ToolDispatched");
    match dispatched.as_slice() {
        [event] => {
            if event["status"] != "refused" || event["refused_by"] != "security_context" {
                complaints.push(format!(
                    "status {} refused_by {}",
                    event["status"], event["refused_by"]
                ));
            }
            let sentence = event["sentence"].as_str().unwrap_or_default();
            if !sentence.contains("explicitly denied") {
                complaints.push(format!("sentence {:?}", event["sentence"]));
            }
            if event["arguments"] != json!({"command": "rm -rf /tmp/x", "cwd": "/tmp"}) {
                complaints.push(format!("arguments {}", event["arguments"]));
            }
        }
        other => complaints.push(format!(
            "the refused call is {} ToolDispatched events: {other:?}",
            other.len()
        )),
    }
    let record = w.records.get(w.execution_id);
    let steps = record.iterations[0].trajectory.clone().unwrap_or_default();
    if steps.first().map(|s| s.status.as_str()) != Some("refused") {
        complaints.push(format!("the trajectory's step is {steps:?}"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// AEGIS ADR-131 U34: each dispatch of a try is an event, in order, with the
/// arguments as the model wrote them (a relative `fs.write` path, before the
/// orchestrator makes it absolute); a command handed to the container is
/// `dispatched`, then ends `failed` with its exit code and its error output's
/// last line, or `succeeded`; every generation before them is an
/// `LlmInteraction`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_dispatch_is_an_event_in_order_with_the_models_arguments() {
    let w = world(
        agent("60s", 30),
        vec![
            call_tool(
                "fs.write",
                json!({"path": "notes.txt", "content": "seven and a half"}),
            ),
            run_command(json!({"command": "echo out; echo bad >&2; exit 3", "cwd": "/tmp"})),
            run_command(json!({"command": "printf ok", "cwd": "/tmp"})),
            answer("done"),
        ],
    )
    .await;
    let _ = w.run(None).await;
    let events = w.records.events();
    let kinds: Vec<String> = events
        .iter()
        .map(|(name, fields)| match name.as_str() {
            "ToolDispatched" => format!(
                "ToolDispatched {} {}",
                fields["tool"].as_str().unwrap_or_default(),
                fields["status"].as_str().unwrap_or_default()
            ),
            "ToolDispatchEnded" => format!(
                "ToolDispatchEnded {}",
                fields["status"].as_str().unwrap_or_default()
            ),
            other => other.to_string(),
        })
        .collect();
    let fs_write_status = events
        .iter()
        .find(|(n, f)| n == "ToolDispatched" && f["tool"] == "fs.write")
        .and_then(|(_, f)| f["status"].as_str().map(str::to_string))
        .unwrap_or_else(|| "none".to_string());
    let expected: Vec<String> = [
        "LlmInteraction".to_string(),
        format!("ToolDispatched fs.write {fs_write_status}"),
        "LlmInteraction".to_string(),
        "ToolDispatched cmd.run dispatched".to_string(),
        "ToolDispatchEnded failed".to_string(),
        "LlmInteraction".to_string(),
        "ToolDispatched cmd.run dispatched".to_string(),
        "ToolDispatchEnded succeeded".to_string(),
        "LlmInteraction".to_string(),
    ]
    .to_vec();
    let mut complaints = Vec::new();
    if kinds != expected {
        complaints.push(format!(
            "the events are {kinds:?}, not in the order of the try's dispatches {expected:?}"
        ));
    }
    let dispatched = w.records.events_named("ToolDispatched");
    let arguments: Vec<Value> = dispatched.iter().map(|e| e["arguments"].clone()).collect();
    let written = vec![
        json!({"path": "notes.txt", "content": "seven and a half"}),
        json!({"command": "echo out; echo bad >&2; exit 3", "cwd": "/tmp"}),
        json!({"command": "printf ok", "cwd": "/tmp"}),
    ];
    if arguments != written {
        complaints.push(format!(
            "the arguments are {arguments:?}, not as the model wrote them"
        ));
    }
    let indexes: Vec<Value> = dispatched.iter().map(|e| e["call_index"].clone()).collect();
    if indexes != vec![json!(0), json!(1), json!(2)] {
        complaints.push(format!("the call indexes are {indexes:?}"));
    }
    let ended = w.records.events_named("ToolDispatchEnded");
    match ended.as_slice() {
        [failed, succeeded] => {
            if failed["exit_code"] != 3
                || failed["sentence"] != "exited with code 3: bad"
                || failed["call_index"] != 1
            {
                complaints.push(format!("the failed command ended {failed}"));
            }
            if succeeded["exit_code"] != 0
                || succeeded.get("sentence").is_some()
                || succeeded["call_index"] != 2
            {
                complaints.push(format!("the succeeded command ended {succeeded}"));
            }
            if failed["dispatch_id"]
                != dispatched
                    .get(1)
                    .map(|d| d["dispatch_id"].clone())
                    .unwrap_or_default()
            {
                complaints.push("the failed command's end names another dispatch".to_string());
            }
        }
        other => complaints.push(format!("the commands ended as {other:?}")),
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// AEGIS ADR-131 U34c: a `cmd.run`'s standard input is named given, and its
/// text is kept out of the event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commands_stdin_is_named_given_and_its_text_kept_out() {
    let w = world(
        agent("60s", 30),
        vec![
            // `wc -c` reads its input and writes only its length, so no
            // output carries the text either.
            run_command(json!({"command": "wc -c", "cwd": "/tmp", "stdin": "the secret words"})),
            answer("done"),
        ],
    )
    .await;
    let _ = w.run(None).await;
    let dispatched = w.records.events_named("ToolDispatched");
    let mut complaints = Vec::new();
    match dispatched.as_slice() {
        [event] => {
            if event["stdin_given"] != true {
                complaints.push(format!("stdin_given {}", event["stdin_given"]));
            }
            if event["arguments"] != json!({"command": "wc -c", "cwd": "/tmp"}) {
                complaints.push(format!("arguments {}", event["arguments"]));
            }
        }
        other => complaints.push(format!("the command is {} events: {other:?}", other.len())),
    }
    let all = serde_json::to_string(&w.records.events()).unwrap();
    if all.contains("the secret words") {
        complaints.push("an event carries the standard input's text".to_string());
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// AEGIS ADR-131 U34, U34d: every generation of a try is an `LlmInteraction`
/// with its text: the first with the try's prompt and the calls it made, the
/// next with the results it was given and its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_generation_is_an_llm_interaction_with_its_text() {
    let w = world(
        agent("60s", 30),
        vec![
            run_command(json!({"command": "printf seventeen", "cwd": "/tmp"})),
            answer("all done: seventeen"),
        ],
    )
    .await;
    let _ = w.run(None).await;
    let generations = w.records.events_named("LlmInteraction");
    let mut complaints = Vec::new();
    match generations.as_slice() {
        [first, second] => {
            let prompt = first["prompt"].as_str().unwrap_or_default();
            if !prompt.contains("count the ticks") {
                complaints.push(format!("the first prompt is {prompt:?}"));
            }
            let calls: Value = serde_json::from_str(first["response"].as_str().unwrap_or_default())
                .unwrap_or(Value::Null);
            if calls[0]["name"] != "cmd.run" || calls[0]["id"].as_str().is_none() {
                complaints.push(format!("the first response is {}", first["response"]));
            }
            if !second["prompt"]
                .as_str()
                .unwrap_or_default()
                .contains("seventeen")
            {
                complaints.push(format!("the second prompt is {}", second["prompt"]));
            }
            if second["response"] != "all done: seventeen" {
                complaints.push(format!("the second response is {}", second["response"]));
            }
            if first["iteration_number"] != 1 || second["iteration_number"] != 1 {
                complaints.push("a generation names another try".to_string());
            }
        }
        other => complaints.push(format!(
            "the try's two generations are {} LlmInteraction events: {other:?}",
            other.len()
        )),
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// A command still running when its try was cut off is named in the next
/// try's history as such, with how long it had run.
#[test]
fn a_command_still_running_when_the_try_was_cut_off_is_named_with_how_long_it_ran() {
    let started = chrono::Utc::now() - chrono::Duration::seconds(700);
    let mut try_one = Execution::new_with_id(
        ExecutionId::new(),
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
        5,
        CONTEXT.to_string(),
    );
    try_one.start_iteration("task".to_string()).unwrap();
    try_one.fail_iteration(IterationError {
        message: "Execution failed: Execution timed out after 600 seconds".to_string(),
        details: None,
    });
    let iteration = &mut try_one.iterations[0];
    iteration.ended_at = Some(started + chrono::Duration::seconds(600));
    iteration.trajectory = Some(vec![
        TrajectoryStep {
            tool_name: "fs.write".to_string(),
            arguments_json: json!({"path": "/workspace/solve.py", "content": "print(1)"})
                .to_string(),
            status: "succeeded".to_string(),
            result_json: Some(json!({"written": true}).to_string()),
            error: None,
        },
        TrajectoryStep {
            tool_name: "cmd.run".to_string(),
            arguments_json: json!({"command": "python /workspace/solve.py"}).to_string(),
            status: "dispatched".to_string(),
            result_json: Some(
                json!({"running_since": started.to_rfc3339(), "timeout_secs": 600}).to_string(),
            ),
            error: None,
        },
    ]);
    let told = previous_tries_section(&try_one.iterations, 2, BOUND);
    assert!(told.contains("python /workspace/solve.py"), "{told}");
    assert!(told.contains("still running"), "{told}");
    assert!(told.contains("600 s"), "how long it ran: {told}");
    assert!(told.contains("/workspace/solve.py"), "{told}");
    assert!(told.contains("timed out after 600 seconds"), "{told}");
}

// ---------------------------------------------------------------------------
// AEGIS ADR-135 D5, ADR-005 O7e: a text answer where a tool call was required
// ---------------------------------------------------------------------------

/// An agent of one try whose `spec` ends with `tail`.
fn requiring_agent(tail: &str) -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(&format!(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: requiring-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: {ALIAS}
  tools:
    - cmd.run
{tail}
"#
    ))
    .unwrap();
    Agent {
        id: AgentId::new(),
        tenant_id: TenantId::default(),
        scope: crate::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

const ONE_TRY: &str = "  execution:\n    mode: iterative\n    max_retries: 1\n    iteration_timeout: \"60s\"\n    llm_timeout_seconds: 30\n";

const PROGRAM_RUN: &str = "printf 7.5";

fn carrying_a_program() -> Agent {
    requiring_agent(&format!(
        "{ONE_TRY}  program:\n    files:\n      - path: solve.py\n        content: print(7.5)\n    run: {PROGRAM_RUN}\n"
    ))
}

fn the_users_after_the_prompt(conversation: &[ChatMessage]) -> Vec<String> {
    conversation
        .iter()
        .filter(|m| m.role == "user")
        .skip(1)
        .map(|m| m.content.as_text())
        .collect()
}

/// T6: an agent carrying a program answers text, is sent back once with the
/// requirement, answers text again, and the try fails with D5's sentence;
/// a command that is not the program's does not meet the requirement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_where_the_program_had_to_run_is_sent_back_once_then_fails_the_try() {
    let mut complaints = Vec::new();
    let w = world(
        carrying_a_program(),
        vec![
            run_command(json!({"command": "printf other", "cwd": "/tmp"})),
            answer("the total is 7.5"),
            answer("the total is still 7.5"),
        ],
    )
    .await;
    let outcome = w.run(None).await;
    if outcome.is_ok() {
        complaints.push(format!("the run completed: {outcome:?}"));
    }
    let calls = w.model.calls.lock().unwrap().len();
    if calls != 3 {
        complaints.push(format!(
            "the model was called {calls} times; the text answer was to be sent back once"
        ));
    } else {
        let reminded = the_users_after_the_prompt(&w.model.conversation(2));
        let expected_reminder = format!(
            "You answered with text, but this agent must run its program before it answers. \
             Call cmd.run with the command \"{PROGRAM_RUN}\" now, then present its output."
        );
        if reminded != vec![expected_reminder] {
            complaints.push(format!("the model was sent back with {reminded:?}"));
        }
    }
    let record = w.records.get(w.execution_id);
    match record.iterations.first().and_then(|i| i.error.clone()) {
        Some(error) if error.message.contains(TEXT_WHERE_A_TOOL_CALL_WAS_REQUIRED) => {}
        other => complaints.push(format!("the try ended with {other:?}")),
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// T6: sent back once, the model runs the program's command and the try
/// completes with its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_before_the_program_ran_completes_once_the_program_runs() {
    let w = world(
        carrying_a_program(),
        vec![
            answer("the total is 7.5"),
            run_command(json!({"command": "printf", "args": ["7.5"], "cwd": "/tmp"})),
            answer("7.5"),
        ],
    )
    .await;
    let outcome = w.run(None).await;
    assert_eq!(outcome.expect("the try completes"), "7.5");
    assert!(
        last_tool_message(&w.model.conversation(2)).contains("7.5"),
        "the program's output was the tool result"
    );
}

/// T6: an executor declaring `require_tool_call` that answers text twice
/// fails the try; one that calls a tool completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_executor_that_must_call_a_tool_fails_on_text_and_completes_after_a_call() {
    let tail = format!("{ONE_TRY}    require_tool_call: true\n");
    let w = world(
        requiring_agent(&tail),
        vec![answer("done"), answer("done, really")],
    )
    .await;
    let outcome = w.run(None).await;
    let record = w.records.get(w.execution_id);
    let error = record.iterations[0].error.clone().map(|e| e.message);
    assert!(
        outcome.is_err()
            && error
                .as_deref()
                .is_some_and(|m| m.contains(TEXT_WHERE_A_TOOL_CALL_WAS_REQUIRED)),
        "outcome {outcome:?}, try error {error:?}"
    );

    let w = world(
        requiring_agent(&tail),
        vec![
            run_command(json!({"command": "printf ok", "cwd": "/tmp"})),
            answer("ok"),
        ],
    )
    .await;
    assert_eq!(w.run(None).await.expect("the try completes"), "ok");
}

// ---------------------------------------------------------------------------
// The provider's time limit refines the next try's prompt
// ---------------------------------------------------------------------------

/// The refinement after a time limit of 120.5 s, word for word.
const REFINEMENT_AFTER_120_5_S: &str = "Your previous answer exceeded the model provider's time \
     limit: the provider ended it after 120.5 s, before it finished, so none of it was kept. \
     Answer this time with a tool call that runs a program to compute the result; do not write \
     the computed result in your text.";

/// The header paragraph of the previous-tries section, as it ends.
const SECTION_HEADER_END: &str = "Each result is shown as the model saw it then.\n\n";

/// The reproduction of execution eea195cd: the provider ends try 1's model
/// call at its time limit; the next try is not sent the same request with
/// the raw error alone. Its prompt carries the refinement naming the limit,
/// right after the previous-tries section's header and before `## Try 1`,
/// and try 1's record carries the same refinement for `RefinementApplied`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_time_limit_refines_the_next_try_with_the_limit() {
    let w = world(
        agent("60s", 30),
        vec![ended_at_the_time_limit(), answer("solved by the program")],
    )
    .await;
    let outcome = w.run(None).await;
    assert_eq!(
        outcome.expect("the second try answers"),
        "solved by the program"
    );

    let told = first_user_message(&w.model.conversation(1));
    assert!(
        told.contains(&format!(
            "{SECTION_HEADER_END}{REFINEMENT_AFTER_120_5_S}\n\n## Try 1"
        )),
        "try 2's prompt must carry the refinement naming the limit after the section's header and before ## Try 1: {told}"
    );
    let try_one = w.records.get(w.execution_id).iterations[0].clone();
    let refinement = try_one
        .code_changes
        .expect("try 1's record must carry the refinement it was given");
    assert_eq!(
        refinement.diff, REFINEMENT_AFTER_120_5_S,
        "try 1's refinement must be the sentence try 2 was sent"
    );
}

/// Another provider error keeps today's path: no refinement on the record
/// and the previous-tries section as it was, the header then `## Try 1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_provider_error_leaves_the_refinement_as_today() {
    let w = world(
        agent("60s", 30),
        vec![
            Box::new(|_| Err(LLMError::Provider("HTTP 500: upstream boom".to_string()))),
            answer("done"),
        ],
    )
    .await;
    assert_eq!(w.run(None).await.expect("the second try answers"), "done");

    let told = first_user_message(&w.model.conversation(1));
    assert!(
        told.contains(&format!("{SECTION_HEADER_END}## Try 1")),
        "the section must read as today, the header then ## Try 1: {told}"
    );
    assert!(
        told.contains("HTTP 500: upstream boom"),
        "how it ended: {told}"
    );
    assert!(
        !told.contains("exceeded the model provider's time limit"),
        "another provider error must not be told the time limit: {told}"
    );
    let try_one = w.records.get(w.execution_id).iterations[0].clone();
    assert!(
        try_one.code_changes.is_none(),
        "another provider error must leave no refinement, got {:?}",
        try_one.code_changes
    );
}

/// A model silent until the agent's own `llm_timeout_seconds` is not the
/// provider's time limit: no refinement, and the next try is not told one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_primary_is_not_the_providers_time_limit() {
    let w = world_with_silent_calls(
        agent("60s", 1),
        vec![answer("never sent"), answer("done")],
        vec![0],
    )
    .await;
    assert_eq!(w.run(None).await.expect("the second try answers"), "done");

    let told = first_user_message(&w.model.conversation(1));
    assert!(told.contains("llm_timeout_seconds"), "how it ended: {told}");
    assert!(
        !told.contains("exceeded the model provider's time limit"),
        "a silent primary must not be told the provider's limit: {told}"
    );
    let try_one = w.records.get(w.execution_id).iterations[0].clone();
    assert!(
        try_one.code_changes.is_none(),
        "a silent primary must leave no refinement, got {:?}",
        try_one.code_changes
    );
}

/// The refinement's sentence without the seconds, where the provider's
/// error states none.
#[test]
fn the_refinement_without_seconds_says_the_provider_ended_it() {
    assert_eq!(
        time_limit_refinement(crate::infrastructure::llm::registry::ProviderTimeLimit {
            seconds: None
        }),
        "Your previous answer exceeded the model provider's time limit: the provider ended it \
         before it finished, so none of it was kept. Answer this time with a tool call that \
         runs a program to compute the result; do not write the computed result in your text."
    );
}

// ---------------------------------------------------------------------------
// The repeated-call guard and the deadline at the dispatch site
// ---------------------------------------------------------------------------

/// The sentence a call answered twice alike gets in place of a third run.
const RAN_TWICE_ALIKE: &str =
    "This call was already run 2 times in this try with the same answer; it is not run again.";

/// The search the requirements analyst of run `ea6d7814` (child `e5f2d38f`)
/// ran about fourteen times in one try, 0 matches each time.
fn the_analysts_grep() -> Turn {
    call_tool(
        "fs.grep",
        json!({
            "path": "/workspace/aegis-examples/repo",
            "pattern": "The Forge is AEGIS's built-in development workflow"
        }),
    )
}

impl World {
    /// Gives this world's execution a `/workspace` volume the file tools
    /// find; nothing is stored in it, so `fs.grep` answers 0 matches.
    fn with_an_empty_workspace(self) -> Self {
        self.volumes
            .register(crate::application::nfs_gateway::VolumeRegistration {
                volume_id: crate::domain::volume::VolumeId::new(),
                execution_id: self.execution_id,
                workflow_execution_id: None,
                container_uid: 1000,
                container_gid: 1000,
                policy: crate::domain::fsal::FsalAccessPolicy {
                    read: vec!["/*".to_string()],
                    write: vec!["/*".to_string()],
                },
                mount_point: std::path::PathBuf::from("/workspace"),
                remote_path: String::new(),
            });
        self
    }

    /// Gives this world's execution a `/workspace` volume the file tools
    /// write to and read from, recorded as the file system layer reads it.
    /// (On a host directory a second file created in one directory is
    /// refused, `host-path-create-file-refuses-existing-dir`, so the tests
    /// write each file in a directory of its own.)
    async fn with_a_writable_workspace(self) -> Self {
        use crate::domain::repository::VolumeRepository;
        use crate::domain::volume::{StorageClass, Volume, VolumeBackend, VolumeOwnership};
        let mut volume = Volume::new(
            "workspace".to_string(),
            self.agent.tenant_id.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: std::path::PathBuf::from("/placeholder"),
            },
            64 * 1024 * 1024,
            VolumeOwnership::execution(self.execution_id),
        )
        .expect("volume");
        volume.backend = VolumeBackend::HostPath {
            path: std::path::PathBuf::from(format!("/hosts/{}", volume.id)),
        };
        volume.mark_available().expect("available");
        self.volume_records.save(&volume).await.expect("saved");
        self.volumes
            .register(crate::application::nfs_gateway::VolumeRegistration {
                volume_id: volume.id,
                execution_id: self.execution_id,
                workflow_execution_id: None,
                container_uid: 1000,
                container_gid: 1000,
                policy: crate::domain::fsal::FsalAccessPolicy {
                    read: vec!["/**".to_string()],
                    write: vec!["/**".to_string()],
                },
                mount_point: std::path::PathBuf::from("/workspace"),
                remote_path: String::new(),
            });
        self
    }

    /// Each `ToolDispatched` event as its status and the layer that refused
    /// it, if any.
    fn dispatch_statuses(&self) -> Vec<String> {
        self.records
            .events_named("ToolDispatched")
            .iter()
            .map(|e| match e["refused_by"].as_str() {
                Some(layer) => format!("{} {layer}", e["status"].as_str().unwrap_or_default()),
                None => e["status"].as_str().unwrap_or_default().to_string(),
            })
            .collect()
    }
}

/// The loop of run `ea6d7814`: the same `fs.grep` four times in one try,
/// each answered 0 matches. The first two run; the third and the fourth are
/// not run, the model is given the guard's sentence in their answer's place,
/// and the try goes on to the model's answer. Each refused call is a
/// `refused` event naming the guard, with the sentence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_answered_twice_alike_is_not_run_a_third_time() {
    let w = world(
        agent("60s", 30),
        vec![
            the_analysts_grep(),
            the_analysts_grep(),
            the_analysts_grep(),
            the_analysts_grep(),
            answer("the sentence is not in the repository"),
        ],
    )
    .await
    .with_an_empty_workspace();
    w.records.update(w.execution_id, |e| {
        e.start_iteration("count the ticks".to_string()).unwrap();
    });
    let answered = w.generate().await;
    let mut complaints = Vec::new();
    if !matches!(answered, Ok(OrchestratorMessage::Final { .. })) {
        complaints.push(format!("the try answered {answered:?}"));
    }
    let statuses = w.dispatch_statuses();
    let expected = vec![
        "succeeded".to_string(),
        "succeeded".to_string(),
        "refused repeated_call".to_string(),
        "refused repeated_call".to_string(),
    ];
    if statuses != expected {
        complaints.push(format!(
            "the four calls were {statuses:?}, not {expected:?}"
        ));
    }
    for (call, seen) in [(1, "0 matches"), (2, "0 matches")] {
        let told = last_tool_message(&w.model.conversation(call));
        if !told.contains("\"count\":0") {
            complaints.push(format!("call {call} was not answered {seen}: {told}"));
        }
    }
    for call in [3, 4] {
        let told = last_tool_message(&w.model.conversation(call));
        if told != RAN_TWICE_ALIKE {
            complaints.push(format!(
                "repeat {} was answered {told:?}, not the guard's sentence",
                call
            ));
        }
    }
    for event in w.records.events_named("ToolDispatched").iter().skip(2) {
        if event["sentence"] != RAN_TWICE_ALIKE {
            complaints.push(format!(
                "the refused event's sentence is {}",
                event["sentence"]
            ));
        }
    }
    let record = w.records.get(w.execution_id);
    let steps: Vec<String> = record.iterations[0]
        .trajectory
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|s| s.status.clone())
        .collect();
    if steps != ["succeeded", "succeeded", "refused", "refused"] {
        complaints.push(format!("the try's trajectory is {steps:?}"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// The same call whose answers differ is no repeat: a command printing the
/// clock, asked for four times in one try, runs four times.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_call_answered_differently_runs_every_time() {
    let clock = || run_command(json!({"command": "date +%s%N", "cwd": "/tmp"}));
    let w = world(
        agent("60s", 30),
        vec![clock(), clock(), clock(), clock(), answer("four readings")],
    )
    .await;
    assert_eq!(w.run(None).await.expect("answered"), "four readings");
    let statuses = w.dispatch_statuses();
    assert_eq!(
        statuses,
        vec!["dispatched"; 4],
        "four calls with four different answers were {statuses:?}"
    );
    assert_eq!(w.records.events_named("ToolDispatchEnded").len(), 4);
}

/// A call asked for once the execution's time is up is not run (run
/// `ea6d7814` dispatched tool calls for two minutes after its 900 s ran
/// out): the try ends there with the sentence, the call is a `refused` event
/// naming the deadline, and the model is not called again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_asked_for_after_the_executions_time_is_up_is_not_run() {
    let w = world(
        agent("60s", 30),
        vec![the_analysts_grep(), answer("as far as I got")],
    )
    .await
    .with_an_empty_workspace();
    w.records.update(w.execution_id, |e| {
        e.timeout_seconds = Some(5);
        e.started_at = chrono::Utc::now() - chrono::Duration::seconds(10);
        e.start_iteration("count the ticks".to_string()).unwrap();
        e.iterations[0].started_at = chrono::Utc::now() - chrono::Duration::seconds(10);
    });
    let outcome = w.generate().await;
    let mut complaints = Vec::new();
    match &outcome {
        Err(e) if e.to_string().contains("fs.grep was not run") => {}
        other => complaints.push(format!(
            "the try did not end with the deadline's sentence: {other:?}"
        )),
    }
    let statuses = w.dispatch_statuses();
    if statuses != ["refused deadline"] {
        complaints.push(format!(
            "the call asked for after the deadline was {statuses:?}"
        ));
    }
    let calls = w.model.calls.lock().unwrap().len();
    if calls != 1 {
        complaints.push(format!("the model was called {calls} times"));
    }
    let record = w.records.get(w.execution_id);
    let steps = record.iterations[0].trajectory.clone().unwrap_or_default();
    if steps.len() != 1 || steps[0].status != "refused" {
        complaints.push(format!("the try's trajectory is {steps:?}"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// The answer a `cmd.run` naming a shell as its program and a program as the
/// shell's first argument gets in place of running.
const SHELL_AS_COMMAND: &str = "[AEGIS] cmd.run was not run: \"bash tail\" would run tail as a shell script. To run the program tail, send command \"tail\" and its arguments as args; to run a shell line, send command \"sh\" and args [\"-c\", \"<line>\"].";

/// On Forge run `c0d37ca8` the coder sent `command: "bash"` with
/// `args: ["tail", …]`: bash read `tail` as a script and each call exited
/// 126, "cannot execute binary file". Such a call is not run: its result is
/// the sentence naming the call to send, the call is a `refused` step and
/// event, and the model is called again in the same try.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_given_a_program_as_its_script_is_refused_with_the_call_to_send() {
    let w = world(
        agent("60s", 30),
        vec![
            run_command(json!({"command": "bash", "args": ["tail", "-c", "20", "x"]})),
            answer("I will send tail itself"),
        ],
    )
    .await;
    w.records.update(w.execution_id, |e| {
        e.start_iteration("count the ticks".to_string()).unwrap();
    });
    let outcome = w.generate().await;
    let mut complaints = Vec::new();
    match &outcome {
        Ok(OrchestratorMessage::Final { content, .. }) if content == "I will send tail itself" => {}
        other => complaints.push(format!(
            "the try did not go on to the model's answer: {other:?}"
        )),
    }
    let calls = w.model.calls.lock().unwrap().len();
    if calls != 2 {
        complaints.push(format!("the model was called {calls} times"));
    } else {
        let given = last_tool_message(&w.model.conversation(1));
        if given != SHELL_AS_COMMAND {
            complaints.push(format!("the refused call's result is {given:?}"));
        }
    }
    let record = w.records.get(w.execution_id);
    let steps = record.iterations[0].trajectory.clone().unwrap_or_default();
    if steps.len() != 1 || steps[0].status != "refused" {
        complaints.push(format!("the try's trajectory is {steps:?}"));
    }
    let dispatched = w.records.events_named("ToolDispatched");
    if dispatched.len() != 1
        || dispatched[0]["status"] != "refused"
        || dispatched[0]["sentence"] != SHELL_AS_COMMAND
    {
        complaints.push(format!("the dispatch events are {dispatched:?}"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// A shell given a flag (`-c`) or a script (`./run.sh`), and a program sent
/// as the command itself, are handed to the container as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_line_a_script_and_a_program_are_dispatched() {
    let mut complaints = Vec::new();
    for args in [
        json!({"command": "bash", "args": ["-c", "echo ok"]}),
        json!({"command": "bash", "args": ["./run.sh"]}),
        json!({"command": "tail", "args": ["-c", "20", "x"]}),
    ] {
        let w = world(agent("60s", 30), vec![run_command(args.clone())]).await;
        w.records.update(w.execution_id, |e| {
            e.start_iteration("count the ticks".to_string()).unwrap();
        });
        match w.generate().await {
            Ok(OrchestratorMessage::Dispatch { .. }) => {}
            other => complaints.push(format!("{args} was answered {other:?}")),
        }
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

// ---------------------------------------------------------------------------
// AEGIS ADR-141, Update of 2026-10-14, R4: a try's call budget
// ---------------------------------------------------------------------------

/// An agent of one 60 s try whose `spec.execution` adds `extra` (each line
/// indented four spaces).
fn agent_executing(extra: &str) -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(&format!(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: budget-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: {ALIAS}
  execution:
    mode: iterative
    max_retries: 1
    iteration_timeout: "60s"
    llm_timeout_seconds: 30
{extra}
  tools: []
"#
    ))
    .unwrap();
    Agent {
        id: AgentId::new(),
        tenant_id: TenantId::default(),
        scope: crate::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

/// A search of the empty workspace for `pattern`: a distinct call per
/// pattern, answered 0 matches, never a repeat.
fn grep_args(pattern: &str) -> serde_json::Value {
    json!({"path": "/workspace", "pattern": pattern})
}

/// The model asks for every call of `calls` in one turn.
fn calls_in_one_turn(calls: Vec<(&str, serde_json::Value)>) -> Turn {
    let calls: Vec<(String, serde_json::Value)> = calls
        .into_iter()
        .map(|(tool, args)| (tool.to_string(), args))
        .collect();
    Box::new(move |_| {
        Ok(ChatResponse::ToolCalls(
            calls
                .iter()
                .map(|(tool, args)| ChatToolCall {
                    id: format!("call-{}", uuid::Uuid::new_v4()),
                    name: tool.clone(),
                    arguments: args.clone(),
                })
                .collect(),
        ))
    })
}

/// The sentence a call past a budget of `budget` calls is refused with.
fn past_the_budget(tool: &str, budget: u32) -> String {
    format!(
        "[AEGIS] {tool} was not run: this try has made its {budget} tool calls, the most one \
         try makes. The try ends here."
    )
}

impl World {
    /// The world's execution with its first iteration started, as the
    /// supervisor leaves it before the try's first generate request.
    fn with_the_first_try_started(self) -> Self {
        self.records.update(self.execution_id, |e| {
            e.start_iteration("count the ticks".to_string()).unwrap();
        });
        self
    }
}

/// R4: with `max_tool_calls: 3` the model's fourth call is not run: it is a
/// refused `ToolDispatched` naming the call budget, the try ends with the
/// refusal's sentence, and the model is not called again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_past_the_budget_is_refused_not_run_and_ends_the_try() {
    let w = world(
        agent_executing("    max_tool_calls: 3"),
        vec![
            call_tool("fs.grep", grep_args("one")),
            call_tool("fs.grep", grep_args("two")),
            call_tool("fs.grep", grep_args("three")),
            call_tool("fs.grep", grep_args("four")),
            answer("never reached"),
        ],
    )
    .await
    .with_an_empty_workspace()
    .with_the_first_try_started();
    let outcome = w.generate().await;
    let mut complaints = Vec::new();
    let sentence = past_the_budget("fs.grep", 3);
    match &outcome {
        Err(e) if e.to_string() == sentence => {}
        other => complaints.push(format!(
            "the try did not end with the budget's sentence: {other:?}"
        )),
    }
    let calls = w.model.calls.lock().unwrap().len();
    if calls != 4 {
        complaints.push(format!("the model was called {calls} times, not 4"));
    }
    let statuses = w.dispatch_statuses();
    let expected = ["succeeded", "succeeded", "succeeded", "refused call_budget"];
    if statuses != expected {
        complaints.push(format!("the calls were {statuses:?}, not {expected:?}"));
    }
    if let Some(event) = w.records.events_named("ToolDispatched").get(3) {
        if event["sentence"] != sentence.as_str() {
            complaints.push(format!(
                "the refused event's sentence is {}",
                event["sentence"]
            ));
        }
    }
    let steps: Vec<String> = w.records.get(w.execution_id).iterations[0]
        .trajectory
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|s| s.status.clone())
        .collect();
    if steps != ["succeeded", "succeeded", "succeeded", "refused"] {
        complaints.push(format!("the try's trajectory is {steps:?}"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// R4: two calls asked for in one turn count two: with `max_tool_calls: 3`,
/// two turns of two calls run three and refuse the fourth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_calls_in_one_turn_count_two_against_the_budget() {
    let w = world(
        agent_executing("    max_tool_calls: 3"),
        vec![
            calls_in_one_turn(vec![
                ("fs.grep", grep_args("one")),
                ("fs.grep", grep_args("two")),
            ]),
            calls_in_one_turn(vec![
                ("fs.grep", grep_args("three")),
                ("fs.grep", grep_args("four")),
            ]),
            answer("never reached"),
        ],
    )
    .await
    .with_an_empty_workspace()
    .with_the_first_try_started();
    let outcome = w.generate().await;
    let mut complaints = Vec::new();
    if !matches!(&outcome, Err(e) if e.to_string() == past_the_budget("fs.grep", 3)) {
        complaints.push(format!("the try ended {outcome:?}"));
    }
    let statuses = w.dispatch_statuses();
    let expected = ["succeeded", "succeeded", "succeeded", "refused call_budget"];
    if statuses != expected {
        complaints.push(format!("the calls were {statuses:?}, not {expected:?}"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// R4, a guard of the default: a manifest that sets no budget runs 50 calls
/// (here 25 turns of two) and refuses the 51st.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unset_budget_runs_fifty_calls_and_refuses_the_fifty_first() {
    let mut turns: Vec<Turn> = (0..25)
        .map(|n| {
            calls_in_one_turn(vec![
                ("fs.grep", grep_args(&format!("a{n}"))),
                ("fs.grep", grep_args(&format!("b{n}"))),
            ])
        })
        .collect();
    turns.push(call_tool("fs.grep", grep_args("the fifty-first")));
    turns.push(answer("never reached"));
    let w = world(agent_executing(""), turns)
        .await
        .with_an_empty_workspace()
        .with_the_first_try_started();
    let outcome = w.generate().await;
    let mut complaints = Vec::new();
    if !matches!(&outcome, Err(e) if e.to_string() == past_the_budget("fs.grep", 50)) {
        complaints.push(format!("the try ended {outcome:?}"));
    }
    let statuses = w.dispatch_statuses();
    let ran = statuses.iter().filter(|s| *s == "succeeded").count();
    if ran != 50 || statuses.last().map(String::as_str) != Some("refused call_budget") {
        complaints.push(format!(
            "{ran} calls ran; the last was {:?}",
            statuses.last()
        ));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

// ---------------------------------------------------------------------------
// AEGIS ADR-141, Update of 2026-10-14, R2: a try that runs out of its budget
// is followed by one opened from a constructed history
// ---------------------------------------------------------------------------

/// The content try 1 writes to the file it leaves.
const LEFT_CONTENT: &str = "def test_readme():\n    assert True\n";

/// The fourteenth proof's shape, shortened: try 1 writes `a.py`, writes and
/// deletes `b.py`, lists `a.py`'s directory eleven times (two run, nine
/// refused as repeats), and its clock ends it while the model is silent. Try
/// 2 opens with the prompt as try 1 had it, then a constructed `fs.list` of
/// `a.py`'s directory and a constructed `fs.read` of `a.py` with its true
/// content, as its own completed calls (correction C3): no replay of try 1,
/// no paragraph naming it, nothing about how it ended, nothing of `b.py` or
/// of the refusals. The facts stay on the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_try_after_an_exhausted_one_opens_from_a_constructed_history() {
    let mut turns: Vec<Turn> = vec![
        call_tool(
            "fs.write",
            json!({"path": "/workspace/t/a.py", "content": LEFT_CONTENT}),
        ),
        call_tool(
            "fs.write",
            json!({"path": "/workspace/u/b.py", "content": "print(2)\n"}),
        ),
        call_tool("fs.delete", json!({"path": "/workspace/u/b.py"})),
    ];
    for _ in 0..11 {
        turns.push(call_tool("fs.list", json!({"path": "/workspace/t"})));
    }
    turns.push(answer("never: the model is silent here"));
    turns.push(answer("the tests are written"));
    let w = world_with_silent_calls(agent("8s", 30), turns, vec![14])
        .await
        .with_a_writable_workspace()
        .await;

    let outcome = w.run(None).await;
    let mut complaints = Vec::new();
    if !matches!(outcome.as_deref(), Ok("the tests are written")) {
        complaints.push(format!("the execution ended {outcome:?}"));
    }
    let record = w.records.get(w.execution_id);
    match record.iterations.first().and_then(|i| i.exhausted.as_ref()) {
        Some(e) if e.by == crate::domain::execution::ExhaustedBy::Clock => {}
        other => complaints.push(format!("try 1's end is {other:?}")),
    }
    let calls = w.model.calls.lock().unwrap().len();
    if calls != 16 {
        complaints.push(format!("the model was called {calls} times, not 16"));
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }
    let opening = w.model.conversation(15);
    let roles: Vec<&str> = opening.iter().map(|m| m.role.as_str()).collect();
    if roles != ["system", "user", "assistant", "tool", "assistant", "tool"] {
        complaints.push(format!("try 2 opened with the messages {roles:?}"));
    }
    // Correction C3: the try owns the earlier tries' work; its user message
    // is the prompt as the first try had it, and nothing names another try.
    let user = first_user_message(&opening);
    if user != "count the ticks" {
        complaints.push(format!("the user message is {user:?}"));
    }
    let constructed: Vec<(String, Value)> = opening
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|c| (c.name.clone(), c.arguments.clone()))
        .collect();
    let expected = vec![
        ("fs.list".to_string(), json!({"path": "/workspace/t"})),
        ("fs.read".to_string(), json!({"path": "/workspace/t/a.py"})),
    ];
    if constructed != expected {
        complaints.push(format!("the constructed calls are {constructed:?}"));
    }
    let results: Vec<String> = opening
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.content.as_text())
        .collect();
    if !results.first().is_some_and(|r| r.contains("a.py")) {
        complaints.push(format!("the listing is {:?}", results.first()));
    }
    let read: Option<Value> = results.get(1).and_then(|r| serde_json::from_str(r).ok());
    if read.as_ref().map(|r| r["content"].clone()) != Some(json!(LEFT_CONTENT)) {
        complaints.push(format!("the read is {:?}", results.get(1)));
    }
    let whole: String = opening
        .iter()
        .map(|m| m.content.as_text())
        .collect::<Vec<_>>()
        .join("\n");
    for absent in [
        "Previous Attempts",
        "What the previous tries did",
        "timed out",
        "Try 1",
        "This task so far",
        "b.py",
        "already run",
    ] {
        if whole.contains(absent) {
            complaints.push(format!("try 2's opening holds {absent:?}"));
        }
    }
    // The facts stay on the record, for the person and the narrative.
    match w.records.get(w.execution_id).iterations[0]
        .exhausted
        .clone()
        .and_then(|e| e.constructed)
    {
        Some(history)
            if history.files == ["/workspace/t/a.py"]
                && history
                    .facts
                    .starts_with("# This task so far\nTry 1 ran 14 tool calls in ")
                && history.facts.ends_with(
                    ": fs.list 11, fs.write 2, fs.delete 1. Files it left: /workspace/t/a.py.",
                ) => {}
        other => complaints.push(format!("the record's constructed history is {other:?}")),
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// R2, a guard: a try that ended another way (here its model call failed)
/// keeps today's opening for the next try, the replay of what it did, and
/// is not recorded exhausted; the replay carries no imperative sentence
/// (correction C2, the Update's Question 4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_try_after_one_that_failed_otherwise_keeps_the_replay() {
    let w = world(
        agent("60s", 30),
        vec![
            run_command(json!({"command": "echo one", "cwd": "/tmp"})),
            fails("model call failed: 502"),
            answer("done"),
        ],
    )
    .await;
    let outcome = w.run(None).await;
    let mut complaints = Vec::new();
    if !matches!(outcome.as_deref(), Ok("done")) {
        complaints.push(format!("the execution ended {outcome:?}"));
    }
    let told = first_user_message(&w.model.conversation(2));
    if !told.contains("What the previous tries did") || told.contains("# This task so far") {
        complaints.push(format!("try 2 was told {told:?}"));
    }
    // Correction C2 (Question 4): the replay carries facts only, no
    // imperative sentence.
    if told.contains("Build on what worked") || told.contains("do not repeat what failed") {
        complaints.push(format!(
            "the replay carries the imperative sentence: {told:?}"
        ));
    }
    if w.records.get(w.execution_id).iterations[0]
        .exhausted
        .is_some()
    {
        complaints.push("try 1 was recorded exhausted".to_string());
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// R2 and R4: a try its call budget ends is recorded exhausted by the call
/// budget, with the history the next try is opened from, before the try
/// fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_try_its_call_budget_ends_is_recorded_exhausted() {
    let w = world(
        agent_executing("    max_tool_calls: 2"),
        vec![
            call_tool(
                "fs.write",
                json!({"path": "/workspace/t/a.py", "content": LEFT_CONTENT}),
            ),
            call_tool("fs.grep", grep_args("one")),
            call_tool("fs.grep", grep_args("two")),
            answer("never reached"),
        ],
    )
    .await
    .with_a_writable_workspace()
    .await
    .with_the_first_try_started();
    let outcome = w.generate().await;
    let mut complaints = Vec::new();
    if outcome.is_ok() {
        complaints.push(format!("the try ended {outcome:?}"));
    }
    match &w.records.get(w.execution_id).iterations[0].exhausted {
        Some(crate::domain::execution::Exhaustion {
            by: crate::domain::execution::ExhaustedBy::CallBudget,
            constructed: Some(history),
        }) if history.calls == 3
            && history.files == ["/workspace/t/a.py"]
            && history
                .facts
                .ends_with(": fs.grep 2, fs.write 1. Files it left: /workspace/t/a.py.") => {}
        other => complaints.push(format!("the iteration's end is {other:?}")),
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
