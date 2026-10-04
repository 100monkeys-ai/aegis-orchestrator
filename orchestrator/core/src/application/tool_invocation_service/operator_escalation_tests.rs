//! The operator escalation in the SEAL tool path (AEGIS ADR-129 D17 to
//! D20), driven through `invoke_tool` on a real `ToolInvocationService`: a
//! SEAL session attested under an escalation (as `/v1/seal/attest` binds
//! it), a real `OperatorEscalationService` over the in-memory store, and an
//! execution service and store holding one execution in the operator's home
//! tenant and one in another tenant. The operator's federated record is a
//! stub [`OperatorRoleLookup`] (ADR-129 — Updates, V9).

use super::*;
use crate::application::operator_escalation_service::{OperatorEscalationService, RedeemingKey};
use crate::application::tool_approval_service::ToolApprovalService;
use crate::domain::agent::{Agent, AgentManifest};
use crate::domain::events::{ExecutionEvent, TenantEvent};
use crate::domain::execution::{Execution, ExecutionId, ExecutionInput, Iteration};
use crate::domain::iam::AegisRole;
use crate::domain::node_config::OperatorEscalationConfig;
use crate::domain::operator_escalation::{
    audit_action, EscalationEndReason, OperatorEscalation, OperatorEscalationRepository,
    OperatorRecord, OperatorRoleLookup, RoleLookupError,
};
use crate::domain::repository::{AgentVersion, ExecutionRepository, WorkflowExecutionRepository};
use crate::domain::seal_session::{SealOperatorEscalation, SealSession};
use crate::domain::secrets::SensitiveString;
use crate::domain::security_context::SecurityContext;
use crate::domain::tool_approval::ToolApprovalRepository;
use crate::infrastructure::event_bus::DomainEvent;
use crate::infrastructure::repositories::postgres_operator_escalation::InMemoryOperatorEscalationRepository;
use crate::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use crate::infrastructure::repositories::{
    InMemoryExecutionRepository, InMemoryVolumeRepository, InMemoryWorkflowExecutionRepository,
};
use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use crate::infrastructure::storage::LocalHostStorageProvider;
use crate::infrastructure::tool_router::ToolRouter;
use async_trait::async_trait;
use futures::Stream;
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

const CONSUMER_SUB: &str = "8d4e0000-operator";
const OTHER_SUB: &str = "9e5f0000-someone";
const SYSTEM_SUB: &str = "system-sub-of-the-operator";
const OPERATOR_CONTEXT: &str = "aegis-system-operator";

fn home() -> TenantId {
    TenantId::for_consumer_user(CONSUMER_SUB).unwrap()
}

fn other() -> TenantId {
    TenantId::for_consumer_user(OTHER_SUB).unwrap()
}

fn execution_in(tenant: &TenantId) -> Execution {
    let mut e = Execution::new_with_id(
        ExecutionId::new(),
        AgentId::new(),
        ExecutionInput {
            intent: Some("an intent".to_string()),
            input: json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        },
        5,
        "zaru-pro".to_string(),
    );
    e.tenant_id = tenant.clone();
    e
}

/// Executions of two tenants, read with the tenant check the real service
/// makes: a `*_for_tenant` read of another tenant's execution fails.
struct TwoTenantExecutions {
    executions: HashMap<ExecutionId, Execution>,
}

#[async_trait]
impl ExecutionService for TwoTenantExecutions {
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
        _: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
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
        self.executions
            .get(&id)
            .filter(|e| &e.tenant_id == tenant)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Execution not found"))
    }
    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        self.executions
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Execution not found"))
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
        tenant: &TenantId,
        _: Option<AgentId>,
        _: Option<crate::domain::workflow::WorkflowId>,
        _: usize,
    ) -> Result<Vec<Execution>> {
        Ok(self
            .executions
            .values()
            .filter(|e| &e.tenant_id == tenant)
            .cloned()
            .collect())
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
        anyhow::bail!("not exercised")
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<crate::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
}

/// A Zaru SEAL session's agent id names no registered agent.
struct NoAgents;

#[async_trait]
impl AgentLifecycleService for NoAgents {
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

struct NoOpPublisher;

#[async_trait]
impl crate::domain::fsal::EventPublisher for NoOpPublisher {
    async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
}

/// One SEAL envelope: a tool call on a session's token.
struct Envelope {
    token: SensitiveString,
    tool: String,
    args: Value,
    nonce: String,
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
        Some(self.args.clone())
    }
    fn replay_nonce(&self) -> String {
        self.nonce.clone()
    }
}

fn operator_context() -> SecurityContext {
    SecurityContext {
        name: OPERATOR_CONTEXT.to_string(),
        description: "operator".to_string(),
        capabilities: vec![crate::domain::security_context::Capability {
            tool_pattern: "aegis.*".to_string(),
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

/// The canonical dispatchers with `requires_approval` on `tool`, as the
/// production configuration gates `aegis.system.info` (ADR-126).
fn dispatchers_gating(tool: &str) -> Vec<crate::domain::node_config::BuiltinDispatcherConfig> {
    ToolRouter::builtin_dispatchers()
        .into_iter()
        .map(|mut d| {
            for cap in &mut d.capabilities {
                if cap.name == tool {
                    cap.requires_approval = true;
                }
            }
            d
        })
        .collect()
}

/// The operator's federated record in the system realm, as a stub answer
/// (ADR-129 — Updates, V9: "the trigger's tests need a stub").
struct StubRoleLookup {
    answer: StdMutex<Result<OperatorRecord, RoleLookupError>>,
    calls: AtomicUsize,
}

impl StubRoleLookup {
    fn new() -> Self {
        Self {
            answer: StdMutex::new(Ok(OperatorRecord::Found {
                aegis_role: Some("aegis:operator".to_string()),
            })),
            calls: AtomicUsize::new(0),
        }
    }

    fn answer(&self, answer: Result<OperatorRecord, RoleLookupError>) {
        *self.answer.lock().unwrap() = answer;
    }

    fn holds(&self, role: &AegisRole) {
        self.answer(Ok(OperatorRecord::Found {
            aegis_role: Some(role.as_claim_str().to_string()),
        }));
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl OperatorRoleLookup for StubRoleLookup {
    async fn lookup(&self, system_sub: &str) -> Result<OperatorRecord, RoleLookupError> {
        assert_eq!(
            system_sub, SYSTEM_SUB,
            "the lookup reads the escalation's system sub"
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answer.lock().unwrap().clone()
    }
}

struct Harness {
    service: ToolInvocationService,
    role_lookup: Option<Arc<StubRoleLookup>>,
    sessions: Arc<InMemorySealSessionRepository>,
    escalations: Arc<OperatorEscalationService>,
    escalation_repo: Arc<InMemoryOperatorEscalationRepository>,
    approvals_repo: Arc<InMemoryToolApprovalRepository>,
    event_bus: Arc<EventBus>,
    now: Arc<StdMutex<chrono::DateTime<chrono::Utc>>>,
    other_execution: ExecutionId,
    other_workflow_execution: ExecutionId,
}

async fn harness() -> Harness {
    harness_with(Some(Arc::new(StubRoleLookup::new()))).await
}

/// `role_lookup: None` is a node with no Keycloak admin client (V5).
async fn harness_with(role_lookup: Option<Arc<StubRoleLookup>>) -> Harness {
    let home_exec = execution_in(&home());
    let other_exec = execution_in(&other());
    let other_execution = other_exec.id;

    let store = Arc::new(InMemoryExecutionRepository::new());
    store.save_for_tenant(&home(), &home_exec).await.unwrap();
    store.save_for_tenant(&other(), &other_exec).await.unwrap();

    let workflow_store = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let other_workflow_execution = ExecutionId::new();
    workflow_store
        .save_for_tenant(
            &other(),
            &crate::domain::workflow::WorkflowExecution {
                id: other_workflow_execution,
                workflow_id: crate::domain::workflow::WorkflowId::new(),
                tenant_id: other(),
                status: crate::domain::execution::ExecutionStatus::Running,
                current_state: crate::domain::workflow::StateName::new("START").unwrap(),
                blackboard: crate::domain::workflow::Blackboard::new(),
                input: json!({}),
                state_outputs: HashMap::new(),
                final_output: None,
                started_at: chrono::Utc::now(),
                last_transition_at: chrono::Utc::now(),
                initiating_user_sub: None,
            },
        )
        .await
        .unwrap();

    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    security_context_repo
        .save(operator_context())
        .await
        .unwrap();

    let router = Arc::new(ToolRouter::new(dispatchers_gating("aegis.system.info")));
    let storage_root =
        std::env::temp_dir().join(format!("aegis-escalation-tests-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));

    let now = Arc::new(StdMutex::new(chrono::Utc::now()));
    let clock = now.clone();
    let escalation_repo = Arc::new(InMemoryOperatorEscalationRepository::new());
    let mut escalations = OperatorEscalationService::new(
        escalation_repo.clone(),
        OperatorEscalationConfig::default(),
    )
    .with_clock(move || *clock.lock().unwrap());
    if let Some(lookup) = &role_lookup {
        escalations = escalations.with_role_lookup(lookup.clone());
    }
    let escalations = Arc::new(escalations);
    let event_bus = Arc::new(EventBus::new(1024));
    let approvals_repo = Arc::new(InMemoryToolApprovalRepository::new());
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let service = ToolInvocationService::new(
        sessions.clone(),
        security_context_repo,
        Arc::new(SealMiddleware::new()),
        router,
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(NoAgents),
        Arc::new(TwoTenantExecutions {
            executions: [home_exec, other_exec]
                .into_iter()
                .map(|e| (e.id, e))
                .collect(),
        }),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
        event_bus.clone(),
        None,
    )
    .with_workflow_execution_repo(workflow_store)
    .with_tool_approvals(Arc::new(ToolApprovalService::new(
        approvals_repo.clone(),
        event_bus.clone(),
    )))
    .with_operator_escalations(escalations.clone())
    .with_execution_repository(store);
    Harness {
        service,
        role_lookup,
        sessions,
        escalations,
        escalation_repo,
        approvals_repo,
        event_bus,
        now,
        other_execution,
        other_workflow_execution,
    }
}

impl Harness {
    /// Mint and redeem a code for a new key, as the operator with `role`,
    /// whose federated record holds that role.
    async fn escalate(&self, role: AegisRole) -> OperatorEscalation {
        if let Some(lookup) = &self.role_lookup {
            lookup.holds(&role);
        }
        let minted = self
            .escalations
            .mint(SYSTEM_SUB, CONSUMER_SUB, role)
            .await
            .unwrap();
        self.escalations
            .redeem(
                &RedeemingKey {
                    api_key_id: uuid::Uuid::new_v4(),
                    user_id: CONSUMER_SUB.to_string(),
                    has_stored_role: false,
                },
                &minted.code,
            )
            .await
            .unwrap()
    }

    /// A session as `/v1/seal/attest` binds it for an escalated key: home
    /// tenant, the consumer sub as user, bound to the escalation. Returns
    /// its token.
    async fn escalated_session(&self, escalation: &OperatorEscalation) -> String {
        let token = format!("token-{}", uuid::Uuid::new_v4());
        let session = SealSession::new(
            AgentId::new(),
            ExecutionId::new(),
            vec![],
            token.clone(),
            operator_context(),
            home(),
        )
        .with_principal_metadata(None, Some(CONSUMER_SUB.to_string()), None, None)
        .with_operator_escalation(SealOperatorEscalation {
            escalation_id: escalation.id,
            aegis_role: escalation.aegis_role.clone(),
        });
        self.sessions.save(session).await.unwrap();
        token
    }

    /// The same consumer's session without an escalation (the comparison:
    /// what the same call answers when not escalated).
    async fn plain_session(&self) -> String {
        let token = format!("token-{}", uuid::Uuid::new_v4());
        let session = SealSession::new(
            AgentId::new(),
            ExecutionId::new(),
            vec![],
            token.clone(),
            operator_context(),
            home(),
        )
        .with_principal_metadata(None, Some(CONSUMER_SUB.to_string()), None, None);
        self.sessions.save(session).await.unwrap();
        token
    }

    async fn call(&self, token: &str, tool: &str, args: Value) -> Result<Value, SealSessionError> {
        self.service
            .invoke_tool(&Envelope {
                token: token.to_string().into(),
                tool: tool.to_string(),
                args,
                nonce: uuid::Uuid::new_v4().to_string(),
            })
            .await
    }

    fn advance(&self, seconds: i64) {
        *self.now.lock().unwrap() += chrono::Duration::seconds(seconds);
    }
}

fn tenants_listed(body: &Value) -> Vec<String> {
    let mut tenants: Vec<String> = body["executions"]
        .as_array()
        .unwrap_or_else(|| panic!("no executions: {body}"))
        .iter()
        .map(|e| e["tenant_id"].as_str().unwrap().to_string())
        .collect();
    tenants.sort();
    tenants.dedup();
    tenants
}

#[tokio::test]
async fn task_list_aggregates_every_tenant_under_escalation() {
    let h = harness().await;
    let plain = h.plain_session().await;
    let before = h.call(&plain, "aegis.task.list", json!({})).await.unwrap();
    assert_eq!(tenants_listed(&before), vec![home().as_str().to_string()]);

    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    let during = h.call(&token, "aegis.task.list", json!({})).await.unwrap();
    let mut expected = vec![home().as_str().to_string(), other().as_str().to_string()];
    expected.sort();
    assert_eq!(tenants_listed(&during), expected, "{during}");
}

#[tokio::test]
async fn task_list_with_agent_id_stays_tenant_scoped() {
    let h = harness().await;
    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    let body = h
        .call(
            &token,
            "aegis.task.list",
            json!({ "agent_id": uuid::Uuid::new_v4().to_string() }),
        )
        .await
        .unwrap();
    assert_eq!(tenants_listed(&body), vec![home().as_str().to_string()]);
}

#[tokio::test]
async fn task_list_names_tenant_for_admin_only() {
    let h = harness().await;
    let operator = h.escalate(AegisRole::Operator).await;
    let operator_token = h.escalated_session(&operator).await;
    let refused = h
        .call(
            &operator_token,
            "aegis.task.list",
            json!({ "tenant_id": other().as_str() }),
        )
        .await;
    assert!(
        matches!(refused, Err(SealSessionError::TenantMismatch { .. })),
        "{refused:?}"
    );

    let mut tenant_events = h.event_bus.subscribe();
    let admin = h.escalate(AegisRole::Admin).await;
    let admin_token = h.escalated_session(&admin).await;
    let body = h
        .call(
            &admin_token,
            "aegis.task.list",
            json!({ "tenant_id": other().as_str() }),
        )
        .await
        .unwrap();
    assert_eq!(tenants_listed(&body), vec![other().as_str().to_string()]);

    // ADR-056: naming another tenant emits AdminCrossTenantAccess.
    let mut saw = false;
    while let Ok(event) = tenant_events.try_recv() {
        if let DomainEvent::Tenant(TenantEvent::AdminCrossTenantAccess {
            admin_identity,
            target_tenant_id,
            ..
        }) = event
        {
            assert_eq!(admin_identity, SYSTEM_SUB);
            assert_eq!(target_tenant_id, other());
            saw = true;
        }
    }
    assert!(saw, "AdminCrossTenantAccess was not published");
}

#[tokio::test]
async fn task_status_other_tenant_answered_under_escalation_refused_without() {
    let h = harness().await;
    let args = json!({ "execution_id": h.other_execution.0.to_string() });
    let plain = h.plain_session().await;
    let refused = h
        .call(&plain, "aegis.task.status", args.clone())
        .await
        .unwrap();
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("Execution not found")),
        "{refused}"
    );

    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    let answered = h.call(&token, "aegis.task.status", args).await.unwrap();
    assert_eq!(answered["execution_id"], h.other_execution.0.to_string());
    assert_eq!(answered["tenant_id"], other().as_str(), "{answered}");
}

#[tokio::test]
async fn task_logs_other_tenant_answered_under_escalation_refused_without() {
    let h = harness().await;
    let args = json!({ "execution_id": h.other_execution.0.to_string() });
    let plain = h.plain_session().await;
    let refused = h
        .call(&plain, "aegis.task.logs", args.clone())
        .await
        .unwrap();
    assert!(refused.get("error").is_some(), "{refused}");

    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    let answered = h.call(&token, "aegis.task.logs", args).await.unwrap();
    assert!(answered.get("error").is_none(), "{answered}");
    assert_eq!(answered["execution_id"], h.other_execution.0.to_string());
}

#[tokio::test]
async fn workflow_logs_other_tenant_answered_under_escalation_refused_without() {
    let h = harness().await;
    let args = json!({ "execution_id": h.other_workflow_execution.0.to_string() });
    let plain = h.plain_session().await;
    let refused = h
        .call(&plain, "aegis.workflow.logs", args.clone())
        .await
        .unwrap();
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("not found")),
        "{refused}"
    );

    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    let answered = h.call(&token, "aegis.workflow.logs", args).await.unwrap();
    assert!(answered.get("error").is_none(), "{answered}");
    assert_eq!(
        answered["execution_id"],
        h.other_workflow_execution.0.to_string()
    );
}

#[tokio::test]
async fn call_after_expires_at_refused_operator_escalation_expired() {
    let h = harness().await;
    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    h.advance(1799);
    assert!(h.call(&token, "aegis.task.list", json!({})).await.is_ok());
    h.advance(1);
    let refused = h.call(&token, "aegis.task.list", json!({})).await;
    assert_eq!(refused, Err(SealSessionError::OperatorEscalationExpired));
    assert_eq!(
        refused.unwrap_err().to_string(),
        "operator_escalation_expired"
    );
}

#[tokio::test]
async fn call_after_release_refused_operator_escalation_expired() {
    let h = harness().await;
    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    assert!(h.call(&token, "aegis.task.list", json!({})).await.is_ok());
    h.escalations
        .end_for_api_key(e.api_key_id, EscalationEndReason::AgentRelease)
        .await
        .unwrap();
    assert_eq!(
        h.call(&token, "aegis.task.list", json!({})).await,
        Err(SealSessionError::OperatorEscalationExpired)
    );
}

#[tokio::test]
async fn tool_call_audited_with_tenant_and_star() {
    let h = harness().await;
    let e = h.escalate(AegisRole::Admin).await;
    let token = h.escalated_session(&e).await;
    h.call(&token, "aegis.task.list", json!({})).await.unwrap();
    h.call(
        &token,
        "aegis.task.list",
        json!({ "tenant_id": other().as_str() }),
    )
    .await
    .unwrap();
    let tool_calls: Vec<(String, String)> = h
        .escalation_repo
        .audit_entries()
        .await
        .into_iter()
        .filter(|a| a.action == audit_action::TOOL_CALL)
        .map(|a| (a.actor_id, a.target_resource))
        .collect();
    assert_eq!(
        tool_calls,
        vec![
            (SYSTEM_SUB.to_string(), "aegis.task.list@*".to_string()),
            (
                SYSTEM_SUB.to_string(),
                format!("aegis.task.list@{}", other().as_str())
            ),
        ]
    );
}

/// ADR-129 D20: a gated tool under an escalation is gated as ADR-126 says,
/// the request's user the escalation's consumer sub and its tenant the home
/// tenant.
#[tokio::test]
async fn gated_tool_under_escalation_pending_for_consumer_sub_home_tenant() {
    let h = harness().await;
    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    let answer = h
        .call(&token, "aegis.system.info", json!({}))
        .await
        .unwrap();
    assert_eq!(answer["status"], "approval_pending", "{answer}");
    let rows = h.approvals_repo.list_requests(None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_sub, CONSUMER_SUB);
    assert_eq!(rows[0].tenant_id, home());
}

#[tokio::test]
async fn unbound_session_is_unaffected_by_the_check() {
    let h = harness().await;
    let plain = h.plain_session().await;
    // A session bound to no escalation is not checked against one.
    assert!(h.call(&plain, "aegis.task.list", json!({})).await.is_ok());
    assert!(h.escalation_repo.audit_entries().await.is_empty());
}

// ── ADR-129 — Updates, V1, V5, V6: a demotion ends an escalation ────────

fn lookup(h: &Harness) -> &StubRoleLookup {
    h.role_lookup.as_deref().expect("a node with a role lookup")
}

/// The `ended` rows written, as (escalation id, end_reason, role_found,
/// whether `checked_at` is present).
async fn ended_rows(h: &Harness) -> Vec<(String, Value, Value, bool)> {
    h.escalation_repo
        .audit_entries()
        .await
        .into_iter()
        .filter(|a| a.action == audit_action::ENDED)
        .map(|a| {
            let state = a.after_state.unwrap();
            (
                state["escalation_id"].as_str().unwrap().to_string(),
                state["end_reason"].clone(),
                state["role_found"].clone(),
                state.get("checked_at").is_some_and(|v| v.is_string()),
            )
        })
        .collect()
}

/// V1: when the record answers absent, disabled, no role, or a role other
/// than the escalation's, the escalated call is refused
/// `operator_escalation_expired` and every active escalation of that
/// `system_sub` is ended `operator_demoted`; V6: each `ended` row carries
/// `role_found` and `checked_at`.
#[tokio::test]
async fn demoted_operator_call_refused_and_every_escalation_ended() {
    let cases = [
        (Ok(OperatorRecord::Absent), json!("user_absent")),
        (Ok(OperatorRecord::Disabled), json!("user_disabled")),
        (Ok(OperatorRecord::Found { aegis_role: None }), Value::Null),
        (
            Ok(OperatorRecord::Found {
                aegis_role: Some("aegis:admin".to_string()),
            }),
            json!("aegis:admin"),
        ),
    ];
    for (answer, role_found) in cases {
        let h = harness().await;
        let first = h.escalate(AegisRole::Operator).await;
        let second = h.escalate(AegisRole::Operator).await;
        let first_token = h.escalated_session(&first).await;
        let second_token = h.escalated_session(&second).await;
        assert!(h
            .call(&first_token, "aegis.task.list", json!({}))
            .await
            .is_ok());

        lookup(&h).answer(answer.clone());
        let refused = h.call(&first_token, "aegis.task.list", json!({})).await;
        assert_eq!(
            refused,
            Err(SealSessionError::OperatorEscalationExpired),
            "{answer:?}"
        );
        for e in [&first, &second] {
            let row = h
                .escalation_repo
                .find_escalation(e.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                row.end_reason,
                Some(EscalationEndReason::OperatorDemoted),
                "{answer:?}"
            );
        }
        let mut ended = ended_rows(&h).await;
        ended.sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected = vec![
            (
                first.id.to_string(),
                json!("operator_demoted"),
                role_found.clone(),
                true,
            ),
            (
                second.id.to_string(),
                json!("operator_demoted"),
                role_found.clone(),
                true,
            ),
        ];
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(ended, expected, "{answer:?}");
        // The other escalation's session is refused too.
        assert_eq!(
            h.call(&second_token, "aegis.task.list", json!({})).await,
            Err(SealSessionError::OperatorEscalationExpired),
            "{answer:?}"
        );
    }
}

/// V1: the record is read on every escalated call, and a record holding the
/// escalation's role answers the call and ends nothing.
#[tokio::test]
async fn escalated_call_answered_while_the_record_grants_its_role() {
    let h = harness().await;
    let e = h.escalate(AegisRole::Admin).await;
    let token = h.escalated_session(&e).await;
    let before = lookup(&h).calls();
    for _ in 0..3 {
        assert!(h.call(&token, "aegis.task.list", json!({})).await.is_ok());
    }
    assert_eq!(
        lookup(&h).calls() - before,
        3,
        "one read per escalated call"
    );
    assert!(h.escalations.check_active(e.id).await.is_ok());
    assert!(ended_rows(&h).await.is_empty());
}

/// V5: a lookup that fails refuses the call as the existing check's failure
/// does, leaves the escalation active and writes no `ended` row.
#[tokio::test]
async fn lookup_error_refuses_the_call_and_leaves_the_escalation_active() {
    let h = harness().await;
    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    lookup(&h).answer(Err(RoleLookupError(
        "realm operation failed: 503 unavailable".to_string(),
    )));
    let refused = h.call(&token, "aegis.task.list", json!({})).await;
    assert_eq!(
        refused,
        Err(SealSessionError::InternalError(
            "operator escalation check failed: realm operation failed: 503 unavailable".to_string()
        ))
    );
    assert!(h.escalations.check_active(e.id).await.is_ok());
    assert!(ended_rows(&h).await.is_empty());
    // When Keycloak answers again, the escalation grants again.
    lookup(&h).holds(&AegisRole::Operator);
    assert!(h.call(&token, "aegis.task.list", json!({})).await.is_ok());
}

/// V5: a node with no `spec.iam.keycloak_admin` cannot make the check and
/// refuses every escalated call; an unescalated session is unaffected.
#[tokio::test]
async fn node_without_keycloak_admin_refuses_every_escalated_call() {
    let h = harness_with(None).await;
    let e = h.escalate(AegisRole::Operator).await;
    let token = h.escalated_session(&e).await;
    let refused = h.call(&token, "aegis.task.list", json!({})).await;
    assert!(
        matches!(&refused, Err(SealSessionError::InternalError(m)) if m.starts_with("operator escalation check failed: ") && m.contains("spec.iam.keycloak_admin")),
        "{refused:?}"
    );
    assert!(h.escalations.check_active(e.id).await.is_ok());
    assert!(ended_rows(&h).await.is_empty());
    let plain = h.plain_session().await;
    assert!(h.call(&plain, "aegis.task.list", json!({})).await.is_ok());
}
