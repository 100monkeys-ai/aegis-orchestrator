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
use aegis_orchestrator_core::infrastructure::seal::attestation::{
    AttestationRequest, AttestationService,
};
use aegis_orchestrator_core::infrastructure::seal::envelope::SealEnvelope;
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::seal::signature::SecurityTokenIssuer;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
use aegis_orchestrator_core::infrastructure::tool_router::{InMemoryToolRegistry, ToolRouter};
use aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter;

use super::{attest_binding, AttestCaller, HttpAttestationRequest};
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
    SecurityContext {
        name: "zaru-pro".to_string(),
        description: "consumer".to_string(),
        capabilities: vec![Capability {
            tool_pattern: "aegis.goal.*".to_string(),
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
}

impl Mcp {
    /// One `tools/call`, signed as the MCP server signs it.
    async fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, String> {
        self.calls += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": self.calls,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        });
        let now = chrono::Utc::now();
        let canonical = serde_json::to_vec(&json!({
            "payload": payload,
            "security_token": self.token,
            "timestamp": now.timestamp(),
        }))
        .unwrap();
        let envelope = SealEnvelope {
            protocol: "seal/v1".to_string(),
            security_token: self.token.as_str().into(),
            signature: STANDARD.encode(self.key.sign(&canonical).to_bytes()),
            payload,
            timestamp: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        self.service
            .invoke_tool(&envelope)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Attest as the Zaru MCP server attests for an `aegis_*` key: no `user_id`
/// in the body, the key the only identity.
async fn attest_as_the_mcp_server() -> Mcp {
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(zaru_pro()).await.unwrap();
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
    let internal = AttestationRequest {
        agent_id: request.agent_id.clone(),
        execution_id: request.execution_id.clone(),
        container_id: request.container_id.clone(),
        public_key_pem: request.public_key.clone(),
        security_context: request.security_context.clone(),
        principal_subject: request.principal_subject.clone(),
        user_id: binding.user_id.clone(),
        workload_id: request.workload_id.clone(),
        zaru_tier: request.zaru_tier.clone(),
        tenant_id: binding.tenant_id.clone(),
        realm: binding.realm.clone(),
        task_summary: request.task_summary.clone(),
    };
    assert_eq!(binding.realm, RealmKind::Consumer);
    let attested = attestation
        .attest_with_escalation(internal, binding.escalation)
        .await
        .expect("attestation succeeds");

    let registry: Arc<dyn aegis_orchestrator_core::domain::mcp::ToolRegistry> =
        Arc::new(InMemoryToolRegistry::new());
    let router = Arc::new(ToolRouter::new(
        registry,
        Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        ToolRouter::builtin_dispatchers(),
    ));
    let storage_root =
        std::env::temp_dir().join(format!("aegis-attest-identity-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let event_bus = Arc::new(EventBus::new(256));
    let service = ToolInvocationService::new(
        sessions,
        contexts,
        Arc::new(SealMiddleware::new()),
        router,
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(AgentId::new())),
        Arc::new(Executions::default()),
        Arc::new(ReqwestWebToolAdapter::unconfigured()),
        event_bus.clone(),
        None,
    )
    .with_goals(Arc::new(GoalService::new(
        Arc::new(InMemoryGoalRepository::new()),
        event_bus,
        GoalsConfig::default(),
    )));
    Mcp {
        service,
        token: attested.security_token.expose().to_string(),
        key,
        calls: 0,
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
