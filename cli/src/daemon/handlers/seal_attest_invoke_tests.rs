// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A SEAL session's user is the authenticated caller's own subject (AEGIS
//! ADR-035 — Updates, S1 to S6).
//!
//! The binding's unit test drives [`attest_binding`] with an `aegis_*` key,
//! a consumer JWT and a shared-tenant user, each with a body that names no
//! user and with a body that names someone else.
//!
//! The attest-then-invoke test goes the way the Zaru MCP server goes: its
//! key resolved by the attest route's own lookup ([`resolve_api_key`]), its
//! attest body exactly as `orchestrator-client.ts` sends it (no `user_id`),
//! the route's own binding ([`attest_binding`]), the real attestation
//! service, then a signed SEAL envelope through
//! `ToolInvocationService::invoke_tool` (what `/v1/seal/invoke` calls) for
//! `aegis.goal.create`, `aegis.goal.evaluate` and `aegis.goal.status`.
//! Not covered here: the handler's header parsing
//! (`authenticate_attest_request`) and its Axum glue, which need an
//! `AppState`.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Result;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use futures::Stream;
use serde_json::{json, Value};

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::attestation_service::AttestationServiceImpl;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::goal_service::GoalService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService;
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentScope};
use aegis_orchestrator_core::domain::events::{ExecutionEvent, StorageEvent};
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, ExecutionStatus, Iteration, LlmInteraction,
    TrajectoryStep,
};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::iam::{IdentityKind, RealmKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::node_config::GoalsConfig;
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::security_context::capability::Capability;
use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
use aegis_orchestrator_core::domain::security_context::{SecurityContext, SecurityContextMetadata};
use aegis_orchestrator_core::domain::shared_kernel::ExecutionId as SharedExecutionId;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::WorkflowId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::seal::attestation::AttestationService;
use aegis_orchestrator_core::infrastructure::seal::envelope::SealEnvelope;
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::seal::signature::SecurityTokenIssuer;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter;

use super::{attest_binding, attestation_request, AttestCaller, HttpAttestationRequest};
use crate::daemon::api_key_identity::resolve_api_key;
use crate::daemon::api_key_identity::test_keys::{key_row, KeyTable};

const SUB: &str = "5e6f0000-key-owner";
const SOMEONE_ELSE: &str = "7a8b0000-someone-else";
const KEY: &str = "aegis_attest-identity-key";

fn no_lookup() -> impl FnOnce(
    SharedExecutionId,
) -> Pin<Box<dyn std::future::Future<Output = Result<TenantId, ()>> + Send>> {
    |_| Box::pin(async { panic!("no execution_id is sent, so no lookup runs") })
}

/// The attest body as the Zaru MCP server sends it
/// (`aegis-mcp-tools` `b8bd5c2`, `orchestrator-client.ts` 663-680), with
/// `extra` merged in.
fn mcp_body(public_key: &str, extra: Value) -> HttpAttestationRequest {
    let mut body = json!({
        "workload_id": format!("zaru:{SUB}:session-1"),
        "security_context": "zaru-pro",
        "zaru_tier": "pro",
        "public_key": public_key,
    });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    serde_json::from_value(body).expect("the MCP server's body parses")
}

fn table() -> KeyTable {
    let tenant = TenantId::for_consumer_user(SUB).unwrap();
    KeyTable {
        rows: vec![key_row(KEY, SUB, tenant.as_str(), None)],
        escalations: None,
    }
}

/// The caller as `authenticate_attest_request` builds it for an `aegis_*`
/// key (`seal.rs`, the API-key branch).
async fn key_caller() -> AttestCaller {
    let table = table();
    let resolved = resolve_api_key(Some(&table), KEY)
        .await
        .expect("the key resolves");
    AttestCaller {
        identity: resolved.identity,
        escalation: resolved
            .escalation
            .map(|escalation| (escalation, resolved.home_tenant)),
    }
}

fn jwt_caller(sub: &str) -> AttestCaller {
    AttestCaller {
        identity: UserIdentity {
            sub: sub.to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Pro,
                tenant_id: TenantId::for_consumer_user(sub).unwrap(),
            },
        },
        escalation: None,
    }
}

fn shared_tenant_caller(sub: &str) -> AttestCaller {
    AttestCaller {
        identity: UserIdentity {
            sub: sub.to_string(),
            realm_slug: "tenant-acme".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "tenant-acme".to_string(),
            },
        },
        escalation: None,
    }
}

/// S1, S5: whatever the body says, the session's user is the subject of the
/// credential that authenticated the call.
#[tokio::test]
async fn attest_binding_binds_the_authenticated_subject_whatever_the_body() {
    let cases: Vec<(&str, AttestCaller)> = vec![
        ("an aegis_* key", key_caller().await),
        ("a consumer JWT", jwt_caller(SUB)),
        ("a member of a shared tenant", shared_tenant_caller(SUB)),
    ];
    for (who, caller) in cases {
        for (body, extra) in [
            ("names no user", json!({})),
            ("names someone else", json!({"user_id": SOMEONE_ELSE})),
        ] {
            let binding = attest_binding(no_lookup(), Some(&caller), &mcp_body("k", extra))
                .await
                .unwrap_or_else(|e| panic!("{who} attests: {e:?}"));
            assert_eq!(
                binding.user_id.as_deref(),
                Some(SUB),
                "{who}, body {body}: the session's user is the caller's own subject"
            );
        }
    }
}

// ── The attest-then-invoke harness ──────────────────────────────────────────

fn zaru_pro() -> SecurityContext {
    zaru_pro_allowing(&["aegis.goal.*"])
}

fn zaru_pro_allowing(patterns: &[&str]) -> SecurityContext {
    SecurityContext {
        name: "zaru-pro".to_string(),
        description: "consumer".to_string(),
        capabilities: patterns
            .iter()
            .map(|pattern| Capability {
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
        metadata: SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

/// Executions as the real service stores them, in the starting identity's
/// tenant; every one ends `failed` at once, so a `goal-judge` run is a judge
/// fault (ADR-131 U11) and an evaluation decides without waiting.
#[derive(Default)]
struct Executions {
    started: StdMutex<HashMap<ExecutionId, Execution>>,
}

#[async_trait]
impl ExecutionService for Executions {
    async fn start_execution(
        &self,
        agent_id: AgentId,
        input: ExecutionInput,
        security_context_name: String,
        identity: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        let id = ExecutionId::new();
        let mut e = Execution::new_with_id(id, agent_id, input, 5, security_context_name);
        if let Some(IdentityKind::ConsumerUser { tenant_id, .. }) =
            identity.map(|i| &i.identity_kind)
        {
            e.tenant_id = tenant_id.clone();
        }
        e.status = ExecutionStatus::Failed;
        e.error = Some("no judge runs in this test".to_string());
        self.started.lock().unwrap().insert(id, e);
        Ok(id)
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
    async fn get_execution_for_tenant(
        &self,
        tenant: &TenantId,
        id: ExecutionId,
    ) -> Result<Execution> {
        self.started
            .lock()
            .unwrap()
            .get(&id)
            .filter(|e| &e.tenant_id == tenant)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Execution not found"))
    }
    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        self.started
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("not exercised"))
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
        _: Option<WorkflowId>,
        _: usize,
    ) -> Result<Vec<Execution>> {
        anyhow::bail!("not exercised")
    }
    async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn record_llm_interaction(&self, _: ExecutionId, _: u8, _: LlmInteraction) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<TrajectoryStep>,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
}

/// Every agent name, `goal-judge` among them, resolves to one agent.
struct OneAgent(AgentId);

#[async_trait]
impl AgentLifecycleService for OneAgent {
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
        Ok(Some(self.0))
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(Some(self.0))
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(Some(self.0))
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

/// A 2048-bit SEAL signing key made for this run, as `aegis init` makes one.
fn seal_signing_pem() -> String {
    use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    key.to_pkcs1_pem(LineEnding::LF).unwrap().to_string()
}

struct Mcp {
    service: ToolInvocationService,
    token: String,
    key: SigningKey,
    calls: u64,
    volumes: Arc<InMemoryVolumeRepository>,
    /// The local storage provider's root: a host volume's file `<path>/<f>`
    /// is the file `<storage_root>/<path>/<f>` on disk.
    storage_root: std::path::PathBuf,
    /// The execution store the service reads, so a test can hold the
    /// execution an agent's tool call runs in (the inner loop's path).
    executions: Arc<Executions>,
}

impl Mcp {
    /// One `tools/call`, signed as the MCP server signs it.
    async fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, String> {
        let envelope = self.envelope(tool, arguments);
        self.service
            .invoke_tool(&envelope)
            .await
            .map_err(|e| e.to_string())
    }

    /// The envelope of one `tools/call`, signed as the MCP server signs it.
    fn envelope(&mut self, tool: &str, arguments: Value) -> SealEnvelope {
        self.calls += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": self.calls,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        });
        self.sign(payload, &self.key.clone(), &self.token.clone())
    }

    fn sign(&self, payload: Value, key: &SigningKey, token: &str) -> SealEnvelope {
        self.sign_at(payload, key, token, chrono::Utc::now())
    }

    fn sign_at(
        &self,
        payload: Value,
        key: &SigningKey,
        token: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> SealEnvelope {
        let canonical = serde_json::to_vec(&json!({
            "payload": payload,
            "security_token": token,
            "timestamp": now.timestamp(),
        }))
        .unwrap();
        SealEnvelope {
            protocol: "seal/v1".to_string(),
            security_token: token.into(),
            signature: STANDARD.encode(key.sign(&canonical).to_bytes()),
            payload,
            timestamp: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }
    }
}

/// Attest as the Zaru MCP server attests for an `aegis_*` key: no `user_id`
/// in the body, the key the only identity.
async fn attest_as_the_mcp_server() -> Mcp {
    attest_with(zaru_pro(), |service, _| service).await
}

/// As [`attest_as_the_mcp_server`], with the session's context and the
/// service as the test configures them; `configure` receives the FSAL.
async fn attest_with(
    context: SecurityContext,
    configure: impl FnOnce(ToolInvocationService, Arc<AegisFSAL>) -> ToolInvocationService,
) -> Mcp {
    attest_through(context, Arc::new(SealMiddleware::new()), configure).await
}

/// As [`attest_with`], the service verifying envelopes with `middleware`.
async fn attest_through(
    context: SecurityContext,
    middleware: Arc<SealMiddleware>,
    configure: impl FnOnce(ToolInvocationService, Arc<AegisFSAL>) -> ToolInvocationService,
) -> Mcp {
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(context).await.unwrap();
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let issuer =
        Arc::new(SecurityTokenIssuer::new(&seal_signing_pem(), "aegis-orchestrator").unwrap());
    let attestation = AttestationServiceImpl::new(contexts.clone(), sessions.clone(), issuer);

    let key = SigningKey::from_bytes(&[42u8; 32]);
    let request = mcp_body(&STANDARD.encode(key.verifying_key().as_bytes()), json!({}));
    let caller = key_caller().await;
    let binding = attest_binding(no_lookup(), Some(&caller), &request)
        .await
        .expect("the key attests");
    // The route's mapping of the body and the binding (`attest_seal_handler`).
    let internal = attestation_request(&request, &binding);
    assert_eq!(binding.realm, RealmKind::Consumer);
    let attested = attestation
        .attest_with_escalation(internal, binding.escalation)
        .await
        .expect("attestation succeeds");

    let router = Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers()));
    let storage_root =
        std::env::temp_dir().join(format!("aegis-attest-identity-{}", uuid::Uuid::new_v4()));
    let volumes = Arc::new(InMemoryVolumeRepository::new());
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        volumes.clone(),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let event_bus = Arc::new(EventBus::new(256));
    let executions = Arc::new(Executions::default());
    let service = ToolInvocationService::new(
        sessions,
        contexts,
        middleware,
        router,
        fsal.clone(),
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(AgentId::new())),
        executions.clone(),
        Arc::new(ReqwestWebToolAdapter::unconfigured()),
        event_bus.clone(),
        None,
    )
    .with_goals(Arc::new(GoalService::new(
        Arc::new(InMemoryGoalRepository::new()),
        event_bus,
        GoalsConfig::default(),
    )));
    let service = configure(service, fsal);
    Mcp {
        service,
        token: attested.security_token.expose().to_string(),
        key,
        calls: 0,
        volumes,
        storage_root,
        executions,
    }
}

/// The defect of 2026-10-04 (production's Worker log at 10:05:08Z:
/// "aegis.goal.create failed: MCP error -32603: AEGIS invoke failed: 400"):
/// a session the MCP server attested for a key carried no user, so the goal
/// tools refused it. Attested as the MCP server attests, the key's session
/// creates a goal, evaluates it and reads it.
#[tokio::test]
async fn an_api_key_attested_as_the_mcp_server_does_creates_evaluates_and_reads_a_goal() {
    let mut mcp = attest_as_the_mcp_server().await;

    let created = mcp
        .call(
            "aegis.goal.create",
            json!({
                "statement": "Create palindrome-checker, then run it on \"racecar\".",
                "client_ref": "conversation-1",
                "channel": "api",
            }),
        )
        .await
        .expect("aegis.goal.create answers");
    let goal_id = created["goal_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a goal_id in {created}"))
        .to_string();

    let evaluated = mcp
        .call(
            "aegis.goal.evaluate",
            json!({"goal_id": goal_id, "companion_answer": "Done."}),
        )
        .await
        .expect("aegis.goal.evaluate answers");
    assert_eq!(evaluated["goal_id"], goal_id, "{evaluated}");
    assert!(evaluated.get("error").is_none(), "{evaluated}");

    let status = mcp
        .call("aegis.goal.status", json!({"goal_id": goal_id}))
        .await
        .expect("aegis.goal.status answers");
    assert_eq!(status["goal_id"], goal_id, "{status}");
    assert_eq!(
        status["statement"], "Create palindrome-checker, then run it on \"racecar\".",
        "{status}"
    );
    assert_eq!(status["channel"], "api", "{status}");
}

// ============================================================================
// What the tool invoke route tells its caller (AEGIS ADR-035, Update of
// 2026-10-04, R1 to R5): ADR-035's error shape, a 4xx with a stable code and
// the caller's own business for a refusal, a 5xx with a fixed message for an
// internal failure whose detail is only in the log under the body's
// request_id.
// ============================================================================

use aegis_orchestrator_core::application::file_operations_service::FileOperationsService;
use aegis_orchestrator_core::domain::repository::VolumeRepository;
use aegis_orchestrator_core::domain::seal_session::{
    CallerAnswer, InternalFailure, SealSessionError,
};
use aegis_orchestrator_core::domain::security_context::PolicyViolation;
use aegis_orchestrator_core::domain::volume::{
    StorageClass, Volume, VolumeBackend, VolumeOwnership,
};

#[derive(Clone, Default)]
struct Captured(Arc<StdMutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The route's answer to one refusal, and the log lines it wrote.
struct Answer {
    status: u16,
    headers: axum::http::HeaderMap,
    body: Value,
    log: String,
}

impl Answer {
    fn request_id(&self) -> String {
        self.body["request_id"]
            .as_str()
            .unwrap_or_else(|| panic!("a request_id in {}", self.body))
            .to_string()
    }

    /// ADR-035's members, the status and the code.
    fn assert_shape(&self, status: u16, code: &str, body_status: &str) {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(self.body["protocol"], "seal/v1", "{}", self.body);
        assert_eq!(self.body["status"], body_status, "{}", self.body);
        assert_eq!(self.body["error"]["code"], code, "{}", self.body);
        assert!(self.body["error"].get("context").is_some(), "{}", self.body);
        assert!(self.body["error"].get("tool").is_some(), "{}", self.body);
        assert!(
            uuid::Uuid::parse_str(&self.request_id()).is_ok(),
            "{}",
            self.body
        );
        assert!(
            self.log.contains(&self.request_id()),
            "the log line does not carry the body's request_id {}:\n{}",
            self.request_id(),
            self.log
        );
    }

    fn message(&self) -> String {
        self.body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    /// None of `detail` reaches the body; all of it reaches the log.
    fn assert_detail_only_in_log(&self, detail: &[&str]) {
        let body = self.body.to_string();
        for d in detail {
            assert!(!body.contains(d), "the body carried {d:?}: {body}");
            assert!(
                self.log.contains(d),
                "the log does not carry {d:?}:\n{}",
                self.log
            );
        }
    }
}

/// The route's answer to `error` for a call to `tool`, with the log the
/// answer wrote captured.
async fn answer(error: &SealSessionError, tool: &str) -> Answer {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {"name": tool, "arguments": {}},
    });
    let response = tracing::subscriber::with_default(subscriber, || {
        super::invoke_refusal_response(error, &payload)
    });
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("a JSON body: {}", String::from_utf8_lossy(&bytes)));
    let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    Answer {
        status,
        headers,
        body,
        log,
    }
}

impl Mcp {
    /// One call through the service, refused, and the route's answer to it.
    async fn refused(&mut self, tool: &str, arguments: Value) -> Answer {
        let envelope = self.envelope(tool, arguments);
        let error = self
            .service
            .invoke_tool(&envelope)
            .await
            .expect_err("the call is refused");
        answer(&error, tool).await
    }

    /// A persistent volume of `owner` in `tenant`, on the host backend whose
    /// path names `backend_path`.
    async fn volume(&self, tenant: &TenantId, owner: &str, backend_path: &str) -> String {
        let mut volume = Volume::new(
            "files".to_string(),
            tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: backend_path.into(),
            },
            1024 * 1024,
            VolumeOwnership::persistent(owner),
        )
        .unwrap();
        volume.mark_available().unwrap();
        self.volumes.save(&volume).await.unwrap();
        volume.id.to_string()
    }
}

fn own_tenant() -> TenantId {
    TenantId::for_consumer_user(SUB).unwrap()
}

/// A storage backend path, as `fsal.rs` 372-380 routes a host volume's file.
const BACKEND: &str = "/aegis-host/Mk7-backend-volume";

async fn with_files() -> Mcp {
    attest_with(
        zaru_pro_allowing(&["aegis.goal.*", "aegis.file.*"]),
        |service, fsal| {
            service.with_file_operations_service(Arc::new(FileOperationsService::new(
                fsal,
                Arc::new(aegis_orchestrator_core::infrastructure::repositories::InMemoryExecutionRepository::new()),
                Arc::new(aegis_orchestrator_core::infrastructure::repositories::InMemoryAgentRepository::new()),
            )))
        },
    )
    .await
}

// ---- caller-facing refusals, through the service ---------------------------

#[tokio::test]
async fn a_tool_the_context_does_not_permit_is_403_tool_not_allowed_in_675984dc_words() {
    let mut mcp = attest_as_the_mcp_server().await;
    let a = mcp.refused("aegis.system.info", json!({})).await;
    a.assert_shape(403, "TOOL_NOT_ALLOWED", "policy_violation");
    assert_eq!(
        a.message(),
        "Policy violation: tool 'aegis.system.info' is not allowed; permitted tools: [aegis.goal.*]"
    );
    assert_eq!(a.body["error"]["tool"], "aegis.system.info");
}

#[tokio::test]
async fn missing_arguments_are_422_invalid_arguments_saying_which() {
    let mut mcp = attest_as_the_mcp_server().await;
    let a = mcp.refused("aegis.goal.status", json!({})).await;
    a.assert_shape(422, "INVALID_ARGUMENTS", "error");
    assert!(a.message().contains("goal_id"), "{}", a.body);
}

#[tokio::test]
async fn an_unknown_session_is_401_session_inactive() {
    let mut mcp = attest_as_the_mcp_server().await;
    mcp.token = "not-a-session-token".to_string();
    let a = mcp
        .refused("aegis.goal.status", json!({"goal_id": "x"}))
        .await;
    a.assert_shape(401, "SESSION_INACTIVE", "error");
}

#[tokio::test]
async fn a_signature_by_another_key_is_401_signature_invalid() {
    let mut mcp = attest_as_the_mcp_server().await;
    mcp.key = SigningKey::from_bytes(&[7u8; 32]);
    let a = mcp
        .refused("aegis.goal.status", json!({"goal_id": "x"}))
        .await;
    a.assert_shape(401, "SIGNATURE_INVALID", "error");
}

#[tokio::test]
async fn an_envelope_outside_the_freshness_window_is_401_envelope_replayed() {
    let mut mcp = attest_as_the_mcp_server().await;
    let (key, token) = (mcp.key.clone(), mcp.token.clone());
    let envelope = mcp.sign_at(
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "aegis.goal.status", "arguments": {}}}),
        &key,
        &token,
        chrono::Utc::now() - chrono::Duration::seconds(120),
    );
    let error = mcp
        .service
        .invoke_tool(&envelope)
        .await
        .expect_err("a stale envelope is refused");
    let a = answer(&error, "aegis.goal.status").await;
    a.assert_shape(401, "ENVELOPE_REPLAYED", "error");
    assert!(a.message().contains("freshness window"), "{}", a.body);
}

#[tokio::test]
async fn a_call_with_no_tool_name_is_400_malformed_envelope() {
    let mut mcp = attest_as_the_mcp_server().await;
    let (key, token) = (mcp.key.clone(), mcp.token.clone());
    let envelope = mcp.sign(
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}}),
        &key,
        &token,
    );
    let error = mcp
        .service
        .invoke_tool(&envelope)
        .await
        .expect_err("refused");
    let a = answer(&error, "").await;
    a.assert_shape(400, "MALFORMED_ENVELOPE", "error");
}

#[tokio::test]
async fn another_tenant_named_in_the_arguments_is_403_tenant_mismatch() {
    let mut mcp = with_files().await;
    let other = TenantId::for_consumer_user(SOMEONE_ELSE).unwrap();
    let a = mcp
        .refused(
            "aegis.file.read",
            json!({"tenant_id": other.as_str(), "volume_id": uuid::Uuid::new_v4().to_string(), "path": "a.txt"}),
        )
        .await;
    a.assert_shape(403, "TENANT_MISMATCH", "error");
}

/// I5: a file of the caller's that does not exist is named by the caller's
/// own path, never by the storage backend's.
#[tokio::test]
async fn a_missing_file_is_404_named_by_the_callers_path_not_the_backend_path() {
    let mut mcp = with_files().await;
    let volume = mcp.volume(&own_tenant(), SUB, BACKEND).await;
    let a = mcp
        .refused(
            "aegis.file.read",
            json!({"volume_id": volume, "path": "notes/missing.txt"}),
        )
        .await;
    a.assert_shape(404, "NOT_FOUND", "error");
    assert!(a.message().contains("notes/missing.txt"), "{}", a.body);
    a.assert_detail_only_in_log(&["Mk7-backend-volume"]);
}

/// I5 and "another tenant's identifier": a volume of another tenant is not
/// found, named by the id the caller sent.
#[tokio::test]
async fn another_tenants_volume_is_404_and_names_nothing_of_that_tenant() {
    let mut mcp = with_files().await;
    let other = TenantId::for_consumer_user(SOMEONE_ELSE).unwrap();
    let volume = mcp.volume(&other, SOMEONE_ELSE, BACKEND).await;
    let a = mcp
        .refused(
            "aegis.file.read",
            json!({"volume_id": volume, "path": "a.txt"}),
        )
        .await;
    a.assert_shape(404, "NOT_FOUND", "error");
    assert!(a.message().contains(&volume), "{}", a.body);
    let body = a.body.to_string();
    assert!(!body.contains(SOMEONE_ELSE), "{body}");
    assert!(!body.contains(other.as_str()), "{body}");
    assert!(!body.contains("Mk7-backend-volume"), "{body}");
}

/// I6: a tool the node does not have is named as the caller named it; the
/// node's list of tools is not in the answer.
#[tokio::test]
async fn an_unknown_tool_is_404_and_the_nodes_tools_are_not_listed() {
    let mut mcp = attest_as_the_mcp_server().await;
    let a = mcp.refused("aegis.goal.no-such-tool", json!({})).await;
    a.assert_shape(404, "NOT_FOUND", "error");
    assert!(
        a.message().contains("aegis.goal.no-such-tool"),
        "{}",
        a.body
    );
    assert!(!a.body.to_string().contains("Available"), "{}", a.body);
    assert!(
        a.log.contains("Tool not found: aegis.goal.no-such-tool"),
        "{}",
        a.log
    );
}

// ---- caller-facing refusals the service cannot be driven to here -----------

#[tokio::test]
async fn every_other_caller_facing_row_has_its_status_code_and_message() {
    let rows: Vec<(SealSessionError, u16, &str, &str, &str)> = vec![
        (
            SealSessionError::SessionExpired,
            401,
            "SESSION_EXPIRED",
            "error",
            "expired",
        ),
        (
            SealSessionError::OperatorEscalationExpired,
            401,
            "OPERATOR_ESCALATION_EXPIRED",
            "error",
            "escalation has ended",
        ),
        (
            SealSessionError::NotFound("approval request 9".into()),
            404,
            "NOT_FOUND",
            "error",
            "approval request 9",
        ),
        (
            SealSessionError::InternalError("x".into()).answered(CallerAnswer::Conflict(
                "a volume named 'files' already exists".into(),
            )),
            409,
            "CONFLICT",
            "error",
            "already exists",
        ),
        (
            SealSessionError::InternalError("x".into()).answered(CallerAnswer::QuotaExceeded(
                "storage quota exceeded for tier".into(),
            )),
            422,
            "QUOTA_EXCEEDED",
            "error",
            "quota",
        ),
        (
            SealSessionError::InternalError("x".into()).answered(CallerAnswer::JudgeRejected(
                "rejected by the semantic judge".into(),
            )),
            403,
            "JUDGE_REJECTED",
            "policy_violation",
            "semantic judge",
        ),
        (
            SealSessionError::InternalError("x".into()).answered(CallerAnswer::NotImplemented(
                "not yet implemented: push".into(),
            )),
            501,
            "NOT_IMPLEMENTED",
            "error",
            "push",
        ),
        (
            SealSessionError::InternalError("x".into())
                .answered(CallerAnswer::EdgeUnavailable("edge n-1 unavailable".into())),
            503,
            "EDGE_UNAVAILABLE",
            "error",
            "edge n-1",
        ),
    ];
    for (error, status, code, body_status, words) in rows {
        let a = answer(&error, "aegis.some.tool").await;
        a.assert_shape(status, code, body_status);
        assert!(a.message().contains(words), "{error:?}: {}", a.body);
    }
}

/// ADR-072 §9: a rate-limit refusal is 429 with Retry-After and the
/// X-RateLimit-* headers.
#[tokio::test]
async fn a_rate_limit_is_429_with_adr_072s_headers() {
    let error = SealSessionError::PolicyViolation(PolicyViolation::RateLimitExceeded {
        resource_type: "tool_call".into(),
        bucket: "per_minute".into(),
        limit: 60,
        current: 61,
        retry_after_seconds: 12,
    });
    let a = answer(&error, "aegis.goal.create").await;
    a.assert_shape(429, "RATE_LIMIT_EXCEEDED", "policy_violation");
    assert_eq!(a.headers["retry-after"], "12");
    assert_eq!(a.headers["x-ratelimit-limit"], "60");
    assert_eq!(a.headers["x-ratelimit-remaining"], "0");
    assert!(a.headers.contains_key("x-ratelimit-reset"));
}

// ---- internal failures: a fixed message, the detail only in the log --------

/// I1: database and repository text (`facade.rs` 336-339, `repository.rs` 581).
#[tokio::test]
async fn a_database_failure_is_500_and_its_text_is_only_in_the_log() {
    let error = SealSessionError::InternalError(
        "session repository lookup failed: Database error: Mk7-db-relation \"seal_sessions\" does not exist"
            .into(),
    );
    let a = answer(&error, "aegis.goal.create").await;
    a.assert_shape(500, "INTERNAL_ERROR", "error");
    a.assert_detail_only_in_log(&["Database error", "Mk7-db-relation"]);
}

/// I2: a configuration key (`discovery.rs` 25), driven through the service.
#[tokio::test]
async fn an_unconfigured_tool_is_503_and_its_configuration_key_is_only_in_the_log() {
    let mut mcp = attest_with(
        zaru_pro_allowing(&["aegis.goal.*", "aegis.agent.search"]),
        |service, _| service,
    )
    .await;
    let a = mcp
        .refused("aegis.agent.search", json!({"query": "summarise"}))
        .await;
    a.assert_shape(503, "SERVICE_UNAVAILABLE", "error");
    a.assert_detail_only_in_log(&["spec.discovery", "aegis-config.yaml"]);
}

/// I2: the operator escalation's V5 refusal (`facade.rs` 474-476) names a
/// node configuration key.
#[tokio::test]
async fn the_escalation_check_failure_is_500_and_its_key_is_only_in_the_log() {
    let error = SealSessionError::InternalError(
        "operator escalation check failed: spec.iam.keycloak_admin is not configured on this node"
            .into(),
    );
    let a = answer(&error, "aegis.task.list").await;
    a.assert_shape(500, "INTERNAL_ERROR", "error");
    a.assert_detail_only_in_log(&["spec.iam.keycloak_admin"]);
}

/// I3: upstream and gateway transport text (`web_tools.rs`, `gateway.rs`).
/// An upstream failure is 503 `UPSTREAM_UNAVAILABLE` with `Retry-After: 5`
/// and its fixed message, so an edge in front of the route that replaces a
/// 502's body passes it through; `SERVICE_UNAVAILABLE` and `INTERNAL_ERROR`
/// keep their status and carry no `Retry-After`. Every row is checked before
/// the test fails, so one run names each wrong one.
#[tokio::test]
async fn an_upstream_failure_is_503_with_retry_after_and_its_text_is_only_in_the_log() {
    let upstream = answer(
        &SealSessionError::UpstreamUnavailable(
            "web.search Brave API returned 429: Mk7-upstream-body".into(),
        ),
        "web.search",
    )
    .await;
    let unavailable = answer(
        &SealSessionError::ConfigurationError("seal_gateway.url is not configured".into()),
        "some.tool",
    )
    .await;
    let gateway = answer(
        &SealSessionError::InternalError(
            "seal tooling gateway connect failed (http://10.9.8.7:50055): Mk7-transport".into(),
        ),
        "some.tool",
    )
    .await;
    let mut wrong = Vec::new();
    for (row, a, status, code, retry_after) in [
        (
            "an upstream failure",
            &upstream,
            503,
            "UPSTREAM_UNAVAILABLE",
            Some("5"),
        ),
        (
            "an unconfigured tool",
            &unavailable,
            503,
            "SERVICE_UNAVAILABLE",
            None,
        ),
        (
            "a gateway transport failure",
            &gateway,
            500,
            "INTERNAL_ERROR",
            None,
        ),
    ] {
        let seen = (
            a.status,
            a.body["error"]["code"].as_str().unwrap_or("").to_string(),
            a.headers
                .get("retry-after")
                .map(|v| v.to_str().unwrap_or("<not text>").to_string()),
        );
        let expected = (status, code.to_string(), retry_after.map(str::to_string));
        if seen != expected {
            wrong.push(format!(
                "{row} was answered (status, code, Retry-After) {seen:?}, expected {expected:?}"
            ));
        }
    }
    if upstream.message()
        != aegis_orchestrator_core::domain::seal_session::UPSTREAM_UNAVAILABLE_MESSAGE
    {
        wrong.push(format!(
            "an upstream failure's message changed: {}",
            upstream.message()
        ));
    }
    assert!(
        wrong.is_empty(),
        "the route answered an R5 row wrongly:\n{}",
        wrong.join("\n")
    );
    upstream.assert_shape(503, "UPSTREAM_UNAVAILABLE", "error");
    upstream.assert_detail_only_in_log(&["Brave", "Mk7-upstream-body"]);
    unavailable.assert_shape(503, "SERVICE_UNAVAILABLE", "error");
    unavailable.assert_detail_only_in_log(&["seal_gateway.url"]);
    gateway.assert_shape(500, "INTERNAL_ERROR", "error");
    gateway.assert_detail_only_in_log(&["10.9.8.7", "Mk7-transport"]);
}

/// I4: platform state and internal ids (`audit.rs` 381, `facade.rs` 563).
#[tokio::test]
async fn a_platform_state_failure_is_500_and_its_ids_are_only_in_the_log() {
    let execution = uuid::Uuid::new_v4().to_string();
    let error = SealSessionError::MalformedPayload(format!(
        "Failed to load execution {execution}: Database error: Mk7-row"
    ))
    .answered(CallerAnswer::Internal(InternalFailure::Server));
    let a = answer(&error, "aegis.task.status").await;
    a.assert_shape(500, "INTERNAL_ERROR", "error");
    a.assert_detail_only_in_log(&[execution.as_str(), "Mk7-row"]);
}

/// I5: a storage failure the file service reports as `FileOperationsError::Fsal`
/// is 500 and its text is only in the log.
#[tokio::test]
async fn a_storage_backend_failure_is_500_and_its_text_is_only_in_the_log() {
    let mut mcp = with_files().await;
    let volume = mcp.volume(&own_tenant(), SUB, BACKEND).await;
    let a = mcp
        .refused(
            "aegis.file.list",
            json!({"volume_id": volume, "path": "no-such-directory"}),
        )
        .await;
    a.assert_shape(500, "INTERNAL_ERROR", "error");
    a.assert_detail_only_in_log(&["readdir failed"]);
}

// ============================================================================
// Existence is not told across tenants (AEGIS ADR-035 R5's NOT_FOUND row:
// "including one of another owner or tenant (existence is not told)"):
// `aegis.execution.file`, `aegis.attachment.read` and `aegis.file.*` answer a
// resource of another tenant exactly as they answer one that does not exist.
// The owner's own read is unchanged.
// ============================================================================

/// A session allowed the three tools, with the file service configured.
async fn with_file_reads() -> Mcp {
    with_file_reads_over(
        Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::InMemoryExecutionRepository::new(
            ),
        ),
        Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::InMemoryAgentRepository::new(),
        ),
    )
    .await
}

/// As [`with_file_reads`], the file service reading execution records and
/// agents from these stores (AEGIS ADR-005 I8).
async fn with_file_reads_over(
    executions: Arc<
        aegis_orchestrator_core::infrastructure::repositories::InMemoryExecutionRepository,
    >,
    agents: Arc<aegis_orchestrator_core::infrastructure::repositories::InMemoryAgentRepository>,
) -> Mcp {
    attest_with(
        zaru_pro_allowing(&["aegis.execution.*", "aegis.attachment.*", "aegis.file.*"]),
        |service, fsal| {
            service.with_file_operations_service(Arc::new(FileOperationsService::new(
                fsal, executions, agents,
            )))
        },
    )
    .await
}

/// A completed execution `execution` of `tenant`, whose agent mounts its
/// volume named "workspace" at `/workspace`: the record the route reads
/// before it reads a file (AEGIS ADR-005 I8, P2).
async fn execution_record(
    executions: &aegis_orchestrator_core::infrastructure::repositories::InMemoryExecutionRepository,
    agents: &aegis_orchestrator_core::infrastructure::repositories::InMemoryAgentRepository,
    tenant: &TenantId,
    execution: uuid::Uuid,
) {
    use aegis_orchestrator_core::domain::repository::{AgentRepository, ExecutionRepository};
    let manifest: AgentManifest = serde_yaml::from_str(
        "apiVersion: aegis.ai/v1\nkind: Agent\nmetadata:\n  name: file-agent\n  \
         version: \"1.0.0\"\nspec:\n  runtime:\n    language: python\n    \
         version: \"3.11\"\n    model: smart\n  volumes:\n    - name: workspace\n      \
         storage_class: persistent\n      mount_path: /workspace\n      \
         access_mode: read-write\n      size_limit: 1Gi\n",
    )
    .unwrap();
    let agent = Agent::new(manifest);
    agents.save_for_tenant(tenant, &agent).await.unwrap();
    let mut record = Execution::new(
        agent.id,
        ExecutionInput {
            intent: None,
            input: json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        },
        1,
        "ctx".to_string(),
    );
    record.id = ExecutionId(execution);
    record.tenant_id = tenant.clone();
    record.start();
    record.complete();
    executions.save_for_tenant(tenant, &record).await.unwrap();
}

impl Mcp {
    /// The workspace volume of `execution` in `tenant`, on a host backend at
    /// `backend_path`.
    async fn execution_volume(&self, tenant: &TenantId, execution: uuid::Uuid, backend_path: &str) {
        let mut volume = Volume::new(
            "workspace".to_string(),
            tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: backend_path.into(),
            },
            1024 * 1024,
            VolumeOwnership::execution(SharedExecutionId(execution)),
        )
        .unwrap();
        volume.mark_available().unwrap();
        self.volumes.save(&volume).await.unwrap();
    }

    /// A persistent volume with this exact id, of `owner` in `tenant`.
    async fn volume_with_id(&self, id: &str, tenant: &TenantId, owner: &str, backend_path: &str) {
        let mut volume = Volume::new(
            "files".to_string(),
            tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: backend_path.into(),
            },
            1024 * 1024,
            VolumeOwnership::persistent(owner),
        )
        .unwrap();
        volume.id =
            aegis_orchestrator_core::domain::volume::VolumeId(uuid::Uuid::parse_str(id).unwrap());
        volume.mark_available().unwrap();
        self.volumes.save(&volume).await.unwrap();
    }

    /// Write `content` as the file `name` of the host volume at `backend_path`.
    fn put_file(&self, backend_path: &str, name: &str, content: &str) {
        let dir = self.storage_root.join(backend_path.trim_start_matches('/'));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }
}

/// The route's answer with its per-call `request_id` (R2: minted for every
/// refused call, so two calls never share one) set aside; everything else,
/// status, headers and body, is compared byte for byte.
fn without_request_id(a: &Answer) -> (u16, Vec<(String, Vec<u8>)>, String) {
    let mut body = a.body.clone();
    body["request_id"] = Value::Null;
    let mut headers: Vec<(String, Vec<u8>)> = a
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
        .collect();
    headers.sort();
    (a.status, headers, body.to_string())
}

#[tokio::test]
async fn an_execution_file_of_another_tenant_answers_byte_for_byte_as_a_missing_one() {
    let execution = uuid::Uuid::new_v4();
    let args = json!({"execution_id": execution.to_string(), "path": "output.md"});
    let other = TenantId::for_consumer_user(SOMEONE_ELSE).unwrap();

    let executions = Arc::new(
        aegis_orchestrator_core::infrastructure::repositories::InMemoryExecutionRepository::new(),
    );
    let agents = Arc::new(
        aegis_orchestrator_core::infrastructure::repositories::InMemoryAgentRepository::new(),
    );
    execution_record(&executions, &agents, &other, execution).await;
    let mut foreign = with_file_reads_over(executions, agents).await;
    foreign
        .execution_volume(&other, execution, "/aegis-host/other-exec")
        .await;
    foreign.put_file("/aegis-host/other-exec", "output.md", "theirs");
    let theirs = foreign
        .call("aegis.execution.file", args.clone())
        .await
        .expect("answered");

    let mut missing = with_file_reads().await;
    let none = missing
        .call("aegis.execution.file", args)
        .await
        .expect("answered");

    assert_eq!(
        serde_json::to_vec(&theirs).unwrap(),
        serde_json::to_vec(&none).unwrap(),
        "another tenant's execution: {theirs}\nno such execution: {none}"
    );
    assert!(!theirs.to_string().contains("theirs"), "{theirs}");
}

#[tokio::test]
async fn an_execution_file_of_the_callers_own_execution_is_read_as_before() {
    let execution = uuid::Uuid::new_v4();
    let executions = Arc::new(
        aegis_orchestrator_core::infrastructure::repositories::InMemoryExecutionRepository::new(),
    );
    let agents = Arc::new(
        aegis_orchestrator_core::infrastructure::repositories::InMemoryAgentRepository::new(),
    );
    execution_record(&executions, &agents, &own_tenant(), execution).await;
    let mut mcp = with_file_reads_over(executions, agents).await;
    mcp.execution_volume(&own_tenant(), execution, "/aegis-host/own-exec")
        .await;
    mcp.put_file("/aegis-host/own-exec", "output.md", "mine");
    let read = mcp
        .call(
            "aegis.execution.file",
            json!({"execution_id": execution.to_string(), "path": "output.md"}),
        )
        .await
        .expect("answered");
    assert_eq!(read["status"], "success", "{read}");
    assert_eq!(read["content"], "mine", "{read}");
}

#[tokio::test]
async fn an_attachment_of_another_tenant_answers_byte_for_byte_as_a_missing_one() {
    let volume = uuid::Uuid::new_v4().to_string();
    let args = json!({"volume_id": volume, "path": "upload.txt"});
    let other = TenantId::for_consumer_user(SOMEONE_ELSE).unwrap();

    let mut foreign = with_file_reads().await;
    foreign
        .volume_with_id(&volume, &other, SOMEONE_ELSE, "/aegis-host/other-files")
        .await;
    foreign.put_file("/aegis-host/other-files", "upload.txt", "theirs");
    let theirs = foreign
        .call("aegis.attachment.read", args.clone())
        .await
        .expect("answered");

    let mut missing = with_file_reads().await;
    let none = missing
        .call("aegis.attachment.read", args)
        .await
        .expect("answered");

    assert_eq!(
        serde_json::to_vec(&theirs).unwrap(),
        serde_json::to_vec(&none).unwrap(),
        "another tenant's volume: {theirs}\nno such volume: {none}"
    );
    assert!(!theirs.to_string().contains("theirs"), "{theirs}");
}

#[tokio::test]
async fn an_attachment_in_the_callers_own_volume_is_read_as_before() {
    let mut mcp = with_file_reads().await;
    let volume = mcp
        .volume(&own_tenant(), SUB, "/aegis-host/own-files")
        .await;
    mcp.put_file("/aegis-host/own-files", "upload.txt", "mine");
    let read = mcp
        .call(
            "aegis.attachment.read",
            json!({"volume_id": volume, "path": "upload.txt"}),
        )
        .await
        .expect("answered");
    assert_eq!(read["status"], "success", "{read}");
    assert_eq!(read["content"], "mine", "{read}");
}

#[tokio::test]
async fn a_file_in_another_tenants_volume_answers_as_one_in_a_missing_volume() {
    let volume = uuid::Uuid::new_v4().to_string();
    let args = json!({"volume_id": volume, "path": "a.txt"});
    let other = TenantId::for_consumer_user(SOMEONE_ELSE).unwrap();

    let mut foreign = with_file_reads().await;
    foreign
        .volume_with_id(&volume, &other, SOMEONE_ELSE, "/aegis-host/other-vol")
        .await;
    foreign.put_file("/aegis-host/other-vol", "a.txt", "theirs");
    let (theirs, theirs_call_log) = logged(foreign.refused("aegis.file.read", args.clone())).await;

    let mut missing = with_file_reads().await;
    let (none, none_call_log) = logged(missing.refused("aegis.file.read", args)).await;

    theirs.assert_shape(404, "NOT_FOUND", "error");
    assert_eq!(
        without_request_id(&theirs),
        without_request_id(&none),
        "another tenant's volume: {}\nno such volume: {}",
        theirs.body,
        none.body
    );
    // The operator's log keeps the distinction; no caller-visible text does.
    let foreign_line = "volume of another tenant or owner; answered as not found";
    assert!(theirs_call_log.contains(foreign_line), "{theirs_call_log}");
    assert!(!none_call_log.contains(foreign_line), "{none_call_log}");
}

#[tokio::test]
async fn a_file_in_the_callers_own_volume_is_read_as_before() {
    let mut mcp = with_file_reads().await;
    let volume = mcp.volume(&own_tenant(), SUB, "/aegis-host/own-vol").await;
    mcp.put_file("/aegis-host/own-vol", "a.txt", "mine");
    let read = mcp
        .call(
            "aegis.file.read",
            json!({"volume_id": volume, "path": "a.txt"}),
        )
        .await
        .expect("answered");
    assert!(read.to_string().contains("mine"), "{read}");
}

/// The log lines written while `call` runs, with its result.
async fn logged<T>(call: impl std::future::Future<Output = T>) -> (T, String) {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let out = call.await;
    drop(guard);
    let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    (out, log)
}

impl Mcp {
    /// One tool call as an agent's iteration makes it: through
    /// `invoke_tool_internal`, the inner loop's path, in an execution of the
    /// caller's own tenant run under the session's security context.
    async fn as_agent(
        &self,
        tool: &str,
        arguments: Value,
    ) -> std::result::Result<
        aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationResult,
        SealSessionError,
    > {
        let agent = AgentId::new();
        let id = ExecutionId::new();
        let mut execution = Execution::new_with_id(
            id,
            agent,
            ExecutionInput {
                intent: None,
                input: json!({}),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: vec![],
            },
            5,
            "zaru-pro".to_string(),
        );
        execution.tenant_id = own_tenant();
        execution.initiating_user_sub = Some(SUB.to_string());
        self.executions
            .started
            .lock()
            .unwrap()
            .insert(id, execution);
        self.service
            .invoke_tool_internal(
                &agent,
                id,
                own_tenant(),
                1,
                Vec::new(),
                tool.to_string(),
                arguments,
            )
            .await
    }
}

/// The body of a direct answer.
fn direct(
    result: std::result::Result<
        aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationResult,
        SealSessionError,
    >,
) -> Value {
    match result {
        Ok(aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationResult::Direct(v)) => v,
        other => panic!("a direct answer, got {other:?}"),
    }
}

/// Through the inner loop's path an agent reading another tenant's volume
/// gets the very error it gets for a volume that does not exist: the same
/// text ("Tool execution error: <it>") and the same class, since the class
/// is the error's variant (AEGIS ADR-035 Update T1).
#[tokio::test]
async fn through_the_inner_loop_a_file_in_another_tenants_volume_reads_as_one_in_a_missing_volume()
{
    let volume = uuid::Uuid::new_v4().to_string();
    let args = json!({"volume_id": volume, "path": "a.txt"});
    let other = TenantId::for_consumer_user(SOMEONE_ELSE).unwrap();

    let foreign = with_file_reads().await;
    foreign
        .volume_with_id(&volume, &other, SOMEONE_ELSE, "/aegis-host/other-vol")
        .await;
    foreign.put_file("/aegis-host/other-vol", "a.txt", "theirs");
    let theirs = foreign
        .as_agent("aegis.file.read", args.clone())
        .await
        .expect_err("refused");

    let missing = with_file_reads().await;
    let none = missing
        .as_agent("aegis.file.read", args)
        .await
        .expect_err("refused");

    assert_eq!(theirs.to_string(), none.to_string());
    assert_eq!(theirs, none);
}

#[tokio::test]
async fn through_the_inner_loop_an_execution_file_of_another_tenant_reads_as_a_missing_one() {
    let execution = uuid::Uuid::new_v4();
    let args = json!({"execution_id": execution.to_string(), "path": "output.md"});
    let other = TenantId::for_consumer_user(SOMEONE_ELSE).unwrap();

    let foreign = with_file_reads().await;
    foreign
        .execution_volume(&other, execution, "/aegis-host/other-exec")
        .await;
    foreign.put_file("/aegis-host/other-exec", "output.md", "theirs");
    let theirs = direct(foreign.as_agent("aegis.execution.file", args.clone()).await);

    let missing = with_file_reads().await;
    let none = direct(missing.as_agent("aegis.execution.file", args).await);

    assert_eq!(
        serde_json::to_vec(&theirs).unwrap(),
        serde_json::to_vec(&none).unwrap(),
        "{theirs}\n{none}"
    );
}

#[tokio::test]
async fn through_the_inner_loop_an_attachment_of_another_tenant_reads_as_a_missing_one() {
    let volume = uuid::Uuid::new_v4().to_string();
    let args = json!({"volume_id": volume, "path": "upload.txt"});
    let other = TenantId::for_consumer_user(SOMEONE_ELSE).unwrap();

    let foreign = with_file_reads().await;
    foreign
        .volume_with_id(&volume, &other, SOMEONE_ELSE, "/aegis-host/other-files")
        .await;
    foreign.put_file("/aegis-host/other-files", "upload.txt", "theirs");
    let theirs = direct(
        foreign
            .as_agent("aegis.attachment.read", args.clone())
            .await,
    );

    let missing = with_file_reads().await;
    let none = direct(missing.as_agent("aegis.attachment.read", args).await);

    assert_eq!(
        serde_json::to_vec(&theirs).unwrap(),
        serde_json::to_vec(&none).unwrap(),
        "{theirs}\n{none}"
    );
}

/// The owner-side change of `255f4737`, pinned: a volume of the caller's own
/// tenant that is not persistent holds no attachment, and is answered as a
/// volume that does not exist (`read_attachment_for_tenant`'s own comment).
#[tokio::test]
async fn an_attachment_in_the_callers_own_non_persistent_volume_answers_not_found() {
    let mut mcp = with_file_reads().await;
    let mut volume = Volume::new(
        "workspace".to_string(),
        own_tenant(),
        StorageClass::persistent(),
        VolumeBackend::HostPath {
            path: "/aegis-host/own-ephemeral".into(),
        },
        1024 * 1024,
        VolumeOwnership::execution(SharedExecutionId(uuid::Uuid::new_v4())),
    )
    .unwrap();
    volume.mark_available().unwrap();
    mcp.volumes.save(&volume).await.unwrap();
    mcp.put_file("/aegis-host/own-ephemeral", "upload.txt", "mine");
    let id = volume.id.to_string();
    let read = mcp
        .call(
            "aegis.attachment.read",
            json!({"volume_id": id, "path": "upload.txt"}),
        )
        .await
        .expect("answered");
    assert_eq!(
        read,
        json!({"status": "error", "error": "not_found", "message": format!("volume {id} not found")}),
    );
}

// ── What a caller may attest: the context and the tier are its identity's ──
//
// The audit `seal-conformance-audit` (2026-10-05, deviation 1, High): the
// attest route took the security context and the tier from the body, so a
// caller holding any valid consumer credential attested a context of a
// higher tier, and the tier's limits followed it. Each test below attests
// through the route's own binding (`attest_binding`), the route's own
// mapping (`attestation_request`) and the real attestation service, with
// every context production configures (`aegis-platform-deployment`
// `podman/pods/core/aegis-config.yaml`: the four `zaru-*` and
// `aegis-system-operator`).

const OTHER_KEY: &str = "aegis_attest-authority-key";

fn named_context(name: &str) -> SecurityContext {
    SecurityContext {
        name: name.to_string(),
        ..zaru_pro()
    }
}

/// What one attestation answered, and the session it saved.
struct Attested {
    result: Result<aegis_orchestrator_core::infrastructure::seal::attestation::AttestationResponse>,
    sessions: Arc<InMemorySealSessionRepository>,
}

impl Attested {
    /// The context and the tier the session was granted.
    async fn granted(&self) -> (String, Option<String>) {
        let response = self
            .result
            .as_ref()
            .unwrap_or_else(|e| panic!("attested: {e:#}"));
        let session = aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository::find_active_by_security_token(
            self.sessions.as_ref(),
            &response.security_token,
        )
        .await
        .unwrap()
        .expect("the attested session is saved");
        (
            session.security_context.name.clone(),
            session.zaru_tier.clone(),
        )
    }

    /// Refused, with nothing saved.
    fn refused(&self) -> &anyhow::Error {
        match &self.result {
            Ok(_) => panic!("attestation was granted"),
            Err(e) => e,
        }
    }
}

/// Attest as `caller` with `body`, through the route's binding and mapping.
async fn attest_as(caller: &AttestCaller, body: Value) -> Attested {
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    for name in [
        "zaru-free",
        "zaru-pro",
        "zaru-business",
        "zaru-enterprise",
        "aegis-system-operator",
    ] {
        contexts.save(named_context(name)).await.unwrap();
    }
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let issuer =
        Arc::new(SecurityTokenIssuer::new(&seal_signing_pem(), "aegis-orchestrator").unwrap());
    let attestation = AttestationServiceImpl::new(contexts, sessions.clone(), issuer);
    let request: HttpAttestationRequest =
        serde_json::from_value(body).expect("the caller's body parses");
    let binding = attest_binding(no_lookup(), Some(caller), &request)
        .await
        .expect("the caller is bound");
    let result = attestation
        .attest_with_escalation(attestation_request(&request, &binding), binding.escalation)
        .await;
    Attested { result, sessions }
}

/// An `aegis_*` key whose stored tier column is `tier` (`None`: NULL), as
/// `authenticate_attest_request` resolves it.
async fn key_with_tier(tier: Option<&str>) -> AttestCaller {
    let tenant = TenantId::for_consumer_user(SUB).unwrap();
    let mut row = key_row(OTHER_KEY, SUB, tenant.as_str(), None);
    row.zaru_tier = tier.map(str::to_string);
    let table = KeyTable {
        rows: vec![row],
        escalations: None,
    };
    let resolved = resolve_api_key(Some(&table), OTHER_KEY)
        .await
        .expect("the key resolves");
    AttestCaller {
        identity: resolved.identity,
        escalation: resolved
            .escalation
            .map(|escalation| (escalation, resolved.home_tenant)),
    }
}

/// A consumer whose verified token says `tier`, as the IAM service resolves it.
fn consumer_jwt(tier: ZaruTier) -> AttestCaller {
    let mut caller = jwt_caller(SUB);
    caller.identity.identity_kind = IdentityKind::ConsumerUser {
        zaru_tier: tier,
        tenant_id: TenantId::for_consumer_user(SUB).unwrap(),
    };
    caller
}

/// A key with no stored role holding an active operator escalation, as
/// `resolve_api_key` resolves it (AEGIS ADR-129 D14).
fn escalated_key() -> AttestCaller {
    let now = chrono::Utc::now();
    let escalation = aegis_orchestrator_core::domain::operator_escalation::OperatorEscalation {
        id: uuid::Uuid::new_v4(),
        api_key_id: uuid::Uuid::new_v4(),
        consumer_sub: SUB.to_string(),
        system_sub: "9f000000-system-sub".to_string(),
        aegis_role: aegis_orchestrator_core::domain::iam::AegisRole::Operator,
        code_id: uuid::Uuid::new_v4(),
        started_at: now,
        expires_at: now + chrono::Duration::minutes(30),
        ended_at: None,
        end_reason: None,
    };
    AttestCaller {
        identity: UserIdentity {
            sub: SUB.to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: aegis_orchestrator_core::domain::iam::AegisRole::Operator,
            },
        },
        escalation: Some((
            escalation,
            TenantId::for_consumer_user(SUB)
                .unwrap()
                .as_str()
                .to_string(),
        )),
    }
}

/// The Zaru MCP server's attest body (`aegis-mcp-tools` `688b587`,
/// `zaru-mcp-server/src/mcp/orchestrator-client.ts` 903-911) for a
/// consumer: the context `zaru-<tier>` and the tier it holds for the caller.
fn mcp_server_body(context: &str, tier: &str) -> Value {
    json!({
        "workload_id": format!("zaru:{SUB}:session-1"),
        "security_context": context,
        "zaru_tier": tier,
        "public_key": STANDARD.encode([7u8; 32]),
    })
}

/// Zaru Web's attest body (`zaru-client` `76ac76e`, `lib/seal/attestation.ts`
/// 30-56; the turn `lib/chat/turn.ts` 759-766 and the goal runner
/// `lib/cloudflare/durable-objects.ts` 281-288 call the same function).
fn zaru_web_body(tier: &str, context: &str) -> Value {
    json!({
        "agent_public_key": STANDARD.encode([9u8; 32]),
        "user_id": SUB,
        "zaru_tier": tier,
        "workload_id": format!("zaru-session-{SUB}-1"),
        "security_context": context,
    })
}

/// The finding: a free key's body asks for `zaru-enterprise` with
/// `zaru_tier: "enterprise"`, as the audit names it. The identity's tier is
/// free; the body is not trusted for the context or the tier.
#[tokio::test]
async fn a_free_key_asking_for_the_enterprise_context_is_refused() {
    let attested = attest_as(
        &key_with_tier(Some("free")).await,
        mcp_server_body("zaru-enterprise", "enterprise"),
    )
    .await;
    if let Ok(_) = &attested.result {
        let (context, tier) = attested.granted().await;
        panic!("a free key was granted {context} with tier {tier:?}");
    }
}

/// The same for a consumer whose verified token says free.
#[tokio::test]
async fn a_free_consumer_token_asking_for_a_higher_tier_is_refused() {
    for (context, tier) in [
        ("zaru-pro", "pro"),
        ("zaru-business", "business"),
        ("zaru-enterprise", "enterprise"),
    ] {
        let attested = attest_as(
            &consumer_jwt(ZaruTier::Free),
            mcp_server_body(context, tier),
        )
        .await;
        if let Ok(_) = &attested.result {
            let (granted, tier) = attested.granted().await;
            panic!("a free token was granted {granted} with tier {tier:?}");
        }
    }
}

/// The tier a session's rate limits follow is the identity's, whatever the
/// body says: a pro key naming its own context with `zaru_tier: "enterprise"`
/// is granted its context with its own tier.
#[tokio::test]
async fn the_body_tier_is_not_trusted() {
    let attested = attest_as(
        &key_with_tier(Some("pro")).await,
        mcp_server_body("zaru-pro", "enterprise"),
    )
    .await;
    assert_eq!(
        attested.granted().await,
        ("zaru-pro".to_string(), Some("pro".to_string()))
    );
}

/// Every caller that attests today, each with what it sends, granted
/// exactly what its verified identity entitles it to.
#[tokio::test]
async fn every_deployed_caller_is_granted_exactly_its_identitys_context_and_tier() {
    let pro = || Some("pro".to_string());
    let cases: Vec<(&str, AttestCaller, Value, (String, Option<String>))> = vec![
        (
            "Zaru MCP server, an API key whose row says pro",
            key_with_tier(Some("pro")).await,
            mcp_server_body("zaru-pro", "pro"),
            ("zaru-pro".to_string(), pro()),
        ),
        (
            "Zaru MCP server, an API key with no stored tier (free)",
            key_with_tier(None).await,
            mcp_server_body("zaru-free", "free"),
            ("zaru-free".to_string(), Some("free".to_string())),
        ),
        (
            "Zaru MCP server, an OAuth user whose token says business",
            consumer_jwt(ZaruTier::Business),
            mcp_server_body("zaru-business", "business"),
            ("zaru-business".to_string(), Some("business".to_string())),
        ),
        (
            "Zaru MCP server, a key holding an operator escalation",
            escalated_key(),
            json!({
                "workload_id": format!("zaru:{SUB}:session-1"),
                "security_context": "aegis-system-operator",
                "aegis_role": "operator",
                "public_key": STANDARD.encode([7u8; 32]),
            }),
            ("aegis-system-operator".to_string(), None),
        ),
        (
            "Zaru Web's turn and goal runner, a signed-in user whose token says enterprise",
            consumer_jwt(ZaruTier::Enterprise),
            zaru_web_body("enterprise", "zaru-enterprise"),
            (
                "zaru-enterprise".to_string(),
                Some("enterprise".to_string()),
            ),
        ),
        (
            "Zaru Web's turn and goal runner, a free user",
            consumer_jwt(ZaruTier::Free),
            zaru_web_body("free", "zaru-free"),
            ("zaru-free".to_string(), Some("free".to_string())),
        ),
        (
            "Zaru Web's turn and goal runner, an API key session whose row says pro",
            key_with_tier(Some("pro")).await,
            zaru_web_body("pro", "zaru-pro"),
            ("zaru-pro".to_string(), pro()),
        ),
        (
            "Zaru Web's turn and goal runner, a key holding an operator escalation",
            escalated_key(),
            zaru_web_body("pro", "aegis-system-operator"),
            ("aegis-system-operator".to_string(), None),
        ),
    ];
    for (who, caller, body, expected) in cases {
        let attested = attest_as(&caller, body).await;
        if let Err(e) = &attested.result {
            panic!("{who}: refused: {e:#}");
        }
        assert_eq!(attested.granted().await, expected, "{who}");
    }
}

/// Nothing above the identity's own context: another tier's context, the
/// operator context, an unknown name and a bare name are refused alike.
#[tokio::test]
async fn a_consumer_is_refused_every_context_but_its_own() {
    for context in [
        "zaru-free",
        "zaru-business",
        "zaru-enterprise",
        "zaru-nonexistent",
        "aegis-system-operator",
        "research-safe",
    ] {
        let attested = attest_as(
            &consumer_jwt(ZaruTier::Pro),
            mcp_server_body(context, "pro"),
        )
        .await;
        attested.refused();
    }
}

/// The route's answer to a failed attestation, and the log it wrote.
async fn attest_answer(error: &anyhow::Error) -> Answer {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let response = tracing::subscriber::with_default(subscriber, || {
        super::attest_failure_response(error, "u-tenant")
    });
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    Answer {
        status,
        headers,
        body,
        log,
    }
}

/// A refused context is 403 `CONTEXT_NOT_ALLOWED` in ADR-035's shape, with
/// one fixed message: the body names neither the context asked for nor the
/// caller's own, and an unknown name is answered exactly as a known one, so
/// nothing tells which contexts exist. The log holds which was asked.
#[tokio::test]
async fn a_refused_context_is_403_context_not_allowed_naming_no_context() {
    let mut bodies = Vec::new();
    for context in [
        "zaru-enterprise",
        "zaru-nonexistent",
        "aegis-system-operator",
    ] {
        let attested = attest_as(
            &key_with_tier(Some("free")).await,
            mcp_server_body(context, "enterprise"),
        )
        .await;
        let answer = attest_answer(attested.refused()).await;
        answer.assert_shape(403, "CONTEXT_NOT_ALLOWED", "policy_violation");
        assert_eq!(
            answer.message(),
            "The security context this attestation asked for is not one your identity may attest."
        );
        assert_eq!(answer.body["error"]["context"], Value::Null);
        assert_eq!(answer.body["error"]["tool"], Value::Null);
        answer.assert_detail_only_in_log(&[context]);
        assert!(!answer.body.to_string().contains("zaru-free"));
        let mut body = answer.body.clone();
        body["request_id"] = Value::Null;
        bodies.push(body);
    }
    assert!(
        bodies.windows(2).all(|w| w[0] == w[1]),
        "every refused context is answered alike: {bodies:?}"
    );
}

// ── Replay protection at the daemon's construction (finding 2) ──────────────
//
// The audit `seal-conformance-audit` (deviation 2, Medium): the daemon built
// its SEAL middleware with no nonce store, so a captured envelope could be
// replayed for as long as it was fresh, although ADR-035's Amendment "SEAL
// Replay-Nonce Store" says a replayed envelope is refused. The middleware
// here is the one the daemon builds (`server::daemon_seal_middleware`), and
// the answer is the route's (`invoke_refusal_response`).

#[tokio::test]
async fn the_daemons_middleware_refuses_a_replayed_envelope_and_admits_a_fresh_one() {
    let mut mcp = attest_through(
        zaru_pro(),
        crate::daemon::server::daemon_seal_middleware(None, None),
        |service, _| service,
    )
    .await;
    let create = json!({
        "statement": "Run palindrome-checker on \"racecar\".",
        "client_ref": "conversation-replay",
        "channel": "api",
    });
    let envelope = mcp.envelope("aegis.goal.create", create.clone());
    mcp.service
        .invoke_tool(&envelope)
        .await
        .expect("the first envelope is answered");

    let replayed = mcp
        .service
        .invoke_tool(&envelope)
        .await
        .expect_err("the same envelope again is refused");
    let answer = answer(&replayed, "aegis.goal.create").await;
    answer.assert_shape(401, "ENVELOPE_REPLAYED", "error");

    let fresh = mcp.envelope("aegis.goal.create", create);
    mcp.service
        .invoke_tool(&fresh)
        .await
        .expect("a fresh envelope of the same call is answered");
}
