// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! AEGIS ADR-072 against a real PostgreSQL, with the migrations the daemon
//! ships (`cli/migrations`, applied in order): a window's limit bounds the sum
//! of the charges inside the window, not one charge (AEGIS known defect
//! `window-enforcer-counts-one-charge`).
//!
//! The enforcer stores each charge at `window_start = charge time - window`
//! (Zaru ADR-0064 U1), so the charges inside a window are the rows whose
//! `window_start >= now - 2 * window`.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere it says it was skipped and passes.

use std::collections::HashMap;
use std::sync::Arc;

use aegis_orchestrator_core::domain::rate_limit::{
    RateLimitBucket, RateLimitPolicy, RateLimitResourceType, RateLimitScope, RateLimitWindow,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::rate_limit::PostgresWindowEnforcer;
use chrono::{Duration, Utc};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;

fn postgres_url() -> Option<String> {
    match std::env::var("AEGIS_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("AEGIS_TEST_POSTGRES_URL is not set; in CI this test must reach PostgreSQL")
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
    /// A database of its own on the test server, migrated as the daemon
    /// migrates its own.
    async fn create() -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_window_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .expect("connect to the test database");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli/migrations");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read cli/migrations")
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
            .collect();
        files.sort();
        for file in files {
            let sql = std::fs::read_to_string(&file).unwrap();
            sqlx::raw_sql(&sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("apply {}: {e}", file.display()));
        }
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

fn scope() -> RateLimitScope {
    RateLimitScope::User {
        tenant_id: TenantId::consumer(),
        user_id: "person-1".into(),
    }
}

fn policy(windows: &[(RateLimitBucket, u64)]) -> RateLimitPolicy {
    RateLimitPolicy {
        resource_type: RateLimitResourceType::AgentExecution,
        windows: windows
            .iter()
            .map(|(bucket, limit)| {
                (
                    *bucket,
                    RateLimitWindow {
                        limit: *limit,
                        window_seconds: bucket.window_seconds(),
                        burst: None,
                    },
                )
            })
            .collect::<HashMap<_, _>>(),
    }
}

/// Store one charge as the enforcer stores it, made `ago` before now.
async fn charge_made(pool: &PgPool, bucket: &str, window: Duration, ago: Duration) {
    let scope = scope();
    let (scope_type, scope_id) = PostgresWindowEnforcer::scope_parts(&scope);
    sqlx::query(
        "INSERT INTO rate_limit_counters \
         (scope_type, scope_id, resource_type, bucket, window_start, counter) \
         VALUES ($1, $2, 'agent_execution', $3, $4, 1)",
    )
    .bind(scope_type)
    .bind(&scope_id)
    .bind(bucket)
    .bind(Utc::now() - ago - window)
    .execute(pool)
    .await
    .expect("store a charge");
}

#[tokio::test]
async fn the_fourth_charge_in_an_hour_of_three_is_refused() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[(RateLimitBucket::Hourly, 3)]);

    for n in 1..=3 {
        enforcer
            .check_and_increment(&scope(), &policy, 1)
            .await
            .unwrap_or_else(|e| panic!("charge {n} of 3 was refused: {e:?}"));
    }
    let fourth = enforcer.check_and_increment(&scope(), &policy, 1).await;
    assert_eq!(
        fourth,
        Err((RateLimitBucket::Hourly, 0)),
        "the fourth charge in an hour whose limit is 3 must be refused"
    );

    db.remove().await;
}

#[tokio::test]
async fn remaining_after_two_charges_is_the_limit_less_two() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[(RateLimitBucket::Hourly, 3), (RateLimitBucket::Daily, 10)]);

    for _ in 0..2 {
        enforcer
            .check_and_increment(&scope(), &policy, 1)
            .await
            .expect("a charge under the limit");
    }
    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    assert_eq!(remaining.get(&RateLimitBucket::Hourly), Some(&1));
    assert_eq!(remaining.get(&RateLimitBucket::Daily), Some(&8));

    db.remove().await;
}

#[tokio::test]
async fn a_charge_older_than_the_window_is_not_counted() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[(RateLimitBucket::Hourly, 3)]);
    let hour = Duration::hours(1);

    // One charge 50 minutes ago is inside the hour; one 70 minutes ago, its
    // row older than two windows, is not.
    charge_made(&db.pool, "hourly", hour, Duration::minutes(50)).await;
    charge_made(&db.pool, "hourly", hour, Duration::minutes(70)).await;

    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    assert_eq!(remaining.get(&RateLimitBucket::Hourly), Some(&2));
    for _ in 0..2 {
        enforcer
            .check_and_increment(&scope(), &policy, 1)
            .await
            .expect("the hour holds one charge, so two more pass");
    }
    assert_eq!(
        enforcer.check_and_increment(&scope(), &policy, 1).await,
        Err((RateLimitBucket::Hourly, 0))
    );

    db.remove().await;
}

#[tokio::test]
async fn a_refusal_in_one_window_charges_no_window() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[(RateLimitBucket::Hourly, 5), (RateLimitBucket::Daily, 2)]);

    for _ in 0..2 {
        enforcer
            .check_and_increment(&scope(), &policy, 1)
            .await
            .expect("a charge under both limits");
    }
    assert_eq!(
        enforcer.check_and_increment(&scope(), &policy, 1).await,
        Err((RateLimitBucket::Daily, 0))
    );
    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    assert_eq!(remaining.get(&RateLimitBucket::Hourly), Some(&3));
    assert_eq!(remaining.get(&RateLimitBucket::Daily), Some(&0));

    db.remove().await;
}

#[tokio::test]
async fn concurrent_charges_cannot_pass_the_limit_together() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = Arc::new(PostgresWindowEnforcer::new(db.pool.clone()));
    let policy = Arc::new(policy(&[(RateLimitBucket::Hourly, 3)]));

    let charges: Vec<_> = (0..8)
        .map(|_| {
            let enforcer = enforcer.clone();
            let policy = policy.clone();
            tokio::spawn(async move { enforcer.check_and_increment(&scope(), &policy, 1).await })
        })
        .collect();
    let mut passed = 0;
    for charge in charges {
        if charge.await.unwrap().is_ok() {
            passed += 1;
        }
    }
    assert_eq!(passed, 3, "eight concurrent charges against a limit of 3");
    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    assert_eq!(remaining.get(&RateLimitBucket::Hourly), Some(&0));

    db.remove().await;
}

#[tokio::test]
async fn the_per_minute_window_is_left_to_the_burst_enforcer() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[
        (RateLimitBucket::PerMinute, 1),
        (RateLimitBucket::Hourly, 5),
    ]);

    for _ in 0..3 {
        let remaining = enforcer
            .check_and_increment(&scope(), &policy, 1)
            .await
            .expect("the per-minute limit is not this enforcer's");
        assert!(!remaining.contains_key(&RateLimitBucket::PerMinute));
    }
    let stored: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rate_limit_counters WHERE bucket = 'per_minute'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(stored, 0);

    db.remove().await;
}
