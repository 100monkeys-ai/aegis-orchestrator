// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! AEGIS ADR-072 against a real PostgreSQL, with the migrations the daemon
//! ships (`cli/migrations`, applied in order): a window's limit bounds the sum
//! of the charges inside the window, not one charge (AEGIS known defect
//! `window-enforcer-counts-one-charge`).
//!
//! Each bucket is a fixed window (ADR-072 Update W1): a counter row is one
//! window, keyed by its open time (`window_start`), and it counts while
//! `now < window_start + window`.
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

/// Store one charge in a window of `bucket` opened `ago` before now, as the
/// enforcer stores the charge that opens a window.
async fn window_opened(pool: &PgPool, bucket: &str, ago: Duration) {
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
    .bind(Utc::now() - ago)
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
    let fourth = enforcer
        .check_and_increment(&scope(), &policy, 1)
        .await
        .map_err(|refusal| (refusal.bucket, refusal.remaining));
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
async fn a_closed_window_is_not_counted() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[(RateLimitBucket::Hourly, 3)]);

    // A window opened 50 minutes ago with one charge is open; one opened 70
    // minutes ago has closed.
    window_opened(&db.pool, "hourly", Duration::minutes(50)).await;
    window_opened(&db.pool, "hourly", Duration::minutes(70)).await;

    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    assert_eq!(remaining.get(&RateLimitBucket::Hourly), Some(&2));
    for _ in 0..2 {
        enforcer
            .check_and_increment(&scope(), &policy, 1)
            .await
            .expect("the hour holds one charge, so two more pass");
    }
    assert_eq!(
        enforcer
            .check_and_increment(&scope(), &policy, 1)
            .await
            .map_err(|refusal| (refusal.bucket, refusal.remaining)),
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
        enforcer
            .check_and_increment(&scope(), &policy, 1)
            .await
            .map_err(|refusal| (refusal.bucket, refusal.remaining)),
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

// The counter cleanup (AEGIS known defect
// `bananas-month-below-week-2026-10-10`): a row is deleted only when its
// window has closed, which is
// `window_start < PostgresWindowEnforcer::open_window_bound(now, bucket)`.

const BUCKETS: [(RateLimitBucket, &str); 4] = [
    (RateLimitBucket::Hourly, "hourly"),
    (RateLimitBucket::Daily, "daily"),
    (RateLimitBucket::Weekly, "weekly"),
    (RateLimitBucket::Monthly, "monthly"),
];

/// The rows stored under `bucket`, of any scope.
async fn rows_in(pool: &PgPool, bucket: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM rate_limit_counters WHERE bucket = $1")
        .bind(bucket)
        .fetch_one(pool)
        .await
        .expect("count a bucket's rows")
}

#[tokio::test]
async fn after_the_cleanup_a_month_reads_its_window_of_six_days_ago_and_at_least_the_week() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let limit = 100;
    let policy = policy(&BUCKETS.map(|(bucket, _)| (bucket, limit)));

    // A window of every bucket opened six days ago: the hour and the day
    // have closed, the week and the month are open.
    for (_, name) in BUCKETS {
        window_opened(&db.pool, name, Duration::days(6)).await;
    }
    enforcer
        .cleanup_expired_counters()
        .await
        .expect("the cleanup runs");

    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    let used = |bucket| limit - remaining[&bucket];
    assert_eq!(
        used(RateLimitBucket::Monthly),
        1,
        "the month must still read its window of six days ago after the cleanup"
    );
    assert!(
        used(RateLimitBucket::Monthly) >= used(RateLimitBucket::Weekly),
        "the month read {} below the week's {}",
        used(RateLimitBucket::Monthly),
        used(RateLimitBucket::Weekly)
    );

    db.remove().await;
}

#[tokio::test]
async fn the_cleanup_deletes_a_monthly_window_of_31_days_ago_and_a_weekly_of_8() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());

    window_opened(&db.pool, "monthly", Duration::days(31)).await;
    window_opened(&db.pool, "weekly", Duration::days(8)).await;
    enforcer
        .cleanup_expired_counters()
        .await
        .expect("the cleanup runs");

    assert_eq!(
        rows_in(&db.pool, "monthly").await,
        0,
        "a monthly window opened 31 days ago has closed and must be deleted"
    );
    assert_eq!(
        rows_in(&db.pool, "weekly").await,
        0,
        "a weekly window opened 8 days ago has closed and must be deleted"
    );

    db.remove().await;
}

#[tokio::test]
async fn the_cleanup_keeps_an_open_daily_window() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());

    // A daily window opened 23 hours ago closes in an hour.
    window_opened(&db.pool, "daily", Duration::hours(23)).await;
    enforcer
        .cleanup_expired_counters()
        .await
        .expect("the cleanup runs");

    assert_eq!(
        rows_in(&db.pool, "daily").await,
        1,
        "an open daily window must be kept"
    );

    db.remove().await;
}

/// The sum of the counters stored in one bucket.
async fn sum_in(pool: &PgPool, bucket: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(counter), 0)::BIGINT FROM rate_limit_counters WHERE bucket = $1",
    )
    .bind(bucket)
    .fetch_one(pool)
    .await
    .expect("sum a bucket's counters")
}

#[tokio::test]
async fn a_record_is_stored_hourly_to_monthly_past_the_limit_and_not_per_minute() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[
        (RateLimitBucket::PerMinute, 3),
        (RateLimitBucket::Hourly, 3),
        (RateLimitBucket::Daily, 3),
        (RateLimitBucket::Weekly, 3),
        (RateLimitBucket::Monthly, 3),
    ]);

    // Twice, five each time: over every window's limit of 3 from the first.
    for _ in 0..2 {
        enforcer
            .record(&scope(), &policy, 5)
            .await
            .expect("a record compares nothing against the limit");
    }

    for bucket in ["hourly", "daily", "weekly", "monthly"] {
        assert_eq!(
            sum_in(&db.pool, bucket).await,
            10,
            "two records of 5 are stored in the {bucket} window past its limit of 3"
        );
    }
    assert_eq!(
        rows_in(&db.pool, "per_minute").await,
        0,
        "per-minute is counted in memory and a record stores no row of it"
    );
    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    assert_eq!(
        remaining.get(&RateLimitBucket::Monthly),
        Some(&0),
        "the window's reader sums the recorded charges"
    );

    db.remove().await;
}
