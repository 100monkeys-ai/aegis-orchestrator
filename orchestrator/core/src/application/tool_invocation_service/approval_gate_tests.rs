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
    ApprovalContract, ToolApprovalDecision, ToolApprovalId, ToolApprovalRepository,
    ToolApprovalRequest, ToolApprovalStatus,
};
use crate::infrastructure::event_bus::DomainEvent;
use crate::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use crate::infrastructure::repositories::InMemoryVolumeRepository;
use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use crate::infrastructure::storage::LocalHostStorageProvider;
use crate::infrastructure::tool_router::{InMemoryToolRegistry, ToolRouter};
use async_trait::async_trait;
use futures::Stream;
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;

const GATED_TOOL: &str = "aegis.task.execute";
const USER: &str = "user-1";

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

struct Harness {
    service: ToolInvocationService,
    approvals: Arc<ToolApprovalService>,
    repo: Arc<InMemoryToolApprovalRepository>,
    event_bus: Arc<EventBus>,
    started: Arc<StdMutex<Vec<Started>>>,
    tenant: TenantId,
    agent_id: AgentId,
    /// The execution whose initiating user is `USER`.
    execution: ExecutionId,
    /// An execution of the same tenant whose initiating user is `user-2`.
    other_users_execution: ExecutionId,
    /// An execution of the same tenant with no initiating user.
    userless_execution: ExecutionId,
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
    let tenant = TenantId::for_consumer_user(USER).unwrap();
    let agent = agent();
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
            "gate-test-context".to_string(),
        );
        e.tenant_id = tenant.clone();
        e.initiating_user_sub = user.map(str::to_string);
        e
    };
    let executions: Vec<Execution> = vec![
        execution_for(Some(USER)),
        execution_for(Some("user-2")),
        execution_for(None),
    ];
    let (execution, other_users_execution, userless_execution) =
        (executions[0].id, executions[1].id, executions[2].id);
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

    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    let router = Arc::new(ToolRouter::new(
        registry,
        servers,
        dispatchers_gating(GATED_TOOL, &contract),
    ));
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
    let service = ToolInvocationService::new(
        Arc::new(InMemorySealSessionRepository::new()),
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
        approvals,
        repo,
        event_bus,
        started,
        tenant,
        agent_id,
        execution,
        other_users_execution,
        userless_execution,
        target_agent: agent.id.to_string(),
    }
}

async fn harness() -> Harness {
    harness_with("aegis.*", Arc::new(InMemoryToolApprovalRepository::new())).await
}

impl Harness {
    fn args(&self, account: &str) -> Value {
        json!({ "agent_id": self.target_agent, "account": account, "input": { "note": "the stored arguments" } })
    }

    async fn call_as(
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

    async fn rows(&self) -> Vec<ToolApprovalRequest> {
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

fn direct(result: Result<ToolInvocationResult, SealSessionError>) -> Value {
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
