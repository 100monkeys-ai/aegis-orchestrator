//! The orchestrator's wire to the SEAL gateway (AEGIS ADR-132's Update (3),
//! H1 to H8), driven where production drives it: `invoke_tool_internal` on a
//! real `ToolInvocationService`, against a loopback gateway that records
//! every request it receives and answers as it is told.

use super::*;
use crate::domain::agent::{Agent, AgentManifest, AgentStatus};
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{Execution, ExecutionId, ExecutionInput, Iteration};
use crate::domain::repository::AgentVersion;
use crate::domain::security_context::SecurityContext;
use crate::infrastructure::event_bus::DomainEvent;
use crate::infrastructure::repositories::InMemoryVolumeRepository;
use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use crate::infrastructure::seal_gateway_proto::gateway_invocation_service_server::{
    GatewayInvocationService as GrpcGatewayInvocationService, GatewayInvocationServiceServer,
};
use crate::infrastructure::seal_gateway_proto::{
    ExploreApiRequest, ExploreApiResponse, InvokeCliRequest as PbInvokeCliRequest,
    InvokeCliResponse, InvokeToolRequest as PbInvokeToolRequest, InvokeToolResponse,
    InvokeWorkflowRequest as PbInvokeWorkflowRequest, InvokeWorkflowResponse,
    ListToolsRequest as PbListToolsRequest, ListToolsResponse, ToolSummary,
};
use crate::infrastructure::storage::LocalHostStorageProvider;
use async_trait::async_trait;
use futures::Stream;
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use tokio::sync::oneshot;

const CONTEXT: &str = "wire-test-context";
const USER: &str = "user-1";

// ---------------------------------------------------------------------------
// The loopback gateway
// ---------------------------------------------------------------------------

/// Every request the stub gateway received.
#[derive(Default)]
struct Received {
    lists: Vec<PbListToolsRequest>,
    workflows: Vec<PbInvokeWorkflowRequest>,
    clis: Vec<PbInvokeCliRequest>,
    tools: Vec<PbInvokeToolRequest>,
}

/// How the stub answers an invocation.
#[derive(Clone)]
enum Answer {
    /// The result JSON.
    Result(String),
    /// A refusal: the gRPC code, the `seal-refusal-code` (none: no metadata)
    /// and the message.
    Refuse(tonic::Code, Option<&'static str>, String),
}

#[derive(Clone)]
struct StubGateway {
    /// The node-wide tools it lists.
    tools: Vec<ToolSummary>,
    answer: Answer,
    received: Arc<StdMutex<Received>>,
}

impl StubGateway {
    fn new(tools: Vec<ToolSummary>, answer: Answer) -> Self {
        Self {
            tools,
            answer,
            received: Arc::new(StdMutex::new(Received::default())),
        }
    }

    fn answer<T>(&self, ok: impl FnOnce(String) -> T) -> Result<tonic::Response<T>, tonic::Status> {
        match &self.answer {
            Answer::Result(json) => Ok(tonic::Response::new(ok(json.clone()))),
            Answer::Refuse(grpc, code, message) => {
                let mut status = tonic::Status::new(*grpc, message.clone());
                if let Some(code) = code {
                    status
                        .metadata_mut()
                        .insert(super::gateway::REFUSAL_CODE_METADATA, code.parse().unwrap());
                }
                Err(status)
            }
        }
    }
}

#[tonic::async_trait]
impl GrpcGatewayInvocationService for StubGateway {
    async fn invoke_workflow(
        &self,
        req: tonic::Request<PbInvokeWorkflowRequest>,
    ) -> Result<tonic::Response<InvokeWorkflowResponse>, tonic::Status> {
        self.received
            .lock()
            .unwrap()
            .workflows
            .push(req.into_inner());
        self.answer(|result_json| InvokeWorkflowResponse { result_json })
    }
    async fn invoke_cli(
        &self,
        req: tonic::Request<PbInvokeCliRequest>,
    ) -> Result<tonic::Response<InvokeCliResponse>, tonic::Status> {
        self.received.lock().unwrap().clis.push(req.into_inner());
        self.answer(|stdout| InvokeCliResponse {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        })
    }
    async fn explore_api(
        &self,
        _req: tonic::Request<ExploreApiRequest>,
    ) -> Result<tonic::Response<ExploreApiResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn list_tools(
        &self,
        req: tonic::Request<PbListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        let listing = req.into_inner();
        let mut tools = self.tools.clone();
        // As the gateway does: each bound server's tools, under its name.
        for bound in &listing.bound_servers {
            if bound.credential.is_some() {
                tools.push(listed(&format!("{}.lookup", bound.server), "mcp"));
            }
        }
        self.received.lock().unwrap().lists.push(listing);
        Ok(tonic::Response::new(ListToolsResponse { tools }))
    }
    async fn invoke_tool(
        &self,
        req: tonic::Request<PbInvokeToolRequest>,
    ) -> Result<tonic::Response<InvokeToolResponse>, tonic::Status> {
        self.received.lock().unwrap().tools.push(req.into_inner());
        self.answer(|result_json| InvokeToolResponse { result_json })
    }
}

fn listed(name: &str, kind: &str) -> ToolSummary {
    ToolSummary {
        name: name.to_string(),
        description: format!("{name} (stub)"),
        kind: kind.to_string(),
        input_schema_json: r#"{"type":"object"}"#.to_string(),
        tags: vec![kind.to_string()],
        category: "external".to_string(),
    }
}

/// Serve `stub` on a loopback port over plaintext; the URL and a shutdown
/// handle.
async fn serve_plaintext(stub: StubGateway) -> (String, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(GatewayInvocationServiceServer::new(stub))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = rx.await;
                },
            )
            .await;
    });
    (format!("http://{addr}"), tx)
}

// ---------------------------------------------------------------------------
// The orchestrator under test
// ---------------------------------------------------------------------------

fn agent(tools: &[&str]) -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(&format!(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: wire-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: {}
"#,
        serde_json::to_string(tools).unwrap()
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

/// Serves the executions that call tools.
struct Executions(HashMap<ExecutionId, Execution>);

#[async_trait]
impl ExecutionService for Executions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
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
        _: Option<crate::domain::workflow::WorkflowId>,
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
        _: crate::domain::execution::LlmInteraction,
    ) -> Result<()> {
        Ok(())
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<crate::domain::execution::TrajectoryStep>,
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
        _: crate::domain::agent::AgentScope,
        _: Option<&crate::domain::iam::UserIdentity>,
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
impl crate::domain::fsal::EventPublisher for NoOpPublisher {
    async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
}

fn security_context(patterns: &[&str]) -> SecurityContext {
    SecurityContext {
        name: CONTEXT.to_string(),
        description: "wire test".to_string(),
        capabilities: patterns
            .iter()
            .map(|pattern| crate::domain::security_context::Capability {
                tool_pattern: pattern.to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            })
            .collect(),
        deny_list: vec![],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

/// The orchestrator, an agent and its executions.
struct Harness {
    service: ToolInvocationService,
    tenant: TenantId,
    agent_id: AgentId,
    /// An execution whose initiating user is `USER`.
    execution: ExecutionId,
}

/// What a harness is built with.
struct Setup {
    gateway_url: Option<String>,
    context_patterns: Vec<&'static str>,
    agent_tools: Vec<&'static str>,
    router: ToolRouter,
}

impl Setup {
    fn gateway(url: &str) -> Self {
        Self {
            gateway_url: Some(url.to_string()),
            context_patterns: vec!["*"],
            agent_tools: vec![],
            router: ToolRouter::new(ToolRouter::builtin_dispatchers()),
        }
    }
}

async fn harness(setup: Setup) -> Harness {
    let tenant = TenantId::for_consumer_user(USER).unwrap();
    let agent = agent(&setup.agent_tools);
    let agent_id = agent.id;
    let execution_for = |user: Option<&str>| {
        let mut e = Execution::new_with_id(
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
        e.tenant_id = tenant.clone();
        e.initiating_user_sub = user.map(str::to_string);
        e
    };
    let executions = vec![execution_for(Some(USER)), execution_for(None)];
    let execution = executions[0].id;

    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    security_context_repo
        .save(security_context(&setup.context_patterns))
        .await
        .unwrap();
    let storage_root =
        std::env::temp_dir().join(format!("aegis-wire-tests-{}", uuid::Uuid::new_v4()));
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
        Arc::new(setup.router),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(agent)),
        Arc::new(Executions(
            executions.into_iter().map(|e| (e.id, e)).collect(),
        )),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
        Arc::new(EventBus::new(1024)),
        setup.gateway_url,
    );
    Harness {
        service,
        tenant,
        agent_id,
        execution,
    }
}

impl Harness {
    async fn call(&self, execution: ExecutionId, tool: &str) -> Result<Value, SealSessionError> {
        match self
            .service
            .invoke_tool_internal(
                &self.agent_id,
                execution,
                self.tenant.clone(),
                0,
                Vec::new(),
                tool.to_string(),
                json!({"query": "q"}),
            )
            .await?
        {
            ToolInvocationResult::Direct(value) => Ok(value),
            ToolInvocationResult::DispatchRequired(action) => {
                panic!("unexpected dispatch: {action:?}")
            }
        }
    }
}

/// The error an agent's inner loop is shown for `error` (ADR-035 R7).
fn shown(error: &SealSessionError) -> &SealSessionError {
    match error {
        SealSessionError::Answered { shown, .. } => shown,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// H5: the gateway's refusals reach the caller
// ---------------------------------------------------------------------------

/// Each refusal the gateway answers reaches the caller of the invoke route
/// by its code's row, never as a tool that does not exist, and the agent's
/// inner loop is shown the error the doc comment of `gateway_refusal` names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_refusal_reaches_the_caller_by_its_code() {
    use tonic::Code;
    let cases: Vec<(Code, &'static str, u16, &'static str)> = vec![
        (
            Code::PermissionDenied,
            "CREDENTIAL_BINDING_REQUIRED",
            403,
            "NotFound",
        ),
        (
            Code::PermissionDenied,
            "CREDENTIAL_REJECTED",
            403,
            "NotFound",
        ),
        (
            Code::FailedPrecondition,
            "REMOTE_TOOL_ERROR",
            422,
            "InvalidArguments",
        ),
        (
            Code::Unavailable,
            "CREDENTIAL_CHANNEL_NOT_CONFIDENTIAL",
            503,
            "InternalError",
        ),
        (Code::NotFound, "NOT_FOUND", 404, "InternalError"),
        (
            Code::InvalidArgument,
            "INVALID_ARGUMENTS",
            422,
            "InvalidArguments",
        ),
        (
            Code::Unavailable,
            "UPSTREAM_UNAVAILABLE",
            502,
            "UpstreamUnavailable",
        ),
        (
            Code::ResourceExhausted,
            "RATE_LIMIT_EXCEEDED",
            502,
            "UpstreamUnavailable",
        ),
        (
            Code::Unavailable,
            "SERVICE_UNAVAILABLE",
            503,
            "InternalError",
        ),
    ];
    for (grpc, code, http_status, shown_as) in cases {
        let message = format!("the gateway's words for {code}");
        let stub = StubGateway::new(
            vec![listed("ext.lookup", "workflow")],
            Answer::Refuse(grpc, Some(code), message.clone()),
        );
        let (url, _shutdown) = serve_plaintext(stub.clone()).await;
        let h = harness(Setup::gateway(&url)).await;

        let err = h.call(h.execution, "ext.lookup").await.expect_err(code);
        let refusal = err.refusal();
        let expected_code = match code {
            "RATE_LIMIT_EXCEEDED" => "UPSTREAM_UNAVAILABLE",
            other => other,
        };
        assert_eq!(refusal.code, expected_code, "{code}: {err:?}");
        assert_eq!(refusal.http_status, http_status, "{code}: {err:?}");
        let shown_variant = format!("{:?}", shown(&err));
        assert!(
            shown_variant.starts_with(shown_as),
            "{code}: the inner loop is shown {shown_variant}"
        );
        if !refusal.internal && code != "INVALID_ARGUMENTS" {
            assert_eq!(refusal.message, message, "{code}");
        }
        assert_eq!(stub.received.lock().unwrap().workflows.len(), 1, "{code}");
    }
}

/// A tool the gateway does not list for the tenant is answered as one that
/// does not exist, and nothing is invoked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_the_gateway_does_not_list_is_not_found_and_not_invoked() {
    let stub = StubGateway::new(vec![], Answer::Result("{}".to_string()));
    let (url, _shutdown) = serve_plaintext(stub.clone()).await;
    let h = harness(Setup::gateway(&url)).await;

    let err = h
        .call(h.execution, "ext.unknown")
        .await
        .expect_err("refused");
    let refusal = err.refusal();
    assert_eq!((refusal.http_status, refusal.code), (404, "NOT_FOUND"));
    assert_eq!(refusal.message, "Not found: tool 'ext.unknown'.");
    let received = stub.received.lock().unwrap();
    assert!(received.workflows.is_empty() && received.clis.is_empty());
    assert_eq!(received.lists.len(), 1);
    assert_eq!(received.lists[0].tenant_id, h.tenant.as_str());
}

/// A gateway failure that carries no refusal code is an internal failure,
/// answered 500 with its detail in the log only, never "Not found".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_failure_without_a_code_is_internal_not_a_missing_tool() {
    let stub = StubGateway::new(
        vec![listed("ext.lookup", "workflow")],
        Answer::Refuse(
            tonic::Code::Unavailable,
            None,
            "Mk7-gateway-detail".to_string(),
        ),
    );
    let (url, _shutdown) = serve_plaintext(stub).await;
    let h = harness(Setup::gateway(&url)).await;

    let err = h
        .call(h.execution, "ext.lookup")
        .await
        .expect_err("refused");
    let refusal = err.refusal();
    assert_eq!((refusal.http_status, refusal.code), (500, "INTERNAL_ERROR"));
    assert!(!refusal.message.contains("Mk7-"));
}

/// A listed result passes to the caller unchanged; a CLI tool goes to
/// `InvokeCli` by its listed kind, not by the shape of its arguments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listed_tool_is_invoked_by_its_kind_and_its_result_passes_unchanged() {
    let stub = StubGateway::new(
        vec![listed("ext.lookup", "workflow")],
        Answer::Result(r#"{"content":[{"type":"text","text":"found"}]}"#.to_string()),
    );
    let (url, _shutdown) = serve_plaintext(stub.clone()).await;
    let h = harness(Setup::gateway(&url)).await;

    let value = h.call(h.execution, "ext.lookup").await.expect("answered");
    assert_eq!(value, json!({"content":[{"type":"text","text":"found"}]}));
    let received = stub.received.lock().unwrap();
    assert_eq!(received.workflows.len(), 1);
    assert_eq!(received.workflows[0].workflow_name, "ext.lookup");
    assert_eq!(received.workflows[0].tenant_id, h.tenant.as_str());
    assert!(received.clis.is_empty());
}
