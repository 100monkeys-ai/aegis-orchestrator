// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Governor-backed Burst Enforcer (ADR-072)
//!
//! Handles `PerMinute` rate limiting using in-memory token buckets from the
//! `governor` crate. Each unique (scope, resource_type) pair gets its own
//! rate limiter, lazily created on first access and stored in a `DashMap`.

use std::num::NonZeroU32;
use std::sync::Arc;

use dashmap::DashMap;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};

use crate::domain::rate_limit::{
    RateLimitBucket, RateLimitError, RateLimitPolicy, RateLimitResourceType, RateLimitScope,
};

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// In-memory per-minute burst enforcer using governor token buckets.
///
/// Thread-safe and lock-free for concurrent access. Each unique
/// (scope, resource_type) pair maintains an independent token bucket.
pub struct GovernorBurstEnforcer {
    limiters: DashMap<String, Arc<Limiter>>,
}

impl GovernorBurstEnforcer {
    pub fn new() -> Self {
        Self {
            limiters: DashMap::new(),
        }
    }

    fn scope_key(scope: &RateLimitScope, resource_type: &RateLimitResourceType) -> String {
        format!("{:?}:{:?}", scope, resource_type)
    }

    /// Check the per-minute bucket. Returns `Ok(Some(remaining))` or
    /// `Err` if rate limited. Returns `Ok(None)` if no per-minute window
    /// is configured in the policy.
    pub fn check_burst(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
        cost: u64,
    ) -> Result<Option<u64>, RateLimitError> {
        let per_min = match policy.windows.get(&RateLimitBucket::PerMinute) {
            Some(w) => w,
            None => return Ok(None),
        };

        let key = Self::scope_key(scope, &policy.resource_type);
        let limiter = self
            .limiters
            .entry(key)
            .or_insert_with(|| {
                let burst = per_min.burst.unwrap_or(per_min.limit);
                let limit = NonZeroU32::new(per_min.limit as u32).unwrap_or(NonZeroU32::MIN);
                let burst_size = NonZeroU32::new(burst as u32).unwrap_or(NonZeroU32::MIN);
                Arc::new(RateLimiter::direct(
                    Quota::per_minute(limit).allow_burst(burst_size),
                ))
            })
            .clone();

        for _ in 0..cost {
            if limiter.check().is_err() {
                return Err(RateLimitError::EnforcementFailed(
                    "per-minute rate limit exceeded".into(),
                ));
            }
        }

        Ok(Some(per_min.limit.saturating_sub(cost)))
    }

    /// Best-effort remaining count for the per-minute bucket.
    ///
    /// Governor does not expose an exact remaining count, so this
    /// approximates by testing whether one more request would succeed.
    pub fn remaining_burst(&self, scope: &RateLimitScope, policy: &RateLimitPolicy) -> Option<u64> {
        let per_min = policy.windows.get(&RateLimitBucket::PerMinute)?;
        let key = Self::scope_key(scope, &policy.resource_type);
        match self.limiters.get(&key) {
            Some(limiter) => {
                if limiter.check().is_ok() {
                    Some(per_min.limit.saturating_sub(1))
                } else {
                    Some(0)
                }
            }
            None => Some(per_min.limit),
        }
    }
}

impl Default for GovernorBurstEnforcer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::domain::rate_limit::{RateLimitScope, RateLimitWindow};

    fn user_scope() -> RateLimitScope {
        RateLimitScope::User {
            user_id: "user-1".into(),
        }
    }

    fn policy_with_per_minute(limit: u64) -> RateLimitPolicy {
        let mut windows = HashMap::new();
        windows.insert(
            RateLimitBucket::PerMinute,
            RateLimitWindow {
                limit,
                window_seconds: 60,
                burst: None,
            },
        );
        RateLimitPolicy {
            resource_type: RateLimitResourceType::LlmCall,
            windows,
        }
    }

    #[test]
    fn allows_within_limit() {
        let enforcer = GovernorBurstEnforcer::new();
        let scope = user_scope();
        let policy = policy_with_per_minute(10);

        let result = enforcer.check_burst(&scope, &policy, 1);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(9));
    }

    #[test]
    fn returns_none_when_no_per_minute_window() {
        let enforcer = GovernorBurstEnforcer::new();
        let scope = user_scope();
        let policy = RateLimitPolicy {
            resource_type: RateLimitResourceType::AgentExecution,
            windows: HashMap::new(),
        };

        let result = enforcer.check_burst(&scope, &policy, 1);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn remaining_returns_full_limit_for_unseen_scope() {
        let enforcer = GovernorBurstEnforcer::new();
        let scope = user_scope();
        let policy = policy_with_per_minute(20);

        let remaining = enforcer.remaining_burst(&scope, &policy);
        assert_eq!(remaining, Some(20));
    }
}
