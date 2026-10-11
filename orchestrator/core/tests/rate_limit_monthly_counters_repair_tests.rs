// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Migration 052 against a real PostgreSQL: the monthly counter rows the old
//! cleanup deleted five days after their charge are written back from the
//! weekly rows of the same charges (AEGIS known defect
//! `month-below-week-after-fix-2026-10-10`).
//!
//! Every charge writes one row per bucket with one `now`, at
//! `window_start = now - window`, so a weekly row's monthly twin has the
//! same scope, resource and counter at `window_start - 23 days`.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere it says it was skipped and passes.

use std::path::{Path, PathBuf};

use aegis_orchestrator_core::domain::rate_limit::{RateLimitBucket, RateLimitScope};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::rate_limit::PostgresWindowEnforcer;
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

fn migrations_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli/migrations")
}

const REPAIR: &str = "052_monthly_counters_from_weekly.sql";

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
        let name = format!("aegis_monthly_{}", Uuid::new_v4().simple());
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
        let mut files: Vec<_> = std::fs::read_dir(migrations_dir())
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

    /// Apply the repair migration once more, as the daemon would at a start
    /// that found it missing.
    async fn repair(&self) {
        let sql = std::fs::read_to_string(migrations_dir().join(REPAIR))
            .expect("read the repair migration");
        sqlx::raw_sql(&sql)
            .execute(&self.pool)
            .await
            .expect("apply the repair migration");
    }

    async fn remove(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("drop the test database");
    }
}

fn scope(user: &str) -> RateLimitScope {
    RateLimitScope::User {
        tenant_id: TenantId::consumer(),
        user_id: user.into(),
    }
}

/// Store one bucket's row of a charge of `counter` made at `charge`, as the
/// enforcer stores it.
async fn row(
    pool: &PgPool,
    user: &str,
    bucket: RateLimitBucket,
    charge: DateTime<Utc>,
    counter: i64,
) {
    let scope = scope(user);
    let (scope_type, scope_id) = PostgresWindowEnforcer::scope_parts(&scope);
    let name = match bucket {
        RateLimitBucket::Weekly => "weekly",
        RateLimitBucket::Monthly => "monthly",
        _ => unreachable!("only the week and the month are seeded"),
    };
    sqlx::query(
        "INSERT INTO rate_limit_counters \
         (scope_type, scope_id, resource_type, bucket, window_start, counter) \
         VALUES ($1, $2, 'llm_token', $3, $4, $5)",
    )
    .bind(scope_type)
    .bind(&scope_id)
    .bind(name)
    .bind(charge - Duration::seconds(bucket.window_seconds() as i64))
    .bind(counter)
    .execute(pool)
    .await
    .expect("store a counter row");
}

/// The week's and the month's sums of the rows stored for `user`: the rows
/// the migration writes, not a window's reading of them.
async fn week_and_month(pool: &PgPool, user: &str) -> (u64, u64) {
    let scope = scope(user);
    let (_, scope_id) = PostgresWindowEnforcer::scope_parts(&scope);
    let mut sums = [0u64; 2];
    for (sum, bucket) in sums.iter_mut().zip(["weekly", "monthly"]) {
        let total: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(counter),0)::BIGINT FROM rate_limit_counters WHERE scope_id = $1 AND bucket = $2",
        )
        .bind(&scope_id)
        .bind(bucket)
        .fetch_one(pool)
        .await
        .expect("sum the stored rows");
        *sum = total as u64;
    }
    (sums[0], sums[1])
}

async fn monthly_rows(pool: &PgPool, user: &str) -> i64 {
    let scope = scope(user);
    let (scope_type, scope_id) = PostgresWindowEnforcer::scope_parts(&scope);
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM rate_limit_counters \
         WHERE scope_type = $1 AND scope_id = $2 AND bucket = 'monthly'",
    )
    .bind(scope_type)
    .bind(&scope_id)
    .fetch_one(pool)
    .await
    .expect("count the monthly rows")
}

#[tokio::test]
async fn the_repair_brings_the_month_to_the_weeks_sum_and_a_second_run_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let now = Utc::now();
    // Three charges this week; the old cleanup deleted the monthly rows of
    // the two older than five days, and kept the newest one's.
    for (days, counter) in [(6, 700), (5, 500)] {
        row(
            &db.pool,
            "person-1",
            RateLimitBucket::Weekly,
            now - Duration::days(days),
            counter,
        )
        .await;
    }
    let newest = now - Duration::days(1);
    row(&db.pool, "person-1", RateLimitBucket::Weekly, newest, 200).await;
    row(&db.pool, "person-1", RateLimitBucket::Monthly, newest, 200).await;
    assert_eq!(week_and_month(&db.pool, "person-1").await, (1_400, 200));

    db.repair().await;
    let (week, month) = week_and_month(&db.pool, "person-1").await;
    assert_eq!(
        month, week,
        "after the repair the month read {month}, not the week's sum {week}"
    );
    assert_eq!(monthly_rows(&db.pool, "person-1").await, 3);

    db.repair().await;
    assert_eq!(
        week_and_month(&db.pool, "person-1").await,
        (1_400, 1_400),
        "a second run of the repair changed the month"
    );
    assert_eq!(
        monthly_rows(&db.pool, "person-1").await,
        3,
        "a second run of the repair wrote a monthly row"
    );

    db.remove().await;
}

#[tokio::test]
async fn the_repair_writes_no_monthly_row_for_a_scope_that_has_none() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let now = Utc::now();
    row(
        &db.pool,
        "person-2",
        RateLimitBucket::Weekly,
        now - Duration::days(6),
        300,
    )
    .await;
    // Another scope with a monthly row, so the repair has work to do.
    row(
        &db.pool,
        "person-1",
        RateLimitBucket::Weekly,
        now - Duration::days(6),
        700,
    )
    .await;
    row(
        &db.pool,
        "person-1",
        RateLimitBucket::Weekly,
        now - Duration::days(1),
        200,
    )
    .await;
    row(
        &db.pool,
        "person-1",
        RateLimitBucket::Monthly,
        now - Duration::days(1),
        200,
    )
    .await;

    db.repair().await;
    assert_eq!(
        monthly_rows(&db.pool, "person-2").await,
        0,
        "the repair wrote a monthly row for a scope that had none"
    );
    assert_eq!(week_and_month(&db.pool, "person-1").await, (900, 900));

    db.remove().await;
}
