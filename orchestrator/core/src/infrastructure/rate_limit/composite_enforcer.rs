// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Composite Rate Limit Enforcer (ADR-072)
//!
//! Combines [`GovernorBurstEnforcer`] (in-memory per-minute) and
//! [`PostgresWindowEnforcer`] (database hourly..monthly) into a single
//! implementation of the [`RateLimitEnforcer`] domain trait.
//!
//! Enforcement order:
//! 1. **Burst check** (fast, in-memory) — fail-fast on per-minute exhaustion
//! 2. **Window check** (PostgreSQL) — atomic increment of longer windows

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::rate_limit::{
    RateLimitBucket, RateLimitDecision, RateLimitEnforcer, RateLimitError, RateLimitPolicy,
    RateLimitScope,
};

/// Threshold (percentage) at which a rate-limit warning is emitted.
const RATE_LIMIT_WARNING_THRESHOLD_PCT: f64 = 80.0;

use super::burst_enforcer::GovernorBurstEnforcer;
use super::postgres_enforcer::PostgresWindowEnforcer;

/// Composite enforcer that delegates to both in-memory burst and
/// PostgreSQL window enforcers.
pub struct CompositeRateLimitEnforcer {
    burst: Arc<GovernorBurstEnforcer>,
    postgres: Arc<PostgresWindowEnforcer>,
}

impl CompositeRateLimitEnforcer {
    pub fn new(burst: Arc<GovernorBurstEnforcer>, postgres: Arc<PostgresWindowEnforcer>) -> Self {
        Self { burst, postgres }
    }
}

#[async_trait]
impl RateLimitEnforcer for CompositeRateLimitEnforcer {
    async fn check_and_increment(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
        cost: u64,
    ) -> Result<RateLimitDecision, RateLimitError> {
        let mut remaining = HashMap::new();

        let resource_label = format!("{:?}", policy.resource_type);
        let scope_label = match scope {
            RateLimitScope::User { .. } => "user",
            RateLimitScope::Tenant { .. } => "tenant",
        };

        // Phase 1: Check burst (fast, in-memory)
        match self.burst.check_burst(scope, policy, cost) {
            Ok(Some(r)) => {
                remaining.insert(RateLimitBucket::PerMinute, r);
            }
            Ok(None) => {} // No per-minute window configured
            Err(_) => {
                let decision = RateLimitDecision {
                    allowed: false,
                    resource_type: policy.resource_type.clone(),
                    scope: scope.clone(),
                    exhausted_bucket: Some(RateLimitBucket::PerMinute),
                    retry_after_seconds: Some(60),
                    remaining,
                };
                emit_decision_metrics(&decision, &resource_label, scope_label);
                return Ok(decision);
            }
        }

        // Phase 2: Check longer windows (PostgreSQL)
        match self.postgres.check_and_increment(scope, policy, cost).await {
            Ok(pg_remaining) => {
                remaining.extend(pg_remaining);
            }
            Err((bucket, _)) => {
                let retry_after = bucket.window_seconds();
                let decision = RateLimitDecision {
                    allowed: false,
                    resource_type: policy.resource_type.clone(),
                    scope: scope.clone(),
                    exhausted_bucket: Some(bucket),
                    retry_after_seconds: Some(retry_after),
                    remaining,
                };
                emit_decision_metrics(&decision, &resource_label, scope_label);
                return Ok(decision);
            }
        }

        let decision = RateLimitDecision {
            allowed: true,
            resource_type: policy.resource_type.clone(),
            scope: scope.clone(),
            exhausted_bucket: None,
            retry_after_seconds: None,
            remaining,
        };
        emit_decision_metrics(&decision, &resource_label, scope_label);

        // Check for 80% threshold warning on allowed decisions
        for (bucket, remaining_count) in &decision.remaining {
            if let Some(window) = policy.windows.get(bucket) {
                if window.limit > 0 {
                    let used_pct =
                        ((window.limit - remaining_count) as f64 / window.limit as f64) * 100.0;
                    if used_pct >= RATE_LIMIT_WARNING_THRESHOLD_PCT {
                        tracing::warn!(
                            resource_type = ?policy.resource_type,
                            bucket = ?bucket,
                            limit = window.limit,
                            remaining = remaining_count,
                            used_percent = %format!("{used_pct:.1}"),
                            "rate limit threshold warning: 80% consumed"
                        );
                        metrics::counter!(
                            "aegis_rate_limit_warnings_total",
                            "resource_type" => resource_label.clone(),
                            "bucket" => format!("{bucket:?}"),
                        )
                        .increment(1);
                    }
                }
            }
        }

        Ok(decision)
    }

    async fn remaining(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
    ) -> Result<HashMap<RateLimitBucket, u64>, RateLimitError> {
        let mut result = HashMap::new();

        if let Some(r) = self.burst.remaining_burst(scope, policy) {
            result.insert(RateLimitBucket::PerMinute, r);
        }

        let pg_remaining = self.postgres.remaining(scope, policy).await?;
        result.extend(pg_remaining);

        Ok(result)
    }
}

/// Emit Prometheus metrics and structured logging for a rate-limit decision.
fn emit_decision_metrics(decision: &RateLimitDecision, resource_label: &str, scope_label: &str) {
    let decision_label = if decision.allowed {
        "allowed"
    } else {
        "rejected"
    };

    metrics::counter!(
        "aegis_rate_limit_checks_total",
        "resource_type" => resource_label.to_owned(),
        "scope_type" => scope_label.to_owned(),
        "decision" => decision_label.to_owned(),
    )
    .increment(1);

    if !decision.allowed {
        metrics::counter!(
            "aegis_rate_limit_rejections_total",
            "resource_type" => resource_label.to_owned(),
            "scope_type" => scope_label.to_owned(),
            "bucket" => decision
                .exhausted_bucket
                .map(|b| format!("{b:?}"))
                .unwrap_or_default(),
        )
        .increment(1);

        tracing::warn!(
            resource_type = %resource_label,
            scope_type = scope_label,
            bucket = ?decision.exhausted_bucket,
            retry_after_seconds = ?decision.retry_after_seconds,
            "rate limit exceeded"
        );
    }
}
