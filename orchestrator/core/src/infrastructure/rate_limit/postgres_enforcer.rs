// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Fixed Window Enforcer (ADR-072)
//!
//! Handles `Hourly`, `Daily`, `Weekly`, and `Monthly` rate limit buckets
//! using PostgreSQL fixed window counters. The `PerMinute` bucket is
//! intentionally skipped — it is handled by [`super::GovernorBurstEnforcer`].
//!
//! Each bucket is a fixed window: it opens at the first charge after the
//! previous window's close and closes one window length later. Every charge
//! inside it counts against the limit; at the close the count is zero, and
//! the next charge opens the next window. A window is one counter row, keyed
//! by its open time (`window_start`), so its close is fixed from the moment
//! it opens.

use std::collections::HashMap;

use chrono::{DateTime, Duration, SubsecRound, Utc};
use sqlx::PgPool;

use crate::domain::rate_limit::{
    RateLimitBucket, RateLimitError, RateLimitPolicy, RateLimitResourceType, RateLimitScope,
};

/// Persistent fixed-window enforcer backed by PostgreSQL.
///
/// A window is a row keyed by its scope, resource, bucket and open time
/// (`window_start`); its `counter` is the sum of the charges made while it
/// is open. `check_and_increment` compares the open window's count plus the
/// new charge against the limit before storing anything.
pub struct PostgresWindowEnforcer {
    pool: PgPool,
}

/// A bucket's open window: its count and its open and close times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenWindow {
    pub count: u64,
    pub opened_at: DateTime<Utc>,
    pub closes_at: DateTime<Utc>,
}

/// A charge one window refused: the bucket, what it had left before the
/// charge, and the whole seconds until it closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRefusal {
    pub bucket: RateLimitBucket,
    pub remaining: u64,
    pub retry_after_seconds: u64,
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

    fn window_length(bucket: &RateLimitBucket) -> Duration {
        Duration::seconds(bucket.window_seconds() as i64)
    }

    /// The present at the precision PostgreSQL stores (`TIMESTAMPTZ` holds
    /// microseconds), so a window's stored open time is the one compared.
    fn now() -> DateTime<Utc> {
        Utc::now().trunc_subsecs(6)
    }

    /// When the window of `bucket` opened at `opened_at` closes: one window
    /// length after it opened.
    pub fn window_close(opened_at: DateTime<Utc>, bucket: &RateLimitBucket) -> DateTime<Utc> {
        opened_at + Self::window_length(bucket)
    }

    /// The lowest `window_start` of a window of `bucket` still open at `now`.
    ///
    /// A window is open while `now < window_start + window`, which is
    /// `window_start > now - window`; open times are stored in whole
    /// microseconds, so that is `window_start >= now - window + 1µs`. A row
    /// below this bound is a closed window, counted by no window.
    pub fn open_window_bound(now: DateTime<Utc>, bucket: &RateLimitBucket) -> DateTime<Utc> {
        now.trunc_subsecs(6) - Self::window_length(bucket) + Duration::microseconds(1)
    }

    /// The whole seconds from `now` until `closes_at`, rounded up.
    fn seconds_until(now: DateTime<Utc>, closes_at: DateTime<Utc>) -> u64 {
        let millis = (closes_at - now).num_milliseconds().max(0);
        (millis as u64).div_ceil(1_000)
    }

    /// The scope's open window of `bucket` at `now`, if one is open.
    async fn open_window<'e, E>(
        executor: E,
        (scope_type, scope_id): (&str, &str),
        resource_type: &str,
        bucket: &RateLimitBucket,
        now: DateTime<Utc>,
    ) -> Result<Option<OpenWindow>, sqlx::Error>
    where
        E: sqlx::PgExecutor<'e>,
    {
        let (opened_at, count) = sqlx::query_as::<_, (Option<DateTime<Utc>>, i64)>(
            r#"
            SELECT MIN(window_start), COALESCE(SUM(counter), 0)::BIGINT
            FROM rate_limit_counters
            WHERE scope_type = $1 AND scope_id = $2 AND resource_type = $3
              AND bucket = $4 AND window_start >= $5
            "#,
        )
        .bind(scope_type)
        .bind(scope_id)
        .bind(resource_type)
        .bind(Self::bucket_str(bucket))
        .bind(Self::open_window_bound(now, bucket))
        .fetch_one(executor)
        .await?;
        Ok(opened_at.map(|opened_at| OpenWindow {
            count: count.max(0) as u64,
            opened_at,
            closes_at: Self::window_close(opened_at, bucket),
        }))
    }

    /// Add `cost` to the window of `bucket` opened at `opened_at`, creating
    /// its row when the charge opens it.
    async fn charge_window(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        (scope_type, scope_id): (&str, &str),
        resource_type: &str,
        bucket: &RateLimitBucket,
        opened_at: DateTime<Utc>,
        cost: u64,
    ) -> Result<(), sqlx::Error> {
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
        .bind(scope_id)
        .bind(resource_type)
        .bind(Self::bucket_str(bucket))
        .bind(opened_at)
        .bind(cost as i64)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Begin a transaction holding the transaction-scoped advisory lock on
    /// the scope and resource, so two charges of one key open and count
    /// windows one after the other.
    async fn locked_transaction(
        &self,
        (scope_type, scope_id): (&str, &str),
        resource_type: &str,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "rate_limit:{scope_type}:{scope_id}:{resource_type}"
            ))
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    /// Check and increment counters for all non-PerMinute windows.
    ///
    /// Each bucket's open window, or the window this charge would open, has
    /// its count plus `cost` compared against its limit (ADR-072), in one
    /// transaction under the advisory lock. The charge is stored in every
    /// window only if all of them admit it, and in none otherwise; a charge
    /// with no open window opens one at the charge's time.
    ///
    /// Returns remaining quota per bucket on success, or the first window
    /// that refused (its remaining count before this charge and the seconds
    /// until it closes) on failure. A charge larger than the limit of a
    /// bucket with no open window is refused with the full window: the close
    /// of the window it would have opened.
    pub async fn check_and_increment(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
        cost: u64,
    ) -> Result<HashMap<RateLimitBucket, u64>, WindowRefusal> {
        let (scope_type, scope_id) = Self::scope_parts(scope);
        let resource_type = Self::resource_type_str(&policy.resource_type);
        let windows: Vec<_> = policy
            .windows
            .iter()
            .filter(|(bucket, _)| **bucket != RateLimitBucket::PerMinute) // GovernorBurstEnforcer's
            .collect();
        let mut remaining = HashMap::new();
        let Some((first_bucket, first_window)) = windows.first() else {
            return Ok(remaining);
        };
        let storage_failed = |e: sqlx::Error| {
            tracing::error!(error = %e, "rate limit window check failed");
            WindowRefusal {
                bucket: **first_bucket,
                remaining: 0,
                retry_after_seconds: first_window.window_seconds,
            }
        };

        let key = (scope_type, scope_id.as_str());
        let mut tx = self
            .locked_transaction(key, &resource_type)
            .await
            .map_err(storage_failed)?;

        let now = Self::now();
        let mut opened = Vec::with_capacity(windows.len());
        for (bucket, window) in &windows {
            let open = Self::open_window(&mut *tx, key, &resource_type, bucket, now)
                .await
                .map_err(storage_failed)?;
            let (used, opened_at) = open.map_or((0, now), |w| (w.count, w.opened_at));
            let after = used.saturating_add(cost);
            if after > window.limit {
                // Nothing was written; dropping the transaction releases the lock.
                return Err(WindowRefusal {
                    bucket: **bucket,
                    remaining: window.limit.saturating_sub(used),
                    retry_after_seconds: Self::seconds_until(
                        now,
                        Self::window_close(opened_at, bucket),
                    ),
                });
            }
            remaining.insert(**bucket, window.limit - after);
            opened.push((**bucket, opened_at));
        }

        for (bucket, opened_at) in &opened {
            Self::charge_window(&mut tx, key, &resource_type, bucket, *opened_at, cost)
                .await
                .map_err(storage_failed)?;
        }
        tx.commit().await.map_err(storage_failed)?;

        Ok(remaining)
    }

    /// Store `cost` in every non-PerMinute window of `policy` for `scope`
    /// without comparing it against the limit.
    ///
    /// The charge counts in each bucket's open window, or opens one, as
    /// [`Self::check_and_increment`] stores an admitted charge, in one
    /// transaction under the same advisory lock, so a concurrent check reads
    /// either none or all of the record's windows.
    pub async fn record(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
        cost: u64,
    ) -> Result<(), RateLimitError> {
        let (scope_type, scope_id) = Self::scope_parts(scope);
        let resource_type = Self::resource_type_str(&policy.resource_type);
        let buckets: Vec<_> = policy
            .windows
            .keys()
            .filter(|bucket| **bucket != RateLimitBucket::PerMinute) // GovernorBurstEnforcer's
            .collect();
        if buckets.is_empty() {
            return Ok(());
        }
        let storage_failed = |e: sqlx::Error| RateLimitError::StorageError(e.to_string());

        let key = (scope_type, scope_id.as_str());
        let mut tx = self
            .locked_transaction(key, &resource_type)
            .await
            .map_err(storage_failed)?;

        let now = Self::now();
        for bucket in buckets {
            let opened_at = Self::open_window(&mut *tx, key, &resource_type, bucket, now)
                .await
                .map_err(storage_failed)?
                .map_or(now, |w| w.opened_at);
            Self::charge_window(&mut tx, key, &resource_type, bucket, opened_at, cost)
                .await
                .map_err(storage_failed)?;
        }
        tx.commit().await.map_err(storage_failed)?;

        Ok(())
    }

    /// The open window of every non-PerMinute bucket of `policy` for
    /// `scope`: its count and its close. A bucket with no open window is
    /// absent.
    pub async fn open_windows(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
    ) -> Result<HashMap<RateLimitBucket, OpenWindow>, RateLimitError> {
        let (scope_type, scope_id) = Self::scope_parts(scope);
        let resource_type = Self::resource_type_str(&policy.resource_type);
        let now = Self::now();
        let mut result = HashMap::new();

        for bucket in policy.windows.keys() {
            if *bucket == RateLimitBucket::PerMinute {
                continue;
            }
            let open = Self::open_window(
                &self.pool,
                (scope_type, &scope_id),
                &resource_type,
                bucket,
                now,
            )
            .await
            .map_err(|e| RateLimitError::StorageError(e.to_string()))?;
            if let Some(open) = open {
                result.insert(*bucket, open);
            }
        }

        Ok(result)
    }

    /// Query current remaining quota for all non-PerMinute windows without
    /// incrementing counters: each window's limit less its open window's
    /// count, the whole limit for a bucket with no open window.
    pub async fn remaining(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
    ) -> Result<HashMap<RateLimitBucket, u64>, RateLimitError> {
        let open = self.open_windows(scope, policy).await?;
        Ok(policy
            .windows
            .iter()
            .filter(|(bucket, _)| **bucket != RateLimitBucket::PerMinute)
            .map(|(bucket, window)| {
                let used = open.get(bucket).map_or(0, |w| w.count);
                (*bucket, window.limit.saturating_sub(used))
            })
            .collect())
    }

    /// Delete the counter rows whose window has closed.
    ///
    /// A row of an hourly, daily, weekly or monthly bucket is one window, and
    /// it has closed when its `window_start` is below
    /// [`Self::open_window_bound`] for that bucket; a closed window is
    /// counted by no window, so no row a window still counts is deleted. A
    /// row of any other bucket (none is stored: the per-minute window is
    /// counted in memory) keeps the rule of 35 days.
    pub async fn cleanup_expired_counters(&self) -> Result<u64, sqlx::Error> {
        let now = Self::now();
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
                .bind(Self::open_window_bound(now, &bucket));
        }
        let result = query
            .bind(now - Duration::days(35))
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }
}
