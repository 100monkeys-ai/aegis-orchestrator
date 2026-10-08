//! The approval gate in the tool dispatch path (AEGIS ADR-126), driven at
//! the level the dispatch path's own tests run: `invoke_tool_internal` on a
//! real `ToolInvocationService`, a real `ToolApprovalService` over the
//! in-memory store, and an execution service that records every execution
//! the gated tool (`aegis.task.execute`) starts, with the identity it was
//! started as. The tool is gated the way production's first check gates
//! one: by `requires_approval` on its capability entry, which also declares
//! the gate's keys (ADR-126, Update of 2026-10-04, clause 1): the binding
//! argument `account` and the summary's arguments, unless a test says not.

use super::*;
use crate::application::tool_approval_service::{ToolApprovalError, ToolApprovalService};
use crate::domain::agent::{Agent, AgentManifest, AgentStatus};
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{Execution, ExecutionId, ExecutionInput, Iteration};
use crate::domain::repository::AgentVersion;
use crate::domain::security_context::SecurityContext;
use crate::domain::tool_approval::{
    ApprovalContract, ToolApprovalDecision, ToolApprovalId, ToolApprovalPolicyEffect,
    ToolApprovalRepository, ToolApprovalRequest, ToolApprovalStatus,
};
use crate::infrastructure::event_bus::DomainEvent;
use crate::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use crate::infrastructure::repositories::InMemoryVolumeRepository;
use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use crate::infrastructure::storage::LocalHostStorageProvider;
use crate::infrastructure::tool_router::ToolRouter;
use async_trait::async_trait;
use futures::Stream;
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;

const GATED_TOOL: &str = "aegis.task.execute";
pub(super) const USER: &str = "user-1";

fn agent() -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: gate-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: []
"#,
    )
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

/// One execution `aegis.task.execute` started: who it ran as, with what input.
type Started = (Option<String>, Value);

/// Serves the executions that call tools, and records the ones the gated
/// tool starts.
struct RecordingExecutionService {
    executions: HashMap<ExecutionId, Execution>,
    started: Arc<StdMutex<Vec<Started>>>,
}

#[async_trait]
impl ExecutionService for RecordingExecutionService {
    async fn start_execution(
        &self,
        _agent_id: AgentId,
        input: ExecutionInput,
        _security_context_name: String,
        identity: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        self.started
            .lock()
            .unwrap()
            .push((identity.map(|id| id.sub.clone()), input.input));
        Ok(ExecutionId::new())
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
        self.executions
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

fn security_context(tool_pattern: &str) -> SecurityContext {
    SecurityContext {
        name: "gate-test-context".to_string(),
        description: "gate test".to_string(),
        capabilities: vec![crate::domain::security_context::Capability {
            tool_pattern: tool_pattern.to_string(),
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

/// The contract the gated tool's capability entry declares in most tests:
/// a binding argument no gate knows by name, and two arguments to show.
fn declared_contract() -> ApprovalContract {
    ApprovalContract {
        binding_argument: Some("account".to_string()),
        approval_summary: Some(vec!["agent_id".to_string(), "input".to_string()]),
    }
}

/// The canonical dispatchers, with `requires_approval: true` and `contract`'s
/// keys on the gated tool's capability entry, as a node configuration sets
/// them.
fn dispatchers_gating(
    tool: &str,
    contract: &ApprovalContract,
) -> Vec<crate::domain::node_config::BuiltinDispatcherConfig> {
    ToolRouter::builtin_dispatchers()
        .into_iter()
        .map(|mut d| {
            for cap in &mut d.capabilities {
                if cap.name == tool {
                    cap.requires_approval = true;
                    cap.binding_argument = contract.binding_argument.clone();
                    cap.approval_summary = contract.approval_summary.clone();
                }
            }
            d
        })
        .collect()
}

pub(super) struct Harness {
    pub(super) service: ToolInvocationService,
    sessions: Arc<InMemorySealSessionRepository>,
    approvals: Arc<ToolApprovalService>,
    pub(super) repo: Arc<InMemoryToolApprovalRepository>,
    event_bus: Arc<EventBus>,
    started: Arc<StdMutex<Vec<Started>>>,
    tenant: TenantId,
    agent_id: AgentId,
    /// The execution whose initiating user is `USER`.
    pub(super) execution: ExecutionId,
    /// An execution of the same tenant whose initiating user is `user-2`.
    other_users_execution: ExecutionId,
    /// An execution of the same tenant with no initiating user.
    userless_execution: ExecutionId,
    /// An execution of `USER`'s whose record names the conversation
    /// `CONVERSATION` it was started from (ADR-126, Update of 2026-10-07
    /// (2), clause 3).
    conversation_execution: ExecutionId,
    /// An execution of `USER`'s whose record's `conversation_id` is not a
    /// UUID (clause 3a).
    garbled_conversation_execution: ExecutionId,
    target_agent: String,
}

async fn harness_with(tool_pattern: &str, repo: Arc<InMemoryToolApprovalRepository>) -> Harness {
    harness_declaring(tool_pattern, repo, declared_contract()).await
}

async fn harness_declaring(
    tool_pattern: &str,
    repo: Arc<InMemoryToolApprovalRepository>,
    contract: ApprovalContract,
) -> Harness {
    harness_gating(tool_pattern, repo, contract, GATED_TOOL).await
}

async fn harness_gating(
    tool_pattern: &str,
    repo: Arc<InMemoryToolApprovalRepository>,
    contract: ApprovalContract,
    gated_tool: &str,
) -> Harness {
    let tenant = TenantId::for_consumer_user(USER).unwrap();
    let agent = agent();
    let agent_id = agent.id;
    let execution_with = |user: Option<&str>, input: Value| {
        let mut e = Execution::new_with_id(
            ExecutionId::new(),
            agent_id,
            ExecutionInput {
                intent: None,
                input,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            5,
            "gate-test-context".to_string(),
        );
        e.tenant_id = tenant.clone();
        e.initiating_user_sub = user.map(str::to_string);
        e
    };
    let execution_for = |user: Option<&str>| execution_with(user, json!({}));
    let executions: Vec<Execution> = vec![
        execution_for(Some(USER)),
        execution_for(Some("user-2")),
        execution_for(None),
        execution_with(Some(USER), json!({ "conversation_id": CONVERSATION })),
        execution_with(Some(USER), json!({ "conversation_id": "not-a-uuid" })),
    ];
    let (execution, other_users_execution, userless_execution) =
        (executions[0].id, executions[1].id, executions[2].id);
    let (conversation_execution, garbled_conversation_execution) =
        (executions[3].id, executions[4].id);
    let started = Arc::new(StdMutex::new(Vec::new()));
    let exec_service = Arc::new(RecordingExecutionService {
        executions: executions.into_iter().map(|e| (e.id, e)).collect(),
        started: started.clone(),
    });

    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    security_context_repo
        .save(security_context(tool_pattern))
        .await
        .unwrap();

    let router = Arc::new(ToolRouter::new(dispatchers_gating(gated_tool, &contract)));
    let storage_root =
        std::env::temp_dir().join(format!("aegis-gate-tests-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let event_bus = Arc::new(EventBus::new(1024));
    let approvals = Arc::new(ToolApprovalService::new(repo.clone(), event_bus.clone()));
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let service = ToolInvocationService::new(
        sessions.clone(),
        security_context_repo,
        Arc::new(SealMiddleware::new()),
        router,
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(agent.clone())),
        exec_service,
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
        event_bus.clone(),
        None,
    )
    .with_tool_approvals(approvals.clone());
    Harness {
        service,
        sessions,
        approvals,
        repo,
        event_bus,
        started,
        tenant,
        agent_id,
        execution,
        other_users_execution,
        userless_execution,
        conversation_execution,
        garbled_conversation_execution,
        target_agent: agent.id.to_string(),
    }
}

pub(super) async fn harness() -> Harness {
    harness_with("aegis.*", Arc::new(InMemoryToolApprovalRepository::new())).await
}

impl Harness {
    fn args(&self, account: &str) -> Value {
        json!({ "agent_id": self.target_agent, "account": account, "input": { "note": "the stored arguments" } })
    }

    pub(super) async fn call_as(
        &self,
        execution: ExecutionId,
        tool: &str,
        args: Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        self.service
            .invoke_tool_internal(
                &self.agent_id,
                execution,
                self.tenant.clone(),
                1,
                vec![],
                tool.to_string(),
                args,
            )
            .await
    }

    async fn call(&self, args: Value) -> Result<ToolInvocationResult, SealSessionError> {
        self.call_as(self.execution, GATED_TOOL, args).await
    }

    pub(super) async fn rows(&self) -> Vec<ToolApprovalRequest> {
        self.repo.list_requests(None).await.unwrap()
    }

    fn started(&self) -> Vec<Started> {
        self.started.lock().unwrap().clone()
    }

    async fn decide(
        &self,
        id: ToolApprovalId,
        decision: ToolApprovalDecision,
    ) -> Result<ToolApprovalRequest, ToolApprovalError> {
        self.approvals
            .decide(id, &self.tenant, USER, decision, &self.service)
            .await
    }
}

pub(super) fn direct(result: Result<ToolInvocationResult, SealSessionError>) -> Value {
    match result {
        Ok(ToolInvocationResult::Direct(v)) => v,
        other => panic!("expected a direct result, got {other:?}"),
    }
}

fn pending_id(result: Result<ToolInvocationResult, SealSessionError>) -> ToolApprovalId {
    let value = direct(result);
    assert_eq!(value["status"], "approval_pending", "{value}");
    ToolApprovalId::from_string(value["approval_id"].as_str().expect("approval_id")).unwrap()
}

#[tokio::test]
async fn a_gated_call_returns_approval_pending_writes_one_row_publishes_and_does_not_run() {
    let h = harness().await;
    let mut events = h.event_bus.subscribe();
    let args = h.args("b-1");

    let value = direct(h.call(args.clone()).await);

    assert_eq!(value["status"], "approval_pending", "{value}");
    let id = ToolApprovalId::from_string(value["approval_id"].as_str().unwrap()).unwrap();
    let summary = value["summary"].as_str().expect("a summary");
    assert!(summary.contains(GATED_TOOL), "{summary}");
    assert!(
        h.started().is_empty(),
        "the gated tool ran: {:?}",
        h.started()
    );

    let rows = h.rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.status, ToolApprovalStatus::Pending);
    assert_eq!(row.user_sub, USER);
    assert_eq!(row.arguments, args, "the exact arguments are stored");
    assert_eq!(row.binding_id.as_deref(), Some("b-1"));
    assert_eq!(row.execution_id, h.execution);

    let mut published = false;
    while let Ok(event) = events.try_recv() {
        if let DomainEvent::MCP(MCPToolEvent::ApprovalRequested {
            approval_id,
            user_sub,
            ..
        }) = event
        {
            published = approval_id == id && user_sub == USER;
        }
    }
    assert!(published, "no ApprovalRequested event for {id}");
}

#[tokio::test]
async fn a_call_with_no_initiating_user_is_refused_with_approval_requires_user() {
    let h = harness().await;
    let result = h
        .call_as(h.userless_execution, GATED_TOOL, h.args("b-1"))
        .await;
    let error = result.expect_err("a gated call with no user must be refused");
    assert!(
        error.to_string().contains("approval_requires_user"),
        "{error}"
    );
    assert!(h.rows().await.is_empty());
    assert!(h.started().is_empty());
}

#[tokio::test]
async fn a_call_the_security_context_forbids_never_writes_a_row() {
    let h = harness_with("fs.*", Arc::new(InMemoryToolApprovalRepository::new())).await;
    let result = h.call(h.args("b-1")).await;
    assert!(
        matches!(result, Err(SealSessionError::PolicyViolation(_))),
        "{result:?}"
    );
    assert!(
        h.rows().await.is_empty(),
        "a forbidden call reached a person"
    );
    assert!(h.started().is_empty());
}

#[tokio::test]
async fn once_runs_the_stored_call_as_its_user_and_records_the_result() {
    let h = harness().await;
    let args = h.args("b-1");
    let id = pending_id(h.call(args.clone()).await);

    let decided = h.decide(id, ToolApprovalDecision::Once).await.unwrap();

    assert_eq!(decided.status, ToolApprovalStatus::ApprovedOnce);
    let started = h.started();
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(
        started[0].0.as_deref(),
        Some(USER),
        "the stored call ran as the request's user"
    );
    assert_eq!(
        started[0].1["note"], args["input"]["note"],
        "the stored call ran with the stored arguments"
    );
    let row = h.repo.find_request(id).await.unwrap().unwrap();
    assert_eq!(row.status, ToolApprovalStatus::ApprovedOnce);
    assert_eq!(row.decided_by.as_deref(), Some(USER));
    assert!(row.result.is_some() && row.error.is_none(), "{row:?}");

    // A second identical call waits again: "once" wrote no policy.
    pending_id(h.call(args).await);
}

#[tokio::test]
async fn deny_records_denied_and_runs_nothing() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    let decided = h.decide(id, ToolApprovalDecision::Deny).await.unwrap();
    assert_eq!(decided.status, ToolApprovalStatus::Denied);
    assert!(h.started().is_empty(), "deny ran the call");
    let row = h.repo.find_request(id).await.unwrap().unwrap();
    assert_eq!(row.status, ToolApprovalStatus::Denied);
    assert!(row.result.is_none());
}

/// "always deny" (ADR-126, Update of 2026-10-08 (2), clause 2): the request
/// reads `denied`, nothing runs, and a deny policy is written for the
/// request's tool and binding.
#[tokio::test]
async fn always_deny_records_denied_writes_a_deny_policy_and_runs_nothing() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    let decided = h
        .decide(id, ToolApprovalDecision::AlwaysDeny)
        .await
        .unwrap();
    let mut wrong = Vec::new();
    if decided.status != ToolApprovalStatus::Denied {
        wrong.push(format!("the request reads {}, not denied", decided.status));
    }
    if !h.started().is_empty() {
        wrong.push(format!("always_deny ran the call: {:?}", h.started()));
    }
    let policies = h.approvals.list_policies(&h.tenant, USER).await.unwrap();
    let deny = policies.iter().any(|p| {
        p.effect == ToolApprovalPolicyEffect::Deny
            && p.tool_name == GATED_TOOL
            && p.binding_id.as_deref() == Some("b-1")
    });
    if policies.len() != 1 || !deny {
        wrong.push(format!(
            "always_deny did not leave one deny policy on the tool and b-1: {policies:?}"
        ));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// A gated call matching a deny policy (clause 3): answered `auto_denied` at
/// once with an `auto_denied` row carrying the policy's id; nothing runs;
/// no `ApprovalRequested`; `aegis.approval.status` answers `auto_denied`;
/// another binding still waits for its user.
#[tokio::test]
async fn a_call_matching_a_deny_policy_answers_auto_denied_and_nothing_runs() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    h.decide(id, ToolApprovalDecision::AlwaysDeny)
        .await
        .unwrap();
    let policy = h.approvals.list_policies(&h.tenant, USER).await.unwrap()[0].clone();
    let mut events = h.event_bus.subscribe();

    let value = direct(h.call(h.args("b-1")).await);

    let mut wrong = Vec::new();
    if value["status"] != "auto_denied" {
        wrong.push(format!("the call answered {value}, not auto_denied"));
    }
    if !h.started().is_empty() {
        wrong.push(format!(
            "a call matching a deny policy ran: {:?}",
            h.started()
        ));
    }
    let auto: Vec<_> = h
        .rows()
        .await
        .into_iter()
        .filter(|r| r.status == ToolApprovalStatus::AutoDenied)
        .collect();
    if auto.len() != 1 || auto[0].policy_id != Some(policy.id) || auto[0].decided_at.is_none() {
        wrong.push(format!(
            "no single auto_denied row with the policy's id and a decision time: {auto:?}"
        ));
    }
    if value["approval_id"].as_str() != auto.first().map(|r| r.id.to_string()).as_deref() {
        wrong.push(format!(
            "the answer's approval_id is not the row's: {value}"
        ));
    }
    while let Ok(event) = events.try_recv() {
        if let DomainEvent::MCP(MCPToolEvent::ApprovalRequested { .. }) = event {
            wrong.push("an auto_denied call published ApprovalRequested".to_string());
        }
    }
    if let Some(row) = auto.first() {
        let status = direct(
            h.call_as(
                h.execution,
                "aegis.approval.status",
                json!({ "approval_id": row.id.to_string() }),
            )
            .await,
        );
        if status["status"] != "auto_denied" {
            wrong.push(format!("aegis.approval.status answered {status}"));
        }
    }
    let other = direct(h.call(h.args("b-2")).await);
    if other["status"] != "approval_pending" {
        wrong.push(format!(
            "the deny policy on b-1 refused a call on b-2: {other}"
        ));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// One standing choice per key (clause 4): with two requests pending, an
/// "always" after an "always_deny" replaces the deny policy, and the next
/// call proceeds as `auto_allowed`.
#[tokio::test]
async fn an_always_after_an_always_deny_replaces_it_and_the_next_call_is_auto_allowed() {
    let h = harness().await;
    let first = pending_id(h.call(h.args("b-1")).await);
    let second = pending_id(h.call(h.args("b-1")).await);
    h.decide(first, ToolApprovalDecision::AlwaysDeny)
        .await
        .unwrap();
    h.decide(second, ToolApprovalDecision::Always)
        .await
        .unwrap();

    let mut wrong = Vec::new();
    let policies = h.approvals.list_policies(&h.tenant, USER).await.unwrap();
    let effects: Vec<_> = policies.iter().map(|p| p.effect).collect();
    if effects != vec![ToolApprovalPolicyEffect::Allow] {
        wrong.push(format!(
            "the key holds {effects:?}, not the one allow that replaced the deny"
        ));
    }
    let value = direct(h.call(h.args("b-1")).await);
    let auto_allowed = h
        .rows()
        .await
        .iter()
        .any(|r| r.status == ToolApprovalStatus::AutoAllowed);
    if value["status"] == "auto_denied" || !auto_allowed {
        wrong.push(format!("the next call was not auto_allowed: {value}"));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn a_second_decision_is_refused() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    h.decide(id, ToolApprovalDecision::Deny).await.unwrap();
    let again = h.decide(id, ToolApprovalDecision::Once).await;
    assert!(
        matches!(
            again,
            Err(ToolApprovalError::AlreadyDecided(
                ToolApprovalStatus::Denied
            ))
        ),
        "{again:?}"
    );
    assert!(h.started().is_empty());
}

#[tokio::test]
async fn always_runs_writes_a_policy_and_the_next_call_proceeds_as_auto_allowed() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    let decided = h.decide(id, ToolApprovalDecision::Always).await.unwrap();
    assert_eq!(decided.status, ToolApprovalStatus::ApprovedAlways);
    assert_eq!(h.started().len(), 1);
    let policies = h.approvals.list_policies(&h.tenant, USER).await.unwrap();
    assert_eq!(policies.len(), 1, "{policies:?}");
    assert_eq!(policies[0].tool_name, GATED_TOOL);
    assert_eq!(policies[0].binding_id.as_deref(), Some("b-1"));

    // The same user, tool and binding: proceeds at once.
    let value = direct(h.call(h.args("b-1")).await);
    assert_ne!(value["status"], "approval_pending", "{value}");
    assert_eq!(h.started().len(), 2, "the allowed call ran");
    let auto: Vec<_> = h
        .rows()
        .await
        .into_iter()
        .filter(|r| r.status == ToolApprovalStatus::AutoAllowed)
        .collect();
    assert_eq!(auto.len(), 1, "{auto:?}");
    assert_eq!(auto[0].policy_id, Some(policies[0].id));
    assert_eq!(
        auto[0].result.as_ref(),
        Some(&value),
        "its result is recorded"
    );

    // Another binding is not covered, nor another user.
    pending_id(h.call(h.args("b-2")).await);
    pending_id(
        h.call_as(h.other_users_execution, GATED_TOOL, h.args("b-1"))
            .await,
    );
    assert_eq!(h.started().len(), 2);
}

#[tokio::test]
async fn a_revoked_policy_stops_applying() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    h.decide(id, ToolApprovalDecision::Always).await.unwrap();
    let policy = h.approvals.list_policies(&h.tenant, USER).await.unwrap()[0].clone();

    h.approvals
        .revoke_policy(policy.id, &h.tenant, USER)
        .await
        .unwrap();

    pending_id(h.call(h.args("b-1")).await);
    assert_eq!(h.started().len(), 1, "only the approved call ran");
    assert!(h
        .approvals
        .list_policies(&h.tenant, USER)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_request_pending_72_hours_is_expired_by_the_sweep_and_never_runs() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    let fresh = pending_id(h.call(h.args("b-2")).await);
    let created = h.repo.find_request(id).await.unwrap().unwrap().created_at;

    // 71 hours on, nothing expires; 72 hours on, both do.
    let swept = h
        .approvals
        .expire_stale(created + chrono::Duration::hours(71))
        .await
        .unwrap();
    assert_eq!(swept, 0);
    let swept = h
        .approvals
        .expire_stale(created + chrono::Duration::hours(73))
        .await
        .unwrap();
    assert_eq!(swept, 2);
    assert_eq!(
        h.repo.find_request(id).await.unwrap().unwrap().status,
        ToolApprovalStatus::Expired
    );

    let refused = h.decide(id, ToolApprovalDecision::Once).await;
    assert!(
        matches!(refused, Err(ToolApprovalError::Expired)),
        "{refused:?}"
    );
    let refused = h.decide(fresh, ToolApprovalDecision::Always).await;
    assert!(
        matches!(refused, Err(ToolApprovalError::Expired)),
        "{refused:?}"
    );
    assert!(h.started().is_empty(), "an expired call ran");
}

#[tokio::test]
async fn a_decision_on_a_stale_request_the_sweep_has_not_reached_is_refused() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    // The same request, as if stored 73 hours earlier.
    let mut stale = h.repo.find_request(id).await.unwrap().unwrap();
    stale.id = ToolApprovalId::new();
    stale.created_at -= chrono::Duration::hours(73);
    h.repo.insert_request(&stale).await.unwrap();

    let refused = h.decide(stale.id, ToolApprovalDecision::Once).await;
    assert!(
        matches!(refused, Err(ToolApprovalError::Expired)),
        "{refused:?}"
    );
    assert_eq!(
        h.repo.find_request(stale.id).await.unwrap().unwrap().status,
        ToolApprovalStatus::Expired
    );
    assert!(h.started().is_empty());
}

#[tokio::test]
async fn approval_status_answers_the_status_and_result_to_the_executions_own_user_only() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    let status_args = json!({ "approval_id": id.to_string() });

    let pending = direct(
        h.call_as(h.execution, "aegis.approval.status", status_args.clone())
            .await,
    );
    assert_eq!(pending["status"], "pending", "{pending}");
    assert!(pending["result"].is_null());

    h.decide(id, ToolApprovalDecision::Once).await.unwrap();
    let done = direct(
        h.call_as(h.execution, "aegis.approval.status", status_args.clone())
            .await,
    );
    assert_eq!(done["status"], "approved_once", "{done}");
    assert!(!done["result"].is_null(), "{done}");

    let other = h
        .call_as(
            h.other_users_execution,
            "aegis.approval.status",
            status_args,
        )
        .await;
    assert!(
        matches!(other, Err(SealSessionError::NotFound(_))),
        "another user read the request: {other:?}"
    );
}

#[tokio::test]
async fn an_ungated_tool_is_not_stopped_by_the_gate() {
    let h = harness().await;
    let result = h.call_as(h.execution, "aegis.task.list", json!({})).await;
    assert!(
        h.rows().await.is_empty(),
        "an ungated call wrote a row: {result:?}"
    );
}

/// Test (a): the policy is keyed on the argument the capability entry
/// declares (`account`); a `mailbox` argument means nothing to the gate.
#[tokio::test]
async fn a_policy_is_keyed_on_the_contract_declared_argument_only() {
    let h = harness().await;
    let id = pending_id(h.call(h.args("b-1")).await);
    h.decide(id, ToolApprovalDecision::Always).await.unwrap();
    let policies = h.approvals.list_policies(&h.tenant, USER).await.unwrap();
    assert_eq!(policies[0].binding_id.as_deref(), Some("b-1"));

    let mut same_account = h.args("b-1");
    same_account["mailbox"] = json!("m-9");
    assert_ne!(
        direct(h.call(same_account).await)["status"],
        "approval_pending"
    );

    let mut other_account = h.args("b-2");
    other_account["mailbox"] = json!("b-1");
    pending_id(h.call(other_account).await);
}

/// Test (b) through the dispatch path: the pending answer's summary lists
/// exactly the declared arguments, each cut at 2,000 characters.
#[tokio::test]
async fn the_pending_summary_lists_exactly_the_declared_arguments() {
    let h = harness().await;
    let mut args = h.args("b-1");
    args["input"] = json!("q".repeat(2_500));
    let value = direct(h.call(args).await);
    assert_eq!(
        value["summary"],
        format!(
            "{GATED_TOOL}\nagent_id: {}\ninput: {}",
            h.target_agent,
            "q".repeat(2_000)
        )
    );
}

/// Test (c) through the dispatch path: a capability entry declaring neither
/// key gives today's fallback, and no binding.
#[tokio::test]
async fn a_tool_declaring_neither_key_keeps_the_fallback_summary() {
    let h = harness_declaring(
        "aegis.*",
        Arc::new(InMemoryToolApprovalRepository::new()),
        ApprovalContract::default(),
    )
    .await;
    let args = h.args("b-1");
    let value = direct(h.call(args.clone()).await);
    assert_eq!(
        value["summary"],
        format!("{GATED_TOOL} with arguments {args}")
    );
    assert_eq!(h.rows().await[0].binding_id, None);
}

// ---------------------------------------------------------------------------
// ADR-126, Update of 2026-10-07 (2), clauses 1 and 2: the conversation a
// gated call was made in, from the call's `_meta.conversation_id`
// ---------------------------------------------------------------------------

use crate::domain::secrets::SensitiveString;

const CONVERSATION: &str = "6c1f0b52-8a3e-4d7b-9f21-0e5d4c3b2a19";

/// A SEAL envelope the middleware accepts as signed, for the invoke route.
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

impl Harness {
    /// A session of `USER` for `execution`: a conversation's session when
    /// `execution` has no record, an agent's when it has one.
    pub(super) async fn session_for(&self, execution: ExecutionId) -> String {
        let token = format!("token-{}", uuid::Uuid::new_v4());
        let session = crate::domain::seal_session::SealSession::new(
            self.agent_id,
            execution,
            vec![],
            token.clone(),
            security_context("aegis.*"),
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

    /// One call of the gated tool through the invoke route, with `meta` as
    /// the payload's `params._meta`.
    async fn route(&self, token: &str, meta: Option<Value>) -> Result<Value, SealSessionError> {
        self.route_with(token, self.args("b-1"), meta).await
    }

    /// One call of the harness's starting tool through the invoke route,
    /// with `args` and `meta` as the payload's `params._meta`.
    async fn route_with(
        &self,
        token: &str,
        args: Value,
        meta: Option<Value>,
    ) -> Result<Value, SealSessionError> {
        self.route_tool(token, GATED_TOOL, args, meta).await
    }

    /// One call of `tool` through the invoke route, with `args` and `meta`
    /// as the payload's `params._meta`.
    pub(super) async fn route_tool(
        &self,
        token: &str,
        tool: &str,
        args: Value,
        meta: Option<Value>,
    ) -> Result<Value, SealSessionError> {
        self.service
            .invoke_tool_with_meta(
                &RouteEnvelope {
                    token: token.to_string().into(),
                    tool: tool.to_string(),
                    args,
                    nonce: uuid::Uuid::new_v4().to_string(),
                },
                meta.as_ref(),
            )
            .await
    }
}

/// Clause 2: a conversation's gated call (its session has no execution
/// record) stores the conversation its payload's `_meta` names.
#[tokio::test]
async fn a_conversations_gated_call_stores_its_meta_conversation_id() {
    let h = harness().await;
    let token = h.session_for(ExecutionId::new()).await;
    let value = h
        .route(&token, Some(json!({ "conversation_id": CONVERSATION })))
        .await
        .expect("the gated call answers");
    assert_eq!(value["status"], "approval_pending", "{value}");
    let rows = h.rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].conversation_id.as_deref(),
        Some(CONVERSATION),
        "the conversation's gated call did not store its _meta.conversation_id"
    );
}

/// Clause 1: a gated call that names no conversation stores none.
#[tokio::test]
async fn a_gated_call_naming_no_conversation_stores_none() {
    let h = harness().await;
    let token = h.session_for(ExecutionId::new()).await;
    for meta in [None, Some(json!({ "contexts": {} }))] {
        h.route(&token, meta).await.expect("the gated call answers");
    }
    let rows = h.rows().await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in &rows {
        assert_eq!(
            row.conversation_id, None,
            "a call naming no conversation stored one"
        );
    }
}

/// Clause 2: a `_meta.conversation_id` that is not a string holding a UUID
/// is refused with its sentence before anything runs.
#[tokio::test]
async fn a_malformed_meta_conversation_id_is_refused_before_anything_runs() {
    let h = harness().await;
    let token = h.session_for(ExecutionId::new()).await;
    let mut failures = Vec::new();
    for bad in [
        json!("not-a-uuid"),
        json!(7),
        json!([CONVERSATION]),
        json!(null),
    ] {
        match h
            .route(&token, Some(json!({ "conversation_id": bad })))
            .await
        {
            Err(e)
                if e.to_string()
                    .contains(super::context_args::CONVERSATION_ID_SHAPE) => {}
            other => failures.push(format!("{bad}: {other:?}")),
        }
    }
    let rows = h.rows().await;
    assert!(
        failures.is_empty() && rows.is_empty(),
        "a malformed _meta.conversation_id was not refused before anything ran: {failures:?}; rows {rows:?}"
    );
}

/// Clause 2: inside an execution with a record, the call's
/// `_meta.conversation_id` is ignored, as S7 ignores its `_meta.contexts`.
#[tokio::test]
async fn inside_an_execution_with_a_record_meta_conversation_id_is_ignored() {
    let h = harness().await;
    let token = h.session_for(h.execution).await;
    h.route(&token, Some(json!({ "conversation_id": CONVERSATION })))
        .await
        .expect("the gated call answers");
    let rows = h.rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].conversation_id, None,
        "inside an execution with a record the call's _meta.conversation_id was stored"
    );
}

// ---------------------------------------------------------------------------
// ADR-126, Update of 2026-10-07 (2), clauses 3 and 3a to 3d: a run started
// from a conversation names it, and its gated calls store it
// ---------------------------------------------------------------------------

/// A harness in which `aegis.task.execute` is not gated, so a start runs.
async fn ungated_start_harness() -> Harness {
    harness_gating(
        "aegis.*",
        Arc::new(InMemoryToolApprovalRepository::new()),
        declared_contract(),
        "aegis.task.status",
    )
    .await
}

/// Clauses 3 and 3a: a conversation's start (its session has no execution
/// record) keeps its `_meta.conversation_id` in the input it starts, and a
/// `conversation_id` the call wrote in its own arguments counts for nothing.
#[tokio::test]
async fn a_conversations_start_keeps_its_meta_conversation_id() {
    let h = ungated_start_harness().await;
    let token = h.session_for(ExecutionId::new()).await;
    let mut args = h.args("b-1");
    args["conversation_id"] = json!("0b0b0b0b-1c1c-4d4d-8e8e-2f2f2f2f2f2f");
    h.route_with(
        &token,
        args,
        Some(json!({ "conversation_id": CONVERSATION })),
    )
    .await
    .expect("the start answers");
    let started = h.started();
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(
        started[0].1.get("conversation_id"),
        Some(&json!(CONVERSATION)),
        "a session with no record did not start an input carrying _meta's conversation id: {}",
        started[0].1
    );
}

/// Clause 3b: a starting tool called inside an execution with a record keeps
/// no conversation, whatever its record or its `_meta` names.
#[tokio::test]
async fn a_starting_tool_called_inside_a_record_keeps_none() {
    let h = ungated_start_harness().await;
    let token = h.session_for(h.conversation_execution).await;
    h.route(&token, Some(json!({ "conversation_id": CONVERSATION })))
        .await
        .expect("the start answers");
    h.call_as(h.conversation_execution, GATED_TOOL, h.args("b-1"))
        .await
        .expect("the start answers");
    let started = h.started();
    assert_eq!(started.len(), 2, "{started:?}");
    for (_, input) in &started {
        assert!(
            input.get("conversation_id").is_none(),
            "a starting tool called inside a record kept a conversation: {input}"
        );
    }
}

/// Clause 3: a gated call of an execution whose record names a conversation
/// stores it, and the user's list (the one `GET /v1/tool-approvals` reads)
/// answers it.
#[tokio::test]
async fn a_gated_call_of_a_run_started_from_a_conversation_stores_it_and_the_list_answers_it() {
    let h = harness().await;
    let value = direct(
        h.call_as(h.conversation_execution, GATED_TOOL, h.args("b-1"))
            .await,
    );
    assert_eq!(value["status"], "approval_pending", "{value}");
    let listed = h
        .approvals
        .list_for_user(&h.tenant, USER, None)
        .await
        .expect("the user's list answers");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(
        listed[0].conversation_id.as_deref(),
        Some(CONVERSATION),
        "the gated call of a run started from a conversation did not store the record's conversation id"
    );
}

/// Clauses 3 and 3a: a gated call of an execution whose record names no
/// conversation, or names one that is not a UUID, stores null.
#[tokio::test]
async fn a_gated_call_of_a_record_naming_none_stores_null() {
    let h = harness().await;
    for execution in [h.execution, h.garbled_conversation_execution] {
        direct(h.call_as(execution, GATED_TOOL, h.args("b-1")).await);
    }
    let rows = h.rows().await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in &rows {
        assert_eq!(
            row.conversation_id, None,
            "a gated call of a record naming no conversation stored one"
        );
    }
}

/// Clause 3 with clause 2: a record's conversation wins over a different
/// one in the call's `_meta`.
#[tokio::test]
async fn a_records_conversation_wins_over_a_different_meta_one() {
    let h = harness().await;
    let token = h.session_for(h.conversation_execution).await;
    h.route(
        &token,
        Some(json!({ "conversation_id": "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d" })),
    )
    .await
    .expect("the gated call answers");
    let rows = h.rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].conversation_id.as_deref(),
        Some(CONVERSATION),
        "the call's _meta conversation id was stored in place of its record's"
    );
}

// ---------------------------------------------------------------------------
// AEGIS ADR-139 N9: the schedule a gated call's run belongs to
// ---------------------------------------------------------------------------

/// A gated call of a run a schedule started is stored pending with that
/// schedule's id and read with its name; a call of a run no schedule
/// started stores none.
#[tokio::test]
async fn a_scheduled_runs_gated_call_is_stored_with_its_schedule_and_its_name() {
    let h = harness().await;
    let schedule = uuid::Uuid::new_v4();
    h.repo
        .bind_run_to_schedule(h.execution, schedule, "Morning digest")
        .await;

    pending_id(h.call(h.args("b-1")).await);
    pending_id(
        h.call_as(h.other_users_execution, GATED_TOOL, h.args("b-1"))
            .await,
    );

    let rows = h.rows().await;
    let by_execution = |execution: ExecutionId| {
        rows.iter()
            .find(|r| r.execution_id == execution)
            .map(|r| (r.status, r.schedule_id, r.schedule_name.clone()))
    };
    assert_eq!(
        (
            by_execution(h.execution),
            by_execution(h.other_users_execution)
        ),
        (
            Some((
                ToolApprovalStatus::Pending,
                Some(schedule),
                Some("Morning digest".to_string())
            )),
            Some((ToolApprovalStatus::Pending, None, None)),
        ),
        "a scheduled run's gated call was not stored pending with its schedule and name, \
         or an unscheduled one was given a schedule"
    );
}
