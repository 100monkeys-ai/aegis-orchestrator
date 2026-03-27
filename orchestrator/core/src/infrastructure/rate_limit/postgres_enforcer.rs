// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Sliding Window Enforcer (ADR-072)
//!
//! Handles `Hourly`, `Daily`, `Weekly`, and `Monthly` rate limit buckets
//! using PostgreSQL sliding window counters. The `PerMinute` bucket is
//! intentionally skipped — it is handled by [`super::GovernorBurstEnforcer`].

use std::collections::HashMap;

use chrono::{Duration, Utc};
use sqlx::PgPool;

use crate::domain::rate_limit::{
    RateLimitBucket, RateLimitError, RateLimitPolicy, RateLimitResourceType, RateLimitScope,
};

/// Persistent sliding-window enforcer backed by PostgreSQL.
///
/// Uses atomic upsert (INSERT ... ON CONFLICT UPDATE ... RETURNING) to
/// check-and-increment in a single round trip. If the new counter exceeds
/// the window limit, the increment is rolled back immediately.
pub struct PostgresWindowEnforcer {
    pool: PgPool,
}

impl PostgresWindowEnforcer {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn scope_parts(scope: &RateLimitScope) -> (&str, String) {
        match scope {
            RateLimitScope::User { user_id } => ("user", user_id.clone()),
            RateLimitScope::Tenant { tenant_id } => ("tenant", tenant_id.as_str().to_owned()),
        }
    }

    fn resource_type_str(resource_type: &RateLimitResourceType) -> String {
        match resource_type {
            RateLimitResourceType::SmcpToolCall { tool_pattern } => {
                format!("smcp_tool:{tool_pattern}")
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

    fn window_start(bucket: &RateLimitBucket) -> chrono::DateTime<Utc> {
        let now = Utc::now();
        let dur = Duration::seconds(bucket.window_seconds() as i64);
        now - dur
    }

    /// Check and increment counters for all non-PerMinute windows.
    ///
    /// Returns remaining quota per bucket on success, or the first exceeded
    /// bucket (with its remaining count) on failure.
    pub async fn check_and_increment(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
        cost: u64,
    ) -> Result<HashMap<RateLimitBucket, u64>, (RateLimitBucket, u64)> {
        let (scope_type, scope_id) = Self::scope_parts(scope);
        let resource_type = Self::resource_type_str(&policy.resource_type);
        let mut remaining = HashMap::new();

        for (bucket, window) in &policy.windows {
            if *bucket == RateLimitBucket::PerMinute {
                continue; // Handled by GovernorBurstEnforcer
            }

            let bucket_str = Self::bucket_str(bucket);
            let window_start = Self::window_start(bucket);

            // Atomic upsert and return new counter value
            let row = sqlx::query_as::<_, (i64,)>(
                r#"
                INSERT INTO rate_limit_counters
                    (scope_type, scope_id, resource_type, bucket, window_start, counter)
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (scope_type, scope_id, resource_type, bucket, window_start)
                DO UPDATE SET counter = rate_limit_counters.counter + $6,
                              updated_at = NOW()
                RETURNING counter
                "#,
            )
            .bind(scope_type)
            .bind(&scope_id)
            .bind(&resource_type)
            .bind(bucket_str)
            .bind(window_start)
            .bind(cost as i64)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "rate limit counter upsert failed");
                (*bucket, 0u64)
            })?;

            let current = row.0 as u64;
            if current > window.limit {
                // Over limit — roll back the increment
                let _ = sqlx::query(
                    r#"
                    UPDATE rate_limit_counters
                    SET counter = counter - $1, updated_at = NOW()
                    WHERE scope_type = $2 AND scope_id = $3 AND resource_type = $4
                      AND bucket = $5 AND window_start = $6
                    "#,
                )
                .bind(cost as i64)
                .bind(scope_type)
                .bind(&scope_id)
                .bind(&resource_type)
                .bind(bucket_str)
                .bind(window_start)
                .execute(&self.pool)
                .await;

                return Err((
                    *bucket,
                    window.limit.saturating_sub(current.saturating_sub(cost)),
                ));
            }

            remaining.insert(*bucket, window.limit.saturating_sub(current));
        }

        Ok(remaining)
    }

    /// Query current remaining quota for all non-PerMinute windows without
    /// incrementing counters.
    pub async fn remaining(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
    ) -> Result<HashMap<RateLimitBucket, u64>, RateLimitError> {
        let (scope_type, scope_id) = Self::scope_parts(scope);
        let resource_type = Self::resource_type_str(&policy.resource_type);
        let mut result = HashMap::new();

        for (bucket, window) in &policy.windows {
            if *bucket == RateLimitBucket::PerMinute {
                continue;
            }

            let bucket_str = Self::bucket_str(bucket);
            let window_start = Self::window_start(bucket);

            let row = sqlx::query_as::<_, (i64,)>(
                r#"
                SELECT COALESCE(SUM(counter), 0)
                FROM rate_limit_counters
                WHERE scope_type = $1 AND scope_id = $2 AND resource_type = $3
                  AND bucket = $4 AND window_start >= $5
                "#,
            )
            .bind(scope_type)
            .bind(&scope_id)
            .bind(&resource_type)
            .bind(bucket_str)
            .bind(window_start)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| RateLimitError::StorageError(e.to_string()))?;

            let current = row.0 as u64;
            result.insert(*bucket, window.limit.saturating_sub(current));
        }

        Ok(result)
    }
}
