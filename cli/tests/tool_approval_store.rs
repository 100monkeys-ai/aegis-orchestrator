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
//! binding, and stops matching once revoked; the sweep expires only what has
//! waited 72 hours; and migration 036 run again changes nothing.

use std::sync::{Arc, Mutex};

use aegis_orchestrator_core::application::tool_approval_service::{
    ApprovedCallRunner, GateOutcome, GatedCall, ToolApprovalService,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::tool_approval::{
    ToolApprovalDecision, ToolApprovalId, ToolApprovalRepository, ToolApprovalRequest,
    ToolApprovalStatus,
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

async fn gate(svc: &ToolApprovalService, user: &str, tool: &str, args: &Value) -> GateOutcome {
    svc.gate(GatedCall {
        tenant_id: &tenant(),
        user_sub: Some(user),
        execution_id: ExecutionId::new(),
        agent_id: AgentId::new(),
        tool_name: tool,
        arguments: args,
        security_context_name: "zaru-pro",
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
    let args = json!({"mailbox": "b-1", "to": "x@example.com", "subject": "Hi", "body": "Hello"});
    let id = {
        let before = service(&db.pool);
        pending_id(gate(&before, USER, "mail.send", &args).await)
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
        listed[0].summary.contains("Subject: Hi"),
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
    let id = pending_id(gate(&svc, USER, "mail.send", &json!({"mailbox": "b-1"})).await);
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
    for args in [json!({"mailbox": "b-1"}), json!({})] {
        let id = pending_id(gate(&svc, USER, "mail.send", &args).await);
        svc.decide(id, &tenant(), USER, ToolApprovalDecision::Always, &runner)
            .await
            .unwrap();
        assert!(
            matches!(
                gate(&svc, USER, "mail.send", &args).await,
                GateOutcome::Proceed { .. }
            ),
            "the policy for {args} did not apply"
        );
    }
    pending_id(gate(&svc, USER, "mail.send", &json!({"mailbox": "b-2"})).await);
    pending_id(gate(&svc, USER, "mail.reply", &json!({"mailbox": "b-1"})).await);
    pending_id(gate(&svc, "other-sub", "mail.send", &json!({"mailbox": "b-1"})).await);

    let auto = svc
        .list_for_user(&tenant(), USER, Some(ToolApprovalStatus::AutoAllowed))
        .await
        .unwrap();
    assert_eq!(auto.len(), 2);
    assert!(auto.iter().all(|r| r.policy_id.is_some()));

    for policy in svc.list_policies(&tenant(), USER).await.unwrap() {
        svc.revoke_policy(policy.id, &tenant(), USER).await.unwrap();
    }
    pending_id(gate(&svc, USER, "mail.send", &json!({"mailbox": "b-1"})).await);
    pending_id(gate(&svc, USER, "mail.send", &json!({})).await);
    db.remove().await;
}

/// The sweep expires what has waited 72 hours and nothing younger.
#[tokio::test]
async fn the_sweep_expires_only_requests_pending_72_hours() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let old = pending_id(gate(&svc, USER, "mail.send", &json!({"mailbox": "b-1"})).await);
    let young = pending_id(gate(&svc, USER, "mail.send", &json!({"mailbox": "b-2"})).await);
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
    pending_id(gate(&svc, USER, "mail.send", &json!({"mailbox": "b-1"})).await);
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
