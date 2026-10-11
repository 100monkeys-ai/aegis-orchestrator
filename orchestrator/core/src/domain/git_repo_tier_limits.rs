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

/// The largest clone volume a binding is given — 500 MiB. A tier whose
/// storage budget is small gives each clone less (see
/// [`GitRepoTierLimits::clone_volume_bytes`]); the hard storage-quota gate
/// lives in `UserVolumeService`.
pub const DEFAULT_CLONE_VOLUME_BYTES: u64 = 500 * 1024 * 1024;

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

    /// The size of one binding's clone volume under a tier whose storage
    /// budget is `total_storage_bytes` (AEGIS ADR-136 Update G2c):
    /// `min(DEFAULT_CLONE_VOLUME_BYTES, total / (2 × max_bindings))`, so the
    /// bindings never take more than half the budget. An unbounded tier
    /// (no `max_bindings`, or a `u64::MAX` budget) gives
    /// [`DEFAULT_CLONE_VOLUME_BYTES`].
    pub fn clone_volume_bytes(&self, total_storage_bytes: u64) -> u64 {
        match self.max_bindings {
            Some(max) if max > 0 && total_storage_bytes != u64::MAX => {
                DEFAULT_CLONE_VOLUME_BYTES.min(total_storage_bytes / (2 * u64::from(max)))
            }
            _ => DEFAULT_CLONE_VOLUME_BYTES,
        }
    }

    /// The bindings' allowance: the clone volume's size times
    /// `max_bindings`. `None` for an unbounded tier.
    pub fn bindings_allowance_bytes(&self, total_storage_bytes: u64) -> Option<u64> {
        match self.max_bindings {
            Some(max) if total_storage_bytes != u64::MAX => Some(
                self.clone_volume_bytes(total_storage_bytes)
                    .saturating_mul(u64::from(max)),
            ),
            _ => None,
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
    fn the_clone_volume_and_the_allowance_per_tier() {
        const MIB: u64 = 1024 * 1024;
        let free = GitRepoTierLimits::for_tier(ZaruTier::Free);
        assert_eq!(free.clone_volume_bytes(500 * MIB), 250 * MIB);
        assert_eq!(free.bindings_allowance_bytes(500 * MIB), Some(250 * MIB));
        let pro = GitRepoTierLimits::for_tier(ZaruTier::Pro);
        assert_eq!(pro.clone_volume_bytes(10 * 1024 * MIB), 500 * MIB);
        assert_eq!(
            pro.bindings_allowance_bytes(10 * 1024 * MIB),
            Some(2500 * MIB)
        );
        let enterprise = GitRepoTierLimits::for_tier(ZaruTier::Enterprise);
        assert_eq!(
            enterprise.clone_volume_bytes(u64::MAX),
            DEFAULT_CLONE_VOLUME_BYTES
        );
        assert_eq!(enterprise.bindings_allowance_bytes(u64::MAX), None);
    }

    #[test]
    fn enterprise_tier_unlimited() {
        let limits = GitRepoTierLimits::for_tier(ZaruTier::Enterprise);
        assert_eq!(limits.max_bindings, None);
        assert!(limits.auto_refresh);
        assert!(limits.sparse_checkout);
    }
}
