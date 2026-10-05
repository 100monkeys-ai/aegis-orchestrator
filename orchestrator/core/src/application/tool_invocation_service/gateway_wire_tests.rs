//! The orchestrator's wire to the SEAL gateway (AEGIS ADR-132's Update (3),
//! H1 to H8), driven where production drives it: `invoke_tool_internal` on a
//! real `ToolInvocationService`, against a loopback gateway that records
//! every request it receives and answers as it is told.

use super::*;
use crate::application::credential_service::{
    OAuthProviderConfig, OAuthProviderRegistry, StandardCredentialManagementService,
};
use crate::domain::agent::{Agent, AgentManifest, AgentStatus};
use crate::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialMetadata,
    CredentialProvider, CredentialScope, CredentialStatus, CredentialType, GrantTarget,
    OAuthPendingState, UserCredentialBinding,
};
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{Execution, ExecutionId, ExecutionInput, Iteration};
use crate::domain::repository::AgentVersion;
use crate::domain::secrets::{AccessContext, SecretPath};
use crate::domain::security_context::SecurityContext;
use crate::infrastructure::event_bus::DomainEvent;
use crate::infrastructure::repositories::InMemoryVolumeRepository;
use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use crate::infrastructure::seal_gateway_proto::gateway_invocation_service_server::{
    GatewayInvocationService as GrpcGatewayInvocationService, GatewayInvocationServiceServer,
};
use crate::infrastructure::seal_gateway_proto::{
    CredentialKind, ExploreApiRequest, ExploreApiResponse, InvokeCliRequest as PbInvokeCliRequest,
    InvokeCliResponse, InvokeToolRequest as PbInvokeToolRequest, InvokeToolResponse,
    InvokeWorkflowRequest as PbInvokeWorkflowRequest, InvokeWorkflowResponse,
    ListToolsRequest as PbListToolsRequest, ListToolsResponse, ToolSummary,
};
use crate::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
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

/// A throwaway TLS certificate for `localhost`, written where the
/// orchestrator's `ca_cert_path` reads it.
struct Tls {
    ca_path: std::path::PathBuf,
}

/// Serve `stub` on a loopback port over TLS with a throwaway certificate
/// for `localhost`; the `https` URL, the certificate (its own CA) and a
/// shutdown handle.
async fn serve_tls(stub: StubGateway) -> (String, Tls, oneshot::Sender<()>) {
    // tonic's TLS server takes rustls's process-wide provider, which this
    // test binary (ring and aws-lc-rs both compiled in) cannot pick alone.
    // The orchestrator's client is unaffected: tonic's client falls back to
    // ring when none is installed.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("throwaway certificate");
    let dir = std::env::temp_dir().join(format!("aegis-wire-tls-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let ca_path = dir.join("gateway-ca.crt");
    std::fs::write(&ca_path, generated.cert.pem()).unwrap();
    let identity = tonic::transport::Identity::from_pem(
        generated.cert.pem(),
        generated.signing_key.serialize_pem(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel::<()>();
    let mut server = tonic::transport::Server::builder()
        .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))
        .expect("server TLS");
    tokio::spawn(async move {
        let _ = server
            .add_service(GatewayInvocationServiceServer::new(stub))
            .serve_with_incoming_shutdown(
                tonic::transport::server::TcpIncoming::from(listener),
                async {
                    let _ = rx.await;
                },
            )
            .await;
    });
    (format!("https://localhost:{port}"), Tls { ca_path }, tx)
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
    /// An execution of the same tenant with no person recorded.
    personless_execution: ExecutionId,
}

/// What a harness is built with.
struct Setup {
    gateway_url: Option<String>,
    context_patterns: Vec<&'static str>,
    agent_tools: Vec<&'static str>,
    router: ToolRouter,
    /// The CA file the gateway's certificate is verified against.
    ca: Option<std::path::PathBuf>,
    /// The credential source remote tool calls resolve through.
    credentials: Option<Arc<StandardCredentialManagementService>>,
    /// The remote servers the orchestrator knows by name.
    remote_servers: Vec<String>,
    /// The workflow the executions' workflow run belongs to, if any.
    workflow_id: Option<uuid::Uuid>,
    /// The approval gate, if enabled.
    approvals: Option<Arc<crate::application::tool_approval_service::ToolApprovalService>>,
}

impl Setup {
    fn gateway(url: &str) -> Self {
        Self {
            gateway_url: Some(url.to_string()),
            context_patterns: vec!["*"],
            agent_tools: vec![],
            router: ToolRouter::new(ToolRouter::builtin_dispatchers()),
            ca: None,
            credentials: None,
            remote_servers: Vec::new(),
            workflow_id: None,
            approvals: None,
        }
    }
}

async fn harness(setup: Setup) -> Harness {
    let tenant = TenantId::for_consumer_user(USER).unwrap();
    let agent = agent(&setup.agent_tools);
    let agent_id = agent.id;
    let workflow_executions =
        Arc::new(crate::infrastructure::repositories::InMemoryWorkflowExecutionRepository::new());
    let workflow_run = match setup.workflow_id {
        Some(workflow_id) => {
            let run = crate::domain::workflow::WorkflowExecution {
                id: ExecutionId::new(),
                workflow_id: crate::domain::workflow::WorkflowId::from_uuid(workflow_id),
                tenant_id: tenant.clone(),
                status: crate::domain::execution::ExecutionStatus::Running,
                current_state: crate::domain::workflow::StateName::new("START").unwrap(),
                blackboard: crate::domain::workflow::Blackboard::new(),
                input: json!({}),
                state_outputs: HashMap::new(),
                final_output: None,
                started_at: chrono::Utc::now(),
                last_transition_at: chrono::Utc::now(),
                initiating_user_sub: Some(USER.to_string()),
            };
            crate::domain::repository::WorkflowExecutionRepository::save_for_tenant(
                workflow_executions.as_ref(),
                &tenant,
                &run,
            )
            .await
            .unwrap();
            Some(run.id.0)
        }
        None => None,
    };
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
                workflow_execution_id: workflow_run,
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
    let (execution, personless_execution) = (executions[0].id, executions[1].id);

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
    let service = service
        .with_workflow_execution_repo(workflow_executions)
        .with_remote_tool_servers(setup.remote_servers);
    let service = match setup.credentials {
        Some(credentials) => service.with_tool_credentials(credentials),
        None => service,
    };
    let service = match setup.approvals {
        Some(approvals) => service.with_tool_approvals(approvals),
        None => service,
    };
    let service = match &setup.ca {
        Some(ca) => service
            .with_seal_gateway_ca_cert(ca)
            .expect("the CA file reads"),
        None => service,
    };
    Harness {
        service,
        tenant,
        agent_id,
        execution,
        personless_execution,
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
    // H4: the existing requests carry the acting identity, for audit.
    for acting in [
        received.workflows[0].acting.as_ref(),
        received.lists[0].acting.as_ref(),
    ] {
        let acting = acting.expect("acting identity");
        assert_eq!(acting.user_id, USER);
        assert_eq!(acting.agent_id, h.agent_id.to_string());
        assert_eq!(acting.workflow_id, "");
    }
}

// ---------------------------------------------------------------------------
// H8 from the caller's side: TLS to the gateway, verified against a CA
// ---------------------------------------------------------------------------

/// An `https` gateway is dialled over TLS and its certificate verified
/// against the CA `seal_gateway.ca_cert_path` names; the call goes through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tls_gateway_is_dialled_and_verified_against_the_configured_ca() {
    let stub = StubGateway::new(
        vec![listed("ext.lookup", "workflow")],
        Answer::Result(r#"{"ok":true}"#.to_string()),
    );
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let h = harness(Setup {
        ca: Some(tls.ca_path.clone()),
        ..Setup::gateway(&url)
    })
    .await;

    let value = h.call(h.execution, "ext.lookup").await.expect("answered");
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(stub.received.lock().unwrap().workflows.len(), 1);
}

/// A gateway whose certificate the configured CA did not sign is refused at
/// the handshake: an internal failure, and nothing reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_certificate_the_ca_does_not_sign_is_refused() {
    let stub = StubGateway::new(
        vec![listed("ext.lookup", "workflow")],
        Answer::Result(r#"{"ok":true}"#.to_string()),
    );
    let (url, _tls, _shutdown) = serve_tls(stub.clone()).await;
    let other = serve_tls(StubGateway::new(vec![], Answer::Result("{}".into()))).await;
    let h = harness(Setup {
        ca: Some(other.1.ca_path.clone()),
        ..Setup::gateway(&url)
    })
    .await;

    let err = h
        .call(h.execution, "ext.lookup")
        .await
        .expect_err("refused");
    let refusal = err.refusal();
    assert_eq!((refusal.http_status, refusal.code), (500, "INTERNAL_ERROR"));
    let received = stub.received.lock().unwrap();
    assert!(received.lists.is_empty() && received.workflows.is_empty());
}

/// `seal_gateway.ca_cert_path` is optional: absent, it reads as none.
#[test]
fn the_gateway_ca_cert_path_is_optional() {
    let with: crate::domain::node_config::SealGatewayConfig = serde_yaml::from_str(
        "url: https://aegis-seal-gateway:50055\nca_cert_path: /etc/aegis/tls/ca.crt\n",
    )
    .unwrap();
    assert_eq!(
        with.ca_cert_path.as_deref(),
        Some(std::path::Path::new("/etc/aegis/tls/ca.crt"))
    );
    let without: crate::domain::node_config::SealGatewayConfig =
        serde_yaml::from_str("url: https://aegis-seal-gateway:50055\n").unwrap();
    assert!(without.ca_cert_path.is_none());
}

// ---------------------------------------------------------------------------
// H1, H3, H4, H6, H8: a remote server's tool, called with the acting user's
// own credential
// ---------------------------------------------------------------------------

/// The marker value of the acting user's stored credential: it must reach
/// the gateway on the call and appear in no log line and no error.
const MARKER: &str = "Mk9-acting-user-credential";
const SERVER: &str = "notes-1";

/// Bindings held in memory, as the Postgres repository holds them.
#[derive(Default)]
struct Bindings(tokio::sync::RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>);

#[async_trait]
impl CredentialBindingRepository for Bindings {
    async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()> {
        self.0.write().await.insert(binding.id, binding.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        Ok(self.0.read().await.get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok(self
            .0
            .read()
            .await
            .values()
            .filter(|b| &b.tenant_id == tenant_id && b.owner_user_id == owner_user_id)
            .cloned()
            .collect())
    }
    async fn find_active_grants_for_target(
        &self,
        _: &TenantId,
        _: &str,
        _: &CredentialProvider,
        _: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>> {
        Ok(Vec::new())
    }
    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
        self.0.write().await.remove(id);
        Ok(())
    }
    async fn save_oauth_state(
        &self,
        _: &str,
        _: &CredentialBindingId,
        _: &str,
        _: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn find_oauth_state(&self, _: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(None)
    }
    async fn delete_oauth_state(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_expired_oauth_states(
        &self,
        _: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<u64> {
        Ok(0)
    }
}

/// The real credential service over in-memory bindings and secrets, with
/// one OAuth provider `SERVER` whose token endpoint is `token_url`.
struct Vault {
    service: Arc<StandardCredentialManagementService>,
    bindings: Arc<Bindings>,
    secrets: Arc<SecretsManager>,
}

impl Vault {
    fn new(token_url: &str) -> Self {
        let bindings = Arc::new(Bindings::default());
        let event_bus = Arc::new(EventBus::new(64));
        let secrets = Arc::new(SecretsManager::from_store(
            Arc::new(TestSecretStore::new()),
            event_bus.clone(),
        ));
        let mut registry: OAuthProviderRegistry = HashMap::new();
        registry.insert(
            CredentialProvider::new(SERVER),
            OAuthProviderConfig {
                authorization_url: "https://auth.example.test/authorize".into(),
                token_url: token_url.into(),
                client_id: "wire-client".to_string(),
                client_secret: Some(SensitiveString::new("wire-client-secret")),
                redirect_uri_allowlist: vec!["https://app.example.test/cb".to_string()],
                scopes: Vec::new(),
                extra_authorization_params: Default::default(),
                display_name: None,
            },
        );
        let service = Arc::new(StandardCredentialManagementService::with_http_client(
            bindings.clone(),
            secrets.clone(),
            event_bus,
            Arc::new(registry),
            reqwest::Client::new(),
        ));
        Self {
            service,
            bindings,
            secrets,
        }
    }

    /// Store a binding of `user` to `server` holding `fields`, granted to
    /// each of `grants`.
    async fn bind(
        &self,
        tenant: &TenantId,
        user: &str,
        server: &str,
        credential_type: CredentialType,
        fields: &[(&str, &str)],
        grants: &[GrantTarget],
    ) -> CredentialBindingId {
        let id = CredentialBindingId::new();
        let path = SecretPath::for_tenant(
            tenant.clone(),
            "kv",
            format!("users/{}/{user}/credentials/{}", tenant.as_str(), id.0),
        );
        self.secrets
            .write_secret(
                &path.effective_mount(),
                &path.path,
                fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), SensitiveString::new(*v)))
                    .collect(),
                &AccessContext::system("wire-test"),
            )
            .await
            .unwrap();
        let now = chrono::Utc::now();
        let mut binding = UserCredentialBinding {
            id,
            owner_user_id: user.to_string(),
            tenant_id: tenant.clone(),
            credential_type,
            provider: CredentialProvider::new(server),
            secret_path: path,
            scope: CredentialScope::Personal,
            status: CredentialStatus::Active,
            metadata: CredentialMetadata {
                label: format!("{server} for {user}"),
                tags: None,
                service_url: None,
                external_account_id: None,
                oauth_scopes: None,
                mailbox: None,
            },
            grants: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        for grant in grants {
            binding.add_grant(grant.clone(), user.to_string());
        }
        self.bindings.save(&binding).await.unwrap();
        id
    }

    async fn stored(&self, id: CredentialBindingId) -> HashMap<String, SensitiveString> {
        let b = self.bindings.find_by_id(&id).await.unwrap().unwrap();
        self.secrets
            .read_secret(
                &b.secret_path.effective_mount(),
                &b.secret_path.path,
                &AccessContext::system("wire-test"),
            )
            .await
            .unwrap()
    }
}

/// A harness on a TLS gateway that knows the remote server `SERVER`, with
/// `vault` as its credential source.
async fn remote_harness(
    url: &str,
    tls: &Tls,
    vault: &Vault,
    patterns: Vec<&'static str>,
) -> Harness {
    harness(Setup {
        context_patterns: patterns,
        ca: Some(tls.ca_path.clone()),
        credentials: Some(vault.service.clone()),
        remote_servers: vec![SERVER.to_string()],
        ..Setup::gateway(url)
    })
    .await
}

fn remote_result() -> Answer {
    Answer::Result(r#"{"content":[{"type":"text","text":"page"}],"isError":false}"#.to_string())
}

/// H1, H4: a call of a remote server's tool by an agent its user granted
/// the binding to reaches the gateway as `InvokeTool`, with the tenant, the
/// acting identity, the server, the tool, the arguments as given and the
/// user's credential, over TLS; the server's result passes unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_granted_agents_call_reaches_the_gateway_with_the_credential_and_the_acting_identity() {
    let stub = StubGateway::new(vec![], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = remote_harness(&url, &tls, &vault, vec!["*"]).await;
    vault
        .bind(
            &h.tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", MARKER)],
            &[GrantTarget::Agent {
                agent_id: h.agent_id,
            }],
        )
        .await;

    let value = h
        .call(h.execution, &format!("{SERVER}.pages.read"))
        .await
        .expect("the call is made");
    assert_eq!(
        value,
        json!({"content":[{"type":"text","text":"page"}],"isError":false})
    );
    let received = stub.received.lock().unwrap();
    assert_eq!(received.tools.len(), 1);
    let call = &received.tools[0];
    assert_eq!(call.server, SERVER);
    assert_eq!(call.tool, "pages.read");
    assert_eq!(call.tenant_id, h.tenant.as_str());
    assert_eq!(call.execution_id, h.execution.to_string());
    assert_eq!(
        serde_json::from_str::<Value>(&call.arguments_json).unwrap(),
        json!({"query": "q"})
    );
    let acting = call.acting.as_ref().expect("acting identity");
    assert_eq!(acting.user_id, USER);
    assert_eq!(acting.agent_id, h.agent_id.to_string());
    assert_eq!(acting.workflow_id, "");
    let credential = call.credential.as_ref().expect("credential");
    assert_eq!(credential.kind, CredentialKind::BearerToken as i32);
    assert_eq!(credential.value, MARKER);
    assert!(received.workflows.is_empty() && received.lists.is_empty());
}

/// H1: a grant to all the owner's agents, or to the workflow the run belongs
/// to, also opens the call; the workflow is named on the acting identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_to_all_agents_or_to_the_runs_workflow_opens_the_call() {
    let workflow_id = uuid::Uuid::new_v4();
    for grant in [
        GrantTarget::AllAgents,
        GrantTarget::Workflow { workflow_id },
    ] {
        let stub = StubGateway::new(vec![], remote_result());
        let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
        let vault = Vault::new("https://token.example.test/token");
        let h = harness(Setup {
            ca: Some(tls.ca_path.clone()),
            credentials: Some(vault.service.clone()),
            remote_servers: vec![SERVER.to_string()],
            workflow_id: Some(workflow_id),
            ..Setup::gateway(&url)
        })
        .await;
        vault
            .bind(
                &h.tenant,
                USER,
                SERVER,
                CredentialType::Secret,
                &[("token", MARKER)],
                &[grant.clone()],
            )
            .await;

        h.call(h.execution, &format!("{SERVER}.pages.read"))
            .await
            .unwrap_or_else(|e| panic!("{grant:?}: {e:?}"));
        let received = stub.received.lock().unwrap();
        assert_eq!(received.tools.len(), 1, "{grant:?}");
        let call = &received.tools[0];
        assert_eq!(
            call.acting.as_ref().unwrap().workflow_id,
            workflow_id.to_string()
        );
        assert_eq!(call.credential.as_ref().unwrap().value, MARKER);
    }
}

/// H1, H3: an agent the binding is not granted to, a user with no binding,
/// and a run with no person recorded are each refused
/// `CREDENTIAL_BINDING_REQUIRED` in ADR-035's shape, and nothing reaches the
/// gateway: no other credential path is tried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_grant_no_binding_or_no_person_is_refused_and_nothing_is_sent() {
    let tool = format!("{SERVER}.pages.read");
    for case in ["ungranted agent", "no binding", "no person"] {
        let stub = StubGateway::new(vec![], remote_result());
        let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
        let vault = Vault::new("https://token.example.test/token");
        let h = remote_harness(&url, &tls, &vault, vec!["*"]).await;
        match case {
            "ungranted agent" => {
                vault
                    .bind(
                        &h.tenant,
                        USER,
                        SERVER,
                        CredentialType::Secret,
                        &[("value", MARKER)],
                        &[GrantTarget::Agent {
                            agent_id: AgentId::new(),
                        }],
                    )
                    .await;
            }
            "no binding" => {
                // A binding of the user's to another server opens nothing here.
                vault
                    .bind(
                        &h.tenant,
                        USER,
                        "other-server",
                        CredentialType::Secret,
                        &[("value", MARKER)],
                        &[GrantTarget::AllAgents],
                    )
                    .await;
            }
            _ => {
                vault
                    .bind(
                        &h.tenant,
                        USER,
                        SERVER,
                        CredentialType::Secret,
                        &[("value", MARKER)],
                        &[GrantTarget::AllAgents],
                    )
                    .await;
            }
        }
        let execution = if case == "no person" {
            h.personless_execution
        } else {
            h.execution
        };

        let err = h.call(execution, &tool).await.expect_err(case);
        let refusal = err.refusal();
        assert_eq!(refusal.http_status, 403, "{case}");
        assert_eq!(refusal.code, "CREDENTIAL_BINDING_REQUIRED", "{case}");
        assert_eq!(refusal.status, "policy_violation", "{case}");
        let expected = if case == "no person" {
            format!(
                "This tool needs your own credential for '{SERVER}', and no person is recorded for this run."
            )
        } else {
            format!("This tool needs your own credential for '{SERVER}', granted to this agent.")
        };
        assert_eq!(refusal.message, expected, "{case}");
        assert!(
            matches!(shown(&err), SealSessionError::NotFound(_)),
            "{case}: the inner loop is shown {:?}",
            shown(&err)
        );
        let received = stub.received.lock().unwrap();
        assert!(
            received.tools.is_empty() && received.lists.is_empty() && received.workflows.is_empty(),
            "{case}: a request reached the gateway"
        );
    }
}

/// H1: an `OAuth2` binding whose access token is within 60 seconds of its
/// expiry is refreshed by the credential service's own `access_token_for`
/// before the call, and the call carries the fresh token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oauth_binding_near_expiry_is_refreshed_before_the_call() {
    let mut provider = mockito::Server::new_async().await;
    let refresh = provider
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
            mockito::Matcher::UrlEncoded("refresh_token".into(), "Mk9-refresh".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"access_token":"{MARKER}-fresh","token_type":"Bearer","expires_in":3600}}"#
        ))
        .expect(1)
        .create_async()
        .await;
    let stub = StubGateway::new(vec![], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new(&format!("{}/token", provider.url()));
    let h = remote_harness(&url, &tls, &vault, vec!["*"]).await;
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
    let id = vault
        .bind(
            &h.tenant,
            USER,
            SERVER,
            CredentialType::OAuth2,
            &[
                ("access_token", "Mk9-stale"),
                ("refresh_token", "Mk9-refresh"),
                ("expires_at", &expires_at),
            ],
            &[GrantTarget::AllAgents],
        )
        .await;

    h.call(h.execution, &format!("{SERVER}.pages.read"))
        .await
        .expect("the call is made");
    refresh.assert_async().await;
    let received = stub.received.lock().unwrap();
    assert_eq!(
        received.tools[0].credential.as_ref().unwrap().value,
        format!("{MARKER}-fresh")
    );
    drop(received);
    assert_eq!(
        vault.stored(id).await["access_token"].expose(),
        format!("{MARKER}-fresh")
    );
}

/// H8: where the configured gateway address is plaintext, a call that would
/// carry a credential is refused with the internal class
/// `CREDENTIAL_CHANNEL_NOT_CONFIDENTIAL`, and nothing is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plaintext_gateway_address_never_carries_a_credential() {
    let stub = StubGateway::new(vec![], remote_result());
    let (url, _shutdown) = serve_plaintext(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = harness(Setup {
        credentials: Some(vault.service.clone()),
        remote_servers: vec![SERVER.to_string()],
        ..Setup::gateway(&url)
    })
    .await;
    vault
        .bind(
            &h.tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", MARKER)],
            &[GrantTarget::AllAgents],
        )
        .await;

    let err = h
        .call(h.execution, &format!("{SERVER}.pages.read"))
        .await
        .expect_err("refused");
    let refusal = err.refusal();
    assert_eq!(
        (refusal.http_status, refusal.code, refusal.internal),
        (503, "CREDENTIAL_CHANNEL_NOT_CONFIDENTIAL", true)
    );
    assert_eq!(refusal.message, "This tool is not available right now.");
    let received = stub.received.lock().unwrap();
    assert!(received.tools.is_empty() && received.lists.is_empty());
}

/// Seal conformance deviation 3: the agent's own security context applies
/// to a remote tool before the orchestrator calls the gateway (the gateway
/// applies only its `internal` context): a remote tool the context does not
/// allow is refused, and nothing is sent or resolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agents_context_refuses_a_remote_tool_before_any_call() {
    let stub = StubGateway::new(vec![], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = remote_harness(&url, &tls, &vault, vec!["aegis.*"]).await;
    vault
        .bind(
            &h.tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", MARKER)],
            &[GrantTarget::AllAgents],
        )
        .await;

    let err = h
        .call(h.execution, &format!("{SERVER}.pages.read"))
        .await
        .expect_err("refused");
    assert_eq!(err.refusal().code, "TOOL_NOT_ALLOWED");
    let received = stub.received.lock().unwrap();
    assert!(received.tools.is_empty() && received.lists.is_empty());
}

/// ADR-126 with H7's `spec.tool_capabilities`: a remote tool an entry marks
/// `requires_approval` waits at the approval gate for its user's answer, and
/// the gateway is not called while it waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_tool_that_requires_approval_waits_for_its_user_before_any_call() {
    let stub = StubGateway::new(vec![], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers()).with_tool_capabilities(&[
        crate::domain::node_config::ToolCapabilityConfig {
            tool_pattern: format!("{SERVER}.*"),
            requires_approval: true,
            binding_argument: None,
            approval_summary: None,
        },
    ]);
    let approvals = Arc::new(
        crate::application::tool_approval_service::ToolApprovalService::new(
            Arc::new(
                crate::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository::new(),
            ),
            Arc::new(EventBus::new(64)),
        ),
    );
    let h = harness(Setup {
        ca: Some(tls.ca_path.clone()),
        credentials: Some(vault.service.clone()),
        remote_servers: vec![SERVER.to_string()],
        router,
        approvals: Some(approvals),
        ..Setup::gateway(&url)
    })
    .await;
    vault
        .bind(
            &h.tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", MARKER)],
            &[GrantTarget::AllAgents],
        )
        .await;

    let answer = h
        .call(h.execution, &format!("{SERVER}.pages.create"))
        .await
        .expect("the gate answers");
    assert!(
        answer.to_string().contains("approval"),
        "the gate's pending answer: {answer}"
    );
    let received = stub.received.lock().unwrap();
    assert!(received.tools.is_empty() && received.lists.is_empty());
}

/// H2, H8: the credential's value is in no log line the orchestrator writes
/// and in no error it answers, on a call that is made, a call the gateway
/// refuses, and a call refused for a plaintext channel.
#[test]
fn the_credential_is_in_no_log_line_and_no_error() {
    let (errors, logs) = crate::presentation::test_log_capture::capture_logs(async {
        let mut errors = Vec::new();
        for answer in [
            remote_result(),
            Answer::Refuse(
                tonic::Code::FailedPrecondition,
                Some("REMOTE_TOOL_ERROR"),
                "the server refused".to_string(),
            ),
        ] {
            let stub = StubGateway::new(vec![], answer);
            let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
            let vault = Vault::new("https://token.example.test/token");
            let h = remote_harness(&url, &tls, &vault, vec!["*"]).await;
            vault
                .bind(
                    &h.tenant,
                    USER,
                    SERVER,
                    CredentialType::Secret,
                    &[("value", MARKER)],
                    &[GrantTarget::AllAgents],
                )
                .await;
            if let Err(e) = h.call(h.execution, &format!("{SERVER}.pages.read")).await {
                errors.push(format!("{e} {e:?} {:?}", e.refusal()));
            }
            assert_eq!(
                stub.received.lock().unwrap().tools[0]
                    .credential
                    .as_ref()
                    .unwrap()
                    .value,
                MARKER,
                "the credential reached the gateway"
            );
        }
        let stub = StubGateway::new(vec![], remote_result());
        let (url, _shutdown) = serve_plaintext(stub).await;
        let vault = Vault::new("https://token.example.test/token");
        let h = harness(Setup {
            credentials: Some(vault.service.clone()),
            remote_servers: vec![SERVER.to_string()],
            ..Setup::gateway(&url)
        })
        .await;
        vault
            .bind(
                &h.tenant,
                USER,
                SERVER,
                CredentialType::Secret,
                &[("value", MARKER)],
                &[GrantTarget::AllAgents],
            )
            .await;
        let e = h
            .call(h.execution, &format!("{SERVER}.pages.read"))
            .await
            .expect_err("refused");
        errors.push(format!("{e} {e:?} {:?}", e.refusal()));
        errors
    });
    assert_eq!(errors.len(), 2);
    for error in &errors {
        assert!(
            !error.contains("Mk9-"),
            "an error carries the credential: {error}"
        );
    }
    assert!(!logs.is_empty(), "the capture saw the calls' log lines");
    assert!(!logs.contains("Mk9-"), "a log line carries the credential");
}
