// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Script Tier Limits (BC-7 Storage Gateway, ADR-110 §D7)
//!
//! Per-[`ZaruTier`] gating for the [`crate::domain::script::Script`]
//! aggregate. Tier gates `max_scripts` (per owner) and `max_code_bytes`
//! (code size upper bound). The size cap is deliberately uniform across
//! tiers today — it's a safety ceiling on the TypeScript source, not a
//! product differentiator.
//!
//! | Tier | Max Scripts (per owner) | Max Code Bytes |
//! |------|-------------------------|----------------|
//! | Free | 5 | 256 KiB |
//! | Pro | 50 | 256 KiB |
//! | Business | 500 | 256 KiB |
//! | Enterprise | Unlimited | 256 KiB |

use crate::domain::iam::ZaruTier;
use crate::domain::script::SCRIPT_CODE_MAX_BYTES;

/// Tier-based limits applied to [`crate::domain::script::Script`]
/// creation and updates.
///
/// `max_scripts == None` indicates an unlimited quota (Enterprise tier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptTierLimits {
    /// Maximum number of active scripts a single owner may hold. `None`
    /// = unlimited.
    pub max_scripts: Option<u32>,
    /// Maximum TypeScript source size in bytes.
    pub max_code_bytes: u32,
}

impl ScriptTierLimits {
    /// Resolve the [`ScriptTierLimits`] for a [`ZaruTier`] per ADR-110
    /// §D7.
    pub fn for_tier(tier: ZaruTier) -> Self {
        let max_code_bytes = SCRIPT_CODE_MAX_BYTES as u32;
        match tier {
            ZaruTier::Free => Self {
                max_scripts: Some(5),
                max_code_bytes,
            },
            ZaruTier::Pro => Self {
                max_scripts: Some(50),
                max_code_bytes,
            },
            ZaruTier::Business => Self {
                max_scripts: Some(500),
                max_code_bytes,
            },
            ZaruTier::Enterprise => Self {
                max_scripts: None,
                max_code_bytes,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_tier_limits() {
        let limits = ScriptTierLimits::for_tier(ZaruTier::Free);
        assert_eq!(limits.max_scripts, Some(5));
        assert_eq!(limits.max_code_bytes, SCRIPT_CODE_MAX_BYTES as u32);
    }

    #[test]
    fn pro_tier_limits() {
        let limits = ScriptTierLimits::for_tier(ZaruTier::Pro);
        assert_eq!(limits.max_scripts, Some(50));
    }

    #[test]
    fn business_tier_limits() {
        let limits = ScriptTierLimits::for_tier(ZaruTier::Business);
        assert_eq!(limits.max_scripts, Some(500));
    }

    #[test]
    fn enterprise_tier_unlimited() {
        let limits = ScriptTierLimits::for_tier(ZaruTier::Enterprise);
        assert_eq!(limits.max_scripts, None);
    }
}
