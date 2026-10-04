// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The operator escalation's durable store (AEGIS ADR-129) against a real
//! PostgreSQL, with the migrations this binary ships.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere the tests say they were skipped and pass.
//!
//! They cover: the code row holds the hash and never the code (D9); a code is
//! consumed once when two redemptions race (D8); the fifth failure
//! invalidates (D8); an escalation outlives a rebuilt service and ends (D19);
//! every audit action lands in `admin_audit_log` (D18); migration 037 run
//! again changes nothing; and the two reads of the Update V1 to V9:
//! `invalidate_code` invalidates that one live code only (V3) and
//! `active_system_subs` names each operator holding an active escalation
//! once (V2).

use std::sync::Arc;

use aegis_orchestrator_core::application::operator_escalation_service::{
    OperatorEscalationError, OperatorEscalationService, RedeemingKey,
};
use aegis_orchestrator_core::domain::iam::AegisRole;
use aegis_orchestrator_core::domain::node_config::OperatorEscalationConfig;
use aegis_orchestrator_core::domain::operator_escalation::{
    audit_action, hash_code, EscalationEndReason, OperatorEscalationRepository,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_operator_escalation::PostgresOperatorEscalationRepository;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;
use uuid::Uuid;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

const CONSUMER: &str = "consumer-sub";
const SYSTEM: &str = "system-sub";

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
        let name = format!("aegis_escalations_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect to the test database");
        MIGRATOR.run(&pool).await.expect("apply every migration");
        Some(Self { server, name, pool })
    }

    async fn remove(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("drop the test database");
    }
}

fn service(pool: &PgPool) -> OperatorEscalationService {
    OperatorEscalationService::new(
        Arc::new(PostgresOperatorEscalationRepository::new(pool.clone())),
        OperatorEscalationConfig::default(),
    )
}

fn consumer_key() -> RedeemingKey {
    RedeemingKey {
        api_key_id: Uuid::new_v4(),
        user_id: CONSUMER.to_string(),
        has_stored_role: false,
    }
}

fn wrong(code: &str) -> String {
    let n: u32 = code.parse().unwrap();
    format!("{:06}", (n + 1) % 1_000_000)
}

#[tokio::test]
async fn code_row_holds_hash_not_code() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let minted = service(&db.pool)
        .mint(SYSTEM, CONSUMER, AegisRole::Operator)
        .await
        .unwrap();
    let row = sqlx::query("SELECT row_to_json(c)::text AS j FROM operator_escalation_codes c")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let stored: String = row.get("j");
    assert!(
        !stored.contains(&minted.code),
        "the code is stored: {stored}"
    );
    assert!(stored
        .contains(&aegis_orchestrator_core::domain::operator_escalation::hash_code(&minted.code)));
    db.remove().await;
}

#[tokio::test]
async fn racing_redemptions_consume_the_code_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = Arc::new(service(&db.pool));
    let minted = svc
        .mint(SYSTEM, CONSUMER, AegisRole::Operator)
        .await
        .unwrap();
    let (a, b) = (consumer_key(), consumer_key());
    let (ra, rb) = tokio::join!(svc.redeem(&a, &minted.code), svc.redeem(&b, &minted.code));
    assert_eq!(
        [ra.is_ok(), rb.is_ok()].iter().filter(|ok| **ok).count(),
        1,
        "exactly one redemption wins: {ra:?} {rb:?}"
    );
    db.remove().await;
}

#[tokio::test]
async fn fifth_failure_invalidates_in_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let minted = svc
        .mint(SYSTEM, CONSUMER, AegisRole::Operator)
        .await
        .unwrap();
    let key = consumer_key();
    for _ in 0..5 {
        assert_eq!(
            svc.redeem(&key, &wrong(&minted.code)).await,
            Err(OperatorEscalationError::InvalidCode)
        );
    }
    assert_eq!(
        svc.redeem(&key, &minted.code).await,
        Err(OperatorEscalationError::InvalidCode)
    );
    db.remove().await;
}

#[tokio::test]
async fn escalation_outlives_a_rebuilt_service_and_ends() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let minted = service(&db.pool)
        .mint(SYSTEM, CONSUMER, AegisRole::Admin)
        .await
        .unwrap();
    let key = consumer_key();
    let e = service(&db.pool).redeem(&key, &minted.code).await.unwrap();
    let rebuilt = service(&db.pool);
    assert_eq!(
        rebuilt
            .active_for_api_key(key.api_key_id)
            .await
            .unwrap()
            .map(|x| x.id),
        Some(e.id)
    );
    let ended = rebuilt
        .end_for_api_key(key.api_key_id, EscalationEndReason::AgentRelease)
        .await
        .unwrap();
    assert_eq!(ended.len(), 1);
    assert!(rebuilt.check_active(e.id).await.is_err());
    db.remove().await;
}

/// ADR-129 D18: each action reaches `admin_audit_log` with the system sub as
/// actor.
#[tokio::test]
async fn audit_rows_land_in_admin_audit_log() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let minted = svc
        .mint(SYSTEM, CONSUMER, AegisRole::Operator)
        .await
        .unwrap();
    let key = consumer_key();
    let _ = svc.redeem(&key, &wrong(&minted.code)).await;
    let e = svc.redeem(&key, &minted.code).await.unwrap();
    svc.audit_tool_call(&e, "aegis.task.list", "*")
        .await
        .unwrap();
    svc.end_for_api_key(key.api_key_id, EscalationEndReason::AgentRelease)
        .await
        .unwrap();
    let rows =
        sqlx::query("SELECT actor_id, action FROM admin_audit_log ORDER BY created_at, action")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    let mut actions: Vec<String> = rows.iter().map(|r| r.get::<String, _>("action")).collect();
    actions.sort();
    let mut expected = vec![
        audit_action::CODE_ISSUED,
        audit_action::REDEEM_FAILED,
        audit_action::REDEEMED,
        audit_action::TOOL_CALL,
        audit_action::ENDED,
    ];
    expected.sort();
    assert_eq!(actions, expected);
    assert!(rows
        .iter()
        .all(|r| r.get::<String, _>("actor_id") == SYSTEM));
    db.remove().await;
}

/// Migration 037 applied again over a migrated schema, with rows in it,
/// changes nothing.
#[tokio::test]
async fn migration_037_run_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let minted = svc
        .mint(SYSTEM, CONSUMER, AegisRole::Operator)
        .await
        .unwrap();
    svc.redeem(&consumer_key(), &minted.code).await.unwrap();
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM operator_escalation_codes) AS codes, \
                    (SELECT count(*) FROM operator_escalations) AS escalations, \
                    (SELECT count(*) FROM pg_indexes WHERE tablename LIKE 'operator_escalation%') AS indexes",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| {
            (
                row.get::<i64, _>("codes"),
                row.get::<i64, _>("escalations"),
                row.get::<i64, _>("indexes"),
            )
        })
        .unwrap()
    };
    let before = snapshot().await;
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 37)
        .expect("migration 037 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 037 run again");
    assert_eq!(snapshot().await, before);
    db.remove().await;
}

/// ADR-129 — Updates, V3: `invalidate_code` sets `invalidated_at` on that one
/// live code, leaves the user's other live code redeemable, and answers
/// `false` for a code no longer live.
#[tokio::test]
async fn invalidate_code_invalidates_that_code_only() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresOperatorEscalationRepository::new(db.pool.clone());
    let svc = service(&db.pool);
    let first = svc
        .mint(SYSTEM, CONSUMER, AegisRole::Operator)
        .await
        .unwrap();
    let second = svc
        .mint(SYSTEM, CONSUMER, AegisRole::Operator)
        .await
        .unwrap();
    let now = chrono::Utc::now();
    assert!(repo.invalidate_code(first.code_id, now).await.unwrap());
    assert!(
        !repo.invalidate_code(first.code_id, now).await.unwrap(),
        "an invalidated code is no longer live"
    );
    let stored = repo
        .find_code(CONSUMER, &hash_code(&first.code))
        .await
        .unwrap()
        .unwrap();
    assert!(stored.invalidated_at.is_some());
    assert!(stored.consumed_at.is_none(), "invalidated, not consumed");
    assert_eq!(stored.failed_attempts, 0, "no failure counted");
    let other = repo
        .find_code(CONSUMER, &hash_code(&second.code))
        .await
        .unwrap()
        .unwrap();
    assert!(other.invalidated_at.is_none());
    assert_eq!(
        svc.redeem(&consumer_key(), &first.code).await,
        Err(OperatorEscalationError::InvalidCode)
    );
    assert!(svc.redeem(&consumer_key(), &second.code).await.is_ok());
    db.remove().await;
}

/// ADR-129 — Updates, V2: `active_system_subs` names each operator holding
/// an active escalation once, and no operator whose escalations ended.
#[tokio::test]
async fn active_system_subs_names_each_active_operator_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresOperatorEscalationRepository::new(db.pool.clone());
    let svc = service(&db.pool);
    for (system, consumer) in [
        ("system-b", "consumer-b"),
        ("system-a", "consumer-a"),
        ("system-a", "consumer-a"),
        ("system-c", "consumer-c"),
    ] {
        let minted = svc
            .mint(system, consumer, AegisRole::Operator)
            .await
            .unwrap();
        let key = RedeemingKey {
            api_key_id: Uuid::new_v4(),
            user_id: consumer.to_string(),
            has_stored_role: false,
        };
        svc.redeem(&key, &minted.code).await.unwrap();
    }
    svc.end_for_operator("system-c").await.unwrap();
    assert_eq!(
        repo.active_system_subs(chrono::Utc::now()).await.unwrap(),
        vec!["system-a".to_string(), "system-b".to_string()]
    );
    db.remove().await;
}
