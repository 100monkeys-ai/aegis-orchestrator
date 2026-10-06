//! An agent that declares a remote server's tool starts (AEGIS ADR-132
//! Update (6): "An agent sees a remote tool only when it declares it by its
//! exact name in `spec.tools`, its context admits it, and its person holds a
//! granted binding"). The start check admits a declared `<server>.<tool>`
//! whose server is one of the node's `seal_gateway.remote_servers`; whether
//! its call is made stays where it is decided at call time: the security
//! context, then the person's granted binding, else
//! `CREDENTIAL_BINDING_REQUIRED` (H3), before anything reaches the gateway.
//!
//! Each test drives the types the daemon constructs: a
//! `StandardExecutionService` given the node's configuration and the
//! `ToolRouter`, and a `ToolInvocationService` wired from the same
//! `seal_gateway` by `with_seal_gateway_config`, calling a loopback gateway
//! double over TLS.

use super::tests::{make_agent, TestRuntime, TestVolumeService};
use super::*;
use crate::application::credential_service::{ToolCallActor, ToolCredentialSource};
use crate::application::nfs_gateway::NfsVolumeRegistry;
use crate::application::tool_invocation_service::{ToolInvocationResult, ToolInvocationService};
use crate::domain::agent::Agent;
use crate::domain::fsal::AegisFSAL;
use crate::domain::iam::{IdentityKind, ZaruTier};
use crate::domain::node_config::{NodeConfigManifest, SealGatewayConfig};
use crate::domain::repository::AgentRepository;
use crate::domain::seal_session::SealSessionError;
use crate::domain::secrets::{SensitiveString, SensitiveUrl};
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::security_context::{Capability, SecurityContext, SecurityContextMetadata};
use crate::domain::tenant::TenantId as CoreTenantId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
};
use crate::infrastructure::seal::middleware::SealMiddleware;
use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use crate::infrastructure::seal_gateway_proto::gateway_invocation_service_server::{
    GatewayInvocationService as GrpcGateway, GatewayInvocationServiceServer,
};
use crate::infrastructure::seal_gateway_proto::{
    ExploreApiRequest, ExploreApiResponse, InvokeCliRequest, InvokeCliResponse, InvokeToolRequest,
    InvokeToolResponse, InvokeWorkflowRequest, InvokeWorkflowResponse, ListToolsRequest,
    ListToolsResponse,
};
use crate::infrastructure::storage::LocalHostStorageProvider;
use crate::infrastructure::tool_router::ToolRouter;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const SERVER: &str = "notes-server";
const CONTEXT: &str = "remote-tools-ctx";
const PERSON: &str = "u-remote-person";
const MARKER: &str = "binding-secret-marker";

/// The gateway double: records each `InvokeTool` and answers a result.
#[derive(Clone, Default)]
struct Gateway {
    calls: Arc<Mutex<Vec<InvokeToolRequest>>>,
}

#[tonic::async_trait]
impl GrpcGateway for Gateway {
    async fn invoke_workflow(
        &self,
        _: tonic::Request<InvokeWorkflowRequest>,
    ) -> Result<tonic::Response<InvokeWorkflowResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn invoke_cli(
        &self,
        _: tonic::Request<InvokeCliRequest>,
    ) -> Result<tonic::Response<InvokeCliResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn explore_api(
        &self,
        _: tonic::Request<ExploreApiRequest>,
    ) -> Result<tonic::Response<ExploreApiResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn list_tools(
        &self,
        _: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        Ok(tonic::Response::new(ListToolsResponse { tools: vec![] }))
    }
    async fn invoke_tool(
        &self,
        req: tonic::Request<InvokeToolRequest>,
    ) -> Result<tonic::Response<InvokeToolResponse>, tonic::Status> {
        self.calls.lock().unwrap().push(req.into_inner());
        Ok(tonic::Response::new(InvokeToolResponse {
            result_json: r#"{"content":[{"type":"text","text":"page"}],"isError":false}"#
                .to_string(),
            grounding_json: String::new(),
        }))
    }
}

/// Serve `gateway` on a loopback port over TLS with a throwaway certificate
/// for `localhost`; the `https` URL, the CA file and a shutdown handle.
async fn serve_tls(
    gateway: Gateway,
) -> (String, std::path::PathBuf, tokio::sync::oneshot::Sender<()>) {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("throwaway certificate");
    let dir = std::env::temp_dir().join(format!("aegis-remote-start-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let ca_path = dir.join("gateway-ca.crt");
    std::fs::write(&ca_path, generated.cert.pem()).unwrap();
    let identity = tonic::transport::Identity::from_pem(
        generated.cert.pem(),
        generated.signing_key.serialize_pem(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tonic::transport::Server::builder()
        .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))
        .expect("server TLS");
    tokio::spawn(async move {
        let _ = server
            .add_service(GatewayInvocationServiceServer::new(gateway))
            .serve_with_incoming_shutdown(
                tonic::transport::server::TcpIncoming::from(listener),
                async {
                    let _ = rx.await;
                },
            )
            .await;
    });
    (format!("https://localhost:{port}"), ca_path, tx)
}

/// The person's binding to `SERVER`, granted to one agent: answers its
/// secret for that agent's call by that person, and none otherwise.
struct GrantedBinding {
    granted_agent: AgentId,
    lookups: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ToolCredentialSource for GrantedBinding {
    async fn tool_server_credential(
        &self,
        actor: &ToolCallActor<'_>,
        server: &str,
    ) -> anyhow::Result<Option<SensitiveString>> {
        self.lookups.lock().unwrap().push(server.to_string());
        Ok(
            (server == SERVER && actor.user_id == PERSON && actor.agent_id == self.granted_agent)
                .then(|| SensitiveString::new(MARKER)),
        )
    }
}

fn agent_declaring(tools: &[&str]) -> Agent {
    let mut agent = make_agent("remote-reader", None, None);
    agent.manifest.spec.tools = tools.iter().map(|t| t.to_string()).collect();
    agent
}

fn person(tenant_id: &CoreTenantId) -> UserIdentity {
    UserIdentity {
        sub: PERSON.to_string(),
        realm_slug: "zaru-consumer".to_string(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Free,
            tenant_id: tenant_id.clone(),
        },
    }
}

fn input() -> ExecutionInput {
    ExecutionInput {
        intent: Some("read a page".to_string()),
        input: serde_json::json!({ "tenant_id": CoreTenantId::consumer().as_str() }),
        workspace_volume_id: None,
        workspace_volume_mount_path: None,
        workspace_remote_path: None,
        workflow_execution_id: None,
        attachments: Vec::new(),
    }
}

fn admit_all() -> SecurityContext {
    SecurityContext {
        name: CONTEXT.to_string(),
        description: "remote tools start test".to_string(),
        capabilities: vec![Capability {
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
        metadata: SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

/// The node: its configuration with `seal_gateway` as given, the execution
/// service as the daemon builds it (with the `ToolRouter`), and the tool
/// invocation service wired from the same `seal_gateway`.
struct Node {
    tenant: CoreTenantId,
    executions: Arc<StandardExecutionService>,
    tools: ToolInvocationService,
}

async fn node(
    agent: &Agent,
    seal_gateway: Option<SealGatewayConfig>,
    credentials: Option<Arc<dyn ToolCredentialSource>>,
) -> Node {
    let tenant = CoreTenantId::consumer();
    let agent_repo = Arc::new(InMemoryAgentRepository::new());
    agent_repo.save_for_tenant(&tenant, agent).await.unwrap();
    let mut config = NodeConfigManifest::default();
    config.spec.seal_gateway = seal_gateway.clone();
    let router = Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers()));
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let runtime = Arc::new(TestRuntime::default());
    let executions = Arc::new(
        StandardExecutionService::new(
            agent_repo.clone(),
            Arc::new(TestVolumeService {
                volumes: HashMap::new(),
            }),
            Arc::new(Supervisor::new(runtime)),
            Arc::new(InMemoryExecutionRepository::new()),
            event_bus.clone(),
            Arc::new(config),
        )
        .with_tool_router(router.clone()),
    );

    let contexts =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    contexts.save(admit_all()).await.unwrap();
    let storage_root =
        std::env::temp_dir().join(format!("aegis-remote-start-fs-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(crate::application::nfs_gateway::EventBusPublisher::new(
            event_bus.clone(),
        )),
    ));
    let tools = ToolInvocationService::new(
        Arc::new(InMemorySealSessionRepository::new()),
        contexts,
        Arc::new(SealMiddleware::new()),
        router,
        fsal,
        NfsVolumeRegistry::new(),
        agent_repo,
        executions.clone(),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
        event_bus,
        seal_gateway.as_ref().map(|g| g.url.expose().to_string()),
    )
    .with_seal_gateway_config(seal_gateway.as_ref(), credentials)
    .expect("the configuration wires");
    Node {
        tenant,
        executions,
        tools,
    }
}

impl Node {
    async fn start(&self, agent: &Agent) -> anyhow::Result<ExecutionId> {
        self.executions
            .start_execution(
                agent.id,
                input(),
                CONTEXT.to_string(),
                Some(&person(&self.tenant)),
            )
            .await
    }

    async fn call(
        &self,
        agent: &Agent,
        execution: ExecutionId,
        tool: &str,
    ) -> Result<serde_json::Value, SealSessionError> {
        match self
            .tools
            .invoke_tool_internal(
                &agent.id,
                execution,
                self.tenant.clone(),
                0,
                Vec::new(),
                tool.to_string(),
                serde_json::json!({"path": "home"}),
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

fn gateway_config(url: &str, ca: &std::path::Path, servers: &[&str]) -> SealGatewayConfig {
    SealGatewayConfig {
        url: SensitiveUrl::new(url),
        ca_cert_path: Some(ca.to_path_buf()),
        remote_servers: servers.iter().map(|s| s.to_string()).collect(),
    }
}

/// The reproduction: an agent declaring `<configured server>.<tool>`
/// starts, and its call reaches the gateway with its person's binding's
/// credential resolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_declaring_a_configured_servers_tool_starts_and_its_call_reaches_the_gateway() {
    let gateway = Gateway::default();
    let (url, ca, _shutdown) = serve_tls(gateway.clone()).await;
    let agent = agent_declaring(&[&format!("{SERVER}.pages.read")]);
    let binding = Arc::new(GrantedBinding {
        granted_agent: agent.id,
        lookups: Mutex::new(Vec::new()),
    });
    let node = node(
        &agent,
        Some(gateway_config(&url, &ca, &[SERVER])),
        Some(binding.clone()),
    )
    .await;

    let execution = node
        .start(&agent)
        .await
        .expect("an agent declaring a configured remote server's tool starts");
    let value = node
        .call(&agent, execution, &format!("{SERVER}.pages.read"))
        .await
        .expect("the granted call is made");

    assert_eq!(
        value,
        serde_json::json!({"content":[{"type":"text","text":"page"}],"isError":false})
    );
    let calls = gateway.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "one InvokeTool");
    assert_eq!(
        (calls[0].server.as_str(), calls[0].tool.as_str()),
        (SERVER, "pages.read")
    );
    assert_eq!(calls[0].acting.as_ref().unwrap().user_id, PERSON);
    assert_eq!(calls[0].credential.as_ref().unwrap().value, MARKER);
}

/// The same agent with no grant starts; its call is refused
/// `CREDENTIAL_BINDING_REQUIRED` and nothing reaches the gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_agent_without_a_grant_starts_and_its_call_is_refused_before_the_gateway() {
    let gateway = Gateway::default();
    let (url, ca, _shutdown) = serve_tls(gateway.clone()).await;
    let agent = agent_declaring(&[&format!("{SERVER}.pages.read")]);
    let binding = Arc::new(GrantedBinding {
        granted_agent: AgentId::new(),
        lookups: Mutex::new(Vec::new()),
    });
    let node = node(
        &agent,
        Some(gateway_config(&url, &ca, &[SERVER])),
        Some(binding.clone()),
    )
    .await;

    let execution = node
        .start(&agent)
        .await
        .expect("the start check does not decide admission");
    let err = node
        .call(&agent, execution, &format!("{SERVER}.pages.read"))
        .await
        .expect_err("no grant, no call");

    let refusal = err.refusal();
    assert_eq!(refusal.code, "CREDENTIAL_BINDING_REQUIRED");
    assert_eq!(refusal.http_status, 403);
    assert_eq!(binding.lookups.lock().unwrap().as_slice(), [SERVER]);
    assert!(gateway.calls.lock().unwrap().is_empty(), "nothing sent");
}

/// A tool of a server the node is not configured for is refused at start,
/// in words that say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_declaring_a_tool_of_an_unconfigured_server_is_refused_at_start() {
    let (url, ca, _shutdown) = serve_tls(Gateway::default()).await;
    let agent = agent_declaring(&["other-server.pages.read"]);
    let node = node(
        &agent,
        Some(gateway_config(&url, &ca, &[SERVER])),
        Some(Arc::new(GrantedBinding {
            granted_agent: agent.id,
            lookups: Mutex::new(Vec::new()),
        })),
    )
    .await;

    let err = node.start(&agent).await.expect_err("refused at start");
    assert_eq!(
        err.to_string(),
        "Failed to extract user input from execution input: Agent requested tool \
         'other-server.pages.read' but it is not available in the current node \
         configuration: it is no tool of this node, and 'other-server' is not one of its \
         remote servers (seal_gateway.remote_servers: [notes-server])."
    );
}

/// With no `seal_gateway`, the start check is today's: a declared
/// `<server>.<tool>` is refused in today's words, and a builtin starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_seal_gateway_the_start_check_is_todays() {
    let remote = agent_declaring(&[&format!("{SERVER}.pages.read")]);
    let node_remote = node(&remote, None, None).await;
    let err = node_remote.start(&remote).await.expect_err("refused");
    assert_eq!(
        err.to_string(),
        format!(
            "Failed to extract user input from execution input: Agent requested tool \
             '{SERVER}.pages.read' but it is not available in the current node configuration."
        )
    );

    let builtin = agent_declaring(&["fs.read"]);
    let node_builtin = node(&builtin, None, None).await;
    node_builtin
        .start(&builtin)
        .await
        .expect("a declared builtin starts as today");
}
