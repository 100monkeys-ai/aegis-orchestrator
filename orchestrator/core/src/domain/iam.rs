// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # IAM & Identity Federation Domain Types (BC-13, ADR-041)
//!
//! OIDC is the single trusted OIDC issuer for all human and service-account
//! authentication on the AEGIS platform. This module defines the aggregate roots,
//! entities, value objects, domain service traits, and error types for BC-13.
//!
//! ## Key Types
//!
//! | Type | Role | Description |
//! |------|------|-------------|
//! | [`IdentityRealm`] | Aggregate Root | Configured OIDC realm known to AEGIS |
//! | [`OidcClient`] | Entity | Registered OIDC client within a realm |
//! | [`UserIdentity`] | Value Object | Resolved identity from a validated JWT |
//! | [`ValidatedIdentityToken`] | Value Object | Immutable decoded JWT (never constructed directly) |
//! | [`IdentityProvider`] | Domain Service | Token validation, tier/role resolution |
//! | [`RealmRepository`] | Repository | Persistence for known realms |
//!
//! ## Design Notes
//!
//! - `UserIdentity` is **never persisted** — reconstructed from JWT on every request.
//! - `ZaruTier` is owned by BC-13 (IAM) and sourced from a OIDC custom claim.
//!   Previously defined on `ZaruSession` in BC-12; the source of truth has moved.
//! - SEAL agent attestation (Ed25519 ephemeral keypair) is **unchanged** by ADR-041.
//!   OIDC is for human and service-account identities only.

use crate::domain::tenant::TenantId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Aggregate Root: IdentityRealm ────────────────────────────────────────────

/// Represents a configured OIDC realm known to the AEGIS platform.
/// Each realm corresponds to a secret-backend namespace (ADR-034 alignment).
/// The realm is the top-level trust boundary for all tokens issued within it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityRealm {
    /// Realm identifier: "aegis-system" | "zaru-consumer" | "tenant-{slug}"
    pub realm_slug: String,
    /// Full issuer URL: `https://auth.example.com/realms/{slug}`
    pub issuer_url: String,
    /// JWKS endpoint: {issuer_url}/protocol/openid-connect/certs
    pub jwks_uri: String,
    /// Expected "aud" claim value for tokens from this realm
    pub audience: String,
    /// Classification of this realm
    pub realm_kind: RealmKind,
}

/// Classification of OIDC realms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RealmKind {
    /// "aegis-system" — operators, SDK service accounts
    System,
    /// "zaru-consumer" — individual Zaru users
    Consumer,
    /// "tenant-{slug}" — enterprise tenant users
    Tenant { slug: String },
}

// ── Entity: OidcClient ───────────────────────────────────────────────────────

/// A registered OIDC client within a OIDC realm.
/// Service accounts use ClientCredentials grant.
/// UI applications use AuthorizationCode + PKCE.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcClient {
    /// OIDC client_id (e.g. "zaru-client", "aegis-sdk-python")
    pub client_id: String,
    /// Which realm this client belongs to
    pub realm_slug: String,
    /// OAuth2 grant type used by this client
    pub grant_type: GrantType,
    /// Scopes this client is allowed to request
    pub allowed_scopes: Vec<String>,
}

/// OAuth2 grant type for OIDC clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrantType {
    /// Zaru Client
    AuthorizationCodePkce,
    /// SDK clients, CI/CD pipelines
    ClientCredentials,
}

// ── Value Objects ────────────────────────────────────────────────────────────

/// Resolved identity from a validated OIDC JWT.
/// This is the canonical human/service identity used throughout the application layer.
/// Never persisted — reconstructed from JWT on every request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserIdentity {
    /// OIDC subject UUID (stable per user per realm)
    pub sub: String,
    /// Which realm issued this token
    pub realm_slug: String,
    /// User email if present in claims
    pub email: Option<String>,
    /// Classification of this identity
    pub identity_kind: IdentityKind,
}

impl UserIdentity {
    /// Derive the SEAL SecurityContext name from this identity (ADR-083).
    ///
    /// - Consumer users map to their Zaru tier context (`zaru-free`, `zaru-pro`, …).
    /// - Operators and service accounts map to `aegis-system-operator`.
    /// - Tenant users map to `aegis-system-operator` (tenant-scoped contexts are a future extension).
    pub fn to_security_context_name(&self) -> String {
        match &self.identity_kind {
            IdentityKind::ConsumerUser { zaru_tier, .. } => {
                zaru_tier.to_security_context_name().to_string()
            }
            IdentityKind::Operator { .. }
            | IdentityKind::ServiceAccount { .. }
            | IdentityKind::TenantUser { .. } => "aegis-system-operator".to_string(),
        }
    }
}

/// Classification of the authenticated entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentityKind {
    /// zaru-consumer realm user (ADR-097: carries per-user tenant_id)
    ConsumerUser {
        zaru_tier: ZaruTier,
        tenant_id: TenantId,
    },
    /// aegis-system realm human operator
    Operator { aegis_role: AegisRole },
    /// aegis-system realm M2M client
    ServiceAccount { client_id: String },
    /// tenant-{slug} realm enterprise user
    TenantUser { tenant_slug: String },
}

/// ZaruTier is owned by BC-13 (IAM) and sourced from a OIDC custom claim.
/// Maps to SEAL SecurityContext names consumed by ZaruAuthMiddleware → AttestationService.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZaruTier {
    Free,
    Pro,
    Business,
    Enterprise,
}

impl ZaruTier {
    /// Map to SEAL SecurityContext name (consumed by ZaruAuthMiddleware → AttestationService).
    pub fn to_security_context_name(&self) -> &'static str {
        match self {
            ZaruTier::Free => "zaru-free",
            ZaruTier::Pro => "zaru-pro",
            ZaruTier::Business => "zaru-business",
            ZaruTier::Enterprise => "zaru-enterprise",
        }
    }

    /// Derive from a SEAL SecurityContext name.
    ///
    /// Returns `None` for non-Zaru contexts (e.g. `aegis-system-operator`).
    pub fn from_security_context_name(name: &str) -> Option<ZaruTier> {
        match name {
            "zaru-free" => Some(ZaruTier::Free),
            "zaru-pro" => Some(ZaruTier::Pro),
            "zaru-business" => Some(ZaruTier::Business),
            "zaru-enterprise" => Some(ZaruTier::Enterprise),
            _ => None,
        }
    }

    /// Parse from the string value in the OIDC `zaru_tier` custom claim.
    pub fn from_claim(value: &str) -> Option<ZaruTier> {
        match value {
            "free" => Some(ZaruTier::Free),
            "pro" => Some(ZaruTier::Pro),
            "business" => Some(ZaruTier::Business),
            "enterprise" => Some(ZaruTier::Enterprise),
            _ => None,
        }
    }
}

/// Role for operators authenticated via the aegis-system realm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AegisRole {
    /// "aegis:admin" — full platform access
    Admin,
    /// "aegis:operator" — deploy/manage agents, view executions
    Operator,
    /// "aegis:readonly" — view-only access to all surfaces
    Readonly,
}

impl AegisRole {
    /// Parse from the string value in the OIDC `aegis_role` custom claim.
    pub fn from_claim(value: &str) -> Option<AegisRole> {
        match value {
            "aegis:admin" => Some(AegisRole::Admin),
            "aegis:operator" => Some(AegisRole::Operator),
            "aegis:readonly" => Some(AegisRole::Readonly),
            _ => None,
        }
    }

    /// Returns `true` if this role has full administrative privileges.
    pub fn is_admin(&self) -> bool {
        matches!(self, AegisRole::Admin)
    }

    /// Convert to the canonical claim string representation.
    pub fn as_claim_str(&self) -> &'static str {
        match self {
            AegisRole::Admin => "aegis:admin",
            AegisRole::Operator => "aegis:operator",
            AegisRole::Readonly => "aegis:readonly",
        }
    }
}

/// A validated, decoded OIDC JWT. Immutable value object.
/// Constructed by `IdentityProvider::validate_token()`; never constructed directly.
#[derive(Debug, Clone)]
pub struct ValidatedIdentityToken {
    /// Resolved identity from the token claims
    pub identity: UserIdentity,
    /// When the token was issued
    pub issued_at: DateTime<Utc>,
    /// When the token expires
    pub expires_at: DateTime<Utc>,
    /// Full claims for audit trail
    pub raw_claims: serde_json::Value,
}

/// Represents a OIDC tenant realm with its JWKS endpoint.
/// Used during tenant onboarding (Phase 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantRealm {
    /// Tenant slug identifier
    pub slug: String,
    /// Full issuer URL for this tenant realm
    pub issuer_url: String,
    /// JWKS endpoint for token validation
    pub jwks_uri: String,
}

// ── Domain Service: IdentityProvider ───────────────────────────────────────

/// Domain service for OIDC IAM token validation and identity resolution.
///
/// Implementations validate Bearer JWTs against the appropriate realm's JWKS
/// endpoint, extract custom claims (zaru_tier, aegis_role), and resolve
/// the canonical `UserIdentity` for use in authorization decisions.
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    /// Validate a Bearer JWT against the appropriate realm's JWKS.
    /// Realm is determined from the JWT's `iss` claim.
    /// Returns a resolved `ValidatedIdentityToken` or an `IamError`.
    async fn validate_token(&self, raw_jwt: &str) -> Result<ValidatedIdentityToken, IamError>;

    /// Resolve the ZaruTier for a consumer user (extracts zaru_tier claim).
    /// Only valid for tokens from the zaru-consumer realm or a tenant realm.
    fn resolve_tier(&self, token: &ValidatedIdentityToken) -> Result<ZaruTier, IamError>;

    /// Resolve the AegisRole for an operator (extracts aegis_role claim).
    /// Only valid for tokens from the aegis-system realm.
    fn resolve_role(&self, token: &ValidatedIdentityToken) -> Result<AegisRole, IamError>;

    /// List all configured realms (used for JWKS cache warm-up on startup).
    fn known_realms(&self) -> Vec<IdentityRealm>;
}

// ── Error Type ───────────────────────────────────────────────────────────────

/// Errors from IAM token validation and identity resolution.
#[derive(Debug, thiserror::Error)]
pub enum IamError {
    #[error("JWT signature verification failed: {0}")]
    SignatureInvalid(String),

    #[error("JWT expired at {expired_at}")]
    TokenExpired { expired_at: DateTime<Utc> },

    #[error("Issuer {issuer} is not a trusted identity realm")]
    UnknownIssuer { issuer: String },

    #[error("Required claim {claim} missing from token")]
    MissingClaim { claim: String },

    #[error("JWKS fetch failed for realm {realm}: {reason}")]
    JwksFetchFailed { realm: String, reason: String },

    #[error("Invalid claim value for {claim}: {value}")]
    InvalidClaimValue { claim: String, value: String },

    #[error("JWT decode error: {0}")]
    DecodeError(String),
}

// ── Repository: RealmRepository ──────────────────────────────────────────────

/// Repository trait for persisting and querying known OIDC realms.
///
/// Phase 1: in-memory implementation backed by the node config.
/// Phase 2: PostgreSQL-backed for dynamic tenant realm discovery.
#[async_trait]
pub trait RealmRepository: Send + Sync {
    /// Persist a realm configuration.
    async fn save(&self, realm: IdentityRealm) -> anyhow::Result<()>;

    /// Look up a realm by its slug identifier.
    async fn find_by_slug(&self, slug: &str) -> anyhow::Result<Option<IdentityRealm>>;

    /// List all known realms.
    async fn list_all(&self) -> anyhow::Result<Vec<IdentityRealm>>;

    /// Remove a realm configuration.
    async fn delete(&self, slug: &str) -> anyhow::Result<()>;
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zaru_tier_to_security_context_name() {
        assert_eq!(ZaruTier::Free.to_security_context_name(), "zaru-free");
        assert_eq!(ZaruTier::Pro.to_security_context_name(), "zaru-pro");
        assert_eq!(
            ZaruTier::Business.to_security_context_name(),
            "zaru-business"
        );
        assert_eq!(
            ZaruTier::Enterprise.to_security_context_name(),
            "zaru-enterprise"
        );
    }

    #[test]
    fn zaru_tier_from_security_context_name() {
        assert_eq!(
            ZaruTier::from_security_context_name("zaru-free"),
            Some(ZaruTier::Free)
        );
        assert_eq!(
            ZaruTier::from_security_context_name("zaru-pro"),
            Some(ZaruTier::Pro)
        );
        assert_eq!(
            ZaruTier::from_security_context_name("zaru-business"),
            Some(ZaruTier::Business)
        );
        assert_eq!(
            ZaruTier::from_security_context_name("zaru-enterprise"),
            Some(ZaruTier::Enterprise)
        );
        assert_eq!(
            ZaruTier::from_security_context_name("aegis-system-operator"),
            None
        );
        assert_eq!(ZaruTier::from_security_context_name("unknown"), None);
    }

    #[test]
    fn zaru_tier_from_claim() {
        assert_eq!(ZaruTier::from_claim("free"), Some(ZaruTier::Free));
        assert_eq!(ZaruTier::from_claim("pro"), Some(ZaruTier::Pro));
        assert_eq!(ZaruTier::from_claim("business"), Some(ZaruTier::Business));
        assert_eq!(
            ZaruTier::from_claim("enterprise"),
            Some(ZaruTier::Enterprise)
        );
        assert_eq!(ZaruTier::from_claim("invalid"), None);
    }

    #[test]
    fn aegis_role_from_claim() {
        assert_eq!(AegisRole::from_claim("aegis:admin"), Some(AegisRole::Admin));
        assert_eq!(
            AegisRole::from_claim("aegis:operator"),
            Some(AegisRole::Operator)
        );
        assert_eq!(
            AegisRole::from_claim("aegis:readonly"),
            Some(AegisRole::Readonly)
        );
        assert_eq!(AegisRole::from_claim("unknown"), None);
    }

    #[test]
    fn aegis_role_as_claim_str_roundtrip() {
        for role in [AegisRole::Admin, AegisRole::Operator, AegisRole::Readonly] {
            let claim_str = role.as_claim_str();
            let parsed = AegisRole::from_claim(claim_str).unwrap();
            assert_eq!(parsed, role);
        }
    }

    #[test]
    fn realm_kind_equality() {
        assert_eq!(RealmKind::System, RealmKind::System);
        assert_eq!(RealmKind::Consumer, RealmKind::Consumer);
        assert_eq!(
            RealmKind::Tenant {
                slug: "acme".to_string()
            },
            RealmKind::Tenant {
                slug: "acme".to_string()
            }
        );
        assert_ne!(
            RealmKind::Tenant {
                slug: "acme".to_string()
            },
            RealmKind::Tenant {
                slug: "beta".to_string()
            }
        );
    }

    #[test]
    fn user_identity_serialization_roundtrip() {
        let identity = UserIdentity {
            sub: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: Some("user@example.com".to_string()),
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Pro,
                tenant_id: TenantId::consumer(),
            },
        };
        let json = serde_json::to_string(&identity).unwrap();
        let deserialized: UserIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, identity);
    }

    #[test]
    fn identity_realm_serialization_roundtrip() {
        let realm = IdentityRealm {
            realm_slug: "aegis-system".to_string(),
            issuer_url: "https://auth.example.com/realms/aegis-system".to_string(),
            jwks_uri: "https://auth.example.com/realms/aegis-system/protocol/openid-connect/certs"
                .to_string(),
            audience: "aegis-orchestrator".to_string(),
            realm_kind: RealmKind::System,
        };
        let json = serde_json::to_string(&realm).unwrap();
        let deserialized: IdentityRealm = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.realm_slug, "aegis-system");
        assert_eq!(deserialized.realm_kind, RealmKind::System);
    }
}
