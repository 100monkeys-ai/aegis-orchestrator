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
/// inner loop stores lands on the iteration it names.
#[derive(Default)]
struct Records(StdMutex<HashMap<ExecutionId, Execution>>);

impl Records {
    fn get(&self, id: ExecutionId) -> Execution {
        self.0.lock().unwrap().get(&id).cloned().expect("execution")
    }
    fn update(&self, id: ExecutionId, f: impl FnOnce(&mut Execution)) {
        let mut map = self.0.lock().unwrap();
        f(map.get_mut(&id).expect("execution"));
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
}

async fn world(agent: Agent, turns: Vec<Turn>) -> World {
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
    contexts.save(every_tool_context()).await.unwrap();
    let storage_root =
        std::env::temp_dir().join(format!("aegis-inner-loop-tests-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(crate::domain::fsal::AegisFSAL::new(
        Arc::new(
            crate::infrastructure::storage::LocalHostStorageProvider::new(&storage_root).unwrap(),
        ),
        Arc::new(crate::infrastructure::repositories::InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
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
        crate::application::nfs_gateway::NfsVolumeRegistry::new(),
        Arc::new(OneAgent(agent.clone())),
        records.clone(),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
        Arc::new(EventBus::new(64)),
        None,
    );
    let model = Arc::new(ScriptedModel {
        turns,
        calls: StdMutex::new(Vec::new()),
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
        .map(|m| m.content.clone())
        .expect("a tool message")
}

fn first_user_message(conversation: &[ChatMessage]) -> String {
    conversation
        .iter()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
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

/// A command asked for when the try has less time left than it keeps for
/// the model's next call is not run; the model is told why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_asked_for_with_no_time_left_is_not_run_and_the_model_is_told_why() {
    let w = world(
        agent("8s", 30),
        vec![
            run_command(json!({"command": "echo never", "cwd": "/tmp"})),
            answer("as far as I got"),
        ],
    )
    .await;
    // The try started 6 s ago: 2 s left, under the 4 s kept for the model.
    w.records.update(w.execution_id, |e| {
        e.start_iteration("count the ticks".to_string()).unwrap();
        e.iterations[0].started_at = chrono::Utc::now() - chrono::Duration::seconds(6);
    });
    let answer = w
        .inner_loop
        .handle_agent_message(AgentMessage::Generate {
            agent_id: w.agent.id.to_string(),
            execution_id: w.execution_id.to_string(),
            iteration_number: 1,
            prompt: "count the ticks".to_string(),
            messages: Vec::new(),
            model_alias: ALIAS.to_string(),
        })
        .await
        .expect("the try answers");
    assert!(
        matches!(answer, OrchestratorMessage::Final { .. }),
        "no command was dispatched: {answer:?}"
    );
    let seen = last_tool_message(&w.model.conversation(1));
    assert!(seen.contains("was not run"), "{seen}");
    assert!(seen.contains("s left"), "{seen}");
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
