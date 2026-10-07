// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The SEAL gateway at the daemon's construction (AEGIS ADR-132 H1, H4, H8).
//!
//! The service is built as `start_daemon` builds it: the gateway's address
//! from [`daemon_seal_gateway_url`], the daemon's own SEAL middleware
//! ([`daemon_seal_middleware`]), then [`daemon_seal_gateway_wiring`] with
//! `seal_gateway` and a credential store. A call goes through
//! `ToolInvocationService::invoke_tool`, what `POST /v1/seal/invoke` calls,
//! and a refusal is answered by the route's own `invoke_refusal_response`.
//! The gateway is a loopback stub that records every request. A TLS round
//! trip is the core crate's test of the same wiring
//! (`the_configured_gateway_takes_a_granted_users_call_through_the_route`):
//! this crate has no certificate generator to serve TLS with.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::credential_service::{
    ToolCallActor, ToolCredentialSource,
};
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService;
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentScope};
use aegis_orchestrator_core::domain::events::{ExecutionEvent, StorageEvent};
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration, LlmInteraction, TrajectoryStep,
};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::node_config::SealGatewayConfig;
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::seal_session::{
    EnvelopeVerifier, SealSession, SealSessionError,
};
use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
use aegis_orchestrator_core::domain::secrets::{SensitiveString, SensitiveUrl};
use aegis_orchestrator_core::domain::security_context::capability::Capability;
use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
use aegis_orchestrator_core::domain::security_context::{SecurityContext, SecurityContextMetadata};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::WorkflowId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::seal_gateway_proto::gateway_invocation_service_server::{
    GatewayInvocationService, GatewayInvocationServiceServer,
};
use aegis_orchestrator_core::infrastructure::seal_gateway_proto::{
    ExploreApiRequest, ExploreApiResponse, InvokeCliRequest, InvokeCliResponse,
    InvokeToolRequest, InvokeToolResponse, InvokeWorkflowRequest, InvokeWorkflowResponse,
    ListToolsRequest, ListToolsResponse,
};
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter;

use super::{daemon_seal_gateway_url, daemon_seal_gateway_wiring, daemon_seal_middleware};
use crate::daemon::handlers::seal::invoke_refusal_response;

const USER: &str = "wiring-user-1";
const SERVER: &str = "nuclear-notes";
/// The marker value of the acting user's stored credential.
const MARKER: &str = "Mk3-daemon-wiring-credential";

// ---------------------------------------------------------------------------
// The loopback gateway: records every request, lists nothing, invokes nothing
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Received {
    lists: Vec<ListToolsRequest>,
    tools: Vec<InvokeToolRequest>,
    other: usize,
}

#[derive(Clone, Default)]
struct Recorder(Arc<StdMutex<Received>>);

#[tonic::async_trait]
impl GatewayInvocationService for Recorder {
    async fn invoke_workflow(
        &self,
        _: tonic::Request<InvokeWorkflowRequest>,
    ) -> Result<tonic::Response<InvokeWorkflowResponse>, tonic::Status> {
        self.0.lock().unwrap().other += 1;
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn invoke_cli(
        &self,
        _: tonic::Request<InvokeCliRequest>,
    ) -> Result<tonic::Response<InvokeCliResponse>, tonic::Status> {
        self.0.lock().unwrap().other += 1;
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn explore_api(
        &self,
        _: tonic::Request<ExploreApiRequest>,
    ) -> Result<tonic::Response<ExploreApiResponse>, tonic::Status> {
        self.0.lock().unwrap().other += 1;
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn list_tools(
        &self,
        req: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        self.0.lock().unwrap().lists.push(req.into_inner());
        Ok(tonic::Response::new(ListToolsResponse { tools: vec![] }))
    }
    async fn invoke_tool(
        &self,
        req: tonic::Request<InvokeToolRequest>,
    ) -> Result<tonic::Response<InvokeToolResponse>, tonic::Status> {
        self.0.lock().unwrap().tools.push(req.into_inner());
        Ok(tonic::Response::new(InvokeToolResponse {
            result_json: "{}".to_string(),
            grounding_json: String::new(),
        }))
    }
}

/// Serve a recorder on a loopback port over plaintext: its `http` URL, what
/// it received, and a shutdown handle.
async fn serve_recorder() -> (String, Recorder, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().unwrap();
    let recorder = Recorder::default();
    let served = recorder.clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(GatewayInvocationServiceServer::new(served))
            .serve_with_incoming_shutdown(
                tonic::transport::server::TcpIncoming::from(listener),
                async {
                    let _ = rx.await;
                },
            )
            .await;
    });
    (format!("http://{addr}"), recorder, tx)
}

// ---------------------------------------------------------------------------
// The credential store: counts lookups, answers a credential or none
// ---------------------------------------------------------------------------

struct Credentials {
    answer: Option<&'static str>,
    lookups: AtomicUsize,
}

impl Credentials {
    fn answering(answer: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            lookups: AtomicUsize::new(0),
        })
    }
    fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ToolCredentialSource for Credentials {
    async fn tool_server_credential(
        &self,
        actor: &ToolCallActor<'_>,
        server: &str,
    ) -> Result<Option<SensitiveString>> {
        assert_eq!((actor.user_id, server), (USER, SERVER));
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Ok(self.answer.map(SensitiveString::new))
    }
}

// ---------------------------------------------------------------------------
// The service as the daemon builds it
// ---------------------------------------------------------------------------

struct NoExecutions;

#[async_trait]
impl ExecutionService for NoExecutions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        _: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<Execution> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_unscoped(&self, _: ExecutionId) -> Result<Execution> {
        anyhow::bail!("not exercised")
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
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<DomainEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn list_executions_for_tenant(
        &self,
        _: &TenantId,
        _: Option<AgentId>,
        _: Option<WorkflowId>,
        _: usize,
    ) -> Result<Vec<Execution>> {
        anyhow::bail!("not exercised")
    }
    async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn record_llm_interaction(&self, _: ExecutionId, _: u8, _: LlmInteraction) -> Result<()> {
        Ok(())
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<TrajectoryStep>,
    ) -> Result<()> {
        Ok(())
    }
}

/// A chat session's agent id is random and has no manifest.
struct NoAgents;

#[async_trait]
impl AgentLifecycleService for NoAgents {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: AgentScope,
        _: Option<&UserIdentity>,
    ) -> Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
        anyhow::bail!("no manifest")
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
        Ok(vec![])
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
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
impl EventPublisher for NoOpPublisher {
    async fn publish_storage_event(&self, _event: StorageEvent) {}
}

fn context() -> SecurityContext {
    SecurityContext {
        name: "zaru-pro".to_string(),
        description: "consumer".to_string(),
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

fn gateway(url: &str, ca: Option<&str>, servers: &[&str]) -> SealGatewayConfig {
    SealGatewayConfig {
        url: SensitiveUrl::new(url),
        ca_cert_path: ca.map(std::path::PathBuf::from),
        remote_servers: servers.iter().map(|s| s.to_string()).collect(),
    }
}

/// The service with no gateway part yet, built as `start_daemon` builds it
/// before [`daemon_seal_gateway_wiring`]: its address from `seal_gateway`,
/// the daemon's own SEAL middleware.
async fn unwired(
    gateway: Option<&SealGatewayConfig>,
) -> (ToolInvocationService, Arc<InMemorySealSessionRepository>) {
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(context()).await.unwrap();
    let storage_root =
        std::env::temp_dir().join(format!("aegis-daemon-wiring-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let service = ToolInvocationService::new(
        sessions.clone(),
        contexts,
        daemon_seal_middleware(None, None),
        Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(NoAgents),
        Arc::new(NoExecutions),
        Arc::new(ReqwestWebToolAdapter::unconfigured()),
        Arc::new(EventBus::new(64)),
        daemon_seal_gateway_url(gateway),
    );
    (service, sessions)
}

/// The daemon's service for `gateway` and `credentials`, with a session as
/// `/v1/seal/attest` binds it for `USER` through the Zaru MCP server.
async fn daemon(gateway: Option<&SealGatewayConfig>, credentials: Arc<Credentials>) -> Daemon {
    let (service, sessions) = unwired(gateway).await;
    let service =
        daemon_seal_gateway_wiring(service, gateway, Some(credentials)).expect("the daemon starts");
    let token = format!("token-{}", uuid::Uuid::new_v4());
    let session = SealSession::new(
        AgentId::new(),
        ExecutionId::new(),
        vec![],
        token.clone(),
        context(),
        TenantId::for_consumer_user(USER).unwrap(),
    )
    .with_principal_metadata(Some(USER.to_string()), Some(USER.to_string()), None, None);
    sessions.save(session).await.unwrap();
    Daemon { service, token }
}

struct Daemon {
    service: ToolInvocationService,
    token: String,
}

struct Envelope {
    token: SensitiveString,
    tool: String,
}

impl EnvelopeVerifier for Envelope {
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
        Some(json!({"query": "q"}))
    }
    fn replay_nonce(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

impl Daemon {
    /// One refused call through the route: its status and body.
    async fn refused(&self, tool: &str) -> (u16, Value) {
        let error = self
            .service
            .invoke_tool(&Envelope {
                token: self.token.clone().into(),
                tool: tool.to_string(),
            })
            .await
            .expect_err("refused");
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": tool, "arguments": {"query": "q"}},
        });
        let response = invoke_refusal_response(&error, &payload);
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// No `seal_gateway` (production today): the daemon's service answers a
/// tool no builtin serves 404 `NOT_FOUND` as before, looks up no
/// credential, and has no gateway to dial.
#[tokio::test]
async fn without_seal_gateway_the_daemon_answers_an_unknown_tool_404_and_nothing_dials() {
    assert_eq!(daemon_seal_gateway_url(None), None);
    let credentials = Credentials::answering(Some(MARKER));
    let daemon = daemon(None, credentials.clone()).await;
    let (status, body) = daemon.refused(&format!("{SERVER}.pages.read")).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "NOT_FOUND", "{body}");
    assert_eq!(
        body["error"]["message"],
        format!("Not found: tool '{SERVER}.pages.read'.")
    );
    assert_eq!(credentials.lookups(), 0, "no credential was looked up");
}

/// `seal_gateway` with `remote_servers`: the daemon dials the configured
/// gateway and wires the servers and the credential store. A user with no
/// granted binding is refused 403 `CREDENTIAL_BINDING_REQUIRED` and the
/// gateway receives nothing; one with a credential, over this plaintext
/// address, is refused 503 `CREDENTIAL_CHANNEL_NOT_CONFIDENTIAL` and the
/// gateway still receives nothing (H8); a tool of a server not named is
/// looked up in the gateway's list with no credential, and answered 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_seal_gateway_the_daemon_wires_the_servers_the_credential_store_and_the_address() {
    let (url, recorder, _shutdown) = serve_recorder().await;
    let config = gateway(&url, None, &[SERVER]);
    assert_eq!(daemon_seal_gateway_url(Some(&config)), Some(url.clone()));

    let none = Credentials::answering(None);
    let daemon_without_binding = daemon(Some(&config), none.clone()).await;
    let (status, body) = daemon_without_binding
        .refused(&format!("{SERVER}.pages.read"))
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(
        body["error"]["code"], "CREDENTIAL_BINDING_REQUIRED",
        "{body}"
    );
    assert_eq!(none.lookups(), 1);

    let held = Credentials::answering(Some(MARKER));
    let daemon_with_binding = daemon(Some(&config), held.clone()).await;
    let (status, body) = daemon_with_binding
        .refused(&format!("{SERVER}.pages.read"))
        .await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(
        body["error"]["code"], "CREDENTIAL_CHANNEL_NOT_CONFIDENTIAL",
        "{body}"
    );
    assert!(!body.to_string().contains(MARKER), "{body}");
    assert_eq!(held.lookups(), 1);
    {
        let received = recorder.0.lock().unwrap();
        assert!(
            received.tools.is_empty() && received.lists.is_empty() && received.other == 0,
            "the gateway received nothing"
        );
    }

    let (status, body) = daemon_with_binding.refused("unnamed.pages.read").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(
        body["error"]["message"],
        "Not found: tool 'unnamed.pages.read'."
    );
    let received = recorder.0.lock().unwrap();
    assert_eq!(
        received.lists.len(),
        1,
        "the configured gateway was dialled"
    );
    assert!(received.lists[0].bound_servers.is_empty());
    assert!(received.tools.is_empty());
}

/// A `seal_gateway` the daemon cannot use stops it, with the reason: a CA
/// file it cannot read, a server name the gateway could not register,
/// remote servers on a node with no database.
#[tokio::test]
async fn a_seal_gateway_the_daemon_cannot_use_stops_it_with_the_reason() {
    let url = "https://aegis-seal-gateway:50055";
    for (config, credentials, reason) in [
        (
            gateway(url, Some("/nonexistent/aegis-daemon-ca.crt"), &[SERVER]),
            true,
            "seal_gateway.ca_cert_path",
        ),
        (
            gateway(url, None, &["nuclear.notes"]),
            true,
            "seal_gateway.remote_servers",
        ),
        (gateway(url, None, &[SERVER]), false, "no credential store"),
    ] {
        let (service, _) = unwired(Some(&config)).await;
        let credentials =
            credentials.then(|| Credentials::answering(None) as Arc<dyn ToolCredentialSource>);
        let error = daemon_seal_gateway_wiring(service, Some(&config), credentials)
            .err()
            .expect("the daemon does not start");
        let text = format!("{error:#}");
        assert!(text.contains(reason), "{reason}: {text}");
    }
}

// ---------------------------------------------------------------------------
// The daemon grounds each remote server with its own tool (AEGIS ADR-136
// G14, G14a to G14c)
// ---------------------------------------------------------------------------

mod remote_grounding {
    use super::super::daemon_remote_grounding;
    use aegis_orchestrator_core::application::credential_service::{
        CredentialManagementService, GroundingRefusal, OAuthProviderRegistry,
        RemoteServerGrounding, StandardCredentialManagementService, StoreApiKeyCommand,
    };
    use aegis_orchestrator_core::domain::credential::{
        CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialProvider,
        CredentialScope, CredentialType, GrantTarget, OAuthPendingState, UserCredentialBinding,
    };
    use aegis_orchestrator_core::domain::node_config::SealGatewayConfig;
    use aegis_orchestrator_core::domain::secrets::SensitiveString;
    use aegis_orchestrator_core::domain::tenant::TenantId;
    use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
    use aegis_orchestrator_core::infrastructure::secrets_manager::{
        SecretsManager, TestSecretStore,
    };
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, Weak};
    use tokio::sync::RwLock;

    const OWNER: &str = "daemon-grounding-owner";

    #[derive(Default)]
    struct Bindings(RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>);

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
            _: &TenantId,
            _: &str,
        ) -> anyhow::Result<Vec<UserCredentialBinding>> {
            Ok(self.0.read().await.values().cloned().collect())
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
            Ok(())
        }
        async fn find_oauth_state(&self, _: &str) -> anyhow::Result<Option<OAuthPendingState>> {
            Ok(None)
        }
        async fn delete_oauth_state(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete_expired_oauth_states(&self, _: DateTime<Utc>) -> anyhow::Result<u64> {
            Ok(0)
        }
    }

    /// Records the (server, tool) of each grounding and answers an instance
    /// payload.
    #[derive(Default)]
    struct Recorded(Mutex<Vec<(String, String)>>);

    #[async_trait]
    impl RemoteServerGrounding for Recorded {
        async fn ground_token(
            &self,
            _: &TenantId,
            _: &str,
            server: &str,
            tool: &str,
            _: &SensitiveString,
        ) -> Result<Value, GroundingRefusal> {
            self.0
                .lock()
                .unwrap()
                .push((server.to_string(), tool.to_string()));
            Ok(json!({"you": {"instances": [{"id": "inst-id", "slug": "play2"}]}}))
        }
    }

    fn config(entries: &str) -> SealGatewayConfig {
        serde_yaml::from_str(&format!(
            "url: https://aegis-seal-gateway:50055\nremote_servers:\n{entries}"
        ))
        .expect("the configuration parses")
    }

    fn service(bindings: Arc<Bindings>) -> StandardCredentialManagementService {
        let event_bus = Arc::new(EventBus::new(64));
        let secrets = Arc::new(SecretsManager::from_store(
            Arc::new(TestSecretStore::new()),
            event_bus.clone(),
        ));
        StandardCredentialManagementService::new(
            bindings,
            secrets,
            event_bus,
            Arc::new(OAuthProviderRegistry::new()),
        )
    }

    fn store(provider: &str) -> StoreApiKeyCommand {
        StoreApiKeyCommand {
            owner_user_id: OWNER.to_string(),
            tenant_id: TenantId::for_consumer_user(OWNER).unwrap(),
            provider: CredentialProvider::new(provider),
            label: "Work".to_string(),
            scope: CredentialScope::Personal,
            api_key_value: SensitiveString::new("daemon-grounding-token"),
            credential_type: CredentialType::Secret,
        }
    }

    /// G14, G14c: the daemon hands the credential service each remote
    /// server with its own grounding tool: `nuclear-notes`, named with
    /// `grounding_tool: cortex.ground`, is grounded with it; `github`,
    /// named bare, is never called and its binding is stored with no reach.
    #[tokio::test]
    async fn the_daemon_grounds_each_remote_server_with_its_own_tool() {
        let bindings = Arc::new(Bindings::default());
        let credentials = service(bindings.clone());
        let recorded = Arc::new(Recorded::default());
        let weak: Weak<Recorded> = Arc::downgrade(&recorded);
        let weak: Weak<dyn RemoteServerGrounding> = weak;
        daemon_remote_grounding(
            &credentials,
            &config("  - name: nuclear-notes\n    grounding_tool: cortex.ground\n  - github\n"),
            weak,
        )
        .expect("the daemon starts");

        let notes = credentials.store_api_key(store("nuclear-notes")).await;
        let github = credentials.store_api_key(store("github")).await;
        let asked = recorded.0.lock().unwrap().clone();
        println!("grounding calls {asked:?}");
        assert_eq!(
            asked,
            vec![("nuclear-notes".to_string(), "cortex.ground".to_string())],
            "the daemon did not ground each server with its own tool"
        );
        let notes = notes.expect("stored");
        let github = github.expect("stored");
        let rows = bindings.0.read().await;
        assert!(rows[&notes].metadata.reach.is_some());
        assert_eq!(rows[&github].metadata.reach, None);
    }

    /// G14a: a grounding tool the rule refuses stops the daemon with the
    /// reason.
    #[tokio::test]
    async fn a_grounding_tool_the_rule_refuses_stops_the_daemon() {
        let credentials = service(Arc::new(Bindings::default()));
        let recorded = Arc::new(Recorded::default());
        let weak: Weak<Recorded> = Arc::downgrade(&recorded);
        let weak: Weak<dyn RemoteServerGrounding> = weak;
        let error = daemon_remote_grounding(
            &credentials,
            &config("  - name: github\n    grounding_tool: get me\n"),
            weak,
        )
        .expect_err("the daemon does not start");
        let text = format!("{error:#}");
        println!("refusal {text}");
        assert!(
            text.contains("seal_gateway configuration")
                && text.contains("names the grounding tool 'get me'"),
            "{text}"
        );
    }
}
