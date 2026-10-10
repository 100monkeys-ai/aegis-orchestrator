// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A person's feedback reaches the paused workflow
//!
//! AEGIS ADR-141 F4 and F11: a person answers a Human gate with
//! `aegis.workflow.signal`, `yes` or `no`, with feedback. The worker reads
//! the `humanInput` signal as `(response, feedback?)`, so the feedback goes
//! as a second `json/plain` payload after the response, and a signal with no
//! feedback is the response alone. The call runs through the tool service
//! and a port wrapping the real `TemporalClient`, against a loopback
//! stand-in for Temporal's `SignalWorkflowExecution` that keeps every
//! request it was sent. The HTTP route sends through the same
//! `TemporalClient::send_human_signal`.
//!
//! | Scenario | Test |
//! |---|---|
//! | With feedback, two payloads | `a_signal_with_feedback_sends_the_feedback_as_a_second_payload` |
//! | Without feedback, one payload | `a_signal_without_feedback_sends_the_response_alone` |

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json::{json, Value};
use tonic::codegen::{http, BoxFuture, Context, Poll, Service};

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::ports::WorkflowExecutionControlPort;
use aegis_orchestrator_core::application::tool_invocation_service::{
    ToolInvocationResult, ToolInvocationService,
};
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::fsal::AegisFSAL;
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::security_context::{
    Capability, SecurityContext, SecurityContextMetadata, SecurityContextRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
use aegis_orchestrator_core::infrastructure::temporal_client::TemporalClient;
use aegis_orchestrator_core::infrastructure::temporal_proto::temporal::api::workflowservice::v1::{
    SignalWorkflowExecutionRequest as TemporalSignal, SignalWorkflowExecutionResponse,
};
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;

const CONTEXT: &str = "signal-feedback-test";

// ===========================================================================
// A stand-in for Temporal's SignalWorkflowExecution
// ===========================================================================

/// Answers `SignalWorkflowExecution` and keeps each request.
#[derive(Clone, Default)]
struct Temporal {
    signals: Arc<Mutex<Vec<TemporalSignal>>>,
}

impl tonic::server::NamedService for Temporal {
    const NAME: &'static str = "temporal.api.workflowservice.v1.WorkflowService";
}

struct Signals(Arc<Mutex<Vec<TemporalSignal>>>);

impl tonic::server::UnaryService<TemporalSignal> for Signals {
    type Response = SignalWorkflowExecutionResponse;
    type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;

    fn call(&mut self, request: tonic::Request<TemporalSignal>) -> Self::Future {
        self.0.lock().unwrap().push(request.into_inner());
        Box::pin(async { Ok(tonic::Response::new(SignalWorkflowExecutionResponse {})) })
    }
}

impl Service<http::Request<tonic::body::Body>> for Temporal {
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let signals = self.signals.clone();
        Box::pin(async move {
            if request.uri().path()
                != "/temporal.api.workflowservice.v1.WorkflowService/SignalWorkflowExecution"
            {
                return Ok(
                    tonic::Status::unimplemented(request.uri().path().to_string()).into_http(),
                );
            }
            let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::default());
            Ok(grpc.unary(Signals(signals), request).await)
        })
    }
}

/// The stand-in, served on a loopback port; answers its address.
async fn serve(temporal: Temporal) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(temporal)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    format!("http://{address}")
}

// ===========================================================================
// The port, wrapping the real client
// ===========================================================================

/// Signals through `TemporalClient::send_human_signal`, as the daemon's
/// adapter does.
struct ClientPort(TemporalClient);

#[async_trait]
impl WorkflowExecutionControlPort for ClientPort {
    async fn cancel_workflow_execution(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("not exercised".into())
    }
    async fn signal_workflow_execution(
        &self,
        _: &TenantId,
        execution_id: ExecutionId,
        response: &str,
        feedback: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0
            .send_human_signal(&execution_id.0.to_string(), response.to_string(), feedback)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })
    }
    async fn remove_workflow_execution(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("not exercised".into())
    }
}

// ===========================================================================
// The tool service, with one agent execution allowed `aegis.workflow.*`
// ===========================================================================

struct Harness {
    service: ToolInvocationService,
    agent_id: AgentId,
    run: ExecutionId,
    temporal: Temporal,
}

async fn harness() -> Harness {
    let temporal = Temporal::default();
    let address = serve(temporal.clone()).await;
    let client = TemporalClient::new(&address, "default", "aegis-queue", "http://127.0.0.1:9")
        .await
        .unwrap_or_else(|e| panic!("the client did not reach the stand-in: {e:#}"));

    let agent = agent();
    let agent_id = agent.id;
    let mut execution = Execution::new_with_id(
        ExecutionId::new(),
        agent_id,
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
    execution.tenant_id = TenantId::default();
    let run = execution.id;

    let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
    security_context_repo
        .save(security_context())
        .await
        .unwrap();
    let storage_root =
        std::env::temp_dir().join(format!("aegis-signal-feedback-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let service = ToolInvocationService::new(
        Arc::new(InMemorySealSessionRepository::new()),
        security_context_repo,
        Arc::new(SealMiddleware::new()),
        Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(agent)),
        Arc::new(Executions(HashMap::from([(run, execution)]))),
        Arc::new(
            aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(
            ),
        ),
        Arc::new(EventBus::new(64)),
        None,
    )
    .with_workflow_execution_control(Arc::new(ClientPort(client)));
    Harness {
        service,
        agent_id,
        run,
        temporal,
    }
}

impl Harness {
    async fn signal(&self, args: Value) -> Value {
        match self
            .service
            .invoke_tool_internal(
                &self.agent_id,
                self.run,
                TenantId::default(),
                0,
                Vec::new(),
                "aegis.workflow.signal".to_string(),
                args,
            )
            .await
        {
            Ok(ToolInvocationResult::Direct(value)) => value,
            other => panic!("aegis.workflow.signal did not answer directly: {other:?}"),
        }
    }

    /// The payloads of each signal the stand-in was sent, as
    /// `(encoding, data)`.
    fn payloads(&self) -> Vec<Vec<(String, Vec<u8>)>> {
        self.temporal
            .signals
            .lock()
            .unwrap()
            .iter()
            .map(|signal| {
                assert_eq!(signal.signal_name, "humanInput");
                signal
                    .input
                    .as_ref()
                    .map(|input| {
                        input
                            .payloads
                            .iter()
                            .map(|p| {
                                (
                                    String::from_utf8(
                                        p.metadata.get("encoding").cloned().unwrap_or_default(),
                                    )
                                    .unwrap(),
                                    p.data.clone(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect()
    }
}

/// F4 and F11: "no, with feedback" sends the response, then the feedback,
/// each JSON-encoded as `json/plain`, as the worker's
/// `defineSignal<[string, string?]>("humanInput")` reads them.
#[tokio::test]
async fn a_signal_with_feedback_sends_the_feedback_as_a_second_payload() {
    let h = harness().await;
    let workflow_run = uuid::Uuid::new_v4().to_string();

    let answer = h
        .signal(json!({
            "execution_id": workflow_run,
            "response": "no",
            "feedback": "their words",
        }))
        .await;

    assert_eq!(answer["signalled"], json!(true), "{answer}");
    assert_eq!(
        h.payloads(),
        vec![vec![
            ("json/plain".to_string(), serde_json::to_vec("no").unwrap()),
            (
                "json/plain".to_string(),
                serde_json::to_vec("their words").unwrap()
            ),
        ]],
    );
}

/// A signal with no feedback is the response alone, as before.
#[tokio::test]
async fn a_signal_without_feedback_sends_the_response_alone() {
    let h = harness().await;
    let workflow_run = uuid::Uuid::new_v4().to_string();

    let answer = h
        .signal(json!({"execution_id": workflow_run, "response": "yes"}))
        .await;

    assert_eq!(answer["signalled"], json!(true), "{answer}");
    assert_eq!(
        h.payloads(),
        vec![vec![(
            "json/plain".to_string(),
            serde_json::to_vec("yes").unwrap()
        )]],
    );
}

// ===========================================================================
// Test doubles the dispatch reads
// ===========================================================================

fn agent() -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: signal-feedback-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: ["aegis.workflow.signal"]
"#,
    )
    .unwrap();
    Agent {
        id: AgentId::new(),
        tenant_id: TenantId::default(),
        scope: aegis_orchestrator_core::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn security_context() -> SecurityContext {
    SecurityContext {
        name: CONTEXT.to_string(),
        description: "signal feedback test".to_string(),
        capabilities: vec![Capability {
            tool_pattern: "aegis.workflow.*".to_string(),
            path_allowlist: None,
            command_allowlist: None,
            subcommand_allowlist: None,
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        }],
        deny_list: vec![],
        metadata: SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

struct Executions(HashMap<ExecutionId, Execution>);

#[async_trait]
impl ExecutionService for Executions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        Ok(execution_id)
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_for_tenant(&self, _: &TenantId, id: ExecutionId) -> Result<Execution> {
        self.get_execution_unscoped(id).await
    }
    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        self.0
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("execution not found"))
    }
    async fn get_iterations_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> Result<Vec<Iteration>> {
        anyhow::bail!("not exercised")
    }
    async fn cancel_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn stream_execution(
        &self,
        _: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn list_executions_for_tenant(
        &self,
        _: &TenantId,
        _: Option<AgentId>,
        _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
        _: usize,
    ) -> Result<Vec<Execution>> {
        anyhow::bail!("not exercised")
    }
    async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: aegis_orchestrator_core::domain::execution::LlmInteraction,
    ) -> Result<()> {
        Ok(())
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Resolves every agent to one agent with no `tool_validation`, so the
/// inner-loop judge does not run.
struct OneAgent(Agent);

#[async_trait]
impl AgentLifecycleService for OneAgent {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: aegis_orchestrator_core::domain::agent::AgentScope,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
        Ok(self.0.clone())
    }
    async fn update_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: AgentManifest,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> Result<Option<AgentId>> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

struct NoOpPublisher;

#[async_trait]
impl aegis_orchestrator_core::domain::fsal::EventPublisher for NoOpPublisher {
    async fn publish_storage_event(
        &self,
        _event: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}
