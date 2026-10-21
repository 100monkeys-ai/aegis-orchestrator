// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! What the carbon-forge tests (AEGIS ADR-147) share: Carbon Forge behind
//! a loopback SEAL gateway over TLS, answering each tool as Carbon Forge at
//! `76d62e5` answers it and recording every call; the person's binding; and
//! the interpreter's tool invocation service dialling that gateway.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::credential_service::{
    ToolCallActor, ToolCredentialSource,
};
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_approval_service::ToolApprovalService;
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService;
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    ContextChoice, Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::repository::{AgentVersion, WorkflowExecutionRepository};
use aegis_orchestrator_core::domain::execution_token::ExecutionToken;
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::security_context::SecurityContextRepository;
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::seal_gateway_proto::gateway_invocation_service_server::{
    GatewayInvocationService, GatewayInvocationServiceServer,
};
use aegis_orchestrator_core::infrastructure::seal_gateway_proto::{
    ExploreApiRequest, ExploreApiResponse, InvokeCliRequest, InvokeCliResponse,
    InvokeToolRequest, InvokeToolResponse, InvokeWorkflowRequest, InvokeWorkflowResponse,
    ListToolsRequest, ListToolsResponse,
};
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;

/// The run's person.
pub const PERSON: &str = "the-person";
/// The Carbon Forge binding the run's `contexts` chose.
pub const BINDING: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
/// The bearer token that binding resolves to (a fake).
pub const TOKEN: &str = "nn-test-token";
/// The secret of the run's execution token, minted from that binding (a
/// fake; AEGIS ADR-147 Update `execution-token-2026-10-17` X2).
pub const CHILD_TOKEN: &str = "nn-child-token-of-the-run";
/// The execution token's id in Carbon Forge.
pub const CHILD_TOKEN_ID: &str = "7d1e0c4a-0000-4000-8000-00000000000a";
/// The Carbon Forge instance the run's workspace lives in.
pub const INSTANCE_ID: &str = "1b2c3d4e-0000-4000-8000-00000000000c";
/// The person as a Carbon Forge user: the owner of the run's lane.
pub const NN_USER_ID: &str = "5a5a5a5a-0000-4000-8000-00000000000b";
/// The remote server's name, as `seal_gateway.remote_servers` names it.
pub const SERVER: &str = "nuclear-notes";
/// The Carbon Forge workspace the run's tree lives in.
pub const WORKSPACE: &str = "c8b0fadf-176c-418a-a8a4-a148dcf9fd1d";
/// The tree's path in that workspace.
pub const TREE_PATH: &str = "code/aegis-platform-deployment";
/// The tree revision the run was materialized from.
pub const REVISION: i64 = 4242;
/// The landing's tool, gated as `aegis.git.land` is.
pub const TREE_LAND_TOOL: &str = "aegis.tree.land";
/// The security context a workflow's landing is evaluated under
/// (`tool_invocation_service/storage.rs` `LANDING_SECURITY_CONTEXT`).
pub const LANDING_CONTEXT: &str = "aegis-system-operator";
/// The README as the tree's revision holds it.
pub const README_BEFORE: &str = "first line\n";
/// The README as the run's agents left it.
pub const README_AFTER: &str = "first line\nsecond line\n";

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The lane a run's change is written to: one of its own, named for it.
pub fn lane_of(run: uuid::Uuid) -> String {
    format!("carbon-forge-{run}")
}

// ===========================================================================
// The loopback gateway, answering as Carbon Forge answers
// ===========================================================================

/// How the stub answers one tool.
#[derive(Clone)]
pub enum Answer {
    /// The tool's result.
    Result(Value),
    /// The server's own error, relayed as the gateway relays a remote
    /// tool's error: `REMOTE_TOOL_ERROR` with its text.
    RemoteError(String),
}

#[derive(Clone, Default)]
pub struct StubGateway {
    /// By the tool's name on the server (`trees.put`, `lanes.land`, ...).
    pub answers: HashMap<String, Answer>,
    pub calls: Arc<Mutex<Vec<InvokeToolRequest>>>,
}

#[tonic::async_trait]
impl GatewayInvocationService for StubGateway {
    async fn invoke_workflow(
        &self,
        _req: tonic::Request<InvokeWorkflowRequest>,
    ) -> Result<tonic::Response<InvokeWorkflowResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn invoke_cli(
        &self,
        _req: tonic::Request<InvokeCliRequest>,
    ) -> Result<tonic::Response<InvokeCliResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn explore_api(
        &self,
        _req: tonic::Request<ExploreApiRequest>,
    ) -> Result<tonic::Response<ExploreApiResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("not exercised"))
    }
    async fn list_tools(
        &self,
        _req: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        Ok(tonic::Response::new(ListToolsResponse { tools: vec![] }))
    }
    async fn invoke_tool(
        &self,
        req: tonic::Request<InvokeToolRequest>,
    ) -> Result<tonic::Response<InvokeToolResponse>, tonic::Status> {
        let call = req.into_inner();
        let answer = self.answers.get(&call.tool).cloned();
        self.calls.lock().unwrap().push(call);
        match answer {
            Some(Answer::Result(value)) => Ok(tonic::Response::new(InvokeToolResponse {
                result_json: value.to_string(),
                grounding_json: String::new(),
            })),
            Some(Answer::RemoteError(message)) => {
                let mut status = tonic::Status::invalid_argument(message);
                status
                    .metadata_mut()
                    .insert("seal-refusal-code", "REMOTE_TOOL_ERROR".parse().unwrap());
                Err(status)
            }
            None => Ok(tonic::Response::new(InvokeToolResponse {
                result_json: "{}".to_string(),
                grounding_json: String::new(),
            })),
        }
    }
}

/// Serve `stub` over TLS on a loopback port with a throwaway certificate
/// for `localhost`: the `https` URL, the CA file and a shutdown handle.
pub async fn serve_tls(
    stub: StubGateway,
    dir: &std::path::Path,
) -> (String, PathBuf, oneshot::Sender<()>) {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("throwaway certificate");
    let ca_path = dir.join("gateway-ca.crt");
    std::fs::write(&ca_path, generated.cert.pem()).unwrap();
    let identity = tonic::transport::Identity::from_pem(
        generated.cert.pem(),
        generated.signing_key.serialize_pem(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
    (format!("https://localhost:{port}"), ca_path, tx)
}

/// The recorded `trees.materialize` answer for the run's revision
/// (`catalog-substrate.ts` 584-598 at `76d62e5`): one file, no signed URL
/// (an fs store), so its bytes are read with `blobs.get`.
pub fn materialized() -> Value {
    json!({
        "tree_id": "8a1f3b2c-0000-4000-8000-000000000001",
        "path": TREE_PATH,
        "current_revision_id": REVISION,
        "revision_id": REVISION,
        "url_expires_at": null,
        "entries": [{
            "path": "README.md",
            "mode": "100644",
            "size": README_BEFORE.len(),
            "sha256": sha256_hex(README_BEFORE.as_bytes()),
            "url": null
        }],
        "next_offset": null
    })
}

/// Carbon Forge as the tests need it: the run's revision, a blob store, a
/// lane and a landing answering as `catalog.ts` 2774-2910 answers.
pub fn nuclear_notes(run: uuid::Uuid) -> StubGateway {
    let mut answers = HashMap::new();
    answers.insert(
        "trees.materialize".to_string(),
        Answer::Result(materialized()),
    );
    answers.insert(
        "blobs.put".to_string(),
        Answer::Result(json!({
            "sha256": sha256_hex(README_AFTER.as_bytes()),
            "byte_size": README_AFTER.len(),
            "deduped": false
        })),
    );
    answers.insert(
        "lanes.create".to_string(),
        Answer::Result(
            json!({ "lane_id": "5c0ffee0-0000-4000-8000-000000000002", "label": lane_of(run) }),
        ),
    );
    answers.insert(
        "trees.put".to_string(),
        Answer::Result(json!({
            "lane": lane_of(run),
            "sequence": 1,
            "tree_id": "8a1f3b2c-0000-4000-8000-000000000001",
            "path": TREE_PATH,
            "base_revision_id": REVISION,
            "upserts": 1,
            "removals": 0
        })),
    );
    // AEGIS ADR-147 Update `execution-token-2026-10-17` X2, X4: the
    // workspace's grounding, the mint and the revoke, as `cortex.server.ts`
    // and `catalog-tokens.ts` at nuclear-notes 86d8d4ef answer them.
    answers.insert(
        "cortex.ground".to_string(),
        Answer::Result(json!({
            "instance": { "id": INSTANCE_ID, "slug": "main" },
            "you": {
                "userId": NN_USER_ID,
                "currentWorkspace": { "id": WORKSPACE, "slug": "aegis-architecture" }
            }
        })),
    );
    answers.insert(
        "tokens.mint_execution".to_string(),
        Answer::Result(json!({
            "token_id": CHILD_TOKEN_ID,
            "parent_token_id": "0f0f0f0f-0000-4000-8000-00000000000d",
            "execution_id": run,
            "instance_id": INSTANCE_ID,
            "workspace_id": WORKSPACE,
            "tree_path": TREE_PATH,
            "lane_label": lane_of(run),
            "role_cap": "inherit",
            "tools": [],
            "expires_at": "2026-10-18T00:00:00.000Z",
            "secret": CHILD_TOKEN
        })),
    );
    answers.insert(
        "tokens.revoke_execution".to_string(),
        Answer::Result(json!({
            "token_id": CHILD_TOKEN_ID,
            "revoked": true,
            "already_revoked": false
        })),
    );
    answers.insert(
        "lanes.land".to_string(),
        Answer::Result(json!({
            "lane": lane_of(run),
            "changeset_id": "c4a9e5e1-0000-4000-8000-000000000003",
            "intent": format!("lane {}: carbon-forge: the task", lane_of(run)),
            "created_at": "2026-10-17T00:00:00.000Z",
            "entity_count": 1,
            "results": []
        })),
    );
    StubGateway {
        answers,
        calls: Arc::default(),
    }
}

// ===========================================================================
// Test doubles
// ===========================================================================

/// The person's Carbon Forge binding, resolved only for the binding the
/// run chose; and the runs' execution tokens, kept by person and execution
/// id as the secret store keeps them (X3).
#[derive(Default)]
pub struct OneBinding {
    pub children: Mutex<HashMap<(String, uuid::Uuid), ExecutionToken>>,
}

impl OneBinding {
    /// The binding, with `run` of `PERSON` holding its execution token, as a
    /// start that minted it leaves it.
    pub fn holding_child(run: uuid::Uuid) -> Self {
        let binding = Self::default();
        binding.children.lock().unwrap().insert(
            (PERSON.to_string(), run),
            ExecutionToken {
                token_id: CHILD_TOKEN_ID.to_string(),
                secret: SensitiveString::new(CHILD_TOKEN.to_string()),
                user_id: NN_USER_ID.to_string(),
            },
        );
        binding
    }

    /// Whether `run` of `PERSON` holds an execution token.
    pub fn holds_child(&self, run: uuid::Uuid) -> bool {
        self.children
            .lock()
            .unwrap()
            .contains_key(&(PERSON.to_string(), run))
    }
}

#[async_trait]
impl ToolCredentialSource for OneBinding {
    async fn tool_server_credential(
        &self,
        actor: &ToolCallActor<'_>,
        server: &str,
    ) -> anyhow::Result<Option<SensitiveString>> {
        let chosen =
            matches!(actor.context, ContextChoice::Binding(id) if id.0.to_string() == BINDING);
        Ok((server == SERVER && actor.user_id == PERSON && chosen)
            .then(|| SensitiveString::new(TOKEN.to_string())))
    }

    async fn store_execution_token(
        &self,
        _tenant_id: &TenantId,
        user_id: &str,
        execution_id: uuid::Uuid,
        token: &ExecutionToken,
    ) -> anyhow::Result<()> {
        self.children
            .lock()
            .unwrap()
            .insert((user_id.to_string(), execution_id), token.clone());
        Ok(())
    }

    async fn execution_token(
        &self,
        _tenant_id: &TenantId,
        user_id: &str,
        execution_id: uuid::Uuid,
    ) -> anyhow::Result<Option<ExecutionToken>> {
        Ok(self
            .children
            .lock()
            .unwrap()
            .get(&(user_id.to_string(), execution_id))
            .cloned())
    }

    async fn delete_execution_token(
        &self,
        _tenant_id: &TenantId,
        user_id: &str,
        execution_id: uuid::Uuid,
    ) -> anyhow::Result<()> {
        self.children
            .lock()
            .unwrap()
            .remove(&(user_id.to_string(), execution_id));
        Ok(())
    }
}

pub struct NoEvents;

#[async_trait]
impl EventPublisher for NoEvents {
    async fn publish_storage_event(
        &self,
        _e: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}

/// No agent execution exists: the steps under test are the interpreter's.
pub struct NoExecutions;

#[async_trait]
impl ExecutionService for NoExecutions {
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
    async fn get_execution_unscoped(&self, _id: ExecutionId) -> Result<Execution> {
        anyhow::bail!("execution not found")
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

/// No agent exists: the steps under test are the interpreter's.
pub struct NoAgents;

#[async_trait]
impl AgentLifecycleService for NoAgents {
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
        anyhow::bail!("no agent")
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

/// The interpreter's tool invocation service as the daemon wires it for a
/// run's remote tools: dialling the gateway at `url` (its certificate in
/// `ca`), knowing `nuclear-notes` as a remote server, resolving the
/// person's binding through [`OneBinding`], reading runs from
/// `executions`, gating through `approvals`, and making a run's tree volume
/// on `fsal`'s own storage (`local_host`, as a single node keeps it).
pub fn tool_service(
    url: String,
    ca: &std::path::Path,
    fsal: Arc<AegisFSAL>,
    contexts: Arc<dyn SecurityContextRepository>,
    executions: Arc<dyn WorkflowExecutionRepository>,
    approvals: Arc<ToolApprovalService>,
) -> Arc<ToolInvocationService> {
    tool_service_with(
        url,
        ca,
        fsal,
        contexts,
        executions,
        approvals,
        Arc::new(OneBinding::default()),
    )
}

/// [`tool_service`], resolving credentials and keeping execution tokens
/// through `binding`.
#[allow(clippy::too_many_arguments)]
pub fn tool_service_with(
    url: String,
    ca: &std::path::Path,
    fsal: Arc<AegisFSAL>,
    contexts: Arc<dyn SecurityContextRepository>,
    executions: Arc<dyn WorkflowExecutionRepository>,
    approvals: Arc<ToolApprovalService>,
    binding: Arc<OneBinding>,
) -> Arc<ToolInvocationService> {
    let event_bus = Arc::new(EventBus::new(256));
    let volume_service =
        aegis_orchestrator_core::application::volume_manager::StandardVolumeService::new(
            fsal.volume_repository().clone(),
            fsal.storage_provider().clone(),
            event_bus.clone(),
            "http://localhost:8888".to_string(),
            "local_host",
        )
        .expect("the volume service");
    let user_volumes = Arc::new(
        aegis_orchestrator_core::application::user_volume_service::UserVolumeService::new(
            fsal.volume_repository().clone(),
            Arc::new(volume_service),
            event_bus.clone(),
            aegis_orchestrator_core::domain::volume::StorageTierLimits::default(),
        ),
    );
    Arc::new(
        ToolInvocationService::new(
            Arc::new(InMemorySealSessionRepository::new()),
            contexts,
            Arc::new(SealMiddleware::new()),
            Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
            fsal,
            NfsVolumeRegistry::new(),
            Arc::new(NoAgents),
            Arc::new(NoExecutions),
            Arc::new(
                aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(),
            ),
            event_bus,
            Some(url),
        )
        .with_seal_gateway_ca_cert(ca)
        .expect("the CA file reads")
        .with_remote_tool_servers(vec![SERVER.to_string()])
        .with_tool_credentials(binding)
        .with_workflow_execution_repo(executions)
        .with_tool_approvals(approvals)
        .with_user_volume_service(user_volumes),
    )
}

/// Each call Carbon Forge received: its tool and its arguments.
pub fn calls_of(calls: &Mutex<Vec<InvokeToolRequest>>) -> Vec<(String, Value)> {
    calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c.tool.clone(),
                serde_json::from_str(&c.arguments_json).unwrap_or(Value::Null),
            )
        })
        .collect()
}
