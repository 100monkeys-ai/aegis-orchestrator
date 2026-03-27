// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Rate Limiting Domain Model (ADR-072)
//!
//! Platform-wide rate limiting across five resource types with multi-window
//! enforcement and a three-level override hierarchy:
//! `ZaruTier defaults < Tenant overrides < User overrides`.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::iam::{UserIdentity, ZaruTier};
use crate::domain::tenant::TenantId;

// ---------------------------------------------------------------------------
// Value Objects
// ---------------------------------------------------------------------------

/// Time-window buckets for rate limiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RateLimitBucket {
    PerMinute,
    Hourly,
    Daily,
    Weekly,
    Monthly,
}

impl RateLimitBucket {
    pub fn window_seconds(self) -> u64 {
        match self {
            Self::PerMinute => 60,
            Self::Hourly => 3_600,
            Self::Daily => 86_400,
            Self::Weekly => 604_800,
            Self::Monthly => 2_592_000,
        }
    }
}

/// Resource types subject to rate limiting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RateLimitResourceType {
    SmcpToolCall { tool_pattern: String },
    AgentExecution,
    WorkflowExecution,
    LlmCall,
    LlmToken,
}

/// A single rate limit for one time window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitWindow {
    pub limit: u64,
    pub window_seconds: u64,
    pub burst: Option<u64>,
}

/// Complete rate limit policy for a resource type across all time windows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitPolicy {
    pub resource_type: RateLimitResourceType,
    pub windows: HashMap<RateLimitBucket, RateLimitWindow>,
}

/// Counter key scope for rate limiting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RateLimitScope {
    User { user_id: String },
    Tenant { tenant_id: TenantId },
}

/// Result of a rate limit evaluation.
#[derive(Debug, Clone)]
pub struct RateLimitDecision {
    pub allowed: bool,
    pub resource_type: RateLimitResourceType,
    pub scope: RateLimitScope,
    pub exhausted_bucket: Option<RateLimitBucket>,
    pub retry_after_seconds: Option<u64>,
    pub remaining: HashMap<RateLimitBucket, u64>,
}

/// Source of a resolved rate limit policy (for audit/debug).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateLimitPolicySource {
    TierDefault { tier: ZaruTier },
    TenantOverride { tenant_id: TenantId },
    UserOverride { user_id: String },
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, thiserror::Error)]
pub enum RateLimitError {
    #[error("policy resolution failed: {0}")]
    PolicyResolutionFailed(String),
    #[error("enforcement failed: {0}")]
    EnforcementFailed(String),
    #[error("storage error: {0}")]
    StorageError(String),
}

// ---------------------------------------------------------------------------
// Domain Service Traits
// ---------------------------------------------------------------------------

/// Resolves the effective rate limit policy by walking the override hierarchy.
#[async_trait]
pub trait RateLimitPolicyResolver: Send + Sync {
    async fn resolve_policy(
        &self,
        identity: &UserIdentity,
        tenant_id: &TenantId,
        resource_type: &RateLimitResourceType,
    ) -> Result<RateLimitPolicy, RateLimitError>;
}

/// Enforces rate limits by checking and atomically incrementing counters.
#[async_trait]
pub trait RateLimitEnforcer: Send + Sync {
    async fn check_and_increment(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
        cost: u64,
    ) -> Result<RateLimitDecision, RateLimitError>;

    async fn remaining(
        &self,
        scope: &RateLimitScope,
        policy: &RateLimitPolicy,
    ) -> Result<HashMap<RateLimitBucket, u64>, RateLimitError>;
}

// ---------------------------------------------------------------------------
// Tier Defaults (ADR-072)
// ---------------------------------------------------------------------------

/// Returns the default rate limit policies for the given ZaruTier.
pub fn tier_defaults(tier: &ZaruTier) -> Vec<RateLimitPolicy> {
    match tier {
        ZaruTier::Free => free_defaults(),
        ZaruTier::Pro => pro_defaults(),
        ZaruTier::Business => business_defaults(),
        ZaruTier::Enterprise => enterprise_defaults(),
    }
}

fn win(bucket: RateLimitBucket, limit: u64) -> (RateLimitBucket, RateLimitWindow) {
    (
        bucket,
        RateLimitWindow {
            limit,
            window_seconds: bucket.window_seconds(),
            burst: None,
        },
    )
}

fn full_windows(
    per_min: u64,
    hourly: u64,
    daily: u64,
    weekly: u64,
    monthly: u64,
) -> HashMap<RateLimitBucket, RateLimitWindow> {
    HashMap::from([
        win(RateLimitBucket::PerMinute, per_min),
        win(RateLimitBucket::Hourly, hourly),
        win(RateLimitBucket::Daily, daily),
        win(RateLimitBucket::Weekly, weekly),
        win(RateLimitBucket::Monthly, monthly),
    ])
}

fn per_minute_only(limit: u64) -> HashMap<RateLimitBucket, RateLimitWindow> {
    HashMap::from([win(RateLimitBucket::PerMinute, limit)])
}

fn mk_policy(
    resource_type: RateLimitResourceType,
    windows: HashMap<RateLimitBucket, RateLimitWindow>,
) -> RateLimitPolicy {
    RateLimitPolicy {
        resource_type,
        windows,
    }
}

fn smcp_policies(fs: u64, web_search: u64, other: u64) -> Vec<RateLimitPolicy> {
    vec![
        mk_policy(
            RateLimitResourceType::SmcpToolCall {
                tool_pattern: "fs_*".into(),
            },
            per_minute_only(fs),
        ),
        mk_policy(
            RateLimitResourceType::SmcpToolCall {
                tool_pattern: "web_search".into(),
            },
            per_minute_only(web_search),
        ),
        mk_policy(
            RateLimitResourceType::SmcpToolCall {
                tool_pattern: "*".into(),
            },
            per_minute_only(other),
        ),
    ]
}

fn free_defaults() -> Vec<RateLimitPolicy> {
    let mut p = vec![
        mk_policy(
            RateLimitResourceType::AgentExecution,
            full_windows(3, 5, 20, 80, 100),
        ),
        mk_policy(
            RateLimitResourceType::WorkflowExecution,
            full_windows(2, 3, 10, 40, 50),
        ),
        mk_policy(
            RateLimitResourceType::LlmCall,
            full_windows(20, 100, 500, 2_500, 5_000),
        ),
        mk_policy(
            RateLimitResourceType::LlmToken,
            full_windows(20_000, 100_000, 500_000, 2_500_000, 5_000_000),
        ),
    ];
    p.extend(smcp_policies(60, 20, 30));
    p
}

fn pro_defaults() -> Vec<RateLimitPolicy> {
    let mut p = vec![
        mk_policy(
            RateLimitResourceType::AgentExecution,
            full_windows(10, 50, 200, 1_000, 2_000),
        ),
        mk_policy(
            RateLimitResourceType::WorkflowExecution,
            full_windows(5, 25, 100, 500, 1_000),
        ),
        mk_policy(
            RateLimitResourceType::LlmCall,
            full_windows(60, 500, 3_000, 15_000, 30_000),
        ),
        mk_policy(
            RateLimitResourceType::LlmToken,
            full_windows(100_000, 500_000, 3_000_000, 15_000_000, 30_000_000),
        ),
    ];
    p.extend(smcp_policies(300, 60, 120));
    p
}

fn business_defaults() -> Vec<RateLimitPolicy> {
    let mut p = vec![
        mk_policy(
            RateLimitResourceType::AgentExecution,
            full_windows(30, 200, 1_000, 5_000, 10_000),
        ),
        mk_policy(
            RateLimitResourceType::WorkflowExecution,
            full_windows(15, 100, 500, 2_500, 5_000),
        ),
        mk_policy(
            RateLimitResourceType::LlmCall,
            full_windows(200, 2_000, 15_000, 75_000, 150_000),
        ),
        mk_policy(
            RateLimitResourceType::LlmToken,
            full_windows(500_000, 2_500_000, 15_000_000, 75_000_000, 150_000_000),
        ),
    ];
    p.extend(smcp_policies(600, 200, 300));
    p
}

fn enterprise_defaults() -> Vec<RateLimitPolicy> {
    let mut p = vec![
        mk_policy(
            RateLimitResourceType::AgentExecution,
            full_windows(100, 1_000, 10_000, 50_000, 200_000),
        ),
        mk_policy(
            RateLimitResourceType::WorkflowExecution,
            full_windows(50, 500, 5_000, 25_000, 100_000),
        ),
        mk_policy(
            RateLimitResourceType::LlmCall,
            full_windows(500, 10_000, 100_000, 500_000, 1_000_000),
        ),
        mk_policy(
            RateLimitResourceType::LlmToken,
            full_windows(
                2_000_000,
                10_000_000,
                100_000_000,
                500_000_000,
                1_000_000_000,
            ),
        ),
    ];
    p.extend(smcp_policies(1_200, 500, 600));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_window_seconds() {
        assert_eq!(RateLimitBucket::PerMinute.window_seconds(), 60);
        assert_eq!(RateLimitBucket::Hourly.window_seconds(), 3_600);
        assert_eq!(RateLimitBucket::Daily.window_seconds(), 86_400);
        assert_eq!(RateLimitBucket::Weekly.window_seconds(), 604_800);
        assert_eq!(RateLimitBucket::Monthly.window_seconds(), 2_592_000);
    }

    #[test]
    fn free_tier_has_seven_policies() {
        let policies = tier_defaults(&ZaruTier::Free);
        assert_eq!(policies.len(), 7);
    }

    #[test]
    fn enterprise_agent_monthly_limit() {
        let policies = tier_defaults(&ZaruTier::Enterprise);
        let agent = policies
            .iter()
            .find(|p| p.resource_type == RateLimitResourceType::AgentExecution)
            .expect("agent execution policy");
        let monthly = agent
            .windows
            .get(&RateLimitBucket::Monthly)
            .expect("monthly window");
        assert_eq!(monthly.limit, 200_000);
    }

    #[test]
    fn smcp_tool_policies_are_per_minute_only() {
        let policies = tier_defaults(&ZaruTier::Pro);
        for p in &policies {
            if let RateLimitResourceType::SmcpToolCall { .. } = &p.resource_type {
                assert_eq!(p.windows.len(), 1);
                assert!(p.windows.contains_key(&RateLimitBucket::PerMinute));
            }
        }
    }
}
