// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `GET /v1/workflow-gates` (`status=pending`, `answered` and `any`) driven
//! over HTTP through the daemon's authentication stack. One world of runs is seeded through the
//! repository traits, as the Temporal listener persists them: once on an
//! in-memory trail (the repository trait's default query) and once on the
//! real PostgreSQL repositories with every migration applied (the Postgres
//! query), when `AEGIS_TEST_POSTGRES_URL` names a server.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionStatus};
use aegis_orchestrator_core::domain::repository::{
    RepositoryError, WorkflowExecutionRepository, WorkflowRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowExecution,
    WorkflowExecutionEventRecord, WorkflowMetadata, WorkflowSpec, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_workflow::PostgresWorkflowRepository;
use aegis_orchestrator_core::infrastructure::repositories::postgres_workflow_execution::PostgresWorkflowExecutionRepository;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use aegis_orchestrator_core::infrastructure::TemporalEventPayload;
use reqwest::Method;
use serde_json::{json, Value};

use super::{workflow_gates_router, WorkflowGatesState};
use crate::daemon::handlers::test_support::{
    consumer, identity_provider, send, serve, service_account, tenant_user,
};

const OWNER: &str = "owner-sub";
const OTHER: &str = "other-sub";
const MEMBER_A: &str = "member-a";
const MEMBER_B: &str = "member-b";
const TEAM: &str = "acme";
const READ: &str = "workflow:read";

/// The in-memory execution repository with an event trail: it stores what
/// `append_event` is given and pages it back in sequence order. It does not
/// override `find_running_with_latest_event_for_tenant`, so the trait's
/// default answers.
struct TrailRepository {
    executions: InMemoryWorkflowExecutionRepository,
    events: Mutex<HashMap<ExecutionId, Vec<WorkflowExecutionEventRecord>>>,
}

#[async_trait::async_trait]
impl WorkflowExecutionRepository for TrailRepository {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution: &WorkflowExecution,
    ) -> Result<(), RepositoryError> {
        self.executions.save_for_tenant(tenant_id, execution).await
    }

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Option<WorkflowExecution>, RepositoryError> {
        self.executions.find_by_id_for_tenant(tenant_id, id).await
    }

    async fn find_active_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.executions.find_active_for_tenant(tenant_id).await
    }

    async fn find_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: aegis_orchestrator_core::domain::workflow::WorkflowId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.executions
            .find_by_workflow_for_tenant(tenant_id, workflow_id, limit, offset)
            .await
    }

    async fn list_paginated_for_tenant(
        &self,
        tenant_id: &TenantId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.executions
            .list_paginated_for_tenant(tenant_id, limit, offset)
            .await
    }

    async fn list_paginated_all(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.executions.list_paginated_all(limit, offset).await
    }

    async fn count_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: aegis_orchestrator_core::domain::workflow::WorkflowId,
    ) -> Result<i64, RepositoryError> {
        self.executions
            .count_by_workflow_for_tenant(tenant_id, workflow_id)
            .await
    }

    async fn update_temporal_linkage_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution_id: ExecutionId,
        _temporal_workflow_id: &str,
        _temporal_run_id: &str,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn append_event(
        &self,
        execution_id: ExecutionId,
        sequence_number: i64,
        event_type: String,
        payload: Value,
        iteration_number: Option<u8>,
    ) -> Result<(), RepositoryError> {
        let mut events = self.events.lock().unwrap();
        let trail = events.entry(execution_id).or_default();
        if trail.iter().all(|event| event.sequence != sequence_number) {
            trail.push(WorkflowExecutionEventRecord {
                sequence: sequence_number,
                event_type,
                state_name: None,
                iteration_number,
                payload,
                recorded_at: chrono::Utc::now(),
            });
        }
        Ok(())
    }

    async fn find_events_by_execution(
        &self,
        id: ExecutionId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecutionEventRecord>, RepositoryError> {
        let mut trail = self
            .events
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_default();
        trail.sort_by_key(|event| event.sequence);
        Ok(trail.into_iter().skip(offset).take(limit).collect())
    }

    async fn find_tenant_id_by_execution(
        &self,
        id: ExecutionId,
    ) -> Result<Option<TenantId>, RepositoryError> {
        self.executions.find_tenant_id_by_execution(id).await
    }
}

fn forge() -> Workflow {
    let mut states = HashMap::new();
    states.insert(
        StateName::new("AWAIT_REQUIREMENTS_APPROVAL").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo gate".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::Always,
                target: StateName::new("END").unwrap(),
                feedback: None,
            }],
            timeout: None,
            max_state_visits: None,
        },
    );
    states.insert(
        StateName::new("END").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo end".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
            max_state_visits: None,
        },
    );
    Workflow::new(
        WorkflowMetadata {
            name: "the-forge".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
            output_schema: None,
            output_template: None,
        },
        WorkflowSpec {
            initial_state: StateName::new("AWAIT_REQUIREMENTS_APPROVAL").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
            max_total_transitions: None,
            default_schedule: None,
            repositories: None,
        },
    )
    .unwrap()
}

/// An event as the listener persists it: the worker's body, read into the
/// payload struct and written back whole.
fn persisted(
    execution: ExecutionId,
    workflow: &Workflow,
    sequence: i64,
    event_type: &str,
    at: &str,
    extra: Value,
) -> Value {
    let mut body = json!({
        "event_type": event_type,
        "execution_id": execution.to_string(),
        "temporal_sequence_number": sequence,
        "workflow_id": workflow.id.to_string(),
        "timestamp": at,
    });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    let payload: TemporalEventPayload = serde_json::from_value(body).unwrap();
    serde_json::to_value(&payload).unwrap()
}

/// What the world holds, by run.
struct World {
    base: String,
    waiting: ExecutionId,
    reposted: ExecutionId,
    past: ExecutionId,
    ended: ExecutionId,
    others: ExecutionId,
    member_b: ExecutionId,
    answered_yes: ExecutionId,
    answered_no: ExecutionId,
    timed_out: ExecutionId,
    others_answered: ExecutionId,
}

/// Seed the runs through the repositories and serve the route over them.
async fn world(
    executions: Arc<dyn WorkflowExecutionRepository>,
    workflows: Arc<dyn WorkflowRepository>,
) -> World {
    let owner_tenant = TenantId::for_consumer_user(OWNER).unwrap();
    let other_tenant = TenantId::for_consumer_user(OTHER).unwrap();
    let team_tenant = TenantId::from_realm_slug(TEAM).unwrap();
    // One workflow of the name per tenant: a workflow's id is unique.
    let mut forges = HashMap::new();
    for tenant in [&owner_tenant, &other_tenant, &team_tenant] {
        let workflow = forge();
        workflows.save_for_tenant(tenant, &workflow).await.unwrap();
        forges.insert(tenant.clone(), workflow);
    }

    // (tenant, starter, status, events as (sequence, type, at, extra))
    type Event = (i64, &'static str, &'static str, Value);
    let gate = |prompt: &str| {
        json!({
            "state_name": "AWAIT_REQUIREMENTS_APPROVAL",
            "prompt": prompt,
            "responses": ["yes", "no"],
            "expires_at": "2026-10-13T07:00:00.000Z",
        })
    };
    let entered = json!({ "state_name": "AWAIT_REQUIREMENTS_APPROVAL" });
    let runs: Vec<(TenantId, Option<&str>, ExecutionStatus, Vec<Event>)> = vec![
        // waiting: its latest event is the gate
        (
            owner_tenant.clone(),
            Some(OWNER),
            ExecutionStatus::Running,
            vec![
                (
                    1,
                    "WorkflowExecutionStarted",
                    "2026-10-13T05:58:00.000Z",
                    json!({}),
                ),
                (
                    2,
                    "WorkflowStateEntered",
                    "2026-10-13T05:59:00.000Z",
                    entered.clone(),
                ),
                (3, "HumanInputRequested", "2026-10-13T06:00:00.000Z", {
                    let mut g = gate("Approve the requirements?");
                    g["default_response"] = json!("no");
                    g
                }),
            ],
        ),
        // reposted: the gate posted again after a refused word
        (
            owner_tenant.clone(),
            Some(OWNER),
            ExecutionStatus::Running,
            vec![
                (
                    1,
                    "WorkflowExecutionStarted",
                    "2026-10-13T06:01:00.000Z",
                    json!({}),
                ),
                (
                    2,
                    "WorkflowStateEntered",
                    "2026-10-13T06:02:00.000Z",
                    entered.clone(),
                ),
                (
                    3,
                    "HumanInputRequested",
                    "2026-10-13T06:03:00.000Z",
                    gate("Approve?"),
                ),
                (
                    5,
                    "HumanInputRequested",
                    "2026-10-13T06:10:00.000Z",
                    gate(
                        "\"maybe\" is not an answer this gate takes; answer yes or no.\n\nApprove?",
                    ),
                ),
            ],
        ),
        // past: the run went on after its gate
        (
            owner_tenant.clone(),
            Some(OWNER),
            ExecutionStatus::Running,
            vec![
                (
                    1,
                    "WorkflowExecutionStarted",
                    "2026-10-13T05:00:00.000Z",
                    json!({}),
                ),
                (
                    2,
                    "HumanInputRequested",
                    "2026-10-13T05:01:00.000Z",
                    gate("Approve?"),
                ),
                (
                    3,
                    "WorkflowStateEntered",
                    "2026-10-13T05:02:00.000Z",
                    json!({ "state_name": "END" }),
                ),
            ],
        ),
        // ended: the record ended with the gate its last event
        (
            owner_tenant.clone(),
            Some(OWNER),
            ExecutionStatus::Failed,
            vec![
                (
                    1,
                    "WorkflowExecutionStarted",
                    "2026-10-13T04:00:00.000Z",
                    json!({}),
                ),
                (
                    2,
                    "HumanInputRequested",
                    "2026-10-13T04:01:00.000Z",
                    gate("Approve?"),
                ),
            ],
        ),
        // another tenant's waiting run
        (
            other_tenant.clone(),
            Some(OTHER),
            ExecutionStatus::Running,
            vec![(
                1,
                "HumanInputRequested",
                "2026-10-13T06:05:00.000Z",
                gate("Theirs?"),
            )],
        ),
        // a waiting run in a shared tenant, started by member B
        (
            team_tenant.clone(),
            Some(MEMBER_B),
            ExecutionStatus::Running,
            vec![(
                1,
                "HumanInputRequested",
                "2026-10-13T06:06:00.000Z",
                gate("B's?"),
            )],
        ),
        // a waiting run in the shared tenant that records no starter
        (
            team_tenant.clone(),
            None,
            ExecutionStatus::Running,
            vec![(
                1,
                "HumanInputRequested",
                "2026-10-13T06:07:00.000Z",
                gate("Nobody's?"),
            )],
        ),
        // answered yes after a refused word: the gate posted twice, then left
        // with the word, and the run went on to its next state
        (
            owner_tenant.clone(),
            Some(OWNER),
            ExecutionStatus::Running,
            vec![
                (
                    1,
                    "WorkflowExecutionStarted",
                    "2026-10-13T06:19:00.000Z",
                    json!({}),
                ),
                (
                    2,
                    "WorkflowStateEntered",
                    "2026-10-13T06:19:30.000Z",
                    entered.clone(),
                ),
                (
                    3,
                    "HumanInputRequested",
                    "2026-10-13T06:20:00.000Z",
                    gate("Approve the design?"),
                ),
                (
                    4,
                    "HumanInputRequested",
                    "2026-10-13T06:22:00.000Z",
                    gate(
                        "\"ok\" is not an answer this gate takes; answer yes or no.\n\nApprove the design?",
                    ),
                ),
                (
                    5,
                    "WorkflowStateExited",
                    "2026-10-13T06:25:00.000Z",
                    json!({
                        "state_name": "AWAIT_REQUIREMENTS_APPROVAL",
                        "output": { "response": "yes", "timeout": false },
                    }),
                ),
                (
                    6,
                    "WorkflowStateEntered",
                    "2026-10-13T06:25:01.000Z",
                    json!({ "state_name": "ARCHITECT" }),
                ),
            ],
        ),
        // answered no with feedback, and the run ended on the answer
        (
            owner_tenant.clone(),
            Some(OWNER),
            ExecutionStatus::Completed,
            vec![
                (
                    1,
                    "WorkflowExecutionStarted",
                    "2026-10-13T05:29:00.000Z",
                    json!({}),
                ),
                (
                    2,
                    "HumanInputRequested",
                    "2026-10-13T05:30:00.000Z",
                    gate("Ship it?"),
                ),
                (
                    3,
                    "WorkflowStateExited",
                    "2026-10-13T05:40:00.000Z",
                    json!({
                        "state_name": "AWAIT_REQUIREMENTS_APPROVAL",
                        "output": {
                            "response": "no",
                            "feedback": "Narrow the scope.",
                            "timeout": false,
                        },
                    }),
                ),
                (
                    4,
                    "WorkflowExecutionCompleted",
                    "2026-10-13T05:40:01.000Z",
                    json!({}),
                ),
            ],
        ),
        // nobody answered: the gate closed on its timeout and failed the run
        (
            owner_tenant.clone(),
            Some(OWNER),
            ExecutionStatus::Failed,
            vec![
                (
                    1,
                    "WorkflowExecutionStarted",
                    "2026-10-13T04:29:00.000Z",
                    json!({}),
                ),
                (
                    2,
                    "HumanInputRequested",
                    "2026-10-13T04:30:00.000Z",
                    gate("Anyone?"),
                ),
                (
                    3,
                    "WorkflowExecutionFailed",
                    "2026-10-13T05:30:00.000Z",
                    json!({ "error": "Human input timeout" }),
                ),
            ],
        ),
        // another tenant's answered gate
        (
            other_tenant.clone(),
            Some(OTHER),
            ExecutionStatus::Completed,
            vec![
                (
                    1,
                    "HumanInputRequested",
                    "2026-10-13T06:30:00.000Z",
                    gate("Theirs, answered?"),
                ),
                (
                    2,
                    "WorkflowStateExited",
                    "2026-10-13T06:31:00.000Z",
                    json!({
                        "state_name": "AWAIT_REQUIREMENTS_APPROVAL",
                        "output": { "response": "yes", "timeout": false },
                    }),
                ),
                (
                    3,
                    "WorkflowExecutionCompleted",
                    "2026-10-13T06:31:01.000Z",
                    json!({}),
                ),
            ],
        ),
    ];

    let mut ids = Vec::new();
    for (tenant, starter, status, events) in runs {
        let id = ExecutionId::new();
        let workflow = &forges[&tenant];
        let mut execution = WorkflowExecution::new(workflow, id, json!({}));
        execution.tenant_id = tenant.clone();
        execution.status = status;
        execution.initiating_user_sub = starter.map(str::to_string);
        executions
            .save_for_tenant(&tenant, &execution)
            .await
            .unwrap();
        for (sequence, event_type, at, extra) in events {
            executions
                .append_event(
                    id,
                    sequence,
                    event_type.to_string(),
                    persisted(id, workflow, sequence, event_type, at, extra),
                    None,
                )
                .await
                .unwrap();
        }
        ids.push(id);
    }

    let iam = identity_provider(&[
        ("owner", consumer(OWNER), READ),
        ("other", consumer(OTHER), READ),
        ("member-a", tenant_user(MEMBER_A, TEAM), READ),
        ("member-b", tenant_user(MEMBER_B, TEAM), READ),
        ("owner-no-scope", consumer(OWNER), "workflow:signal"),
        ("service", service_account(), READ),
    ]);
    let base = serve(
        workflow_gates_router(WorkflowGatesState {
            executions,
            workflows,
        }),
        Some(iam),
        None,
    )
    .await;
    World {
        base,
        waiting: ids[0],
        reposted: ids[1],
        past: ids[2],
        ended: ids[3],
        others: ids[4],
        member_b: ids[5],
        answered_yes: ids[7],
        answered_no: ids[8],
        timed_out: ids[9],
        others_answered: ids[10],
    }
}

async fn in_memory_world() -> World {
    world(
        Arc::new(TrailRepository {
            executions: InMemoryWorkflowExecutionRepository::new(),
            events: Mutex::new(HashMap::new()),
        }),
        Arc::new(InMemoryWorkflowRepository::new()),
    )
    .await
}

/// The caller's pending gates, by execution id.
async fn gates(world: &World, token: &str) -> Vec<Value> {
    listing(world, token, "status=pending").await
}

/// The caller's gates for a query string such as `status=answered`.
async fn listing(world: &World, token: &str, query: &str) -> Vec<Value> {
    let (status, answer) = send(
        &world.base,
        &Method::GET,
        &format!("/v1/workflow-gates?{query}"),
        &None,
        Some(token),
    )
    .await;
    assert_eq!(
        status, 200,
        "the gates ({query}) were not answered: {answer}"
    );
    let listed = answer["gates"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        answer["count"],
        json!(listed.len()),
        "count is not the number of gates: {answer}"
    );
    listed
}

fn ids(listed: &[Value]) -> Vec<String> {
    listed
        .iter()
        .map(|gate| {
            gate["execution_id"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn gate_of<'a>(listed: &'a [Value], id: ExecutionId) -> Option<&'a Value> {
    listed
        .iter()
        .find(|gate| gate["execution_id"] == json!(id.to_string()))
}

/// A waiting run is listed with its workflow, state, prompt, the words its
/// gate takes, its default and its deadline.
#[tokio::test]
async fn a_waiting_run_is_listed_with_its_prompt_words_and_deadline() {
    let world = in_memory_world().await;
    let listed = gates(&world, "owner").await;
    let gate = gate_of(&listed, world.waiting)
        .unwrap_or_else(|| panic!("the waiting run was not listed: {listed:?}"));
    assert_eq!(
        gate,
        &json!({
            "execution_id": world.waiting.to_string(),
            "workflow_name": "the-forge",
            "state_name": "AWAIT_REQUIREMENTS_APPROVAL",
            "prompt": "Approve the requirements?",
            "responses": ["yes", "no"],
            "default_response": "no",
            "expires_at": "2026-10-13T07:00:00+00:00",
            "sequence": 3,
            "requested_at": "2026-10-13T06:00:00+00:00",
        }),
        "the waiting run's gate is not as the worker posted it"
    );
}

/// A run whose latest event follows its gate is past it, and is not listed.
#[tokio::test]
async fn a_run_past_its_gate_is_not_listed() {
    let world = in_memory_world().await;
    let listed = gates(&world, "owner").await;
    assert!(
        gate_of(&listed, world.waiting).is_some(),
        "the waiting run was not listed: {listed:?}"
    );
    assert!(
        gate_of(&listed, world.past).is_none(),
        "a run past its gate was listed: {listed:?}"
    );
}

/// An ended run is not listed, though its last event is a gate.
#[tokio::test]
async fn an_ended_run_is_not_listed() {
    let world = in_memory_world().await;
    let listed = gates(&world, "owner").await;
    assert!(
        gate_of(&listed, world.waiting).is_some(),
        "the waiting run was not listed: {listed:?}"
    );
    assert!(
        gate_of(&listed, world.ended).is_none(),
        "an ended run was listed: {listed:?}"
    );
}

/// Another tenant's waiting run is not listed; its own person sees it.
#[tokio::test]
async fn another_tenants_run_is_not_listed() {
    let world = in_memory_world().await;
    let owners = gates(&world, "owner").await;
    assert!(
        gate_of(&owners, world.others).is_none(),
        "another tenant's run was listed: {owners:?}"
    );
    assert_eq!(
        ids(&gates(&world, "other").await),
        vec![world.others.to_string()],
        "the other person's own gate was not listed to them alone"
    );
}

/// A gate posted again after a refused word is listed once, with the
/// latest sequence and prompt.
#[tokio::test]
async fn a_reposted_gate_is_listed_once_with_its_latest_sequence() {
    let world = in_memory_world().await;
    let listed = gates(&world, "owner").await;
    let reposted: Vec<&Value> = listed
        .iter()
        .filter(|gate| gate["execution_id"] == json!(world.reposted.to_string()))
        .collect();
    assert_eq!(
        reposted.len(),
        1,
        "the reposted gate was not listed once: {listed:?}"
    );
    assert_eq!(reposted[0]["sequence"], json!(5));
    assert_eq!(
        reposted[0]["prompt"],
        json!("\"maybe\" is not an answer this gate takes; answer yes or no.\n\nApprove?")
    );
    assert_eq!(
        ids(&listed),
        vec![world.reposted.to_string(), world.waiting.to_string()],
        "the owner's gates are not exactly theirs, newest first"
    );
}

/// In a tenant several people share, a run is listed only to the person
/// who started it; a run that records no starter is listed to nobody.
#[tokio::test]
async fn a_run_another_person_started_in_the_tenant_is_not_listed() {
    let world = in_memory_world().await;
    assert_eq!(
        ids(&gates(&world, "member-a").await),
        Vec::<String>::new(),
        "member A was shown runs they did not start"
    );
    assert_eq!(
        ids(&gates(&world, "member-b").await),
        vec![world.member_b.to_string()],
        "member B was not shown their own run alone"
    );
}

/// The route needs `workflow:read` and a person, and a status it knows.
#[tokio::test]
async fn the_route_needs_workflow_read_a_person_and_a_known_status() {
    let world = in_memory_world().await;
    let mut answers = Vec::new();
    for (token, path) in [
        (Some("owner-no-scope"), "/v1/workflow-gates?status=pending"),
        (Some("service"), "/v1/workflow-gates?status=pending"),
        (None, "/v1/workflow-gates?status=pending"),
        (Some("owner"), "/v1/workflow-gates?status=closed"),
        (Some("owner"), "/v1/workflow-gates"),
    ] {
        let (status, _) = send(&world.base, &Method::GET, path, &None, token).await;
        answers.push((token, path, status));
    }
    assert_eq!(
        answers,
        vec![
            (
                Some("owner-no-scope"),
                "/v1/workflow-gates?status=pending",
                403
            ),
            (Some("service"), "/v1/workflow-gates?status=pending", 403),
            (None, "/v1/workflow-gates?status=pending", 401),
            (Some("owner"), "/v1/workflow-gates?status=closed", 400),
            (Some("owner"), "/v1/workflow-gates", 200),
        ]
    );
}

/// The answered gate of `world.answered_yes`, as the route lists it.
fn answered_yes_view(world: &World) -> Value {
    json!({
        "execution_id": world.answered_yes.to_string(),
        "workflow_name": "the-forge",
        "state_name": "AWAIT_REQUIREMENTS_APPROVAL",
        "prompt": "\"ok\" is not an answer this gate takes; answer yes or no.\n\nApprove the design?",
        "responses": ["yes", "no"],
        "response": "yes",
        "feedback": null,
        "answered_at": "2026-10-13T06:25:00+00:00",
        "answered_by": null,
        "sequence": 4,
        "requested_at": "2026-10-13T06:22:00+00:00",
        "outcome": "went_on",
    })
}

/// An answered gate is listed under `answered` with the word chosen, when,
/// the feedback, and whether the run went on or ended; newest first.
#[tokio::test]
async fn an_answered_gate_is_listed_with_its_word_and_time() {
    let world = in_memory_world().await;
    let listed = listing(&world, "owner", "status=answered").await;
    assert_eq!(
        gate_of(&listed, world.answered_yes),
        Some(&answered_yes_view(&world)),
        "the gate answered yes is not listed with its word and time: {listed:?}"
    );
    assert_eq!(
        gate_of(&listed, world.answered_no),
        Some(&json!({
            "execution_id": world.answered_no.to_string(),
            "workflow_name": "the-forge",
            "state_name": "AWAIT_REQUIREMENTS_APPROVAL",
            "prompt": "Ship it?",
            "responses": ["yes", "no"],
            "response": "no",
            "feedback": "Narrow the scope.",
            "answered_at": "2026-10-13T05:40:00+00:00",
            "answered_by": null,
            "sequence": 2,
            "requested_at": "2026-10-13T05:30:00+00:00",
            "outcome": "ended",
        })),
        "the gate answered no is not listed with its word, feedback and end: {listed:?}"
    );
    assert_eq!(
        ids(&listed),
        vec![
            world.answered_yes.to_string(),
            world.answered_no.to_string(),
            world.timed_out.to_string(),
        ],
        "the owner's answered gates are not exactly theirs, newest first"
    );
}

/// A gate nobody answered before its timeout is listed under `answered`
/// with no word and the outcome `timed_out`.
#[tokio::test]
async fn a_gate_closed_by_its_timeout_is_listed_as_timed_out() {
    let world = in_memory_world().await;
    let listed = listing(&world, "owner", "status=answered").await;
    assert_eq!(
        gate_of(&listed, world.timed_out),
        Some(&json!({
            "execution_id": world.timed_out.to_string(),
            "workflow_name": "the-forge",
            "state_name": "AWAIT_REQUIREMENTS_APPROVAL",
            "prompt": "Anyone?",
            "responses": ["yes", "no"],
            "response": null,
            "feedback": null,
            "answered_at": "2026-10-13T05:30:00+00:00",
            "answered_by": null,
            "sequence": 2,
            "requested_at": "2026-10-13T04:30:00+00:00",
            "outcome": "timed_out",
        })),
        "the gate closed by its timeout is not listed as timed out: {listed:?}"
    );
}

/// A gate still waiting, or one whose run ended or went on without an
/// answer, is not listed under `answered`.
#[tokio::test]
async fn a_pending_gate_is_not_listed_as_answered() {
    let world = in_memory_world().await;
    let listed = listing(&world, "owner", "status=answered").await;
    for (run, what) in [
        (world.waiting, "a waiting gate"),
        (world.reposted, "a reposted waiting gate"),
        (world.past, "a run past its gate with no answer recorded"),
        (world.ended, "a run that ended at its gate unanswered"),
    ] {
        assert!(
            gate_of(&listed, run).is_none(),
            "{what} was listed as answered: {listed:?}"
        );
    }
}

/// Another tenant's answered gate is not listed; its own person sees it.
#[tokio::test]
async fn another_tenants_answered_gate_is_not_listed() {
    let world = in_memory_world().await;
    let owners = listing(&world, "owner", "status=answered").await;
    assert!(
        gate_of(&owners, world.others_answered).is_none(),
        "another tenant's answered gate was listed: {owners:?}"
    );
    assert_eq!(
        ids(&listing(&world, "other", "status=answered").await),
        vec![world.others_answered.to_string()],
        "the other person's own answered gate was not listed to them alone"
    );
    assert_eq!(
        ids(&listing(&world, "member-a", "status=answered").await),
        Vec::<String>::new(),
        "a member was shown answered gates of runs they did not start"
    );
}

/// `status=any` lists the pending and the answered gates together, newest
/// first by the gate's latest time (its answer, else its post); an answered
/// gate is the one that carries an `outcome`.
#[tokio::test]
async fn any_lists_pending_and_answered_gates() {
    let world = in_memory_world().await;
    let listed = listing(&world, "owner", "status=any").await;
    assert_eq!(
        ids(&listed),
        vec![
            world.answered_yes.to_string(),
            world.reposted.to_string(),
            world.waiting.to_string(),
            world.answered_no.to_string(),
            world.timed_out.to_string(),
        ],
        "status=any does not list the owner's pending and answered gates, newest first"
    );
    assert_eq!(
        gate_of(&listed, world.answered_yes),
        Some(&answered_yes_view(&world)),
        "status=any lists an answered gate unlike status=answered"
    );
    assert_eq!(
        gate_of(&listed, world.waiting),
        gate_of(&gates(&world, "owner").await, world.waiting),
        "status=any lists a pending gate unlike status=pending"
    );
}

/// `limit` (default 50, at most 200) and `offset` page the answered gates.
#[tokio::test]
async fn limit_and_offset_page_the_answered_gates() {
    let world = in_memory_world().await;
    assert_eq!(
        ids(&listing(&world, "owner", "status=answered&limit=1&offset=1").await),
        vec![world.answered_no.to_string()],
        "limit=1&offset=1 is not the second answered gate"
    );
    assert_eq!(
        ids(&listing(&world, "owner", "status=answered&offset=3").await),
        Vec::<String>::new(),
        "an offset past the last answered gate lists one"
    );
    let mut statuses = Vec::new();
    for query in [
        "status=answered&limit=0",
        "status=answered&limit=201",
        "status=answered&limit=200",
    ] {
        let (status, _) = send(
            &world.base,
            &Method::GET,
            &format!("/v1/workflow-gates?{query}"),
            &None,
            Some("owner"),
        )
        .await;
        statuses.push((query, status));
    }
    assert_eq!(
        statuses,
        vec![
            ("status=answered&limit=0", 400),
            ("status=answered&limit=201", 400),
            ("status=answered&limit=200", 200),
        ]
    );
}

/// The same world on the real PostgreSQL repositories, every migration
/// applied: the Postgres query lists exactly what the default does. Runs
/// when `AEGIS_TEST_POSTGRES_URL` names a server; CI sets it, and there a
/// missing one fails the test.
#[tokio::test]
async fn the_postgres_repository_lists_the_same_gates() {
    let server_url = match std::env::var("AEGIS_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => url,
        _ if std::env::var_os("CI").is_some() => {
            panic!("AEGIS_TEST_POSTGRES_URL is not set; in CI this test must reach PostgreSQL")
        }
        _ => {
            eprintln!("skipped: AEGIS_TEST_POSTGRES_URL is not set");
            return;
        }
    };
    let server = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&server_url)
        .await
        .expect("connect to the test PostgreSQL");
    let name = format!("aegis_gates_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&server)
        .await
        .expect("create the test database");
    let options = server_url
        .parse::<sqlx::postgres::PgConnectOptions>()
        .expect("the PostgreSQL URL parses")
        .database(&name);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("connect to the test database");
    crate::daemon::migrations::apply_migrations(&pool)
        .await
        .expect("apply the migrations");
    for tenant in [
        TenantId::for_consumer_user(OWNER).unwrap(),
        TenantId::for_consumer_user(OTHER).unwrap(),
        TenantId::from_realm_slug(TEAM).unwrap(),
    ] {
        sqlx::query(
            "INSERT INTO tenants (slug, display_name, keycloak_realm, openbao_namespace) \
             VALUES ($1, $1, $1, $1) ON CONFLICT (slug) DO NOTHING",
        )
        .bind(tenant.as_str())
        .execute(&pool)
        .await
        .expect("insert the tenant");
    }

    let world = world(
        Arc::new(PostgresWorkflowExecutionRepository::new(pool.clone())),
        Arc::new(PostgresWorkflowRepository::new_with_pool(pool.clone())),
    )
    .await;
    let owners = gates(&world, "owner").await;
    let answered = listing(&world, "owner", "status=answered").await;
    let listed = (
        ids(&owners),
        gate_of(&owners, world.waiting).cloned(),
        ids(&gates(&world, "other").await),
        ids(&gates(&world, "member-a").await),
        ids(&gates(&world, "member-b").await),
    );
    let answered_listed = (
        ids(&answered),
        gate_of(&answered, world.answered_yes).cloned(),
        ids(&listing(&world, "other", "status=answered").await),
        ids(&listing(&world, "owner", "status=any").await),
    );

    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .execute(&server)
        .await
        .expect("drop the test database");

    assert_eq!(
        listed,
        (
            vec![world.reposted.to_string(), world.waiting.to_string()],
            Some(json!({
                "execution_id": world.waiting.to_string(),
                "workflow_name": "the-forge",
                "state_name": "AWAIT_REQUIREMENTS_APPROVAL",
                "prompt": "Approve the requirements?",
                "responses": ["yes", "no"],
                "default_response": "no",
                "expires_at": "2026-10-13T07:00:00+00:00",
                "sequence": 3,
                "requested_at": "2026-10-13T06:00:00+00:00",
            })),
            vec![world.others.to_string()],
            Vec::<String>::new(),
            vec![world.member_b.to_string()],
        ),
        "the Postgres repository's gates differ from the trail's"
    );
    assert_eq!(
        answered_listed,
        (
            vec![
                world.answered_yes.to_string(),
                world.answered_no.to_string(),
                world.timed_out.to_string(),
            ],
            Some(answered_yes_view(&world)),
            vec![world.others_answered.to_string()],
            vec![
                world.answered_yes.to_string(),
                world.reposted.to_string(),
                world.waiting.to_string(),
                world.answered_no.to_string(),
                world.timed_out.to_string(),
            ],
        ),
        "the Postgres repository's answered gates differ from the trail's"
    );
}
