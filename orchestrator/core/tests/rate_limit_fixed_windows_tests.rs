// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! AEGIS ADR-072 Update W1 against a real PostgreSQL, with the migrations the
//! daemon ships (`cli/migrations`, applied in order): each stored bucket is a
//! fixed window. A window opens at the first charge after the previous
//! window's close and closes one window length later; every charge inside it
//! counts; at the close the count is zero and the next charge opens the next
//! window. A counter row is one window, keyed by its open time
//! (`window_start`).
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere it says it was skipped and passes.

use std::collections::HashMap;
use std::sync::Arc;

use aegis_orchestrator_core::domain::rate_limit::{
    RateLimitBucket, RateLimitEnforcer, RateLimitPolicy, RateLimitResourceType, RateLimitScope,
    RateLimitWindow,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::rate_limit::{
    CompositeRateLimitEnforcer, GovernorBurstEnforcer, PostgresWindowEnforcer,
};
use chrono::{DateTime, Duration, Utc};
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
        let name = format!("aegis_fixed_{}", Uuid::new_v4().simple());
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

fn bucket_name(bucket: RateLimitBucket) -> &'static str {
    match bucket {
        RateLimitBucket::PerMinute => "per_minute",
        RateLimitBucket::Hourly => "hourly",
        RateLimitBucket::Daily => "daily",
        RateLimitBucket::Weekly => "weekly",
        RateLimitBucket::Monthly => "monthly",
    }
}

fn window_of(bucket: RateLimitBucket) -> Duration {
    Duration::seconds(bucket.window_seconds() as i64)
}

/// Store a counter row for `scope()` with this `window_start` and count.
async fn row(pool: &PgPool, bucket: RateLimitBucket, window_start: DateTime<Utc>, count: i64) {
    let scope = scope();
    let (scope_type, scope_id) = PostgresWindowEnforcer::scope_parts(&scope);
    sqlx::query(
        "INSERT INTO rate_limit_counters \
         (scope_type, scope_id, resource_type, bucket, window_start, counter) \
         VALUES ($1, $2, 'agent_execution', $3, $4, $5)",
    )
    .bind(scope_type)
    .bind(&scope_id)
    .bind(bucket_name(bucket))
    .bind(window_start)
    .bind(count)
    .execute(pool)
    .await
    .expect("store a counter row");
}

/// Every `(window_start, counter)` stored in `bucket`, oldest first.
async fn rows_in(pool: &PgPool, bucket: RateLimitBucket) -> Vec<(DateTime<Utc>, i64)> {
    sqlx::query_as(
        "SELECT window_start, counter FROM rate_limit_counters \
         WHERE bucket = $1 ORDER BY window_start",
    )
    .bind(bucket_name(bucket))
    .fetch_all(pool)
    .await
    .expect("read a bucket's rows")
}

#[tokio::test]
async fn a_charge_after_a_windows_close_opens_a_new_window_with_a_zero_count() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[(RateLimitBucket::Hourly, 3)]);
    let hour = window_of(RateLimitBucket::Hourly);

    // A full hourly window opened 61 minutes ago: it closed a minute ago.
    let opened = Utc::now() - hour - Duration::minutes(1);
    row(&db.pool, RateLimitBucket::Hourly, opened, 3).await;

    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    assert_eq!(
        remaining.get(&RateLimitBucket::Hourly),
        Some(&3),
        "after the window's close its count is zero, so the whole limit remains"
    );
    let after = enforcer
        .check_and_increment(&scope(), &policy, 1)
        .await
        .expect("the charge after the close opens a new window");
    assert_eq!(
        after.get(&RateLimitBucket::Hourly),
        Some(&2),
        "the new window counts only the charge that opened it"
    );
    let rows = rows_in(&db.pool, RateLimitBucket::Hourly).await;
    assert_eq!(
        rows.len(),
        2,
        "the new window is a row of its own: {rows:?}"
    );
    assert!(
        rows[1].0 > opened + hour - Duration::seconds(1),
        "the new window opens at the charge, after the old one closed: {rows:?}"
    );
    assert_eq!(rows[1].1, 1, "the new window's count is the one charge");

    db.remove().await;
}

#[tokio::test]
async fn a_windows_close_does_not_move_with_later_charges() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[(RateLimitBucket::Daily, 100)]);

    // A daily window opened 23 hours ago with 5 charges: it closes in an hour.
    let opened = Utc::now() - Duration::hours(23);
    row(&db.pool, RateLimitBucket::Daily, opened, 5).await;

    for _ in 0..2 {
        enforcer
            .check_and_increment(&scope(), &policy, 1)
            .await
            .expect("a charge under the limit");
    }
    enforcer
        .record(&scope(), &policy, 4)
        .await
        .expect("a record");

    let rows = rows_in(&db.pool, RateLimitBucket::Daily).await;
    assert_eq!(
        rows.len(),
        1,
        "later charges count in the open window and open no window of their own: {rows:?}"
    );
    assert!(
        (rows[0].0 - opened).num_milliseconds().abs() < 1,
        "the window keeps its open time, so its close stays one day after it: {rows:?}"
    );
    assert_eq!(rows[0].1, 11, "5, then 1, 1 and 4 in the same window");
    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    assert_eq!(remaining.get(&RateLimitBucket::Daily), Some(&89));

    db.remove().await;
}

#[tokio::test]
async fn a_refusals_retry_after_is_the_time_to_the_windows_close() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = CompositeRateLimitEnforcer::new(
        Arc::new(GovernorBurstEnforcer::new()),
        Arc::new(PostgresWindowEnforcer::new(db.pool.clone())),
    );
    let policy = policy(&[(RateLimitBucket::Daily, 3)]);

    // A full daily window opened 23 hours ago: it closes in an hour.
    row(
        &db.pool,
        RateLimitBucket::Daily,
        Utc::now() - Duration::hours(23),
        3,
    )
    .await;

    let decision = enforcer
        .check_and_increment(&scope(), &policy, 1)
        .await
        .expect("check the charge");
    assert!(!decision.allowed, "a full window refuses the charge");
    assert_eq!(decision.exhausted_bucket, Some(RateLimitBucket::Daily));
    let retry = decision.retry_after_seconds.expect("a refusal's retry");
    assert!(
        (3_590..=3_600).contains(&retry),
        "retry after {retry}s; the window closes in an hour (3,600s), not a full day"
    );

    db.remove().await;
}

#[tokio::test]
async fn the_cleanup_keeps_every_open_window_and_deletes_closed_ones() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let now = Utc::now();

    // For each bucket, a window that opened a minute short of its length ago
    // (open, counted) and one that opened a minute past it (closed).
    let buckets = [
        RateLimitBucket::Hourly,
        RateLimitBucket::Daily,
        RateLimitBucket::Weekly,
        RateLimitBucket::Monthly,
    ];
    for bucket in buckets {
        row(
            &db.pool,
            bucket,
            now - window_of(bucket) + Duration::minutes(1),
            1,
        )
        .await;
        row(
            &db.pool,
            bucket,
            now - window_of(bucket) - Duration::minutes(1),
            1,
        )
        .await;
    }
    enforcer
        .cleanup_expired_counters()
        .await
        .expect("the cleanup runs");

    for bucket in buckets {
        let rows = rows_in(&db.pool, bucket).await;
        assert_eq!(
            rows.len(),
            1,
            "the {} cleanup keeps the open window and deletes the closed one: {rows:?}",
            bucket_name(bucket)
        );
        assert!(
            rows[0].0 > now - window_of(bucket),
            "the {} row kept is the open window: {rows:?}",
            bucket_name(bucket)
        );
    }

    db.remove().await;
}

#[tokio::test]
async fn a_row_of_the_sliding_form_reads_as_a_closed_window() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let enforcer = PostgresWindowEnforcer::new(db.pool.clone());
    let policy = policy(&[(RateLimitBucket::Daily, 10), (RateLimitBucket::Monthly, 10)]);

    // Rows the sliding enforcer stored for a charge an hour ago:
    // `window_start` the charge's time less the window. Read as windows, each
    // opened one window length before the charge and closed at the charge.
    let charged = Utc::now() - Duration::hours(1);
    for bucket in [RateLimitBucket::Daily, RateLimitBucket::Monthly] {
        row(&db.pool, bucket, charged - window_of(bucket), 7).await;
    }

    let remaining = enforcer.remaining(&scope(), &policy).await.unwrap();
    assert_eq!(
        remaining.get(&RateLimitBucket::Daily),
        Some(&10),
        "an old row's window closed at its charge, so the day reads zero"
    );
    assert_eq!(remaining.get(&RateLimitBucket::Monthly), Some(&10));

    enforcer
        .check_and_increment(&scope(), &policy, 1)
        .await
        .expect("the next charge opens a new window");
    for bucket in [RateLimitBucket::Daily, RateLimitBucket::Monthly] {
        let rows = rows_in(&db.pool, bucket).await;
        assert_eq!(
            rows.len(),
            2,
            "the old row stays a window of its own: {rows:?}"
        );
        assert_eq!(rows[1].1, 1, "the new window counts only the new charge");
    }

    // A window of the new form opened two hours ago is open, whatever form
    // the rows before it had.
    let other = RateLimitScope::User {
        tenant_id: TenantId::consumer(),
        user_id: "person-2".into(),
    };
    let (scope_type, scope_id) = PostgresWindowEnforcer::scope_parts(&other);
    sqlx::query(
        "INSERT INTO rate_limit_counters \
         (scope_type, scope_id, resource_type, bucket, window_start, counter) \
         VALUES ($1, $2, 'agent_execution', 'daily', $3, 4)",
    )
    .bind(scope_type)
    .bind(&scope_id)
    .bind(Utc::now() - Duration::hours(2))
    .execute(&db.pool)
    .await
    .unwrap();
    let remaining = enforcer.remaining(&other, &policy).await.unwrap();
    assert_eq!(
        remaining.get(&RateLimitBucket::Daily),
        Some(&6),
        "a window opened two hours ago is open and counts its 4"
    );

    db.remove().await;
}
