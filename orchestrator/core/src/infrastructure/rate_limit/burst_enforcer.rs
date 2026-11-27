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
    /// How many live members share the burst (AEGIS ADR-152 D4.6): each
    /// grants `ceil(limit / members)` and `ceil(burst / members)`, so the
    /// cluster together grants what one node did. 1 off a cluster.
    members: std::sync::atomic::AtomicU32,
}

/// `ceil(n / members)`, at least 1.
pub fn share_of(n: u64, members: u32) -> u32 {
    let members = u64::from(members.max(1));
    n.div_ceil(members).clamp(1, u64::from(u32::MAX)) as u32
}

impl GovernorBurstEnforcer {
    pub fn new() -> Self {
        Self {
            limiters: DashMap::new(),
            members: std::sync::atomic::AtomicU32::new(1),
        }
    }

    /// The cluster now has `members` live members (ADR-152 D4.6). On a
    /// change the buckets are rebuilt at the new share; a bucket starts full.
    pub fn set_members(&self, members: u32) {
        let members = members.max(1);
        if self
            .members
            .swap(members, std::sync::atomic::Ordering::SeqCst)
            != members
        {
            self.limiters.clear();
            tracing::info!(
                members,
                "Burst rate limits shared across the cluster's live members"
            );
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
                let members = self.members.load(std::sync::atomic::Ordering::SeqCst);
                let burst = per_min.burst.unwrap_or(per_min.limit);
                let limit =
                    NonZeroU32::new(share_of(per_min.limit, members)).unwrap_or(NonZeroU32::MIN);
                let burst_size =
                    NonZeroU32::new(share_of(burst, members)).unwrap_or(NonZeroU32::MIN);
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
            tenant_id: crate::domain::tenant::TenantId::consumer(),
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

    #[test]
    fn the_burst_is_shared_by_the_live_members() {
        assert_eq!(share_of(10, 1), 10);
        assert_eq!(share_of(10, 4), 3, "rounded up");
        assert_eq!(share_of(1, 4), 1, "never below one");
        assert_eq!(share_of(10, 0), 10, "no members counts as one");
    }

    #[test]
    fn a_node_of_four_grants_its_share_of_the_burst() {
        let enforcer = GovernorBurstEnforcer::new();
        enforcer.set_members(4);
        let policy = policy_with_per_minute(8);
        assert!(
            enforcer.check_burst(&user_scope(), &policy, 2).is_ok(),
            "two of eight, a quarter each"
        );
        assert!(
            enforcer.check_burst(&user_scope(), &policy, 1).is_err(),
            "the third is another node's"
        );
        enforcer.set_members(1);
        assert!(
            enforcer.check_burst(&user_scope(), &policy, 8).is_ok(),
            "alone again, the whole burst, the bucket rebuilt full"
        );
    }
}
