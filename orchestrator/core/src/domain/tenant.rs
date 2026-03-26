// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Canonical tenant identifier used across bounded contexts.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Keycloak realm slug for the consumer product (Zaru).
pub const CONSUMER_SLUG: &str = "zaru-consumer";

/// Reserved slug for the AEGIS platform itself (internal operations).
pub const SYSTEM_SLUG: &str = "aegis-system";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantId(String);

impl TenantId {
    pub fn new(value: impl Into<String>) -> Result<Self, TenantIdError> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    pub fn from_string(value: &str) -> Result<Self, TenantIdError> {
        Self::new(value)
    }

    /// Build a `TenantId` from a Keycloak realm slug, applying RFC-1123 label
    /// validation (1-63 chars, lowercase alphanumeric + hyphens).
    pub fn from_realm_slug(slug: impl Into<String>) -> Result<Self, TenantIdError> {
        let slug = slug.into();
        Self::validate(&slug)?;
        Ok(Self(slug))
    }

    /// Extract the tenant slug from a Keycloak issuer URL.
    ///
    /// Expects a URL of the form `https://<host>/realms/<slug>` and validates
    /// the extracted slug with the standard rules.
    pub fn from_issuer_url(issuer: &str) -> Result<Self, TenantIdError> {
        let slug = issuer
            .split("/realms/")
            .nth(1)
            .and_then(|rest| {
                let segment = rest.split('/').next().unwrap_or(rest);
                if segment.is_empty() {
                    None
                } else {
                    Some(segment)
                }
            })
            .ok_or_else(|| TenantIdError::InvalidIssuerUrl(issuer.to_string()))?;
        Self::from_realm_slug(slug)
    }

    /// Well-known consumer tenant (Zaru).
    pub fn consumer() -> Self {
        Self(CONSUMER_SLUG.to_string())
    }

    /// Well-known system tenant (AEGIS internals).
    pub fn system() -> Self {
        Self(SYSTEM_SLUG.to_string())
    }

    /// Transitional alias — delegates to [`Self::consumer()`].
    pub fn local_default() -> Self {
        Self::consumer()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_system(&self) -> bool {
        self.0 == SYSTEM_SLUG
    }

    pub fn is_consumer(&self) -> bool {
        self.0 == CONSUMER_SLUG
    }

    fn validate(value: &str) -> Result<(), TenantIdError> {
        if value.is_empty() {
            return Err(TenantIdError::Empty);
        }

        if value.len() > 63 {
            return Err(TenantIdError::InvalidLength { len: value.len() });
        }

        let valid = value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');

        if !valid {
            return Err(TenantIdError::InvalidFormat(value.to_string()));
        }

        if value.starts_with('-') || value.ends_with('-') {
            return Err(TenantIdError::InvalidFormat(value.to_string()));
        }

        Ok(())
    }
}

impl Default for TenantId {
    fn default() -> Self {
        Self::consumer()
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<TenantId> for String {
    fn from(value: TenantId) -> Self {
        value.0
    }
}

#[derive(Debug, Error)]
pub enum TenantIdError {
    #[error("tenant id cannot be empty")]
    Empty,
    #[error("tenant id must be a lowercase slug, got '{0}'")]
    InvalidFormat(String),
    #[error("could not extract realm slug from issuer URL '{0}'")]
    InvalidIssuerUrl(String),
    #[error("tenant id length {len} exceeds RFC-1123 label limit of 63 characters")]
    InvalidLength { len: usize },
}

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
    fn local_default_delegates_to_consumer() {
        assert_eq!(TenantId::local_default(), TenantId::consumer());
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
    fn display_outputs_inner_string() {
        assert_eq!(TenantId::consumer().to_string(), "zaru-consumer");
    }
}
