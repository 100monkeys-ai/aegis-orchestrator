// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tenant Identity (re-exported from Shared Kernel)
//!
//! This module re-exports [`TenantId`] and related types from the
//! [`shared_kernel`](super::shared_kernel) module for backward compatibility.

pub use crate::domain::shared_kernel::{TenantId, TenantIdError, CONSUMER_SLUG, SYSTEM_SLUG};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumer_returns_zaru_consumer() {
        assert_eq!(TenantId::consumer().as_str(), "zaru-consumer");
    }

    #[test]
    fn system_returns_aegis_system() {
        assert_eq!(TenantId::system().as_str(), "aegis-system");
    }

    #[test]
    fn default_returns_consumer() {
        assert_eq!(TenantId::default(), TenantId::consumer());
    }

    #[test]
    fn from_realm_slug_accepts_valid_slugs() {
        let tenant = TenantId::from_realm_slug("acme-corp").unwrap();
        assert_eq!(tenant.as_str(), "acme-corp");
    }

    #[test]
    fn from_realm_slug_rejects_empty() {
        assert!(TenantId::from_realm_slug("").is_err());
    }

    #[test]
    fn from_realm_slug_rejects_uppercase() {
        assert!(TenantId::from_realm_slug("Acme").is_err());
    }

    #[test]
    fn from_realm_slug_rejects_leading_hyphen() {
        assert!(TenantId::from_realm_slug("-acme").is_err());
    }

    #[test]
    fn from_realm_slug_rejects_trailing_hyphen() {
        assert!(TenantId::from_realm_slug("acme-").is_err());
    }

    #[test]
    fn from_realm_slug_rejects_over_63_chars() {
        let long = "a".repeat(64);
        assert!(TenantId::from_realm_slug(&long).is_err());
    }

    #[test]
    fn from_realm_slug_accepts_exactly_63_chars() {
        let exact = "a".repeat(63);
        assert!(TenantId::from_realm_slug(&exact).is_ok());
    }

    #[test]
    fn from_issuer_url_extracts_slug() {
        let tenant =
            TenantId::from_issuer_url("https://auth.aegis.local/realms/zaru-consumer").unwrap();
        assert_eq!(tenant.as_str(), "zaru-consumer");
    }

    #[test]
    fn from_issuer_url_extracts_slug_with_trailing_slash() {
        let tenant =
            TenantId::from_issuer_url("https://auth.aegis.local/realms/acme-corp/").unwrap();
        assert_eq!(tenant.as_str(), "acme-corp");
    }

    #[test]
    fn from_issuer_url_rejects_missing_realms_segment() {
        assert!(TenantId::from_issuer_url("https://auth.aegis.local/notarealm/x").is_err());
    }

    #[test]
    fn from_issuer_url_rejects_empty_slug() {
        assert!(TenantId::from_issuer_url("https://auth.aegis.local/realms/").is_err());
    }

    #[test]
    fn is_system_and_is_consumer_flags() {
        assert!(TenantId::system().is_system());
        assert!(!TenantId::system().is_consumer());
        assert!(TenantId::consumer().is_consumer());
        assert!(!TenantId::consumer().is_system());
    }

    #[test]
    fn tenant_id_is_team_flag() {
        // Team colony slugs are of shape `t-{uuid}` (see ADR-111 / TeamSlug).
        let team = TenantId::from_realm_slug("t-abc123").unwrap();
        assert!(team.is_team());
        assert!(!team.is_consumer());
        assert!(!team.is_system());

        // Consumer / system / per-user tenants are not teams.
        assert!(!TenantId::consumer().is_team());
        assert!(!TenantId::system().is_team());
        let personal = TenantId::from_realm_slug("u-abc123").unwrap();
        assert!(!personal.is_team());
        let enterprise = TenantId::from_realm_slug("acme-corp").unwrap();
        assert!(!enterprise.is_team());
    }

    #[test]
    fn display_outputs_inner_string() {
        assert_eq!(TenantId::consumer().to_string(), "zaru-consumer");
    }
}
