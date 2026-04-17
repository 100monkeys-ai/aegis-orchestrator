// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Repository Binding Tier Limits (BC-7 Storage Gateway, ADR-081)
//!
//! Per-[`ZaruTier`] gating for [`crate::domain::git_repo::GitRepoBinding`]
//! aggregates. Mirrors ADR-081 §Sub-Decision 6 verbatim.
//!
//! | Tier | Max Git Repo Bindings | Auto-Refresh (Webhook) | Sparse Checkout |
//! |------|----------------------|------------------------|-----------------|
//! | Free | 1 | No | No |
//! | Pro | 5 | Yes | Yes |
//! | Business | 20 per team | Yes | Yes |
//! | Enterprise | Unlimited | Yes | Yes |

use crate::domain::iam::ZaruTier;

/// Tier-based limits applied to [`crate::domain::git_repo::GitRepoBinding`]
/// creation and feature gating.
///
/// `max_bindings == None` indicates an unlimited quota (Enterprise tier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepoTierLimits {
    /// Maximum number of bindings a single owner may hold. `None` = unlimited.
    pub max_bindings: Option<u32>,
    /// Whether the auto-refresh (webhook) feature is enabled for this tier.
    pub auto_refresh: bool,
    /// Whether sparse-checkout support is enabled for this tier.
    pub sparse_checkout: bool,
}

impl GitRepoTierLimits {
    /// Resolve the [`GitRepoTierLimits`] for a [`ZaruTier`] per ADR-081
    /// §Sub-Decision 6.
    pub fn for_tier(tier: ZaruTier) -> Self {
        match tier {
            ZaruTier::Free => Self {
                max_bindings: Some(1),
                auto_refresh: false,
                sparse_checkout: false,
            },
            ZaruTier::Pro => Self {
                max_bindings: Some(5),
                auto_refresh: true,
                sparse_checkout: true,
            },
            ZaruTier::Business => Self {
                max_bindings: Some(20),
                auto_refresh: true,
                sparse_checkout: true,
            },
            ZaruTier::Enterprise => Self {
                max_bindings: None,
                auto_refresh: true,
                sparse_checkout: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_tier_limits() {
        let limits = GitRepoTierLimits::for_tier(ZaruTier::Free);
        assert_eq!(limits.max_bindings, Some(1));
        assert!(!limits.auto_refresh);
        assert!(!limits.sparse_checkout);
    }

    #[test]
    fn pro_tier_limits() {
        let limits = GitRepoTierLimits::for_tier(ZaruTier::Pro);
        assert_eq!(limits.max_bindings, Some(5));
        assert!(limits.auto_refresh);
        assert!(limits.sparse_checkout);
    }

    #[test]
    fn business_tier_limits() {
        let limits = GitRepoTierLimits::for_tier(ZaruTier::Business);
        assert_eq!(limits.max_bindings, Some(20));
        assert!(limits.auto_refresh);
        assert!(limits.sparse_checkout);
    }

    #[test]
    fn enterprise_tier_unlimited() {
        let limits = GitRepoTierLimits::for_tier(ZaruTier::Enterprise);
        assert_eq!(limits.max_bindings, None);
        assert!(limits.auto_refresh);
        assert!(limits.sparse_checkout);
    }
}
