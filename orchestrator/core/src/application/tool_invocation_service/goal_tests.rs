//! Goals in the tool path (AEGIS ADR-131 D2, Update U6 and U8), driven
//! through the tool handlers of a real `ToolInvocationService` with a real
//! `GoalService` over the in-memory store: the three goal tools, and the
//! optional `goal_id` of a starting tool, refused for another user's goal or
//! a closed one before anything starts, and written on the execution it
//! starts. The judge's decisions are `goal_service.rs`'s tests.

use super::*;
use crate::application::goal_service::GoalService;
use crate::domain::agent::{Agent, AgentManifest};
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{Execution, ExecutionId, ExecutionInput, Iteration};
use crate::domain::goal::{BoundKind, GoalId, GoalRepository, GoalState};
use crate::domain::iam::{IdentityKind, TenantScope, UserIdentity, ZaruTier};
use crate::domain::node_config::GoalsConfig;
use crate::domain::repository::AgentVersion;
use crate::domain::security_context::SecurityContext;
use crate::infrastructure::event_bus::DomainEvent;
use crate::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
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

const OWNER: &str = "1a2b0000-owner";
const OTHER: &str = "3c4d0000-other";

fn identity(sub: &str) -> UserIdentity {
    UserIdentity {
        sub: sub.to_string(),
        realm_slug: "zaru-consumer".to_string(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Free,
            tenant_id: TenantId::for_consumer_user(sub).unwrap(),
        },
    }
}

fn scope(sub: &str) -> TenantScope {
    let id = identity(sub);
    TenantScope::new(TenantId::for_consumer_user(sub).unwrap(), id.identity_kind)
}

fn zaru_free() -> SecurityContext {
    SecurityContext {
        name: "zaru-free".to_string(),
        description: "consumer".to_string(),
        capabilities: vec![],
        deny_list: vec![],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

/// Starts an execution and keeps it, as the real service stores the row
/// before it answers.
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
        if let Some(identity) = identity {
            e.tenant_id = TenantId::for_consumer_user(&identity.sub).unwrap();
        }
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

/// Every agent name resolves to one agent, which has no manifest to read.
struct OneAgent(AgentId);

#[async_trait]
impl AgentLifecycleService for OneAgent {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: crate::domain::agent::AgentScope,
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
impl crate::domain::fsal::EventPublisher for NoOpPublisher {
    async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
}

struct Harness {
    service: ToolInvocationService,
    executions: Arc<Executions>,
    goals: Arc<InMemoryGoalRepository>,
}

fn harness() -> Harness {
    harness_with(|goals| goals)
}

/// The harness with its goal service shaped by `shape` (a shorter hold).
fn harness_with(shape: impl FnOnce(GoalService) -> GoalService) -> Harness {
    let router = Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers()));
    let storage_root =
        std::env::temp_dir().join(format!("aegis-goal-tests-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let event_bus = Arc::new(EventBus::new(256));
    let executions = Arc::new(Executions::default());
    let goals = Arc::new(InMemoryGoalRepository::new());
    let service = ToolInvocationService::new(
        Arc::new(InMemorySealSessionRepository::new()),
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new()),
        Arc::new(SealMiddleware::new()),
        router,
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(AgentId::new())),
        executions.clone(),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
        event_bus.clone(),
        None,
    )
    .with_goals(Arc::new(shape(GoalService::new(
        goals.clone(),
        event_bus,
        GoalsConfig::default(),
    ))));
    Harness {
        service,
        executions,
        goals,
    }
}

impl Harness {
    async fn create(&self, sub: &str) -> GoalId {
        let id = identity(sub);
        let answer = self
            .service
            .invoke_aegis_goal_create_tool(
                &json!({
                    "statement": "Create palindrome-checker, then run it on \"racecar\".",
                    "client_ref": "conversation-1",
                    "channel": "web",
                }),
                Some(&id),
                &scope(sub),
            )
            .await
            .unwrap();
        let ToolInvocationResult::Direct(answer) = answer else {
            panic!("aegis.goal.create answers directly");
        };
        GoalId::from_string(answer["goal_id"].as_str().unwrap()).unwrap()
    }

    async fn task_execute(&self, sub: &str, goal_id: GoalId) -> Value {
        let id = identity(sub);
        let mut args = json!({
            "agent_id": "palindrome-checker",
            "input": {"word": "racecar"},
            "goal_id": goal_id.to_string(),
        });
        match self
            .service
            .invoke_aegis_task_execute_tool(&mut args, &zaru_free(), Some(&id), &scope(sub))
            .await
            .unwrap()
        {
            ToolInvocationResult::Direct(v) => v,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn started(&self) -> usize {
        self.executions.started.lock().unwrap().len()
    }

    /// Change the stored execution `id` as the supervisor would.
    fn with_execution<T>(&self, id: ExecutionId, change: impl FnOnce(&mut Execution) -> T) -> T {
        change(
            self.executions
                .started
                .lock()
                .unwrap()
                .get_mut(&id)
                .unwrap(),
        )
    }

    async fn evaluate(&self, sub: &str, goal: GoalId, round: Option<u32>) -> Value {
        let id = identity(sub);
        let mut args = json!({"goal_id": goal.to_string(), "companion_answer": "Running it."});
        if let Some(round) = round {
            args["round"] = json!(round);
        }
        match self
            .service
            .invoke_aegis_goal_evaluate_tool(&args, &zaru_free(), Some(&id), &scope(sub))
            .await
            .unwrap()
        {
            ToolInvocationResult::Direct(v) => v,
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// A goal-judge verdict as the judge's last iteration writes it.
fn verdict_output(score: f64) -> String {
    format!(
        "```json\n{}\n```",
        json!({
            "score": score,
            "confidence": 0.9,
            "reasoning": "The word was checked and the result reached the user.",
            "signals": [
                {"category": "delivered", "score": score, "message": "m"},
                {"category": "evidence", "score": score, "message": "m"},
                {"category": "chain", "score": score, "message": "m"},
                {"category": "feasibility", "score": 0.9, "message": "m"},
                {"category": "alignment", "score": score, "message": "m"},
            ],
        })
    )
}

/// AEGIS ADR-131 U21 to U24, through the real types the daemon uses: the
/// tool handler, the goal service, and the world it judges in
/// (`GoalCallWorld`) reading a real `Execution` from the execution service.
/// Running inside its recorded limit, the evaluation answers judging with
/// `waiting_on: "execution"` and `wait_until` its start plus its limit plus
/// the reaper's 600 s; with no limit recorded, the node's 1,800 s plus
/// 600 s; ended, one verdict from one goal-judge execution.
#[tokio::test]
async fn a_real_running_execution_holds_the_round_to_its_recorded_limit_then_one_verdict() {
    use crate::domain::execution::REAPER_MARGIN_SECONDS;
    use crate::domain::goal::wait_time_text;
    use crate::domain::supervisor::DEFAULT_EXECUTION_TIMEOUT_SECONDS;

    for limit in [Some(300u64), None] {
        let h = harness_with(|goals| {
            goals.with_wait(
                std::time::Duration::from_millis(60),
                std::time::Duration::from_millis(5),
            )
        });
        let goal = h.create(OWNER).await;
        let started = h.task_execute(OWNER, goal).await;
        let solver =
            ExecutionId(uuid::Uuid::parse_str(started["execution_id"].as_str().unwrap()).unwrap());
        let started_at = h.with_execution(solver, |e| {
            e.start();
            e.timeout_seconds = limit;
            e.started_at
        });
        let bound = limit.unwrap_or(DEFAULT_EXECUTION_TIMEOUT_SECONDS) + REAPER_MARGIN_SECONDS;

        let answer = h.evaluate(OWNER, goal, None).await;
        assert_eq!(answer["state"], "judging", "{limit:?}: {answer}");
        assert_eq!(answer["waiting_on"], "execution");
        assert_eq!(answer["execution_ids"], json!([solver.to_string()]));
        assert_eq!(
            answer["wait_until"],
            wait_time_text(started_at + chrono::Duration::seconds(bound as i64)),
            "{limit:?}: start plus limit plus the reaper's margin"
        );
        assert_eq!(h.started(), 1, "no goal-judge while the work runs");

        // The work ends: the round's judge starts.
        h.with_execution(solver, |e| {
            e.start_iteration("check the word".to_string()).unwrap();
            e.complete_iteration("racecar is a palindrome".to_string());
            e.complete();
        });
        let answer = h.evaluate(OWNER, goal, Some(0)).await;
        assert_eq!(answer["state"], "judging", "{answer}");
        assert!(
            answer.get("waiting_on").is_none(),
            "the judge runs: {answer}"
        );
        assert_eq!(h.started(), 2, "one goal-judge execution");
        let judge = *h
            .executions
            .started
            .lock()
            .unwrap()
            .keys()
            .find(|id| **id != solver)
            .unwrap();
        h.with_execution(judge, |e| {
            e.start();
            e.start_iteration("judge".to_string()).unwrap();
            e.complete_iteration(verdict_output(0.95));
            e.complete();
        });
        let answer = h.evaluate(OWNER, goal, Some(0)).await;
        assert_eq!(answer["outcome"], "met", "{answer}");
        assert_eq!(answer["state"], "met");
        assert_eq!(h.started(), 2, "one verdict, one judge");
        let evaluations = h.goals.list_evaluations(goal).await.unwrap();
        let waits: Vec<_> = evaluations.iter().filter(|e| e.is_wait()).collect();
        assert_eq!(waits.len(), 1);
        assert!(waits[0].decided_at.is_some(), "the wait ended");
        assert_eq!(evaluations.iter().filter(|e| e.decides_round()).count(), 1);
    }
}

/// ADR-131 clause 1: `goal_not_open` for another user's goal, before
/// anything starts.
#[tokio::test]
async fn a_starting_tool_refuses_another_users_goal_with_goal_not_open() {
    let h = harness();
    let goal = h.create(OWNER).await;
    let answer = h.task_execute(OTHER, goal).await;
    assert_eq!(answer["error"], "goal_not_open", "{answer}");
    assert_eq!(h.started(), 0, "nothing started");
    assert!(h.goals.list_bound(goal).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_starting_tool_refuses_a_closed_goal_with_goal_not_open() {
    let h = harness();
    let first = h.create(OWNER).await;
    // A second goal under the same client_ref supersedes the first (D2).
    let _second = h.create(OWNER).await;
    assert_eq!(
        h.goals.find_goal(first).await.unwrap().unwrap().state,
        GoalState::Superseded
    );
    let answer = h.task_execute(OWNER, first).await;
    assert_eq!(answer["error"], "goal_not_open");
    assert_eq!(h.started(), 0);
}

#[tokio::test]
async fn a_starting_tool_writes_the_goal_on_the_execution_it_starts() {
    let h = harness();
    let goal = h.create(OWNER).await;
    let answer = h.task_execute(OWNER, goal).await;
    assert_eq!(answer["status"], "started", "{answer}");
    let execution = answer["execution_id"].as_str().unwrap();
    let bound = h.goals.list_bound(goal).await.unwrap();
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].execution_id.to_string(), execution);
    assert_eq!(bound[0].kind, BoundKind::Agent);

    // aegis.goal.status lists it, read-only.
    let id = identity(OWNER);
    let ToolInvocationResult::Direct(status) = h
        .service
        .invoke_aegis_goal_status_tool(
            &json!({"goal_id": goal.to_string()}),
            &zaru_free(),
            Some(&id),
            &scope(OWNER),
        )
        .await
        .unwrap()
    else {
        panic!("aegis.goal.status answers directly");
    };
    assert_eq!(status["state"], "open");
    assert_eq!(status["executions"][0]["execution_id"], execution);
    assert_eq!(status["executions"][0]["kind"], "agent");
    assert_eq!(status["verdicts"], json!([]));
}

#[tokio::test]
async fn evaluate_refuses_another_users_goal_and_a_round_neither_decided_nor_current() {
    let h = harness();
    let goal = h.create(OWNER).await;
    let other = identity(OTHER);
    let ToolInvocationResult::Direct(answer) = h
        .service
        .invoke_aegis_goal_evaluate_tool(
            &json!({"goal_id": goal.to_string(), "companion_answer": "done"}),
            &zaru_free(),
            Some(&other),
            &scope(OTHER),
        )
        .await
        .unwrap()
    else {
        panic!("answers directly");
    };
    assert_eq!(answer["error"], "goal_not_open");

    let owner = identity(OWNER);
    let ToolInvocationResult::Direct(answer) = h
        .service
        .invoke_aegis_goal_evaluate_tool(
            &json!({"goal_id": goal.to_string(), "companion_answer": "done", "round": 2}),
            &zaru_free(),
            Some(&owner),
            &scope(OWNER),
        )
        .await
        .unwrap()
    else {
        panic!("answers directly");
    };
    assert_eq!(answer["error"], "goal_round_mismatch");
}

#[tokio::test]
async fn a_goal_belongs_to_a_user_so_a_call_without_one_is_refused() {
    let h = harness();
    let err = h
        .service
        .invoke_aegis_goal_create_tool(
            &json!({"statement": "s", "client_ref": "c", "channel": "web"}),
            None,
            &scope(OWNER),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, SealSessionError::InvalidArguments(_)),
        "{err}"
    );
}

/// U6: the four starting tools accept `goal_id` and never advertise it, so
/// no model is shown it; the three goal tools are listed.
#[tokio::test]
async fn goal_id_is_not_advertised_by_the_four_starting_tools() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let tools = router.list_tools().await.unwrap();
    for name in [
        "aegis.agent.generate",
        "aegis.task.execute",
        "aegis.workflow.generate",
        "aegis.execute.intent",
    ] {
        let tool = tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} is listed"));
        assert!(
            !tool.input_schema.to_string().contains("goal_id"),
            "{name} advertises goal_id"
        );
    }
    for name in [
        "aegis.goal.create",
        "aegis.goal.evaluate",
        "aegis.goal.status",
    ] {
        let tool = tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} is listed"));
        assert_eq!(tool.input_schema["type"], "object");
    }
    let evaluate = tools
        .iter()
        .find(|t| t.name == "aegis.goal.evaluate")
        .unwrap();
    assert_eq!(
        evaluate.input_schema["properties"]["round"]["type"],
        "integer"
    );
}
