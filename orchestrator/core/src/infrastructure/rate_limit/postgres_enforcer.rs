// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Sliding Window Enforcer (ADR-072)
//!
//! Handles `Hourly`, `Daily`, `Weekly`, and `Monthly` rate limit buckets
//! using PostgreSQL sliding window counters. The `PerMinute` bucket is
//! intentionally skipped — it is handled by [`super::GovernorBurstEnforcer`].

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::domain::rate_limit::{
    RateLimitBucket, RateLimitError, RateLimitPolicy, RateLimitResourceType, RateLimitScope,
};

/// Persistent sliding-window enforcer backed by PostgreSQL.
///
/// Each charge is stored as a row keyed by its scope, resource, bucket and
/// `window_start`; a window's usage is the sum of its rows inside the window,
/// and `check_and_increment` compares that sum plus the new charge against
/// the limit before storing anything.
pub struct PostgresWindowEnforcer {
    pool: PgPool,
}

impl PostgresWindowEnforcer {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The `(scope_type, scope_id)` a scope's counters are stored under.
    ///
    /// Every writer keys its counters through this function, and the usage
    /// a person reads is keyed through it too, so the two cannot drift.
    pub fn scope_parts(scope: &RateLimitScope) -> (&str, String) {
        match scope {
            RateLimitScope::User { tenant_id, user_id } => {
                ("user", format!("{}:{}", tenant_id.as_str(), user_id))
            }
            RateLimitScope::Tenant { tenant_id } => ("tenant", tenant_id.as_str().to_owned()),
        }
    }

    fn resource_type_str(resource_type: &RateLimitResourceType) -> String {
        match resource_type {
            RateLimitResourceType::SealToolCall { tool_pattern } => {
                format!("seal_tool:{tool_pattern}")
            }
            RateLimitResourceType::AgentExecution => "agent_execution".into(),
            RateLimitResourceType::WorkflowExecution => "workflow_execution".into(),
            RateLimitResourceType::LlmCall => "llm_call".into(),
            RateLimitResourceType::LlmToken => "llm_token".into(),
        }
    }

    fn bucket_str(bucket: &RateLimitBucket) -> &'static str {
        match bucket {
            RateLimitBucket::PerMinute => "per_minute",
            RateLimitBucket::Hourly => "hourly",
            RateLimitBucket::Daily => "daily",
            RateLimitBucket::Weekly => "weekly",
            RateLimitBucket::Monthly => "monthly",
        }
    }

    /// The `window_start` a charge made at `now` is stored with: the charge's
    /// time minus the bucket's window. It is part of the counters' unique
    /// key, so each charge is a row of its own.
    fn window_start(now: DateTime<Utc>, bucket: &RateLimitBucket) -> DateTime<Utc> {
        now - Duration::seconds(bucket.window_seconds() as i64)
    }

    /// The lowest `window_start` a row inside the bucket's window carries at
    /// `now` (Zaru ADR-0064 U1).
    ///
    /// A row's charge time is its `window_start` plus the window, and the
    /// charge is inside the window when that time is at or after
    /// `now - window`, which is `window_start >= now - 2 * window`.
    pub fn window_lower_bound(now: DateTime<Utc>, bucket: &RateLimitBucket) -> DateTime<Utc> {
        now - Duration::seconds(2 * bucket.window_seconds() as i64)
    }

    /// The sum of the scope's charges inside the bucket's window at `now`.
    async fn window_sum<'e, E>(
        executor: E,
        (scope_type, scope_id): (&str, &str),
        resource_type: &str,
        bucket: &RateLimitBucket,
        now: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error>
    where
        E: sqlx::PgExecutor<'e>,
    {
        let (sum,) = sqlx::query_as::<_, (i64,)>(
            r#"
            SELECT COALESCE(SUM(counter), 0)::BIGINT
            FROM rate_limit_counters
            WHERE scope_type = $1 AND scope_id = $2 AND resource_type = $3
              AND bucket = $4 AND window_start >= $5
            "#,
        )
        .bind(scope_type)
        .bind(scope_id)
        .bind(resource_type)
        .bind(Self::bucket_str(bucket))
        .bind(Self::window_lower_bound(now, bucket))
        .fetch_one(executor)
        .await?;
        Ok(sum.max(0) as u64)
    }

    /// Check and increment counters for all non-PerMinute windows.
    ///
    /// Each window's limit bounds the sum of the charges inside the window
    /// (ADR-072). In one transaction, holding a transaction-scoped advisory
    /// lock on the scope and resource so that two concurrent charges are
    /// summed one after the other, every window's sum plus `cost` is
    /// compared against its limit; the charge is stored in every window only
    /// if all of them admit it, and in none otherwise.
    ///
    /// Returns remaining quota per bucket on success, or the first exceeded
    /// bucket (with its remaining count before this charge) on failure.
    pub async fn check_and_increment(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
        cost: u64,
    ) -> Result<HashMap<RateLimitBucket, u64>, (RateLimitBucket, u64)> {
        let (scope_type, scope_id) = Self::scope_parts(scope);
        let resource_type = Self::resource_type_str(&policy.resource_type);
        let windows: Vec<_> = policy
            .windows
            .iter()
            .filter(|(bucket, _)| **bucket != RateLimitBucket::PerMinute) // GovernorBurstEnforcer's
            .collect();
        let mut remaining = HashMap::new();
        let Some((first_bucket, _)) = windows.first() else {
            return Ok(remaining);
        };
        let storage_failed = |e: sqlx::Error| {
            tracing::error!(error = %e, "rate limit window check failed");
            (**first_bucket, 0u64)
        };

        let mut tx = self.pool.begin().await.map_err(storage_failed)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "rate_limit:{scope_type}:{scope_id}:{resource_type}"
            ))
            .execute(&mut *tx)
            .await
            .map_err(storage_failed)?;

        let now = Utc::now();
        for (bucket, window) in &windows {
            let used = Self::window_sum(
                &mut *tx,
                (scope_type, &scope_id),
                &resource_type,
                bucket,
                now,
            )
            .await
            .map_err(storage_failed)?;
            let after = used.saturating_add(cost);
            if after > window.limit {
                // Nothing was written; dropping the transaction releases the lock.
                return Err((**bucket, window.limit.saturating_sub(used)));
            }
            remaining.insert(**bucket, window.limit - after);
        }

        for (bucket, _) in &windows {
            sqlx::query(
                r#"
                INSERT INTO rate_limit_counters
                    (scope_type, scope_id, resource_type, bucket, window_start, counter)
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (scope_type, scope_id, resource_type, bucket, window_start)
                DO UPDATE SET counter = rate_limit_counters.counter + $6,
                              updated_at = NOW()
                "#,
            )
            .bind(scope_type)
            .bind(&scope_id)
            .bind(&resource_type)
            .bind(Self::bucket_str(bucket))
            .bind(Self::window_start(now, bucket))
            .bind(cost as i64)
            .execute(&mut *tx)
            .await
            .map_err(storage_failed)?;
        }
        tx.commit().await.map_err(storage_failed)?;

        Ok(remaining)
    }

    /// Query current remaining quota for all non-PerMinute windows without
    /// incrementing counters: each window's limit less the sum of the
    /// charges inside it.
    pub async fn remaining(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
    ) -> Result<HashMap<RateLimitBucket, u64>, RateLimitError> {
        let (scope_type, scope_id) = Self::scope_parts(scope);
        let resource_type = Self::resource_type_str(&policy.resource_type);
        let now = Utc::now();
        let mut result = HashMap::new();

        for (bucket, window) in &policy.windows {
            if *bucket == RateLimitBucket::PerMinute {
                continue;
            }

            let used = Self::window_sum(
                &self.pool,
                (scope_type, &scope_id),
                &resource_type,
                bucket,
                now,
            )
            .await
            .map_err(|e| RateLimitError::StorageError(e.to_string()))?;
            result.insert(*bucket, window.limit.saturating_sub(used));
        }

        Ok(result)
    }

    /// Delete the counter rows whose charge has left its own bucket's window.
    ///
    /// A row of an hourly, daily, weekly or monthly bucket is expired when
    /// its `window_start` is below [`Self::window_lower_bound`] for that
    /// bucket, the bound the readers sum from, so no row a window still
    /// counts is deleted. A row of any other bucket (none is stored: the
    /// per-minute window is counted in memory) keeps the rule of 35 days.
    pub async fn cleanup_expired_counters(&self) -> Result<u64, sqlx::Error> {
        let now = Utc::now();
        let mut query = sqlx::query(
            r#"
            DELETE FROM rate_limit_counters
            WHERE window_start < CASE bucket
                WHEN $1 THEN $2
                WHEN $3 THEN $4
                WHEN $5 THEN $6
                WHEN $7 THEN $8
                ELSE $9
            END
            "#,
        );
        for bucket in [
            RateLimitBucket::Hourly,
            RateLimitBucket::Daily,
            RateLimitBucket::Weekly,
            RateLimitBucket::Monthly,
        ] {
            query = query
                .bind(Self::bucket_str(&bucket))
                .bind(Self::window_lower_bound(now, &bucket));
        }
        let result = query
            .bind(now - Duration::days(35))
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }
}
