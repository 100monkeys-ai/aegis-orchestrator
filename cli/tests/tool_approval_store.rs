// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The approval gate's durable store (AEGIS ADR-126 D3) against a real
//! PostgreSQL, with the migrations this binary ships.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere the tests say they were skipped and pass.
//!
//! They cover: a pending request is still there, and can be answered, after
//! the service is rebuilt on the same database (the redeploy case); one
//! answer wins when two race; a policy matches only its own user, tool and
//! binding (the argument the tool's contract declares, ADR-126 Update of
//! 2026-10-04 clause 1), and stops matching once revoked; the sweep expires only what has
//! waited 72 hours; migration 036 run again changes nothing; and the
//! conversation a request was made in (ADR-126, Update of 2026-10-07 (2),
//! clause 1): migration 044 run again changes nothing, and the store
//! round-trips `conversation_id`, a value and null.

use std::sync::{Arc, Mutex};

use aegis_orchestrator_core::application::tool_approval_service::{
    ApprovedCallRunner, GateOutcome, GatedCall, ToolApprovalService,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::tool_approval::{
    ApprovalContract, ToolApprovalDecision, ToolApprovalId, ToolApprovalRepository,
    ToolApprovalRequest, ToolApprovalStatus,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::PostgresToolApprovalRepository;
use serde_json::{json, Value};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

const USER: &str = "owner-sub";

fn postgres_url() -> Option<String> {
    match std::env::var("AEGIS_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("AEGIS_TEST_POSTGRES_URL is not set; in CI these tests must reach PostgreSQL")
        }
        _ => {
            eprintln!("skipped: AEGIS_TEST_POSTGRES_URL is not set");
            None
        }
    }
}

struct TestDb {
    server: PgPool,
    name: String,
    options: PgConnectOptions,
    pool: PgPool,
}

impl TestDb {
    async fn create() -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_approvals_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
        let pool = Self::connect(&options).await;
        MIGRATOR.run(&pool).await.expect("apply every migration");
        Some(Self {
            server,
            name,
            options,
            pool,
        })
    }

    async fn connect(options: &PgConnectOptions) -> PgPool {
        PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.clone())
            .await
            .expect("connect to the test database")
    }

    async fn remove(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("drop the test database");
    }
}

#[derive(Default)]
struct RecordingRunner {
    ran: Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl ApprovedCallRunner for RecordingRunner {
    async fn run_approved_call(&self, request: &ToolApprovalRequest) -> Result<Value, String> {
        self.ran.lock().unwrap().push(request.arguments.clone());
        Ok(json!({"message_id": "<1@example.com>"}))
    }
}

fn service(pool: &PgPool) -> ToolApprovalService {
    ToolApprovalService::new(
        Arc::new(PostgresToolApprovalRepository::new(pool.clone())),
        Arc::new(EventBus::new(64)),
    )
}

fn tenant() -> TenantId {
    TenantId::for_consumer_user(USER).unwrap()
}

/// The contract the gated tools declare: a binding argument the gate knows
/// by no name of its own, and the arguments a user reads.
fn contract() -> ApprovalContract {
    ApprovalContract {
        binding_argument: Some("account".to_string()),
        approval_summary: Some(vec![
            "to".to_string(),
            "subject".to_string(),
            "body".to_string(),
        ]),
    }
}

async fn gate(svc: &ToolApprovalService, user: &str, tool: &str, args: &Value) -> GateOutcome {
    gate_declaring(svc, user, tool, args, contract()).await
}

async fn gate_declaring(
    svc: &ToolApprovalService,
    user: &str,
    tool: &str,
    args: &Value,
    contract: ApprovalContract,
) -> GateOutcome {
    svc.gate(GatedCall {
        tenant_id: &tenant(),
        user_sub: Some(user),
        execution_id: ExecutionId::new(),
        agent_id: AgentId::new(),
        tool_name: tool,
        arguments: args,
        security_context_name: "zaru-pro",
        conversation_id: None,
        contract,
    })
    .await
    .expect("gate")
}

fn pending_id(outcome: GateOutcome) -> ToolApprovalId {
    match outcome {
        GateOutcome::Pending { result } => {
            assert_eq!(result["status"], "approval_pending");
            ToolApprovalId::from_string(result["approval_id"].as_str().unwrap()).unwrap()
        }
        other => panic!("expected pending, got {other:?}"),
    }
}

/// The redeploy case: a request stored by one process is listed, and
/// answered, by a service built afresh on the same database.
#[tokio::test]
async fn a_pending_request_survives_a_rebuild_of_the_service_on_the_same_database() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let args = json!({"account": "b-1", "to": "x@example.com", "subject": "Hi", "body": "Hello"});
    let id = {
        let before = service(&db.pool);
        pending_id(gate(&before, USER, "outbound.send", &args).await)
    };

    let pool = TestDb::connect(&db.options).await;
    let after = service(&pool);
    let listed = after
        .list_for_user(&tenant(), USER, Some(ToolApprovalStatus::Pending))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].arguments, args, "the exact arguments are stored");
    assert!(
        listed[0].summary.contains("subject: Hi"),
        "{}",
        listed[0].summary
    );

    let runner = RecordingRunner::default();
    let decided = after
        .decide(id, &tenant(), USER, ToolApprovalDecision::Once, &runner)
        .await
        .unwrap();
    assert_eq!(decided.status, ToolApprovalStatus::ApprovedOnce);
    assert_eq!(*runner.ran.lock().unwrap(), vec![args]);
    let stored = PostgresToolApprovalRepository::new(pool.clone())
        .find_request(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.result,
        Some(json!({"message_id": "<1@example.com>"}))
    );
    pool.close().await;
    db.remove().await;
}

/// Two answers at once: exactly one wins and the call runs once.
#[tokio::test]
async fn one_of_two_racing_answers_wins() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = Arc::new(service(&db.pool));
    let id = pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    let runner = Arc::new(RecordingRunner::default());
    let mut handles = Vec::new();
    for _ in 0..4 {
        let (svc, runner) = (svc.clone(), runner.clone());
        handles.push(tokio::spawn(async move {
            svc.decide(
                id,
                &tenant(),
                USER,
                ToolApprovalDecision::Once,
                runner.as_ref(),
            )
            .await
            .is_ok()
        }));
    }
    let mut wins = 0;
    for h in handles {
        if h.await.unwrap() {
            wins += 1;
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(runner.ran.lock().unwrap().len(), 1);
    db.remove().await;
}

/// A policy matches only its own user, tool and binding (a tool with no
/// binding included), and stops matching once revoked.
#[tokio::test]
async fn a_policy_matches_only_its_own_user_tool_and_binding_until_revoked() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let runner = RecordingRunner::default();
    for args in [json!({"account": "b-1"}), json!({})] {
        let id = pending_id(gate(&svc, USER, "outbound.send", &args).await);
        svc.decide(id, &tenant(), USER, ToolApprovalDecision::Always, &runner)
            .await
            .unwrap();
        assert!(
            matches!(
                gate(&svc, USER, "outbound.send", &args).await,
                GateOutcome::Proceed { .. }
            ),
            "the policy for {args} did not apply"
        );
    }
    pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-2"})).await);
    pending_id(gate(&svc, USER, "outbound.reply", &json!({"account": "b-1"})).await);
    pending_id(
        gate(
            &svc,
            "other-sub",
            "outbound.send",
            &json!({"account": "b-1"}),
        )
        .await,
    );

    let auto = svc
        .list_for_user(&tenant(), USER, Some(ToolApprovalStatus::AutoAllowed))
        .await
        .unwrap();
    assert_eq!(auto.len(), 2);
    assert!(auto.iter().all(|r| r.policy_id.is_some()));

    for policy in svc.list_policies(&tenant(), USER).await.unwrap() {
        svc.revoke_policy(policy.id, &tenant(), USER).await.unwrap();
    }
    pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    pending_id(gate(&svc, USER, "outbound.send", &json!({})).await);
    db.remove().await;
}

/// The sweep expires what has waited 72 hours and nothing younger.
#[tokio::test]
async fn the_sweep_expires_only_requests_pending_72_hours() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let old = pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    let young = pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-2"})).await);
    sqlx::query(
        "UPDATE tool_approval_requests SET created_at = now() - interval '73 hours' WHERE id = $1",
    )
    .bind(old.0)
    .execute(&db.pool)
    .await
    .unwrap();

    assert_eq!(svc.expire_stale(chrono::Utc::now()).await.unwrap(), 1);
    let repo = PostgresToolApprovalRepository::new(db.pool.clone());
    assert_eq!(
        repo.find_request(old).await.unwrap().unwrap().status,
        ToolApprovalStatus::Expired
    );
    assert_eq!(
        repo.find_request(young).await.unwrap().unwrap().status,
        ToolApprovalStatus::Pending
    );
    db.remove().await;
}

/// Migration 036 applied again over a migrated schema, with rows in it,
/// changes nothing.
#[tokio::test]
async fn migration_036_run_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM tool_approval_requests) AS requests, \
                    (SELECT count(*) FROM pg_indexes WHERE tablename LIKE 'tool_approval%') AS indexes",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| (row.get::<i64, _>("requests"), row.get::<i64, _>("indexes")))
        .unwrap()
    };
    let before = snapshot().await;
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 36)
        .expect("migration 036 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 036 run again");
    assert_eq!(snapshot().await, before);
    db.remove().await;
}

/// Test (a) against PostgreSQL: a policy keyed on the contract-declared
/// argument `account` matches only its own tool and binding; where the tool
/// declares no binding argument, `account` is no key and the call waits.
#[tokio::test]
async fn a_policy_keyed_on_a_contract_declared_argument_matches_only_its_own_tool_and_binding() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let runner = RecordingRunner::default();
    let id = pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    let decided = svc
        .decide(id, &tenant(), USER, ToolApprovalDecision::Always, &runner)
        .await
        .unwrap();
    assert_eq!(decided.binding_id.as_deref(), Some("b-1"));
    assert!(matches!(
        gate(
            &svc,
            USER,
            "outbound.send",
            &json!({"account": "b-1", "to": "x"})
        )
        .await,
        GateOutcome::Proceed { .. }
    ));
    pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-2"})).await);
    pending_id(gate(&svc, USER, "outbound.reply", &json!({"account": "b-1"})).await);
    pending_id(
        gate_declaring(
            &svc,
            USER,
            "outbound.send",
            &json!({"account": "b-1"}),
            ApprovalContract::default(),
        )
        .await,
    );
    let stored = PostgresToolApprovalRepository::new(db.pool.clone())
        .list_requests_for_user(&tenant(), USER, Some(ToolApprovalStatus::Pending))
        .await
        .unwrap();
    let summaries: Vec<&str> = stored.iter().map(|r| r.summary.as_str()).collect();
    assert!(
        summaries.contains(&"outbound.reply\nto: \nsubject: \nbody: "),
        "{summaries:?}"
    );
    db.remove().await;
}

const CONVERSATION: &str = "6c1f0b52-8a3e-4d7b-9f21-0e5d4c3b2a19";

/// A pending request of `USER`'s, made in `conversation` when given, as the
/// gate builds one.
fn request_in(conversation: Option<&str>) -> ToolApprovalRequest {
    ToolApprovalRequest {
        id: ToolApprovalId::new(),
        tenant_id: tenant(),
        user_sub: USER.to_string(),
        execution_id: ExecutionId::new(),
        agent_id: AgentId::new(),
        tool_name: "outbound.send".to_string(),
        arguments: json!({"account": "b-1"}),
        summary: "outbound.send".to_string(),
        binding_id: Some("b-1".to_string()),
        security_context_name: "zaru-pro".to_string(),
        conversation_id: conversation.map(str::to_string),
        policy_id: None,
        status: ToolApprovalStatus::Pending,
        created_at: chrono::Utc::now(),
        decided_at: None,
        decided_by: None,
        result: None,
        error: None,
    }
}

/// Migration 044 adds `conversation_id`, and applied again over a migrated
/// schema, with a row naming a conversation in it, changes nothing.
#[tokio::test]
async fn migration_044_run_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM tool_approval_requests) AS requests, \
                    (SELECT count(*) FROM information_schema.columns \
                      WHERE table_name = 'tool_approval_requests' \
                        AND column_name = 'conversation_id' AND is_nullable = 'YES') AS columns",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| (row.get::<i64, _>("requests"), row.get::<i64, _>("columns")))
        .unwrap()
    };
    assert_eq!(
        snapshot().await.1,
        1,
        "tool_approval_requests has no nullable conversation_id column after every migration"
    );
    let repo = PostgresToolApprovalRepository::new(db.pool.clone());
    let stored = request_in(Some(CONVERSATION));
    repo.insert_request(&stored).await.unwrap();
    let before = snapshot().await;
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 44)
        .expect("migration 044 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 044 run again");
    assert_eq!(
        snapshot().await,
        before,
        "migration 044 run again changed the schema or the rows"
    );
    let found = repo.find_request(stored.id).await.unwrap().unwrap();
    assert_eq!(
        found.conversation_id.as_deref(),
        Some(CONVERSATION),
        "migration 044 run again lost a stored conversation_id"
    );
    db.remove().await;
}

/// The store keeps a request's conversation: a value, and null for a
/// request no conversation started, read back by id and in the user's list.
#[tokio::test]
async fn a_conversation_id_round_trips_through_the_store_a_value_and_null() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresToolApprovalRepository::new(db.pool.clone());
    let in_conversation = request_in(Some(CONVERSATION));
    let in_none = request_in(None);
    repo.insert_request(&in_conversation).await.unwrap();
    repo.insert_request(&in_none).await.unwrap();
    let by_id = (
        repo.find_request(in_conversation.id)
            .await
            .unwrap()
            .unwrap()
            .conversation_id,
        repo.find_request(in_none.id)
            .await
            .unwrap()
            .unwrap()
            .conversation_id,
    );
    let listed = repo
        .list_requests_for_user(&tenant(), USER, None)
        .await
        .unwrap();
    let in_list = |id: ToolApprovalId| {
        listed
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.conversation_id.clone())
    };
    assert_eq!(
        (by_id, in_list(in_conversation.id), in_list(in_none.id)),
        (
            (Some(CONVERSATION.to_string()), None),
            Some(Some(CONVERSATION.to_string())),
            Some(None)
        ),
        "the store did not round-trip conversation_id (a value and null)"
    );
    db.remove().await;
}
