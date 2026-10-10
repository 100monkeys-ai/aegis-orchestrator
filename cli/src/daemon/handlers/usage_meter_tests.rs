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

// ----------------------------------------------------------------------
// `POST /v1/user/rate-limits/usage` (Zaru ADR-0064 D3): a client records the
// model calls it made for the caller, under the caller's own token, into
// the counters the writers write and the GET reads.
// ----------------------------------------------------------------------

use super::{user_usage_record_response, UserUsageRecordRequest};
use axum::http::StatusCode;
use axum::Json;

/// Record `llm_calls` and `llm_tokens` for `identity` through the route's
/// answer and return its status and JSON body.
async fn record(
    repo: Option<&RateLimitOverrideRepository>,
    identity: Option<UserIdentity>,
    llm_calls: u64,
    llm_tokens: u64,
) -> (StatusCode, serde_json::Value) {
    let response = user_usage_record_response(
        repo,
        identity,
        Ok(Json(UserUsageRecordRequest {
            llm_calls,
            llm_tokens,
        })),
    )
    .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the answer's body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn the_route_records_two_calls_and_their_summed_tokens_and_the_get_reads_the_sum() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = RateLimitOverrideRepository::new(db.pool.clone());
    let alice = consumer("record-alice");

    let (status, body) = record(Some(&repo), Some(alice.clone()), 1, 120).await;
    assert_eq!(status, StatusCode::OK, "the first step is recorded: {body}");
    let (status, body) = record(Some(&repo), Some(alice.clone()), 1, 80).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the second step is recorded: {body}"
    );
    assert_eq!(
        body["refused"],
        serde_json::json!([]),
        "no window refuses two calls and 200 tokens"
    );
    let answered = body["usage"]
        .as_array()
        .expect("the answer carries the GET's usage view");
    let hourly = |resource: &str| {
        answered
            .iter()
            .find(|item| item["resource_type"] == resource && item["bucket"] == "hourly")
            .unwrap_or_else(|| panic!("the answer carries no {resource} hourly item"))
            ["current_count"]
            .clone()
    };
    assert_eq!(hourly("llm_call"), 2, "the answer reads the two calls");
    assert_eq!(
        hourly("llm_token"),
        200,
        "the answer reads 120 and 80 tokens"
    );

    let items = user_rate_limit_usage(&repo, &alice)
        .await
        .expect("read the usage");
    for bucket in ["hourly", "daily", "weekly", "monthly"] {
        assert_eq!(
            count(&items, "llm_call", bucket),
            2,
            "two recorded calls read as 2 in the {bucket} window"
        );
        assert_eq!(
            count(&items, "llm_token", bucket),
            200,
            "120 and 80 recorded tokens read as 200 in the {bucket} window"
        );
    }
    db.remove().await;
}

#[tokio::test]
async fn a_record_over_the_request_bound_is_refused() {
    let alice = consumer("record-alice");
    for (calls, tokens) in [(1_001, 0), (0, 50_000_001)] {
        let (status, body) = record(None, Some(alice.clone()), calls, tokens).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{calls} calls and {tokens} tokens are over the request bound"
        );
        assert_eq!(body["error"], "usage over the request bound");
    }
    // At the bound the request passes it and reaches the store, which this
    // node does not have.
    let (status, _) = record(None, Some(alice), 1_000, 50_000_000).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "1,000 calls and 50,000,000 tokens are within the request bound"
    );
}

#[tokio::test]
async fn a_caller_with_no_identity_or_no_sub_is_refused() {
    let mut no_sub = consumer("record-alice");
    no_sub.sub = String::new();
    for identity in [None, Some(no_sub)] {
        let (status, body) = record(None, identity, 1, 1).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "Authentication required");
    }
}

#[tokio::test]
async fn recording_for_one_person_leaves_a_second_persons_counters_untouched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = RateLimitOverrideRepository::new(db.pool.clone());
    let alice = consumer("record-alice");
    let bob = consumer("record-bob");

    let (status, body) = record(Some(&repo), Some(alice), 3, 300).await;
    assert_eq!(status, StatusCode::OK, "alice's usage is recorded: {body}");

    let items = user_rate_limit_usage(&repo, &bob)
        .await
        .expect("read the usage");
    assert!(!items.is_empty(), "the usage carries the policy's windows");
    for item in &items {
        assert_eq!(
            item.current_count, 0,
            "bob reads 0 on {} {} after alice's record",
            item.resource_type, item.bucket
        );
    }
    db.remove().await;
}

#[tokio::test]
async fn the_route_leaves_the_per_minute_burst_as_the_writers_leave_it() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = RateLimitOverrideRepository::new(db.pool.clone());
    let alice = consumer("record-alice");

    // Over the Free tier's per-minute limits (20 calls, 20,000 tokens) and
    // within its hourly ones (100 calls, 100,000 tokens). Per-minute is the
    // in-memory burst enforcer's, which the writers charge in their own
    // process and never store.
    let (status, body) = record(Some(&repo), Some(alice.clone()), 30, 30_000).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["refused"],
        serde_json::json!([]),
        "no stored window refuses 30 calls and 30,000 tokens"
    );

    let (per_minute_rows,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM rate_limit_counters WHERE bucket = 'per_minute'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("count the per-minute rows");
    assert_eq!(per_minute_rows, 0, "the route stores no per-minute row");

    let items = user_rate_limit_usage(&repo, &alice)
        .await
        .expect("read the usage");
    assert_eq!(count(&items, "llm_call", "hourly"), 30);
    assert_eq!(count(&items, "llm_token", "hourly"), 30_000);
    assert!(
        items.iter().all(|item| item.bucket != "per_minute"),
        "the usage carries no per_minute item"
    );
    db.remove().await;
}

#[tokio::test]
async fn a_refused_window_answers_200_with_the_refusal_and_stores_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = RateLimitOverrideRepository::new(db.pool.clone());
    let alice = consumer("record-alice");

    // 101 calls are over the Free tier's hourly limit of 100 and within its
    // daily, weekly and monthly ones; the 50 tokens are within every window.
    let (status, body) = record(Some(&repo), Some(alice.clone()), 101, 50).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["refused"],
        serde_json::json!([{ "resource_type": "llm_call", "bucket": "hourly" }]),
        "the hourly call window refuses"
    );

    let items = user_rate_limit_usage(&repo, &alice)
        .await
        .expect("read the usage");
    for bucket in ["hourly", "daily", "weekly", "monthly"] {
        assert_eq!(
            count(&items, "llm_call", bucket),
            0,
            "a refused charge is stored in no {bucket} window"
        );
        assert_eq!(
            count(&items, "llm_token", bucket),
            50,
            "the tokens no window refused are stored in the {bucket} window"
        );
    }
    db.remove().await;
}

#[tokio::test]
async fn the_handlers_window_bound_is_the_enforcers_for_every_stored_bucket() {
    // The reader and the writers share one bound: the handler calls the
    // enforcer's `window_lower_bound` and keeps no copy of its own, so the two
    // cannot drift apart.
    let handler_source = include_str!("admin.rs");
    assert!(
        !handler_source.contains("fn window_lower_bound("),
        "the usage handler keeps its own copy of the read bound (`fn window_lower_bound` in admin.rs); it must call `PostgresWindowEnforcer::window_lower_bound`"
    );
    assert!(
        handler_source.contains("PostgresWindowEnforcer::window_lower_bound(now, bucket)"),
        "the usage handler does not call `PostgresWindowEnforcer::window_lower_bound(now, bucket)`"
    );

    let Some(db) = TestDb::create().await else {
        return;
    };
    let alice = consumer("meters-alice");
    let tenant_id = resolve_effective_tenant(Some(&alice), None);
    let scope = RateLimitScope::User {
        tenant_id: tenant_id.clone(),
        user_id: alice.sub.clone(),
    };
    let (scope_type, scope_id) = PostgresWindowEnforcer::scope_parts(&scope);
    // For every stored bucket, one charge a minute inside the enforcer's bound
    // and one a minute outside it.
    let now = chrono::Utc::now();
    for bucket in &super::STORED_BUCKETS {
        let bound = PostgresWindowEnforcer::window_lower_bound(now, bucket);
        for window_start in [
            bound + chrono::Duration::minutes(1),
            bound - chrono::Duration::minutes(1),
        ] {
            sqlx::query(
                "INSERT INTO rate_limit_counters \
                 (scope_type, scope_id, resource_type, bucket, window_start, counter) \
                 VALUES ($1, $2, 'llm_call', $3, $4, 1)",
            )
            .bind(scope_type)
            .bind(&scope_id)
            .bind(super::bucket_to_str(bucket))
            .bind(window_start)
            .execute(&db.pool)
            .await
            .expect("insert the charge");
        }
    }

    let repo = RateLimitOverrideRepository::new(db.pool.clone());
    let items = user_rate_limit_usage(&repo, &alice)
        .await
        .expect("read the usage");
    let policy = HierarchicalPolicyResolver::new(db.pool.clone())
        .resolve_policy(&alice, &tenant_id, &RateLimitResourceType::LlmCall)
        .await
        .expect("resolve the policy");
    let remaining = PostgresWindowEnforcer::new(db.pool.clone())
        .remaining(&scope, &policy)
        .await
        .expect("read the enforcer's remaining");

    for bucket in &super::STORED_BUCKETS {
        let name = super::bucket_to_str(bucket);
        let item = items
            .iter()
            .find(|item| item.resource_type == "llm_call" && item.bucket == name)
            .unwrap_or_else(|| panic!("the usage carries no llm_call {name} item"));
        assert_eq!(
            item.current_count, 1,
            "the {name} read counts the charge inside the enforcer's bound and not the one outside it"
        );
        assert_eq!(
            item.limit_value - remaining[bucket] as i64,
            item.current_count,
            "the handler's {name} count is the enforcer's"
        );
    }
    db.remove().await;
}
