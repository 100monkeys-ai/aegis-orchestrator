// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A schedule's run asked for now, in the store
//!
//! A run its owner asks for now is a fire with no scheduled time: every such
//! press is its own row, and a schedule's fires are listed newest first, a
//! press by the time it fired. Over the in-memory store always, and, when
//! `AEGIS_TEST_POSTGRES_URL` names a database, over the Postgres store with
//! migration 051 applied twice.

use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::schedule::{
    FireClaim, FireOutcome, Schedule, ScheduleDraft, ScheduleFire, ScheduleRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::InMemoryScheduleRepository;
use chrono::{DateTime, Duration, TimeZone, Utc};

fn owner() -> UserIdentity {
    UserIdentity {
        sub: "owner".into(),
        realm_slug: "zaru-consumer".into(),
        email: None,
        email_verified: true,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Pro,
            tenant_id: TenantId::for_consumer_user("owner").unwrap(),
        },
    }
}

/// A whole second, as PostgreSQL keeps microseconds.
fn at(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 9, 15, 0, 0).unwrap() + Duration::minutes(minutes)
}

fn schedule() -> Schedule {
    let draft: ScheduleDraft = serde_json::from_value(serde_json::json!({
        "name": "Weekday triage",
        "target_kind": "agent",
        "target": "mail-triage",
        "recurrence": { "cron": "0 15 * * 1-5", "timezone": "UTC" },
    }))
    .expect("draft");
    let mut schedule = Schedule::create(
        draft,
        &owner(),
        TenantId::for_consumer_user("owner").unwrap(),
        Utc::now(),
    )
    .expect("schedule");
    schedule.created_at = at(0);
    schedule.updated_at = at(0);
    schedule
}

fn claimed(claim: FireClaim) -> ScheduleFire {
    match claim {
        FireClaim::Claimed(fire) => fire,
        other => panic!("expected a claim, got {other:?}"),
    }
}

/// Two presses and a scheduled fire, claimed in this order: a fire
/// scheduled for 15:00, a press at 15:30, a press at 15:45. Answers the
/// fires as the store lists them, as (scheduled time, fired at).
async fn two_presses_and_a_fire(
    store: &dyn ScheduleRepository,
    schedule: &Schedule,
) -> Vec<(Option<DateTime<Utc>>, DateTime<Utc>, FireOutcome)> {
    let fired = claimed(
        store
            .claim_fire(schedule.id, Some(at(0)), at(0))
            .await
            .unwrap(),
    );
    let first = claimed(store.claim_fire(schedule.id, None, at(30)).await.unwrap());
    let mut second = claimed(store.claim_fire(schedule.id, None, at(45)).await.unwrap());
    assert_ne!(first.id, second.id, "two presses shared one row");
    assert_eq!(fired.scheduled_time, Some(at(0)));
    second.outcome = FireOutcome::Started;
    store.finish_fire(&second).await.unwrap();
    store
        .fires(schedule.id, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.scheduled_time, f.fired_at, f.outcome))
        .collect()
}

fn expected() -> Vec<(Option<DateTime<Utc>>, DateTime<Utc>, FireOutcome)> {
    vec![
        (None, at(45), FireOutcome::Started),
        (None, at(30), FireOutcome::Starting),
        (Some(at(0)), at(0), FireOutcome::Starting),
    ]
}

/// Each press is its own fire with no scheduled time, listed newest first
/// by the time it fired, beside the scheduled fires.
#[tokio::test]
async fn presses_are_fires_with_no_scheduled_time_listed_newest_first_in_memory() {
    let store = InMemoryScheduleRepository::new();
    let schedule = schedule();
    store.insert(&schedule).await.unwrap();
    assert_eq!(
        two_presses_and_a_fire(&store, &schedule).await,
        expected(),
        "the in-memory store did not keep each press as its own fire, newest first"
    );
}

mod postgres {
    use super::*;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::PostgresScheduleRepository;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use sqlx::{Executor, Row};

    const MIGRATION_036: &str = include_str!("../../../cli/migrations/036_tool_approvals.sql");
    const MIGRATION_048: &str = include_str!("../../../cli/migrations/048_schedules.sql");
    const MIGRATION_050: &str = include_str!("../../../cli/migrations/050_profile_scoping.sql");
    const MIGRATION_051: &str = include_str!("../../../cli/migrations/051_schedule_run_now.sql");

    /// The two run tables, in the columns migration 048 touches.
    const RUN_TABLES: &str = "
        CREATE TABLE executions (id UUID PRIMARY KEY, status VARCHAR(50) NOT NULL);
        CREATE TABLE workflow_executions (id UUID PRIMARY KEY, status VARCHAR(50) NOT NULL);
    ";

    async fn pool_in_fresh_schema(url: &str) -> (PgPool, String) {
        let schema = format!("runnow_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await
            .expect("connect");
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        let search_path = format!("SET search_path TO {schema}, public");
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |conn, _| {
                let sql = search_path.clone();
                Box::pin(async move {
                    conn.execute(sql.as_str()).await?;
                    Ok(())
                })
            })
            .connect(url)
            .await
            .expect("connect in schema");
        (pool, schema)
    }

    async fn scheduled_time_nullable(pool: &PgPool, schema: &str) -> String {
        sqlx::query(
            "SELECT is_nullable FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = 'schedule_fires' \
             AND column_name = 'scheduled_time'",
        )
        .bind(schema)
        .fetch_one(pool)
        .await
        .unwrap()
        .get("is_nullable")
    }

    /// Migration 051 makes `schedule_fires.scheduled_time` nullable and,
    /// applied again, changes nothing; the Postgres store then keeps each
    /// press as its own fire with no scheduled time, listed newest first.
    #[tokio::test]
    async fn migration_051_applied_twice_keeps_presses_with_no_scheduled_time() {
        let Ok(url) = std::env::var("AEGIS_TEST_POSTGRES_URL") else {
            eprintln!("skipped: no AEGIS_TEST_POSTGRES_URL");
            return;
        };
        let (pool, schema) = pool_in_fresh_schema(&url).await;
        pool.execute(RUN_TABLES).await.expect("run tables");
        pool.execute(MIGRATION_036).await.expect("migration 036");
        pool.execute(MIGRATION_048).await.expect("migration 048");
        pool.execute(MIGRATION_050).await.expect("migration 050");
        pool.execute(MIGRATION_051).await.expect("migration 051");
        let once = scheduled_time_nullable(&pool, &schema).await;
        pool.execute(MIGRATION_051)
            .await
            .expect("migration 051 applied a second time");
        let twice = scheduled_time_nullable(&pool, &schema).await;
        assert_eq!(
            (once.as_str(), twice.as_str()),
            ("YES", "YES"),
            "migration 051 did not leave schedule_fires.scheduled_time nullable"
        );

        let store = PostgresScheduleRepository::new(pool.clone());
        let schedule = schedule();
        store.insert(&schedule).await.expect("insert");
        assert_eq!(
            two_presses_and_a_fire(&store, &schedule).await,
            expected(),
            "the Postgres store did not keep each press as its own fire, newest first"
        );
    }
}
