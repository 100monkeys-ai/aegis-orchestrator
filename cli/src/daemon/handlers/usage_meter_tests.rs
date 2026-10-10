// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The usage a person reads at `GET /v1/user/rate-limits/usage` is what the
//! writers counted for that person (Zaru ADR-0064 D2).
//!
//! Counters are written through the Postgres window enforcer, as every writer
//! writes them, and read through [`super::user_rate_limit_usage`], the
//! handler's body. CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL`;
//! each test makes its own database from the migrations this binary ships and
//! drops it. Without the variable the tests say they were skipped and pass.

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use super::{user_rate_limit_usage, UserRateLimitUsageItem};
use crate::daemon::handlers::test_support::consumer;
use aegis_orchestrator_core::domain::iam::{resolve_effective_tenant, UserIdentity};
use aegis_orchestrator_core::domain::rate_limit::{
    RateLimitPolicyResolver, RateLimitResourceType, RateLimitScope,
};
use aegis_orchestrator_core::infrastructure::rate_limit::{
    HierarchicalPolicyResolver, PostgresWindowEnforcer, RateLimitOverrideRepository,
};

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
        let name = format!("aegis_usage_{}", uuid::Uuid::new_v4().simple());
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
        crate::daemon::migrations::MIGRATOR
            .run(&pool)
            .await
            .expect("apply the migrations");
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

/// Write one charge through the enforcer under the key the writers use for
/// this identity: the tenant the execution, inner-loop and SEAL writers bind
/// (the identity's claim) and the identity's `sub`.
async fn write(pool: &PgPool, identity: &UserIdentity, resource: RateLimitResourceType, cost: u64) {
    let tenant_id = resolve_effective_tenant(Some(identity), None);
    let policy = HierarchicalPolicyResolver::new(pool.clone())
        .resolve_policy(identity, &tenant_id, &resource)
        .await
        .expect("resolve the policy");
    let scope = RateLimitScope::User {
        tenant_id,
        user_id: identity.sub.clone(),
    };
    PostgresWindowEnforcer::new(pool.clone())
        .check_and_increment(&scope, &policy, cost)
        .await
        .expect("the charge is within the limit");
}

fn count(items: &[UserRateLimitUsageItem], resource: &str, bucket: &str) -> i64 {
    items
        .iter()
        .find(|item| item.resource_type == resource && item.bucket == bucket)
        .unwrap_or_else(|| panic!("the usage carries no {resource} {bucket} item"))
        .current_count
}

#[tokio::test]
async fn two_llm_calls_in_one_window_read_as_their_sum_under_the_writers_key() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let alice = consumer("meters-alice");
    write(&db.pool, &alice, RateLimitResourceType::LlmCall, 1).await;
    write(&db.pool, &alice, RateLimitResourceType::LlmToken, 120).await;
    write(&db.pool, &alice, RateLimitResourceType::LlmCall, 1).await;
    write(&db.pool, &alice, RateLimitResourceType::LlmToken, 80).await;

    let repo = RateLimitOverrideRepository::new(db.pool.clone());
    let items = user_rate_limit_usage(&repo, &alice)
        .await
        .expect("read the usage");

    for bucket in ["hourly", "daily", "weekly", "monthly"] {
        assert_eq!(
            count(&items, "llm_call", bucket),
            2,
            "two LLM calls in one {bucket} window read as 2"
        );
        assert_eq!(
            count(&items, "llm_token", bucket),
            200,
            "120 and 80 tokens in one {bucket} window read as 200"
        );
    }
    db.remove().await;
}

#[tokio::test]
async fn usage_carries_no_per_minute_item() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let alice = consumer("meters-alice");
    write(&db.pool, &alice, RateLimitResourceType::LlmCall, 1).await;

    let repo = RateLimitOverrideRepository::new(db.pool.clone());
    let items = user_rate_limit_usage(&repo, &alice)
        .await
        .expect("read the usage");

    assert!(!items.is_empty(), "the usage carries the policy's windows");
    let per_minute: Vec<_> = items
        .iter()
        .filter(|item| item.bucket == "per_minute")
        .map(|item| item.resource_type.as_str())
        .collect();
    assert!(
        per_minute.is_empty(),
        "per-minute is counted only in memory, so the usage carries no per_minute item; found {per_minute:?}"
    );
    db.remove().await;
}

#[tokio::test]
async fn identity_with_no_rows_reads_zero() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let alice = consumer("meters-alice");
    let bob = consumer("meters-bob");
    write(&db.pool, &alice, RateLimitResourceType::LlmCall, 1).await;

    let repo = RateLimitOverrideRepository::new(db.pool.clone());
    let items = user_rate_limit_usage(&repo, &bob)
        .await
        .expect("read the usage");

    assert!(!items.is_empty(), "the usage carries the policy's windows");
    for item in &items {
        assert_eq!(
            item.current_count, 0,
            "an identity with no rows reads 0 on {} {}",
            item.resource_type, item.bucket
        );
    }
    db.remove().await;
}

#[tokio::test]
async fn a_charge_older_than_the_window_is_not_counted() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let alice = consumer("meters-alice");
    let scope = RateLimitScope::User {
        tenant_id: resolve_effective_tenant(Some(&alice), None),
        user_id: alice.sub.clone(),
    };
    let (scope_type, scope_id) = PostgresWindowEnforcer::scope_parts(&scope);
    // A charge made two days ago, stored as the enforcer stores it: its
    // `window_start` is the charge's time minus the bucket's window.
    let charged_at = chrono::Utc::now() - chrono::Duration::days(2);
    for (bucket, window) in [
        ("daily", chrono::Duration::days(1)),
        ("weekly", chrono::Duration::days(7)),
    ] {
        sqlx::query(
            "INSERT INTO rate_limit_counters \
             (scope_type, scope_id, resource_type, bucket, window_start, counter) \
             VALUES ($1, $2, 'llm_call', $3, $4, 1)",
        )
        .bind(scope_type)
        .bind(&scope_id)
        .bind(bucket)
        .bind(charged_at - window)
        .execute(&db.pool)
        .await
        .expect("insert the old charge");
    }

    let repo = RateLimitOverrideRepository::new(db.pool.clone());
    let items = user_rate_limit_usage(&repo, &alice)
        .await
        .expect("read the usage");

    assert_eq!(
        count(&items, "llm_call", "daily"),
        0,
        "a charge two days old is outside the daily window"
    );
    assert_eq!(
        count(&items, "llm_call", "weekly"),
        1,
        "a charge two days old is inside the weekly window"
    );
    db.remove().await;
}
