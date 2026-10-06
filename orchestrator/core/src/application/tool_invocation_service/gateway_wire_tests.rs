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
    /// The `grounding_json` it writes beside an `InvokeTool` result (empty:
    /// none, as a gateway answers when the server's `initialize` carried no
    /// `_grounding`).
    grounding_json: String,
    received: Arc<StdMutex<Received>>,
}

impl StubGateway {
    fn new(tools: Vec<ToolSummary>, answer: Answer) -> Self {
        Self {
            tools,
            answer,
            grounding_json: String::new(),
            received: Arc::new(StdMutex::new(Received::default())),
        }
    }

    /// The same stub, writing `grounding_json` beside every `InvokeTool`
    /// result (AEGIS ADR-132 H9a).
    fn with_grounding(mut self, grounding_json: String) -> Self {
        self.grounding_json = grounding_json;
        self
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
        self.answer(|result_json| InvokeToolResponse {
            result_json,
            grounding_json: self.grounding_json.clone(),
        })
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
    agent_with_contexts(tools, &[])
}

/// An agent declaring `tools` and the contexts `(service, required)` (Zaru
/// ADR-0055 D16).
fn agent_with_contexts(tools: &[&str], contexts: &[(&str, bool)]) -> Agent {
    let mut manifest: AgentManifest = serde_yaml::from_str(&format!(
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
    manifest.spec.contexts = contexts
        .iter()
        .map(
            |(service, required)| crate::domain::agent::ContextDeclaration {
                service: service.to_string(),
                required: *required,
            },
        )
        .collect();
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
    /// The SEAL sessions `invoke_tool` (the invoke route) looks a call's
    /// token up in.
    sessions: Arc<InMemorySealSessionRepository>,
    /// The executions the service and an inner loop read.
    executions: Arc<Executions>,
    tenant: TenantId,
    agent_id: AgentId,
    /// An execution whose initiating user is `USER`.
    execution: ExecutionId,
    /// An execution of the same tenant with no person recorded.
    personless_execution: ExecutionId,
    /// An execution of the same tenant whose person is `user-2`.
    other_users_execution: ExecutionId,
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
    /// The contexts the agent declares, `(service, required)`.
    agent_contexts: Vec<(&'static str, bool)>,
    /// The executions' dispatch choices, kept in their input's reserved key
    /// `contexts` (Zaru ADR-0055 D14), if any.
    contexts: Option<Value>,
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
            agent_contexts: Vec::new(),
            contexts: None,
        }
    }
}

async fn harness(setup: Setup) -> Harness {
    let tenant = TenantId::for_consumer_user(USER).unwrap();
    let agent = agent_with_contexts(&setup.agent_tools, &setup.agent_contexts);
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
                input: match &setup.contexts {
                    Some(contexts) => json!({ "contexts": contexts }),
                    None => json!({}),
                },
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
    let executions = vec![
        execution_for(Some(USER)),
        execution_for(None),
        execution_for(Some("user-2")),
    ];
    let (execution, personless_execution, other_users_execution) =
        (executions[0].id, executions[1].id, executions[2].id);

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
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let executions = Arc::new(Executions(
        executions.into_iter().map(|e| (e.id, e)).collect(),
    ));
    let service = ToolInvocationService::new(
        sessions.clone(),
        security_context_repo,
        Arc::new(SealMiddleware::new()),
        Arc::new(setup.router),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(agent)),
        executions.clone(),
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
        sessions,
        executions,
        tenant,
        agent_id,
        execution,
        personless_execution,
        other_users_execution,
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
                reach: None,
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

// ---------------------------------------------------------------------------
// G5, H4: the tools a user's agent sees
// ---------------------------------------------------------------------------

/// The names in a tool list.
fn names(tools: &[crate::infrastructure::tool_router::ToolMetadata]) -> Vec<String> {
    let mut names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    names.sort();
    names
}

/// A run's agent sees the `<server>.<tool>` tools of a server its person
/// holds a granted binding to, listed by the gateway with that person's
/// credential; a run of another person, or of no person, does not, and
/// nothing of the first person's reaches the gateway for it; the agent's
/// security context still filters the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_users_agent_lists_the_tools_of_the_servers_its_person_granted_it() {
    let stub = StubGateway::new(vec![listed("ext.lookup", "workflow")], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let declared = vec!["ext.lookup", "notes-1.lookup"];
    let h = harness(Setup {
        agent_tools: declared.clone(),
        ca: Some(tls.ca_path.clone()),
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
            &[GrantTarget::Agent {
                agent_id: h.agent_id,
            }],
        )
        .await;

    let mine = h
        .service
        .get_available_tools_for_agent_run(&h.tenant, h.agent_id, h.execution, CONTEXT)
        .await
        .unwrap();
    assert_eq!(names(&mine), vec!["ext.lookup", "notes-1.lookup"]);
    {
        let received = stub.received.lock().unwrap();
        let listing = received.lists.last().unwrap();
        assert_eq!(listing.tenant_id, h.tenant.as_str());
        assert_eq!(listing.acting.as_ref().unwrap().user_id, USER);
        assert_eq!(listing.bound_servers.len(), 1);
        assert_eq!(listing.bound_servers[0].server, SERVER);
        assert_eq!(
            listing.bound_servers[0].credential.as_ref().unwrap().value,
            MARKER
        );
    }

    for (execution, who) in [
        (h.other_users_execution, "user-2"),
        (h.personless_execution, ""),
    ] {
        let theirs = h
            .service
            .get_available_tools_for_agent_run(&h.tenant, h.agent_id, execution, CONTEXT)
            .await
            .unwrap();
        assert_eq!(names(&theirs), vec!["ext.lookup"], "{who:?}");
        let received = stub.received.lock().unwrap();
        let listing = received.lists.last().unwrap();
        assert_eq!(listing.acting.as_ref().unwrap().user_id, who);
        assert!(listing.bound_servers.is_empty(), "{who:?}");
    }

    let narrow = harness(Setup {
        agent_tools: declared,
        context_patterns: vec!["ext.*"],
        ca: Some(tls.ca_path.clone()),
        credentials: Some(vault.service.clone()),
        remote_servers: vec![SERVER.to_string()],
        ..Setup::gateway(&url)
    })
    .await;
    vault
        .bind(
            &narrow.tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", MARKER)],
            &[GrantTarget::AllAgents],
        )
        .await;
    let filtered = narrow
        .service
        .get_available_tools_for_agent_run(
            &narrow.tenant,
            narrow.agent_id,
            narrow.execution,
            CONTEXT,
        )
        .await
        .unwrap();
    assert_eq!(names(&filtered), vec!["ext.lookup"]);
}

/// The node-wide list, the one the unauthenticated `GET /v1/seal/tools`
/// answers, names no user and carries no credential, so no person's remote
/// tool is ever in it; and over a plaintext channel an agent's run lists no
/// remote server's tools and sends no credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_persons_remote_tools_are_in_the_node_wide_list_or_listed_over_plaintext() {
    let stub = StubGateway::new(vec![listed("ext.lookup", "workflow")], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = harness(Setup {
        agent_tools: vec!["ext.lookup", "notes-1.lookup"],
        ca: Some(tls.ca_path.clone()),
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

    let node_wide = h.service.get_available_tools().await.unwrap();
    assert!(node_wide.iter().all(|t| t.name != "notes-1.lookup"));
    {
        let received = stub.received.lock().unwrap();
        let listing = received.lists.last().unwrap();
        assert!(listing.acting.is_none() && listing.bound_servers.is_empty());
    }

    let plain_stub = StubGateway::new(vec![listed("ext.lookup", "workflow")], remote_result());
    let (plain_url, _plain_shutdown) = serve_plaintext(plain_stub.clone()).await;
    let plain = harness(Setup {
        agent_tools: vec!["ext.lookup", "notes-1.lookup"],
        credentials: Some(vault.service.clone()),
        remote_servers: vec![SERVER.to_string()],
        ..Setup::gateway(&plain_url)
    })
    .await;
    vault
        .bind(
            &plain.tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", MARKER)],
            &[GrantTarget::AllAgents],
        )
        .await;
    let listed_plain = plain
        .service
        .get_available_tools_for_agent_run(&plain.tenant, plain.agent_id, plain.execution, CONTEXT)
        .await
        .unwrap();
    assert_eq!(names(&listed_plain), vec!["ext.lookup"]);
    let received = plain_stub.received.lock().unwrap();
    assert!(received.lists.last().unwrap().bound_servers.is_empty());
}

// ---------------------------------------------------------------------------
// The wiring: `seal_gateway` as the daemon configures it, through the route
// ---------------------------------------------------------------------------

/// A SEAL envelope as `POST /v1/seal/invoke` hands it to `invoke_tool`; the
/// signature is not what these tests are about.
struct RouteEnvelope {
    token: SensitiveString,
    tool: String,
    args: Value,
    nonce: String,
}

impl EnvelopeVerifier for RouteEnvelope {
    fn security_token(&self) -> &SensitiveString {
        &self.token
    }
    fn verify_signature(&self, _: &[u8]) -> Result<(), SealSessionError> {
        Ok(())
    }
    fn extract_tool_name(&self) -> Option<String> {
        Some(self.tool.clone())
    }
    fn extract_arguments(&self) -> Option<Value> {
        Some(self.args.clone())
    }
    fn replay_nonce(&self) -> String {
        self.nonce.clone()
    }
}

/// Counts every credential lookup, answering none.
#[derive(Default)]
struct CountedCredentials(std::sync::atomic::AtomicUsize);

#[async_trait]
impl crate::application::credential_service::ToolCredentialSource for CountedCredentials {
    async fn tool_server_credential(
        &self,
        _: &crate::application::credential_service::ToolCallActor<'_>,
        _: &str,
    ) -> anyhow::Result<Option<SensitiveString>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(None)
    }
}

/// `seal_gateway` as a node configures it.
fn gateway_config(
    url: &str,
    ca: Option<&std::path::Path>,
    servers: &[&str],
) -> crate::domain::node_config::SealGatewayConfig {
    crate::domain::node_config::SealGatewayConfig {
        url: SensitiveUrl::new(url),
        ca_cert_path: ca.map(std::path::Path::to_path_buf),
        remote_servers: servers.iter().map(|s| s.to_string()).collect(),
    }
}

impl Harness {
    /// The gateway part of the service wired from `config`, the one way the
    /// daemon wires it.
    fn configured(
        mut self,
        config: Option<&crate::domain::node_config::SealGatewayConfig>,
        credentials: Option<Arc<dyn crate::application::credential_service::ToolCredentialSource>>,
    ) -> Self {
        self.service = self
            .service
            .with_seal_gateway_config(config, credentials)
            .expect("the configuration wires");
        self
    }

    /// A session as `/v1/seal/attest` binds it for `USER` through the Zaru
    /// MCP server: the user's tenant and subject, a random agent id (a
    /// chat session has no agent of its own), the test's context.
    async fn session(&self) -> String {
        let token = format!("token-{}", uuid::Uuid::new_v4());
        let session = crate::domain::seal_session::SealSession::new(
            AgentId::new(),
            ExecutionId::new(),
            vec![],
            token.clone(),
            security_context(&["*"]),
            self.tenant.clone(),
        )
        .with_principal_metadata(
            Some(USER.to_string()),
            Some(USER.to_string()),
            None,
            None,
        );
        self.sessions.save(session).await.unwrap();
        token
    }

    /// One `tools/call` through `invoke_tool`, what the invoke route calls.
    async fn route(&self, token: &str, tool: &str) -> Result<Value, SealSessionError> {
        self.service
            .invoke_tool(&RouteEnvelope {
                token: token.to_string().into(),
                tool: tool.to_string(),
                args: json!({"query": "q"}),
                nonce: uuid::Uuid::new_v4().to_string(),
            })
            .await
    }
}

/// The wiring, configured: `seal_gateway` with an `https` gateway, its CA
/// and `remote_servers: [SERVER]` reaches the gateway through the invoke
/// route with the user's credential over TLS, and answers the server's
/// result unchanged. Nothing but the configuration wires the CA, the
/// servers and the credential source here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_configured_gateway_takes_a_granted_users_call_through_the_route() {
    let stub = StubGateway::new(vec![], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = harness(Setup::gateway(&url)).await.configured(
        Some(&gateway_config(&url, Some(&tls.ca_path), &[SERVER])),
        Some(vault.service.clone()),
    );
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

    let token = h.session().await;
    let value = h
        .route(&token, &format!("{SERVER}.pages.read"))
        .await
        .expect("the call is made");
    assert_eq!(
        value,
        json!({"content":[{"type":"text","text":"page"}],"isError":false})
    );
    let received = stub.received.lock().unwrap();
    assert_eq!(received.tools.len(), 1, "one InvokeTool");
    let call = &received.tools[0];
    assert_eq!(
        (call.server.as_str(), call.tool.as_str()),
        (SERVER, "pages.read")
    );
    assert_eq!(call.acting.as_ref().unwrap().user_id, USER);
    assert_eq!(call.credential.as_ref().unwrap().value, MARKER);
}

/// The wiring refuses before any dial: a user with no binding to the server
/// granted to the session's agent is answered 403
/// `CREDENTIAL_BINDING_REQUIRED`, and the gateway receives nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_configured_gateway_refuses_an_ungranted_call_before_any_dial() {
    let stub = StubGateway::new(vec![], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = harness(Setup::gateway(&url)).await.configured(
        Some(&gateway_config(&url, Some(&tls.ca_path), &[SERVER])),
        Some(vault.service.clone()),
    );
    // Granted to one named agent, not to the session's.
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

    let token = h.session().await;
    let error = h
        .route(&token, &format!("{SERVER}.pages.read"))
        .await
        .expect_err("refused");
    let refusal = error.refusal();
    assert_eq!(
        (refusal.http_status, refusal.code),
        (403, "CREDENTIAL_BINDING_REQUIRED"),
        "{error:?}"
    );
    assert_eq!(
        refusal.message,
        format!("This tool needs your own credential for '{SERVER}', granted to this agent.")
    );
    let received = stub.received.lock().unwrap();
    assert!(
        received.tools.is_empty() && received.lists.is_empty(),
        "nothing reached the gateway"
    );
}

/// The two components name a server once each, and the names must match:
/// the orchestrator's `seal_gateway.remote_servers` and the gateway's
/// `spec.mcp_servers[].name`. A server the orchestrator names and the
/// gateway does not register is answered by the gateway's own refusal
/// (`aegis-seal-gateway` `cc0db0f` `remote_mcp/mod.rs` 173-180: NOT_FOUND,
/// "Not found: server '<name>'."), relayed as 404 with that reason. A server
/// the gateway registers and the orchestrator does not name is never sent a
/// credential: its tool is looked up in the gateway's tenant list, which
/// lists a remote server only for a user who holds a binding, and answered
/// 404 "Not found: tool '<name>'.", nothing invoked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatched_server_names_are_refused_with_the_reason() {
    let stub = StubGateway::new(
        vec![],
        Answer::Refuse(
            tonic::Code::NotFound,
            Some("NOT_FOUND"),
            format!("Not found: server '{SERVER}'."),
        ),
    );
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = harness(Setup::gateway(&url)).await.configured(
        Some(&gateway_config(&url, Some(&tls.ca_path), &[SERVER])),
        Some(vault.service.clone()),
    );
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
    let token = h.session().await;

    // Named here, not registered there.
    let error = h
        .route(&token, &format!("{SERVER}.pages.read"))
        .await
        .expect_err("refused");
    let refusal = error.refusal();
    assert_eq!((refusal.http_status, refusal.code), (404, "NOT_FOUND"));
    assert_eq!(refusal.message, format!("Not found: server '{SERVER}'."));

    // Registered there, not named here.
    let error = h
        .route(&token, "unnamed-server.pages.read")
        .await
        .expect_err("refused");
    let refusal = error.refusal();
    assert_eq!((refusal.http_status, refusal.code), (404, "NOT_FOUND"));
    assert_eq!(
        refusal.message,
        "Not found: tool 'unnamed-server.pages.read'."
    );
    let received = stub.received.lock().unwrap();
    assert_eq!(received.tools.len(), 1, "only the named server was called");
    assert_eq!(received.lists.len(), 1, "the unnamed one was looked up");
    assert!(
        received.lists[0].bound_servers.is_empty(),
        "no credential rode the lookup"
    );
}

/// With no `seal_gateway` (production today) the wiring changes nothing:
/// a tool no builtin serves is answered 404 as before, no credential is
/// looked up, nothing dials.
#[tokio::test]
async fn without_a_gateway_the_wiring_changes_nothing() {
    let credentials = Arc::new(CountedCredentials::default());
    let mut setup = Setup::gateway("unused");
    setup.gateway_url = None;
    let h = harness(setup)
        .await
        .configured(None, Some(credentials.clone()));
    let token = h.session().await;
    let error = h
        .route(&token, &format!("{SERVER}.pages.read"))
        .await
        .expect_err("not found");
    let refusal = error.refusal();
    assert_eq!((refusal.http_status, refusal.code), (404, "NOT_FOUND"));
    assert_eq!(
        refusal.message,
        format!("Not found: tool '{SERVER}.pages.read'.")
    );
    assert_eq!(
        credentials.0.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no credential was looked up"
    );
}

/// What the daemon does not start with, each with its reason: a CA file it
/// cannot read, a server name the gateway could not register, and remote
/// servers on a node with no credential store.
#[tokio::test]
async fn a_gateway_configuration_that_cannot_work_is_refused_with_the_reason() {
    let url = "https://aegis-seal-gateway:50055";
    let credentials: Arc<dyn crate::application::credential_service::ToolCredentialSource> =
        Arc::new(CountedCredentials::default());
    for (config, credentials, reason) in [
        (
            gateway_config(
                url,
                Some(std::path::Path::new("/nonexistent/aegis-wiring-ca.crt")),
                &[SERVER],
            ),
            Some(credentials.clone()),
            "seal_gateway.ca_cert_path",
        ),
        (
            gateway_config(url, None, &["nuclear.notes"]),
            Some(credentials.clone()),
            "seal_gateway.remote_servers",
        ),
        (
            gateway_config(url, None, &[SERVER]),
            None,
            "no credential store",
        ),
    ] {
        let error = harness(Setup::gateway(url))
            .await
            .service
            .with_seal_gateway_config(Some(&config), credentials)
            .err()
            .expect("refused")
            .to_string();
        assert!(error.contains(reason), "{reason}: {error}");
    }
}

// ---------------------------------------------------------------------------
// The inner loop: an agent's run sees and calls its person's remote tools
// ---------------------------------------------------------------------------

/// A model that records the tools each turn offers it, calls `call` once,
/// then answers.
struct ScriptedModel {
    call: String,
    offered: StdMutex<Vec<Vec<crate::domain::llm::ToolSchema>>>,
}

#[async_trait]
impl crate::domain::llm::LLMProvider for ScriptedModel {
    async fn generate(
        &self,
        _: &str,
        _: &crate::domain::llm::GenerationOptions,
    ) -> Result<crate::domain::llm::GenerationResponse, crate::domain::llm::LLMError> {
        unimplemented!("not used by the inner loop")
    }
    async fn generate_chat(
        &self,
        messages: &[crate::domain::llm::ChatMessage],
        tools: &[crate::domain::llm::ToolSchema],
        _: &crate::domain::llm::GenerationOptions,
    ) -> Result<crate::domain::llm::ChatResponse, crate::domain::llm::LLMError> {
        self.offered.lock().unwrap().push(tools.to_vec());
        if messages.iter().any(|m| m.role == "tool") {
            return Ok(crate::domain::llm::ChatResponse::FinalText(
                crate::domain::llm::GenerationResponse {
                    text: "done".to_string(),
                    usage: Default::default(),
                    provider: "scripted".to_string(),
                    model: "scripted".to_string(),
                    finish_reason: crate::domain::llm::FinishReason::Stop,
                },
            ));
        }
        Ok(crate::domain::llm::ChatResponse::ToolCalls(vec![
            crate::domain::llm::ChatToolCall {
                id: "call-1".to_string(),
                name: self.call.clone(),
                arguments: json!({"query": "q"}),
            },
        ]))
    }
    async fn health_check(&self) -> Result<(), crate::domain::llm::LLMError> {
        Ok(())
    }
}

/// One run of `h`'s agent in `h.execution` through the inner loop, its
/// model calling `call`: the tools each turn offered, and the conversation.
async fn run_inner_loop(
    h: Harness,
    call: &str,
) -> (
    Vec<Vec<crate::domain::llm::ToolSchema>>,
    Vec<crate::domain::dispatch::ConversationMessage>,
) {
    let model = Arc::new(ScriptedModel {
        call: call.to_string(),
        offered: StdMutex::new(Vec::new()),
    });
    let registry = crate::infrastructure::llm::registry::ProviderRegistry::new_for_test(
        model.clone(),
        None,
        1,
        0,
        60,
    );
    let inner_loop = crate::application::inner_loop_service::InnerLoopService::new(
        Arc::new(h.service),
        h.executions.clone(),
        Arc::new(registry),
    );
    let answer = inner_loop
        .handle_agent_message(crate::domain::dispatch::AgentMessage::Generate {
            agent_id: h.agent_id.to_string(),
            execution_id: h.execution.to_string(),
            iteration_number: 1,
            prompt: "look it up".to_string(),
            messages: Vec::new(),
            model_alias: "default".to_string(),
        })
        .await
        .expect("the run ends");
    let crate::domain::dispatch::OrchestratorMessage::Final { conversation, .. } = answer else {
        panic!("the run ends with its answer: {answer:?}");
    };
    let offered = model.offered.lock().unwrap().clone();
    (offered, conversation)
}

fn schema_names(tools: &[crate::domain::llm::ToolSchema]) -> Vec<String> {
    let mut names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    names.sort();
    names
}

/// AEGIS ADR-132 G5, H4 end to end: an agent's run is offered the
/// `<server>.<tool>` tools of the servers its person bound and granted it,
/// calls one, and the call reaches the gateway with the person's credential;
/// the result is the tool message the model reads next. Every tool it was
/// offered before (a builtin, a gateway workflow) is offered unchanged
/// (ADR-035 R7).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agents_run_lists_and_calls_its_persons_granted_remote_tool() {
    let stub = StubGateway::new(vec![listed("ext.lookup", "workflow")], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = harness(Setup {
        agent_tools: vec!["aegis.schema.get", "ext.lookup", "notes-1.lookup"],
        ..Setup::gateway(&url)
    })
    .await
    .configured(
        Some(&gateway_config(&url, Some(&tls.ca_path), &[SERVER])),
        Some(vault.service.clone()),
    );
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
    let before = h
        .service
        .get_available_tools_for_agent_in_context(&h.tenant, h.agent_id, CONTEXT)
        .await
        .unwrap();
    assert_eq!(names(&before), vec!["aegis.schema.get", "ext.lookup"]);

    let (offered, conversation) = run_inner_loop(h, "notes-1.lookup").await;
    assert_eq!(
        schema_names(&offered[0]),
        vec!["aegis.schema.get", "ext.lookup", "notes-1.lookup"]
    );
    for tool in &before {
        let shown = offered[0]
            .iter()
            .find(|t| t.name == tool.name)
            .expect("offered");
        assert_eq!(
            (&shown.description, &shown.parameters),
            (&tool.description, &tool.input_schema),
            "{} is offered as before",
            tool.name
        );
    }
    {
        let received = stub.received.lock().unwrap();
        assert_eq!(received.tools.len(), 1, "one InvokeTool");
        let call = &received.tools[0];
        assert_eq!(
            (call.server.as_str(), call.tool.as_str()),
            (SERVER, "lookup")
        );
        assert_eq!(call.acting.as_ref().unwrap().user_id, USER);
        assert_eq!(call.credential.as_ref().unwrap().value, MARKER);
    }
    let result = conversation
        .iter()
        .find(|m| m.role == "tool")
        .expect("the tool's result is in the conversation");
    assert_eq!(
        serde_json::from_str::<Value>(&result.content).unwrap(),
        json!({"content":[{"type":"text","text":"page"}],"isError":false})
    );
}

/// An agent its person did not grant the binding to is not offered the
/// server's tools, and a call of one anyway is refused before any dial:
/// the gateway receives no InvokeTool, and the model reads that the tool
/// is not available.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ungranted_agents_run_is_not_offered_the_remote_tool_and_its_call_is_refused_before_any_dial(
) {
    let stub = StubGateway::new(vec![], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = harness(Setup {
        agent_tools: vec!["aegis.schema.get", "notes-1.lookup"],
        ..Setup::gateway(&url)
    })
    .await
    .configured(
        Some(&gateway_config(&url, Some(&tls.ca_path), &[SERVER])),
        Some(vault.service.clone()),
    );
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

    let (offered, conversation) = run_inner_loop(h, "notes-1.lookup").await;
    assert_eq!(schema_names(&offered[0]), vec!["aegis.schema.get"]);
    let received = stub.received.lock().unwrap();
    assert!(received.tools.is_empty(), "nothing was invoked");
    assert!(
        received.lists.iter().all(|l| l.bound_servers.is_empty()),
        "no credential rode a listing"
    );
    let result = conversation
        .iter()
        .find(|m| m.role == "tool")
        .expect("the refusal is in the conversation");
    assert!(
        result
            .content
            .contains("Tool 'notes-1.lookup' is not available. Do not retry this tool."),
        "{}",
        result.content
    );
}

/// With no `seal_gateway` an agent's run is offered exactly what its
/// context list offered before the inner loop asked for the run's own list
/// (ADR-035 R7), each tool with its description and schema.
#[tokio::test]
async fn without_a_gateway_an_agents_run_is_offered_what_it_was_offered_before() {
    let mut setup = Setup::gateway("unused");
    setup.gateway_url = None;
    setup.agent_tools = vec!["aegis.schema.get", "fs.read", "notes-1.lookup"];
    let h = harness(setup).await.configured(None, None);
    let before = h
        .service
        .get_available_tools_for_agent_in_context(&h.tenant, h.agent_id, CONTEXT)
        .await
        .unwrap();
    assert_eq!(names(&before), vec!["aegis.schema.get", "fs.read"]);
    let (offered, _) = run_inner_loop(h, "aegis.schema.get").await;
    let as_offered: Vec<(String, String, Value)> = before
        .iter()
        .map(|t| {
            (
                t.name.clone(),
                t.description.clone(),
                t.input_schema.clone(),
            )
        })
        .collect();
    let mut shown: Vec<(String, String, Value)> = offered[0]
        .iter()
        .map(|t| (t.name.clone(), t.description.clone(), t.parameters.clone()))
        .collect();
    shown.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = as_offered;
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(shown, expected);
}

// ---------------------------------------------------------------------------
// A remote server's token grounded through the gateway (ADR-132 (7a) S2, B1)
// ---------------------------------------------------------------------------

/// The `tools/call` result of `cortex.ground` whose token reaches one
/// instance, as Nuclear Notes answers it: the payload as the text of the
/// first content item.
fn grounding_result(instances: &[(&str, &str)]) -> Answer {
    let payload = json!({
        "instance": {"id": "root-id", "slug": "main"},
        "you": {
            "currentWorkspace": null,
            "instances": instances
                .iter()
                .map(|(id, slug)| json!({"id": id, "slug": slug, "name": slug, "role": "member"}))
                .collect::<Vec<_>>(),
        },
    });
    Answer::Result(json!({"content": [{"type": "text", "text": payload.to_string()}]}).to_string())
}

/// The credential service of `vault`, handed `service` as its grounding of
/// the remote server `SERVER`, as the daemon hands it.
fn grounded_by(vault: &Vault, service: &Arc<ToolInvocationService>) {
    let weak = Arc::downgrade(service);
    let weak: std::sync::Weak<dyn crate::application::credential_service::RemoteServerGrounding> =
        weak;
    assert!(vault
        .service
        .set_remote_grounding(weak, vec![SERVER.to_string()]));
}

fn store_command(tenant: &TenantId) -> crate::application::credential_service::StoreApiKeyCommand {
    crate::application::credential_service::StoreApiKeyCommand {
        owner_user_id: USER.to_string(),
        tenant_id: tenant.clone(),
        provider: CredentialProvider::new(SERVER),
        label: "Work instance".to_string(),
        scope: CredentialScope::Personal,
        api_key_value: SensitiveString::new(MARKER),
        credential_type: CredentialType::Secret,
    }
}

/// B1: a token being stored is grounded by the gateway's `InvokeTool` of
/// `cortex.ground`, arguments `{}`, the token as the credential and its
/// owner as the acting identity, over TLS; the payload in the result's text
/// gives the binding its reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_token_is_grounded_through_invoke_tool_and_its_reach_recorded() {
    use crate::application::credential_service::CredentialManagementService;
    let stub = StubGateway::new(vec![], grounding_result(&[("inst-play2-id", "play2")]));
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = remote_harness(&url, &tls, &vault, vec!["*"]).await;
    let tenant = h.tenant.clone();
    let service = Arc::new(h.service);
    grounded_by(&vault, &service);

    let id = vault
        .service
        .store_api_key(store_command(&tenant))
        .await
        .expect("stored");

    let binding = vault.bindings.find_by_id(&id).await.unwrap().unwrap();
    let reach = binding.metadata.reach.expect("reach recorded");
    println!("reach {}", serde_json::to_value(&reach).unwrap());
    assert_eq!(reach.kind, crate::domain::credential::ReachKind::Instance);
    assert_eq!(reach.instance_slug.as_deref(), Some("play2"));
    assert_eq!(reach.instance_id.as_deref(), Some("inst-play2-id"));
    let received = stub.received.lock().unwrap();
    assert_eq!(received.tools.len(), 1);
    let call = &received.tools[0];
    assert_eq!(
        (call.server.as_str(), call.tool.as_str()),
        (SERVER, "cortex.ground")
    );
    assert_eq!(call.arguments_json, "{}");
    assert_eq!(call.tenant_id, tenant.as_str());
    let acting = call.acting.as_ref().expect("acting identity");
    assert_eq!(
        (
            acting.user_id.as_str(),
            acting.agent_id.as_str(),
            acting.workflow_id.as_str()
        ),
        (USER, "", "")
    );
    let credential = call.credential.as_ref().expect("credential");
    assert_eq!(credential.kind, CredentialKind::BearerToken as i32);
    assert_eq!(credential.value, MARKER);
}

/// B5: the gateway's refusal of the grounding (a 401 from the server is
/// `CREDENTIAL_REJECTED` on this path) is the store's refusal with that code
/// and sentence; nothing is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_refusal_of_the_grounding_stores_nothing() {
    use crate::application::credential_service::{CredentialError, CredentialManagementService};
    let stub = StubGateway::new(
        vec![],
        Answer::Refuse(
            tonic::Code::PermissionDenied,
            Some("CREDENTIAL_REJECTED"),
            format!("The '{SERVER}' server refused your stored credential."),
        ),
    );
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = remote_harness(&url, &tls, &vault, vec!["*"]).await;
    let tenant = h.tenant.clone();
    let service = Arc::new(h.service);
    grounded_by(&vault, &service);

    let err = vault
        .service
        .store_api_key(store_command(&tenant))
        .await
        .expect_err("refused");
    println!("refusal {err}");
    assert_eq!(
        err.to_string(),
        format!(
            "The token was not stored: grounding it at the remote server '{SERVER}' was refused \
             (CREDENTIAL_REJECTED): The '{SERVER}' server refused your stored credential."
        )
    );
    assert!(matches!(
        err.downcast_ref::<CredentialError>(),
        Some(CredentialError::ReachRefused { .. })
    ));
    assert!(
        vault.bindings.0.read().await.is_empty(),
        "a binding was stored"
    );
    assert_eq!(stub.received.lock().unwrap().tools.len(), 1);
}

/// H8: over a plaintext gateway address the token is never sent to be
/// grounded; the grounding answers the channel's code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plaintext_gateway_address_never_carries_a_token_to_be_grounded() {
    let stub = StubGateway::new(vec![], grounding_result(&[("inst-play2-id", "play2")]));
    let (url, _shutdown) = serve_plaintext(stub.clone()).await;
    let h = harness(Setup {
        remote_servers: vec![SERVER.to_string()],
        ..Setup::gateway(&url)
    })
    .await;

    let refusal = h
        .service
        .ground_remote_token(&h.tenant, USER, SERVER, &SensitiveString::new(MARKER))
        .await
        .expect_err("refused");
    assert!(matches!(
        refusal,
        crate::application::credential_service::GroundingRefusal::Unreachable { ref code, .. }
            if code == "CREDENTIAL_CHANNEL_NOT_CONFIDENTIAL"
    ));
    assert!(stub.received.lock().unwrap().tools.is_empty());
}

// ---------------------------------------------------------------------------
// The grounding a gateway answers beside the result (ADR-132 H9a)
// ---------------------------------------------------------------------------

/// The `_grounding` object a server answers on `initialize`, as the gateway
/// writes it into `grounding_json`, for a token reaching `instances`.
fn grounding_json(instances: &[(&str, &str)]) -> String {
    json!({
        "instance": {"id": "root-id", "slug": "main"},
        "you": {
            "currentWorkspace": null,
            "instances": instances
                .iter()
                .map(|(id, slug)| json!({"id": id, "slug": slug, "name": slug, "role": "member"}))
                .collect::<Vec<_>>(),
        },
    })
    .to_string()
}

/// Store a token through a TLS gateway served by `stub`: the store's answer
/// and the reach the binding recorded, if one was stored.
async fn store_through(
    stub: &StubGateway,
) -> (
    anyhow::Result<CredentialBindingId>,
    Option<crate::domain::credential::BindingReach>,
) {
    use crate::application::credential_service::CredentialManagementService;
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let h = remote_harness(&url, &tls, &vault, vec!["*"]).await;
    let tenant = h.tenant.clone();
    let service = Arc::new(h.service);
    grounded_by(&vault, &service);
    let stored = vault.service.store_api_key(store_command(&tenant)).await;
    let reach = match &stored {
        Ok(id) => vault
            .bindings
            .find_by_id(id)
            .await
            .unwrap()
            .and_then(|binding| binding.metadata.reach),
        Err(_) => None,
    };
    (stored, reach)
}

/// H9a: a gateway that answers `grounding_json` beside the `cortex.ground`
/// result grounds the binding from it, and the result is not read: neither
/// a result naming another instance nor one that is not JSON changes the
/// reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grounding_answered_beside_the_result_grounds_the_binding_without_the_result() {
    let cases = [
        (
            "a result naming another instance",
            grounding_result(&[("inst-other-id", "other")]),
        ),
        (
            "a result that is not JSON",
            Answer::Result("not json".to_string()),
        ),
    ];
    let mut failures = Vec::new();
    for (case, answer) in cases {
        let stub = StubGateway::new(vec![], answer)
            .with_grounding(grounding_json(&[("inst-play2-id", "play2")]));
        let (stored, reach) = store_through(&stub).await;
        if let Err(e) = &stored {
            failures.push(format!("{case}: the store refused: {e}"));
            continue;
        }
        let reach = reach.expect("reach recorded");
        println!("{case}: reach {}", serde_json::to_value(&reach).unwrap());
        let left = (reach.instance_slug.as_deref(), reach.instance_id.as_deref());
        let right = (Some("play2"), Some("inst-play2-id"));
        if left != right {
            failures.push(format!(
                "{case}: the reach was not read from grounding_json: left {left:?} right {right:?}"
            ));
        }
        if stub.received.lock().unwrap().tools.len() != 1 {
            failures.push(format!("{case}: the grounding was not one InvokeTool"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// H9a: a gateway that answers `grounding_json` empty (an older gateway, or
/// a server whose `initialize` carried no `_grounding`) leaves the grounding
/// to the result's text, as before the field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_grounding_falls_back_to_the_results_text() {
    let stub = StubGateway::new(vec![], grounding_result(&[("inst-play2-id", "play2")]));
    let (stored, reach) = store_through(&stub).await;
    if let Err(e) = &stored {
        panic!("the store refused a grounding in the result's text: {e}");
    }
    let reach = reach.expect("reach recorded");
    println!("fallback: reach {}", serde_json::to_value(&reach).unwrap());
    assert_eq!(reach.kind, crate::domain::credential::ReachKind::Instance);
    assert_eq!(
        (reach.instance_slug.as_deref(), reach.instance_id.as_deref()),
        (Some("play2"), Some("inst-play2-id")),
        "the reach was not read from the result's text"
    );
}

// ---------------------------------------------------------------------------
// Zaru ADR-0055 D15, D16: the dispatch chooses the binding, and a declared
// context it fills brings the server's tools
// ---------------------------------------------------------------------------

const CHOSEN: &str = "Mk12-chosen-binding-token";

/// The dispatch's `contexts`: `choice` for `SERVER`.
fn choosing(choice: Value) -> Value {
    let mut contexts = serde_json::Map::new();
    contexts.insert(SERVER.to_string(), choice);
    Value::Object(contexts)
}
const NEWEST_GRANTED: &str = "Mk12-newest-granted-token";

/// D15: a call carries the binding the execution's dispatch chose, though
/// it is older and granted to nothing, where the newest binding is granted
/// to all the person's agents.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_carries_the_binding_the_dispatch_chose() {
    let stub = StubGateway::new(vec![], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let tenant = TenantId::for_consumer_user(USER).unwrap();
    let chosen = vault
        .bind(
            &tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", CHOSEN)],
            &[],
        )
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    vault
        .bind(
            &tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", NEWEST_GRANTED)],
            &[GrantTarget::AllAgents],
        )
        .await;
    let h = harness(Setup {
        ca: Some(tls.ca_path.clone()),
        credentials: Some(vault.service.clone()),
        remote_servers: vec![SERVER.to_string()],
        contexts: Some(choosing(json!(chosen.0.to_string()))),
        ..Setup::gateway(&url)
    })
    .await;

    h.call(h.execution, &format!("{SERVER}.pages.read"))
        .await
        .expect("the call is made");
    let received = stub.received.lock().unwrap();
    assert_eq!(received.tools.len(), 1);
    assert_eq!(
        received.tools[0].credential.as_ref().unwrap().value,
        CHOSEN,
        "the call did not carry the chosen binding's secret"
    );
}

/// D2, D15: a choice of none refuses the call `CREDENTIAL_BINDING_REQUIRED`
/// under an all-agents grant, and a chosen binding that is not the person's
/// own active one for the server is refused the same; each with its
/// sentence, and nothing reaches the gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_choice_of_none_or_of_a_binding_not_the_persons_is_refused_and_nothing_is_sent() {
    let tool = format!("{SERVER}.pages.read");
    let vault = Vault::new("https://token.example.test/token");
    let tenant = TenantId::for_consumer_user(USER).unwrap();
    vault
        .bind(
            &tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", NEWEST_GRANTED)],
            &[GrantTarget::AllAgents],
        )
        .await;
    let others = vault
        .bind(
            &TenantId::for_consumer_user("user-2").unwrap(),
            "user-2",
            SERVER,
            CredentialType::Secret,
            &[("value", CHOSEN)],
            &[GrantTarget::AllAgents],
        )
        .await;
    let mut complaints = Vec::new();
    for (case, choice, expected) in [
        (
            "none",
            Value::Null,
            format!("This tool needs your own credential for '{SERVER}', and none was chosen for this run."),
        ),
        (
            "another person's binding",
            json!(others.0.to_string()),
            format!(
                "This tool needs your own credential for '{SERVER}', and the one chosen for this run is not an active credential of yours for it."
            ),
        ),
    ] {
        let stub = StubGateway::new(vec![], remote_result());
        let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
        let h = harness(Setup {
            ca: Some(tls.ca_path.clone()),
            credentials: Some(vault.service.clone()),
            remote_servers: vec![SERVER.to_string()],
            contexts: Some(choosing(choice)),
            ..Setup::gateway(&url)
        })
        .await;
        match h.call(h.execution, &tool).await {
            Ok(value) => complaints.push(format!("{case}: the call was made: {value}")),
            Err(err) => {
                let refusal = err.refusal();
                if refusal.code != "CREDENTIAL_BINDING_REQUIRED" {
                    complaints.push(format!("{case}: refused {}", refusal.code));
                }
                if refusal.message != expected {
                    complaints.push(format!("{case}: the sentence was \"{}\"", refusal.message));
                }
            }
        }
        if !stub.received.lock().unwrap().tools.is_empty() {
            complaints.push(format!("{case}: a call reached the gateway"));
        }
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// D16: an agent declaring the server as a context, filled with a binding,
/// lists the server's tools without `tools` naming them, the gateway listing
/// them with the chosen binding; an agent that does not declare it lists
/// none of them; the security context still filters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_filled_context_lists_its_servers_tools_and_an_undeclared_one_none() {
    let stub = StubGateway::new(vec![listed("ext.lookup", "workflow")], remote_result());
    let (url, tls, _shutdown) = serve_tls(stub.clone()).await;
    let vault = Vault::new("https://token.example.test/token");
    let tenant = TenantId::for_consumer_user(USER).unwrap();
    let chosen = vault
        .bind(
            &tenant,
            USER,
            SERVER,
            CredentialType::Secret,
            &[("value", CHOSEN)],
            &[],
        )
        .await;
    let contexts = Some(choosing(json!(chosen.0.to_string())));

    let mut complaints = Vec::new();
    for (case, tools, declared, patterns, expected) in [
        (
            "declared",
            vec![],
            vec![(SERVER, false)],
            vec!["*"],
            vec!["notes-1.lookup"],
        ),
        (
            "undeclared",
            vec!["ext.lookup"],
            vec![],
            vec!["*"],
            vec!["ext.lookup"],
        ),
        (
            "declared, filtered out",
            vec!["ext.lookup"],
            vec![(SERVER, true)],
            vec!["ext.*"],
            vec!["ext.lookup"],
        ),
    ] {
        let h = harness(Setup {
            agent_tools: tools,
            agent_contexts: declared,
            context_patterns: patterns,
            ca: Some(tls.ca_path.clone()),
            credentials: Some(vault.service.clone()),
            remote_servers: vec![SERVER.to_string()],
            contexts: contexts.clone(),
            ..Setup::gateway(&url)
        })
        .await;
        let listed = h
            .service
            .get_available_tools_for_agent_run(&h.tenant, h.agent_id, h.execution, CONTEXT)
            .await
            .unwrap();
        if names(&listed) != expected {
            complaints.push(format!(
                "{case}: listed {:?}, expected {expected:?}",
                names(&listed)
            ));
        }
        if case == "declared" {
            let received = stub.received.lock().unwrap();
            let listing = received.lists.last();
            let carried = listing
                .and_then(|l| l.bound_servers.first())
                .and_then(|b| b.credential.as_ref())
                .map(|c| c.value.clone());
            if carried.as_deref() != Some(CHOSEN) {
                complaints.push(format!(
                    "{case}: the listing carried {carried:?}, not the chosen binding's secret"
                ));
            }
        }
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
