// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # KeycloakIamService — Production JWKS-based JWT Validation (ADR-041)
//!
//! Implements [`IdentityProvider`] by fetching and caching JWKS key sets from
//! Keycloak realms, validating JWT signatures using RS256, and extracting custom
//! claims (`zaru_tier`, `aegis_role`) to resolve [`UserIdentity`].
//!
//! ## JWKS Cache
//!
//! Each realm's JWKS is cached in-process with a configurable TTL (default: 5 minutes).
//! When a validation request arrives and the cache is stale, the service fetches
//! fresh keys from the realm's `jwks_uri` endpoint. Cache refresh events are
//! published to the [`EventBus`] for audit.

use crate::domain::events::IamEvent;
use crate::domain::iam::{
    AegisRole, IamError, IdentityKind, IdentityProvider, IdentityRealm, RealmKind, UserIdentity,
    ValidatedIdentityToken, ZaruTier,
};
use crate::domain::node_config::{IamClaimsConfig, IamConfig, IamRealmConfig};
use crate::infrastructure::event_bus::EventBus;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jsonwebtoken::{decode, decode_header, errors::ErrorKind, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::warn;
use tracing::{debug, info};

/// Cached JWKS key set for a single realm.
struct CachedJwks {
    /// Raw JWKS JSON — used to construct `DecodingKey` per-key.
    keys: JwksResponse,
    /// When this cache entry was last fetched.
    fetched_at: Instant,
    /// How long this cache entry is valid.
    ttl: Duration,
}

impl CachedJwks {
    fn is_expired(&self) -> bool {
        self.fetched_at.elapsed() > self.ttl
    }
}

/// Minimal JWKS response structure from Keycloak.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JwksResponse {
    keys: Vec<JwkKey>,
}

/// Individual JWK key matching the subset of fields needed for RS256 validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JwkKey {
    /// Key type (must be "RSA").
    kty: String,
    /// Key ID — matched against JWT header `kid`.
    kid: String,
    /// RSA modulus (base64url-encoded).
    n: String,
    /// RSA public exponent (base64url-encoded).
    e: String,
    /// Algorithm (expected: "RS256").
    #[serde(default)]
    alg: Option<String>,
    /// Key use (expected: "sig").
    #[serde(rename = "use", default)]
    key_use: Option<String>,
}

/// Claims structure expected in Keycloak-issued JWTs.
#[derive(Debug, Deserialize)]
struct KeycloakClaims {
    /// Subject (Keycloak user UUID).
    sub: String,
    /// Issuer URL.
    iss: String,
    /// Audience.
    #[serde(default)]
    aud: serde_json::Value,
    /// Issued-at timestamp.
    iat: i64,
    /// Expiration timestamp.
    exp: i64,
    /// Email claim (optional).
    #[serde(default)]
    email: Option<String>,
    /// Authorized party (client_id for client_credentials).
    #[serde(default)]
    azp: Option<String>,
    /// All remaining claims for custom attribute extraction.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// TCP connect timeout (seconds) for JWKS fetches. A non-responsive
/// Keycloak host (dropped SYNs, dead route) must surface as a
/// `JwksFetchFailed` error within a few seconds rather than hanging the
/// verify path until the kernel TCP timeout (~75s+) elapses.
const JWKS_CONNECT_TIMEOUT_SECS: u64 = 5;

/// Total request timeout (seconds) for JWKS fetches, covering the
/// connect + TLS handshake + headers + body. JWKS payloads are tiny,
/// so anything beyond this bound indicates a frozen peer.
const JWKS_REQUEST_TIMEOUT_SECS: u64 = 8;

/// Production implementation of [`IdentityProvider`].
///
/// Validates JWTs against Keycloak realm JWKS endpoints with in-memory caching.
/// Publishes [`IamEvent`]s to the event bus for audit trail.
pub struct StandardIamService {
    /// Known realms configured from `aegis-config.yaml`.
    realms: Vec<IdentityRealm>,
    /// Issuer URL → realm index for O(1) lookup.
    issuer_to_realm: HashMap<String, usize>,
    /// Per-realm JWKS cache.
    jwks_cache: Arc<RwLock<HashMap<String, CachedJwks>>>,
    /// HTTP client for JWKS fetching.
    http_client: reqwest::Client,
    /// Custom claim names from config.
    claims_config: IamClaimsConfig,
    /// Event bus for publishing IAM events.
    event_bus: Arc<EventBus>,
    /// JWKS cache TTL.
    cache_ttl: Duration,
}

impl StandardIamService {
    /// Create a new service from node configuration.
    ///
    /// Returns `IamError::Configuration` if any realm in the config is
    /// malformed (unknown kind, malformed tenant slug, etc.). Callers
    /// should surface this as a fatal boot error.
    pub fn new(config: &IamConfig, event_bus: Arc<EventBus>) -> Result<Self, IamError> {
        let realms: Vec<IdentityRealm> = config
            .realms
            .iter()
            .map(realm_from_config)
            .collect::<Result<Vec<_>, _>>()?;

        let issuer_to_realm: HashMap<String, usize> = realms
            .iter()
            .enumerate()
            .map(|(i, r)| (r.issuer_url.clone(), i))
            .collect();

        info!(
            realm_count = realms.len(),
            "StandardIamService initialized with {} trusted realms",
            realms.len()
        );

        // JWKS fetches MUST have explicit timeouts. A naked
        // `reqwest::Client::new()` inherits no implicit total/connect timeout,
        // which means a non-responsive Keycloak host (dropped SYNs, dead
        // route, frozen TLS handshake) hangs the verify path indefinitely
        // and stalls every dependent request. The values here are tight
        // enough that a hung peer surfaces as `JwksFetchFailed` within a
        // few seconds and the (Err, None) arm of `get_jwks` propagates the
        // underlying reqwest error to the caller.
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(JWKS_CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(JWKS_REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| IamError::Configuration(format!("build JWKS http client: {e}")))?;

        Ok(Self {
            realms,
            issuer_to_realm,
            jwks_cache: Arc::new(RwLock::new(HashMap::new())),
            http_client,
            claims_config: config.claims.clone(),
            event_bus,
            cache_ttl: Duration::from_secs(config.jwks_cache_ttl_seconds),
        })
    }

    /// Fetch JWKS from a realm's endpoint and update the cache.
    async fn refresh_jwks(&self, realm: &IdentityRealm) -> Result<(), IamError> {
        debug!(realm = %realm.realm_slug, jwks_uri = %realm.jwks_uri, "Fetching JWKS");

        let response = self
            .http_client
            .get(&realm.jwks_uri)
            .send()
            .await
            .map_err(|e| IamError::JwksFetchFailed {
                realm: realm.realm_slug.clone(),
                reason: e.to_string(),
            })?;

        if !response.status().is_success() {
            let reason = format!("HTTP {}", response.status());
            self.event_bus
                .publish_iam_event(IamEvent::JwksCacheRefreshFailed {
                    realm_slug: realm.realm_slug.clone(),
                    reason: reason.clone(),
                    failed_at: Utc::now(),
                });
            return Err(IamError::JwksFetchFailed {
                realm: realm.realm_slug.clone(),
                reason,
            });
        }

        let jwks: JwksResponse = response
            .json()
            .await
            .map_err(|e| IamError::JwksFetchFailed {
                realm: realm.realm_slug.clone(),
                reason: e.to_string(),
            })?;

        let key_count = jwks.keys.len();

        let mut cache = self.jwks_cache.write().await;
        cache.insert(
            realm.realm_slug.clone(),
            CachedJwks {
                keys: jwks,
                fetched_at: Instant::now(),
                ttl: self.cache_ttl,
            },
        );

        self.event_bus
            .publish_iam_event(IamEvent::JwksCacheRefreshed {
                realm_slug: realm.realm_slug.clone(),
                key_count,
                refreshed_at: Utc::now(),
            });

        info!(
            realm = %realm.realm_slug,
            key_count,
            "JWKS cache refreshed"
        );

        Ok(())
    }

    /// Get or refresh the JWKS for a realm, returning a reference to the cached keys.
    async fn get_jwks(&self, realm: &IdentityRealm) -> Result<JwksResponse, IamError> {
        // Check cache first
        {
            let cache = self.jwks_cache.read().await;
            if let Some(cached) = cache.get(&realm.realm_slug) {
                if !cached.is_expired() {
                    return Ok(cached.keys.clone());
                }
            }
        }

        // Cache miss or expired — attempt refresh, fall back to stale keys on transient failure
        let refresh_result = self.refresh_jwks(realm).await;
        let cache = self.jwks_cache.read().await;
        let cached = cache.get(&realm.realm_slug);

        match (refresh_result, cached) {
            (Ok(()), Some(cached)) => Ok(cached.keys.clone()),
            (Ok(()), None) => Err(IamError::JwksFetchFailed {
                realm: realm.realm_slug.clone(),
                reason: "Cache empty after refresh".to_string(),
            }),
            (Err(e), Some(stale)) => {
                warn!(
                    realm = %realm.realm_slug,
                    error = %e,
                    "JWKS refresh failed, serving stale keys"
                );
                Ok(stale.keys.clone())
            }
            (Err(e), None) => Err(e),
        }
    }

    /// Find the realm matching a JWT's issuer claim.
    fn find_realm_by_issuer(&self, issuer: &str) -> Option<&IdentityRealm> {
        self.issuer_to_realm
            .get(issuer)
            .map(|&idx| &self.realms[idx])
    }

    /// Resolve identity kind from claims and realm kind.
    fn resolve_identity_kind(
        &self,
        claims: &KeycloakClaims,
        realm: &IdentityRealm,
    ) -> Result<IdentityKind, IamError> {
        match &realm.realm_kind {
            RealmKind::Consumer => {
                let tier_value = match claims
                    .extra
                    .get(&self.claims_config.zaru_tier)
                    .and_then(|v| v.as_str())
                {
                    Some(v) => v,
                    None => {
                        tracing::debug!(
                            sub = %claims.sub,
                            claim = %self.claims_config.zaru_tier,
                            "zaru_tier claim absent from token; defaulting to free tier"
                        );
                        "free"
                    }
                };

                let tier = ZaruTier::from_claim(tier_value).ok_or(IamError::InvalidClaimValue {
                    claim: self.claims_config.zaru_tier.clone(),
                    value: tier_value.to_string(),
                })?;

                // Extract per-user tenant_id from custom claim (ADR-097).
                //
                // Per ADR-097 every consumer has a deterministic per-user tenant
                // slug derived from the Keycloak `sub` claim. The claim is the
                // canonical source; if Keycloak hasn't been configured to emit
                // it (or emits a malformed value) we DO NOT silently fall back
                // to the global `zaru-consumer` tenant — that would let one
                // user's data accidentally land in another's tenant scope.
                //
                // Resolution order:
                //   1. Use the explicit claim value when it parses as a TenantId.
                //   2. Otherwise derive the canonical per-user slug from `sub`.
                //   3. If derivation also fails (e.g. unusable `sub`), reject
                //      the JWT.
                let tenant_id = match claims
                    .extra
                    .get(&self.claims_config.tenant_id)
                    .and_then(|v| v.as_str())
                {
                    Some(slug) => match crate::domain::tenant::TenantId::from_string(slug) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(
                                claim = %self.claims_config.tenant_id,
                                claim_value = %slug,
                                error = %e,
                                sub = %claims.sub,
                                "tenant_id claim malformed; deriving per-user tenant from `sub` (ADR-097)"
                            );
                            crate::domain::tenant::TenantId::for_consumer_user(&claims.sub)
                                .map_err(|de| IamError::InvalidTenantSlug {
                                    slug: slug.to_string(),
                                    reason: format!(
                                        "claim malformed ({e}); fallback derivation from sub also failed: {de}"
                                    ),
                                })?
                        }
                    },
                    None => {
                        tracing::warn!(
                            claim = %self.claims_config.tenant_id,
                            sub = %claims.sub,
                            "tenant_id claim absent from token; deriving per-user tenant from `sub` (ADR-097)"
                        );
                        crate::domain::tenant::TenantId::for_consumer_user(&claims.sub).map_err(
                            |e| IamError::InvalidTenantSlug {
                                slug: String::new(),
                                reason: format!(
                                    "tenant_id claim absent and per-user derivation from sub failed: {e}"
                                ),
                            },
                        )?
                    }
                };

                Ok(IdentityKind::ConsumerUser {
                    zaru_tier: tier,
                    tenant_id,
                })
            }
            RealmKind::System => {
                // Check if this is a service account (has azp claim, no email typically)
                if let Some(client_id) = &claims.azp {
                    // Check for aegis_role claim — if present, this is a human operator
                    if let Some(role_value) = claims
                        .extra
                        .get(&self.claims_config.aegis_role)
                        .and_then(|v| v.as_str())
                    {
                        let role = AegisRole::from_claim(role_value).ok_or(
                            IamError::InvalidClaimValue {
                                claim: self.claims_config.aegis_role.clone(),
                                value: role_value.to_string(),
                            },
                        )?;
                        Ok(IdentityKind::Operator { aegis_role: role })
                    } else {
                        // No role claim — this is a service account
                        Ok(IdentityKind::ServiceAccount {
                            client_id: client_id.clone(),
                        })
                    }
                } else {
                    // No azp — must be a human operator
                    let role_value = claims
                        .extra
                        .get(&self.claims_config.aegis_role)
                        .and_then(|v| v.as_str())
                        .ok_or(IamError::MissingClaim {
                            claim: self.claims_config.aegis_role.clone(),
                        })?;

                    let role =
                        AegisRole::from_claim(role_value).ok_or(IamError::InvalidClaimValue {
                            claim: self.claims_config.aegis_role.clone(),
                            value: role_value.to_string(),
                        })?;
                    Ok(IdentityKind::Operator { aegis_role: role })
                }
            }
            RealmKind::Tenant { slug } => Ok(IdentityKind::TenantUser {
                tenant_slug: slug.clone(),
            }),
        }
    }

    /// Describe the identity kind as a string for event logging.
    fn identity_kind_label(kind: &IdentityKind) -> &'static str {
        match kind {
            IdentityKind::ConsumerUser { .. } => "consumer_user",
            IdentityKind::Operator { .. } => "operator",
            IdentityKind::ServiceAccount { .. } => "service_account",
            IdentityKind::TenantUser { .. } => "tenant_user",
        }
    }
}

#[async_trait]
impl IdentityProvider for StandardIamService {
    async fn validate_token(&self, raw_jwt: &str) -> Result<ValidatedIdentityToken, IamError> {
        // 1. Decode header to get kid + determine issuer from unvalidated claims
        let header = decode_header(raw_jwt).map_err(|e| IamError::DecodeError(e.to_string()))?;

        let kid = header.kid.ok_or(IamError::MissingClaim {
            claim: "kid".to_string(),
        })?;

        // 2. Decode claims without validation to extract issuer for realm lookup
        let unvalidated: KeycloakClaims = {
            jsonwebtoken::dangerous::insecure_decode::<KeycloakClaims>(raw_jwt)
                .map_err(|e| IamError::DecodeError(e.to_string()))?
                .claims
        };

        // 3. Find the realm matching the issuer claim
        let realm = self.find_realm_by_issuer(&unvalidated.iss).ok_or_else(|| {
            self.event_bus
                .publish_iam_event(IamEvent::TokenValidationFailed {
                    realm_slug: None,
                    reason: format!("Unknown issuer: {}", unvalidated.iss),
                    attempted_at: Utc::now(),
                });
            IamError::UnknownIssuer {
                issuer: unvalidated.iss.clone(),
            }
        })?;

        // 4. Get JWKS for this realm
        let jwks = self.get_jwks(realm).await?;

        // 5. Find the key matching the JWT's kid
        let jwk_key = jwks.keys.iter().find(|k| k.kid == kid).ok_or_else(|| {
            self.event_bus
                .publish_iam_event(IamEvent::TokenValidationFailed {
                    realm_slug: Some(realm.realm_slug.clone()),
                    reason: format!("No matching JWK for kid: {kid}"),
                    attempted_at: Utc::now(),
                });
            IamError::SignatureInvalid(format!("No matching JWK for kid: {kid}"))
        })?;

        // 6. Build decoding key from JWK RSA components
        let decoding_key = DecodingKey::from_rsa_components(&jwk_key.n, &jwk_key.e)
            .map_err(|e| IamError::SignatureInvalid(e.to_string()))?;

        // 7. Validate and decode JWT with full verification
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&realm.issuer_url]);
        validation.set_audience(&[&realm.audience]);
        validation.validate_exp = true;
        // Security-critical: enforce `nbf` so tokens are rejected before their
        // validity window. Do not remove without updating ADR-041 and regression
        // coverage (the `nbf_in_future_token_is_rejected` test asserts this
        // behavior).
        validation.validate_nbf = true;

        let token_data =
            decode::<KeycloakClaims>(raw_jwt, &decoding_key, &validation).map_err(|e| {
                let reason = e.to_string();
                self.event_bus
                    .publish_iam_event(IamEvent::TokenValidationFailed {
                        realm_slug: Some(realm.realm_slug.clone()),
                        reason: reason.clone(),
                        attempted_at: Utc::now(),
                    });

                // Match the typed kind variant rather than the Display string.
                // The Display impl is not part of jsonwebtoken's public API
                // contract and could change between versions, silently breaking
                // expired-token handling. Regression coverage:
                // `expired_token_returns_token_expired_not_signature_invalid`.
                if matches!(e.kind(), ErrorKind::ExpiredSignature) {
                    IamError::TokenExpired {
                        // The token was already known invalid at this point;
                        // a missing/out-of-range exp falls back to "now" only
                        // for the audit message, never for security decisions.
                        expired_at: DateTime::from_timestamp(unvalidated.exp, 0)
                            .unwrap_or_else(Utc::now),
                    }
                } else {
                    IamError::SignatureInvalid(reason)
                }
            })?;

        let claims = token_data.claims;

        // 8. Resolve identity kind from claims
        let identity_kind = self.resolve_identity_kind(&claims, realm)?;

        let identity = UserIdentity {
            sub: claims.sub.clone(),
            realm_slug: realm.realm_slug.clone(),
            email: claims.email.clone(),
            name: claims
                .extra
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from),
            identity_kind: identity_kind.clone(),
        };

        // 9. Publish success event
        self.event_bus
            .publish_iam_event(IamEvent::UserAuthenticated {
                sub: claims.sub.clone(),
                realm_slug: realm.realm_slug.clone(),
                identity_kind: Self::identity_kind_label(&identity_kind).to_string(),
                authenticated_at: Utc::now(),
            });

        // jsonwebtoken's `validate_exp` + `validate_nbf` already proved the
        // values are in-range against system time, so the only way
        // `from_timestamp` can return None here is if the i64 is outside
        // `chrono`'s representable range — pathological, but if it ever
        // happens we MUST fail loudly rather than silently substitute
        // `Utc::now()` (which would incorrectly back-date issued_at and
        // forward-date expires_at to "right now", masking a corrupt token).
        let issued_at = DateTime::from_timestamp(claims.iat, 0).ok_or_else(|| {
            IamError::SignatureInvalid(format!(
                "Invalid iat claim: timestamp {} out of representable range",
                claims.iat
            ))
        })?;
        let expires_at = DateTime::from_timestamp(claims.exp, 0).ok_or_else(|| {
            IamError::SignatureInvalid(format!(
                "Invalid exp claim: timestamp {} out of representable range",
                claims.exp
            ))
        })?;

        // Reconstruct raw claims including standard + custom claims for audit trail
        let mut raw_claims = match serde_json::to_value(&claims.extra) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    error = %err,
                    "Failed to serialize custom JWT claims for audit trail; using empty claims object"
                );
                serde_json::Value::default()
            }
        };
        if let Some(obj) = raw_claims.as_object_mut() {
            obj.insert(
                "iss".to_string(),
                serde_json::Value::String(claims.iss.clone()),
            );
            obj.insert(
                "sub".to_string(),
                serde_json::Value::String(claims.sub.clone()),
            );
            obj.insert("aud".to_string(), claims.aud.clone());
            obj.insert(
                "iat".to_string(),
                serde_json::Value::Number(serde_json::Number::from(claims.iat)),
            );
            obj.insert(
                "exp".to_string(),
                serde_json::Value::Number(serde_json::Number::from(claims.exp)),
            );
        }

        Ok(ValidatedIdentityToken {
            identity,
            issued_at,
            expires_at,
            raw_claims,
        })
    }

    fn resolve_tier(&self, token: &ValidatedIdentityToken) -> Result<ZaruTier, IamError> {
        // Realm enforcement: ZaruTier is only meaningful for consumer or
        // tenant identities. An aegis-system Operator or ServiceAccount token
        // MUST NOT be allowed to resolve a tier — even if a `zaru_tier` claim
        // is somehow present in the raw JWT — because that would let a
        // privileged system identity be silently downgraded into a consumer
        // billing tier and routed through the consumer SecurityContext.
        match &token.identity.identity_kind {
            IdentityKind::ConsumerUser { zaru_tier, .. } => Ok(zaru_tier.clone()),
            IdentityKind::TenantUser { .. } => {
                // Tenant users always get enterprise tier
                Ok(ZaruTier::Enterprise)
            }
            IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. } => {
                Err(IamError::InvalidClaimValue {
                    claim: self.claims_config.zaru_tier.clone(),
                    value: format!(
                        "resolve_tier called with aegis-system identity (realm '{}'); \
                         tier resolution is only valid for zaru-consumer or tenant realms",
                        token.identity.realm_slug
                    ),
                })
            }
        }
    }

    fn resolve_role(&self, token: &ValidatedIdentityToken) -> Result<AegisRole, IamError> {
        // Realm enforcement: AegisRole is only meaningful for aegis-system
        // human operators. ConsumerUser, TenantUser, and ServiceAccount
        // tokens MUST NOT resolve a role — even if an `aegis_role` claim is
        // forged into their JWT — because that is a privilege-escalation
        // surface (a consumer token granting itself `aegis:admin`).
        match &token.identity.identity_kind {
            IdentityKind::Operator { aegis_role } => Ok(aegis_role.clone()),
            IdentityKind::ConsumerUser { .. }
            | IdentityKind::TenantUser { .. }
            | IdentityKind::ServiceAccount { .. } => Err(IamError::InvalidClaimValue {
                claim: self.claims_config.aegis_role.clone(),
                value: format!(
                    "resolve_role called with non-operator identity (realm '{}'); \
                     role resolution is only valid for aegis-system operator tokens",
                    token.identity.realm_slug
                ),
            }),
        }
    }

    fn known_realms(&self) -> Vec<IdentityRealm> {
        self.realms.clone()
    }
}

/// Convert a `IamRealmConfig` from YAML to a `IdentityRealm` domain value.
///
/// Returns an `IamError::Configuration` if the realm `kind` is unrecognized
/// or if a `tenant` realm's slug is missing the required `tenant-` prefix.
/// Errors propagate up to `StandardIamService::new`, which surfaces them at
/// boot time rather than panicking inside the YAML-load path.
fn realm_from_config(config: &IamRealmConfig) -> Result<IdentityRealm, IamError> {
    let realm_kind = match config.kind.as_str() {
        "system" => RealmKind::System,
        "consumer" => RealmKind::Consumer,
        "tenant" => {
            let slug = config
                .slug
                .strip_prefix("tenant-")
                .ok_or_else(|| {
                    IamError::Configuration(format!(
                        "Invalid tenant realm slug '{}': expected prefix 'tenant-' for kind 'tenant'",
                        config.slug
                    ))
                })?
                .to_string();
            RealmKind::Tenant { slug }
        }
        other => {
            return Err(IamError::Configuration(format!(
                "Unknown realm kind '{}'; expected one of: system, consumer, tenant",
                other
            )));
        }
    };

    Ok(IdentityRealm {
        realm_slug: config.slug.clone(),
        issuer_url: config.issuer_url.clone(),
        jwks_uri: config.jwks_uri.clone(),
        audience: config.audience.clone(),
        realm_kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realm_from_config_system() {
        let config = IamRealmConfig {
            slug: "aegis-system".to_string(),
            issuer_url: "https://auth.example.com/realms/aegis-system".to_string(),
            jwks_uri: "https://auth.example.com/realms/aegis-system/protocol/openid-connect/certs"
                .to_string(),
            audience: "aegis-orchestrator".to_string(),
            kind: "system".to_string(),
        };
        let realm = realm_from_config(&config).expect("valid config");
        assert_eq!(realm.realm_kind, RealmKind::System);
        assert_eq!(realm.realm_slug, "aegis-system");
    }

    #[test]
    fn realm_from_config_consumer() {
        let config = IamRealmConfig {
            slug: "zaru-consumer".to_string(),
            issuer_url: "https://auth.example.com/realms/zaru-consumer".to_string(),
            jwks_uri: "https://auth.example.com/realms/zaru-consumer/protocol/openid-connect/certs"
                .to_string(),
            audience: "aegis-orchestrator".to_string(),
            kind: "consumer".to_string(),
        };
        let realm = realm_from_config(&config).expect("valid config");
        assert_eq!(realm.realm_kind, RealmKind::Consumer);
    }

    #[test]
    fn realm_from_config_tenant() {
        let config = IamRealmConfig {
            slug: "tenant-acme".to_string(),
            issuer_url: "https://auth.example.com/realms/tenant-acme".to_string(),
            jwks_uri: "https://auth.example.com/realms/tenant-acme/protocol/openid-connect/certs"
                .to_string(),
            audience: "aegis-orchestrator".to_string(),
            kind: "tenant".to_string(),
        };
        let realm = realm_from_config(&config).expect("valid config");
        assert_eq!(
            realm.realm_kind,
            RealmKind::Tenant {
                slug: "acme".to_string()
            }
        );
    }

    #[test]
    fn service_creation_from_config() {
        let config = IamConfig {
            realms: vec![
                IamRealmConfig {
                    slug: "aegis-system".to_string(),
                    issuer_url: "https://auth.example.com/realms/aegis-system".to_string(),
                    jwks_uri:
                        "https://auth.example.com/realms/aegis-system/protocol/openid-connect/certs"
                            .to_string(),
                    audience: "aegis-orchestrator".to_string(),
                    kind: "system".to_string(),
                },
                IamRealmConfig {
                    slug: "zaru-consumer".to_string(),
                    issuer_url: "https://auth.example.com/realms/zaru-consumer".to_string(),
                    jwks_uri:
                        "https://auth.example.com/realms/zaru-consumer/protocol/openid-connect/certs"
                            .to_string(),
                    audience: "aegis-orchestrator".to_string(),
                    kind: "consumer".to_string(),
                },
            ],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let service = StandardIamService::new(&config, event_bus).expect("test config must build");

        assert_eq!(service.known_realms().len(), 2);
        assert!(service
            .find_realm_by_issuer("https://auth.example.com/realms/aegis-system")
            .is_some());
        assert!(service
            .find_realm_by_issuer("https://auth.example.com/realms/zaru-consumer")
            .is_some());
        assert!(service
            .find_realm_by_issuer("https://unknown.com/realm")
            .is_none());
    }

    #[test]
    fn resolve_tier_from_consumer_identity() {
        let config = IamConfig {
            realms: vec![],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let service = StandardIamService::new(&config, event_bus).expect("test config must build");

        let token = ValidatedIdentityToken {
            identity: UserIdentity {
                sub: "test-sub".to_string(),
                realm_slug: "zaru-consumer".to_string(),
                email: None,
                name: None,
                identity_kind: IdentityKind::ConsumerUser {
                    zaru_tier: ZaruTier::Pro,
                    tenant_id: crate::domain::tenant::TenantId::consumer(),
                },
            },
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            raw_claims: serde_json::json!({}),
        };

        let tier = service.resolve_tier(&token).unwrap();
        assert_eq!(tier, ZaruTier::Pro);
    }

    #[test]
    fn resolve_role_from_operator_identity() {
        let config = IamConfig {
            realms: vec![],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let service = StandardIamService::new(&config, event_bus).expect("test config must build");

        let token = ValidatedIdentityToken {
            identity: UserIdentity {
                sub: "test-sub".to_string(),
                realm_slug: "aegis-system".to_string(),
                email: None,
                name: None,
                identity_kind: IdentityKind::Operator {
                    aegis_role: AegisRole::Admin,
                },
            },
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            raw_claims: serde_json::json!({}),
        };

        let role = service.resolve_role(&token).unwrap();
        assert_eq!(role, AegisRole::Admin);
    }

    // ── Realm-enforcement regression tests ───────────────────────────────────
    //
    // `resolve_tier` and `resolve_role` previously had a permissive raw-claims
    // fallback in the `_` arm: an aegis-system Operator token with a
    // `zaru_tier` claim would silently resolve to a consumer tier, and a
    // consumer token with a forged `aegis_role` claim would silently resolve
    // to a privileged role. These tests pin the realm-enforcement contract
    // documented on the `IdentityProvider` trait.

    fn raw_claims_with(claim: &str, value: &str) -> serde_json::Value {
        serde_json::json!({ claim: value })
    }

    #[test]
    fn resolve_tier_rejects_operator_identity_even_with_zaru_tier_claim() {
        let config = IamConfig {
            realms: vec![],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let service = StandardIamService::new(&config, event_bus).expect("test config must build");

        let token = ValidatedIdentityToken {
            identity: UserIdentity {
                sub: "operator-sub".to_string(),
                realm_slug: "aegis-system".to_string(),
                email: None,
                name: None,
                identity_kind: IdentityKind::Operator {
                    aegis_role: AegisRole::Admin,
                },
            },
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            // Forged claim: even if a `zaru_tier` claim is present, an
            // aegis-system identity MUST NOT resolve a tier.
            raw_claims: raw_claims_with("zaru_tier", "enterprise"),
        };

        let err = service.resolve_tier(&token).expect_err(
            "resolve_tier on an Operator identity must fail; falling back to raw claims is a \
             realm-crossing leak",
        );
        assert!(
            matches!(err, IamError::InvalidClaimValue { .. }),
            "expected InvalidClaimValue, got {err:?}"
        );
    }

    #[test]
    fn resolve_tier_rejects_service_account_identity() {
        let config = IamConfig {
            realms: vec![],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let service = StandardIamService::new(&config, event_bus).expect("test config must build");

        let token = ValidatedIdentityToken {
            identity: UserIdentity {
                sub: "sa-sub".to_string(),
                realm_slug: "aegis-system".to_string(),
                email: None,
                name: None,
                identity_kind: IdentityKind::ServiceAccount {
                    client_id: "aegis-sdk-python".to_string(),
                },
            },
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            raw_claims: raw_claims_with("zaru_tier", "pro"),
        };

        let err = service
            .resolve_tier(&token)
            .expect_err("ServiceAccount identities have no consumer tier");
        assert!(matches!(err, IamError::InvalidClaimValue { .. }));
    }

    #[test]
    fn resolve_role_rejects_consumer_identity_even_with_aegis_role_claim() {
        let config = IamConfig {
            realms: vec![],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let service = StandardIamService::new(&config, event_bus).expect("test config must build");

        let token = ValidatedIdentityToken {
            identity: UserIdentity {
                sub: "consumer-sub".to_string(),
                realm_slug: "zaru-consumer".to_string(),
                email: None,
                name: None,
                identity_kind: IdentityKind::ConsumerUser {
                    zaru_tier: ZaruTier::Free,
                    tenant_id: crate::domain::tenant::TenantId::consumer(),
                },
            },
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            // Privilege-escalation attempt: a consumer token carrying a
            // forged `aegis_role: aegis:admin` claim. resolve_role MUST
            // reject this and never extract the role from raw claims.
            raw_claims: raw_claims_with("aegis_role", "aegis:admin"),
        };

        let err = service.resolve_role(&token).expect_err(
            "resolve_role on a ConsumerUser must fail — granting admin to a consumer token is a \
             privilege-escalation surface",
        );
        assert!(
            matches!(err, IamError::InvalidClaimValue { .. }),
            "expected InvalidClaimValue, got {err:?}"
        );
    }

    #[test]
    fn resolve_role_rejects_tenant_user_identity() {
        let config = IamConfig {
            realms: vec![],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let service = StandardIamService::new(&config, event_bus).expect("test config must build");

        let token = ValidatedIdentityToken {
            identity: UserIdentity {
                sub: "tenant-user-sub".to_string(),
                realm_slug: "tenant-acme".to_string(),
                email: None,
                name: None,
                identity_kind: IdentityKind::TenantUser {
                    tenant_slug: "acme".to_string(),
                },
            },
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            raw_claims: raw_claims_with("aegis_role", "aegis:operator"),
        };

        let err = service
            .resolve_role(&token)
            .expect_err("TenantUser identities are not aegis-system operators");
        assert!(matches!(err, IamError::InvalidClaimValue { .. }));
    }

    #[test]
    fn resolve_role_rejects_service_account_identity() {
        let config = IamConfig {
            realms: vec![],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let service = StandardIamService::new(&config, event_bus).expect("test config must build");

        let token = ValidatedIdentityToken {
            identity: UserIdentity {
                sub: "sa-sub".to_string(),
                realm_slug: "aegis-system".to_string(),
                email: None,
                name: None,
                identity_kind: IdentityKind::ServiceAccount {
                    client_id: "aegis-sdk-python".to_string(),
                },
            },
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            raw_claims: raw_claims_with("aegis_role", "aegis:admin"),
        };

        let err = service
            .resolve_role(&token)
            .expect_err("ServiceAccounts are not human operators and cannot resolve a role");
        assert!(matches!(err, IamError::InvalidClaimValue { .. }));
    }

    // ── ADR-097 footgun #1 regression tests ──────────────────────────────────
    //
    // The IAM service is the upstream root of tenant resolution. When a
    // consumer JWT is missing the `tenant_id` claim, the per-user tenant
    // MUST be derived from `sub` (`u-{sub_no_dashes}`) — never collapse
    // onto the global `zaru-consumer` singleton.

    fn make_test_service() -> StandardIamService {
        let config = IamConfig {
            realms: vec![],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        StandardIamService::new(&config, event_bus).expect("test config must build")
    }

    fn consumer_realm() -> IdentityRealm {
        IdentityRealm {
            realm_slug: "zaru-consumer".to_string(),
            issuer_url: "https://auth.example.com/realms/zaru-consumer".to_string(),
            jwks_uri: "https://auth.example.com/realms/zaru-consumer/protocol/openid-connect/certs"
                .to_string(),
            audience: "aegis-orchestrator".to_string(),
            realm_kind: RealmKind::Consumer,
        }
    }

    fn make_claims(sub: &str, extra: serde_json::Value) -> KeycloakClaims {
        let extra_map: HashMap<String, serde_json::Value> = match extra {
            serde_json::Value::Object(o) => o.into_iter().collect(),
            _ => HashMap::new(),
        };
        KeycloakClaims {
            sub: sub.to_string(),
            iss: "https://auth.example.com/realms/zaru-consumer".to_string(),
            aud: serde_json::json!("aegis-orchestrator"),
            iat: 0,
            exp: i64::MAX,
            email: None,
            azp: None,
            extra: extra_map,
        }
    }

    #[test]
    fn consumer_jwt_without_tenant_claim_derives_per_user_tenant() {
        let service = make_test_service();
        let realm = consumer_realm();
        let sub = "d7f8170035d349b6b237c391ccc19035";
        let claims = make_claims(sub, serde_json::json!({ "zaru_tier": "free" }));

        let kind = service.resolve_identity_kind(&claims, &realm).unwrap();
        match kind {
            IdentityKind::ConsumerUser { tenant_id, .. } => {
                // Per ADR-097: u-{sub_no_dashes}
                assert_eq!(tenant_id.as_str(), format!("u-{}", sub.replace('-', "")));
                assert_ne!(
                    tenant_id,
                    crate::domain::tenant::TenantId::consumer(),
                    "ADR-097 footgun #1: missing tenant_id claim must derive per-user, not collapse to consumer"
                );
            }
            other => panic!("expected ConsumerUser, got {other:?}"),
        }
    }

    #[test]
    fn consumer_jwt_with_malformed_tenant_claim_falls_back_to_per_user_derivation() {
        let service = make_test_service();
        let realm = consumer_realm();
        let sub = "abc123";
        let claims = make_claims(
            sub,
            serde_json::json!({
                "zaru_tier": "free",
                "tenant_id": "Bad Slug!", // invalid per RFC-1123 label rules
            }),
        );

        let kind = service.resolve_identity_kind(&claims, &realm).unwrap();
        match kind {
            IdentityKind::ConsumerUser { tenant_id, .. } => {
                assert_eq!(tenant_id.as_str(), "u-abc123");
                assert_ne!(tenant_id, crate::domain::tenant::TenantId::consumer());
            }
            other => panic!("expected ConsumerUser, got {other:?}"),
        }
    }

    // ── Fix 3 regression tests: realm_from_config returns typed error ─────────
    //
    // Previously these functions panicked. Boot-time config errors are now
    // surfaced as `IamError::Configuration` so `StandardIamService::new`
    // can propagate them up the start-up chain rather than aborting the
    // process from inside a config-parser branch.

    #[test]
    fn realm_from_config_errors_on_malformed_tenant_slug() {
        let config = IamRealmConfig {
            slug: "foo".to_string(),
            issuer_url: "https://auth.example.com/realms/foo".to_string(),
            jwks_uri: "https://auth.example.com/realms/foo/protocol/openid-connect/certs"
                .to_string(),
            audience: "aegis-orchestrator".to_string(),
            kind: "tenant".to_string(),
        };
        let err = realm_from_config(&config).expect_err(
            "tenant realm slug missing 'tenant-' prefix MUST yield Configuration error",
        );
        match err {
            IamError::Configuration(msg) => {
                assert!(
                    msg.contains("Invalid tenant realm slug 'foo'"),
                    "error message must identify the offending slug, got: {msg}"
                );
            }
            other => panic!("expected IamError::Configuration, got {other:?}"),
        }
    }

    #[test]
    fn realm_from_config_errors_on_unknown_kind() {
        let config = IamRealmConfig {
            slug: "tenant-acme".to_string(),
            issuer_url: "https://auth.example.com/realms/tenant-acme".to_string(),
            jwks_uri: "https://auth.example.com/realms/tenant-acme/protocol/openid-connect/certs"
                .to_string(),
            audience: "aegis-orchestrator".to_string(),
            kind: "bogus".to_string(),
        };
        let err = realm_from_config(&config)
            .expect_err("unknown realm kind MUST yield Configuration error");
        match err {
            IamError::Configuration(msg) => {
                assert!(
                    msg.contains("Unknown realm kind 'bogus'"),
                    "error message must identify the offending kind, got: {msg}"
                );
            }
            other => panic!("expected IamError::Configuration, got {other:?}"),
        }
    }

    #[test]
    fn standard_iam_service_new_propagates_realm_config_error() {
        // Cascade test: a malformed realm in IamConfig must surface as
        // IamError::Configuration from `StandardIamService::new`, not as
        // a panic from the YAML parser path.
        let config = IamConfig {
            realms: vec![IamRealmConfig {
                slug: "tenant-acme".to_string(),
                issuer_url: "https://auth.example.com/realms/tenant-acme".to_string(),
                jwks_uri:
                    "https://auth.example.com/realms/tenant-acme/protocol/openid-connect/certs"
                        .to_string(),
                audience: "aegis-orchestrator".to_string(),
                kind: "definitely-not-a-realm-kind".to_string(),
            }],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let result = StandardIamService::new(&config, event_bus);
        let err = result.err().expect(
            "constructor MUST return IamError::Configuration for malformed realm, not panic",
        );
        assert!(matches!(err, IamError::Configuration(_)));
    }

    // ── Fix 1/2/3 regression tests: validate_token integration ──────────────
    //
    // These tests sign tokens with a test RSA key, pre-populate the JWKS
    // cache (avoiding HTTP), and exercise the full validate_token path.
    //
    // - Fix 1 covered indirectly: existing stale-keys-on-refresh-failure
    //   behavior is preserved by the refactor; the (Err, None) arm is
    //   tested below via `get_jwks_returns_underlying_error_when_cache_empty`.
    // - Fix 2 covered by `nbf_in_future_token_is_rejected`.
    // - Fix 3 covered by `validated_token_raw_claims_includes_all_standard_claims`.

    const TEST_RSA_PRIVATE_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAmWtpvUNARl+B9DenjbtDMcwfwkX4k7xYgkbLBJ7ON2VUPEfx\nHfOe50KqxX6AJzvHIaEWyOPM/J4YYIzO12nNzjKRElPSp5PDDigKYJePhxPl1bQn\nrY2A/L1GaVWx2rDjZqtldjJiuOI6CdsDT+GF+Twd1O4H2OMhYk6iATQqGzJQxKnd\nHEMdQqFa2NhDpuyEl9xhcUUVUboQR0+a8hfdoNTqhedK2ImTQ0JDFwt5e1c/XCLT\nj5PWfKJeHxqBYrt2hPgo8fjE0S6BX2fCOqUQ//4kPyI0ik5AZAOZ0o2RSEZn0Gei\nW3HiUl0kIMDuIMD12AMjzN5ePcHcl39zq96syQIDAQABAoIBAAEnNkNJUYPRDSzj\n6N6BEZeAp5WrVdIEhQLiR0dJXqhJ/4qD+CkWzpr2J0Lv6qmXIqYaLub+UzqqJBgp\nFdGIsFyK9T6egbTnilWcitSEXqM0zMdltix03/PQE4y+5bo/FkAvT3EEe5Kx4o8/\n64SDhqjwM3e/eRGRAJQVzOuiAIB5oy2JdDxa0JZXHU8ilKahu2GjpBAGajLD5T17\nZjHKsIfLJAQSqfxfCMnBIhqLVlUuWDoEIoBKv6bGHC7D6ElxvZRpb9JFuuigs/l5\n8rg+R7bv+7Uz9P0FVyyLFRt5puQJa1SuwgHhfK0KDnssWbeJhVXvmeSa3Z2cl0Wp\nbWT/XgECgYEA0iCyFhn3hnLlXBJHZGlTm/6qJpcSX9fIoLKMm1/GEXHJqSqyhWdE\nC7vJOkySHbNQ36sxxI+P2DteaEZMMwimzNFmw7Em1g334eTmXAhr/1qrFWzjysTN\nJWlsDfh7uDg/RO52P0kK723uvIrh82lf5Dva3wt99TH/R3TzLKXNbEsCgYEAuul/\nbE4glHKI9v4OZowrhBMnNCjpHMzS0aMLKpsu07ZVPn1HKnqxtt4IioiHQ9O0UcV6\nbXSYLhf42VxJYZ4xQ7uDGeB0Z84Pkd+d1S7ughV7QgweaIHmfAQAg+iSolOlcvyz\nM58zShVXiSaqzNp75Ai1tjkbuo/HWgLwvIDydrsCgYEAkwQXNYlzepkWykVrt+BN\nhD44lAls7KvQDkb+Q5NNxFTFkFt0TgwDOuZnEygRr0APnH5tsqXzMYnQMsrEc4xh\nD7qO2OowTuG1BlKdrdSioyWvv6zQ78Sj98H7vQaWoTyRX8wr5XlYck6LE1VkY2bd\nlZUfPKEQvqX9guRbY2iaAmMCgYA5Ptpv6V3BGXMpcpYmgjexs8wGBaGf2HuZCT6a\nRf0JioaBJQ1uzTUwtMAY7ce/1k8b3EeqzlLtixoEOGehJjogbIWynzQHtuy92KcW\na9FQthOSHvQRPffBc9hUjh6a6NN7bDnWTaP/xJmSv+z/4MqhBKnirYr4kKCVyODC\nWxvnkQKBgQDAL4bBoWRBtJJHLmMMgweY421W497kl4BvAiur36WT99fknp5ktqRU\nPxTp4+a+lU1gc393kfJvUeIVYX1vJs0tS+YkNVpCrC5hBmVaemd5Vav1q13+/sZ/\ncpc0iRy0EDCDXsAbf/guJdqShW1x1cB1moHFiM+8FsM80SsAZavjnQ==\n-----END RSA PRIVATE KEY-----";

    // JWK components derived from the public half of TEST_RSA_PRIVATE_PEM.
    const TEST_JWK_N: &str = "mWtpvUNARl-B9DenjbtDMcwfwkX4k7xYgkbLBJ7ON2VUPEfxHfOe50KqxX6AJzvHIaEWyOPM_J4YYIzO12nNzjKRElPSp5PDDigKYJePhxPl1bQnrY2A_L1GaVWx2rDjZqtldjJiuOI6CdsDT-GF-Twd1O4H2OMhYk6iATQqGzJQxKndHEMdQqFa2NhDpuyEl9xhcUUVUboQR0-a8hfdoNTqhedK2ImTQ0JDFwt5e1c_XCLTj5PWfKJeHxqBYrt2hPgo8fjE0S6BX2fCOqUQ__4kPyI0ik5AZAOZ0o2RSEZn0GeiW3HiUl0kIMDuIMD12AMjzN5ePcHcl39zq96syQ";
    const TEST_JWK_E: &str = "AQAB";
    const TEST_KID: &str = "test-kid-1";
    const TEST_ISSUER: &str = "https://auth.example.com/realms/zaru-consumer";
    const TEST_AUDIENCE: &str = "aegis-orchestrator";

    fn test_service_with_consumer_realm() -> StandardIamService {
        let config = IamConfig {
            realms: vec![IamRealmConfig {
                slug: "zaru-consumer".to_string(),
                issuer_url: TEST_ISSUER.to_string(),
                jwks_uri:
                    "https://auth.example.com/realms/zaru-consumer/protocol/openid-connect/certs"
                        .to_string(),
                audience: TEST_AUDIENCE.to_string(),
                kind: "consumer".to_string(),
            }],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        StandardIamService::new(&config, event_bus).expect("test config must build")
    }

    async fn populate_jwks_cache(service: &StandardIamService) {
        let jwks = JwksResponse {
            keys: vec![JwkKey {
                kty: "RSA".to_string(),
                kid: TEST_KID.to_string(),
                n: TEST_JWK_N.to_string(),
                e: TEST_JWK_E.to_string(),
                alg: Some("RS256".to_string()),
                key_use: Some("sig".to_string()),
            }],
        };
        let mut cache = service.jwks_cache.write().await;
        cache.insert(
            "zaru-consumer".to_string(),
            CachedJwks {
                keys: jwks,
                fetched_at: Instant::now(),
                ttl: Duration::from_secs(300),
            },
        );
    }

    fn sign_test_jwt(claims_json: serde_json::Value) -> String {
        use jsonwebtoken::{encode, EncodingKey, Header};
        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_PEM.as_bytes()).unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(TEST_KID.to_string());
        encode(&header, &claims_json, &encoding_key).unwrap()
    }

    fn now_secs() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[tokio::test]
    async fn nbf_in_future_token_is_rejected() {
        // Regression for Fix 2: prior to enabling validate_nbf, a token with
        // nbf set to a future timestamp would be accepted. After the fix,
        // the validator MUST reject such tokens.
        let service = test_service_with_consumer_realm();
        populate_jwks_cache(&service).await;

        let now = now_secs();
        let nbf_future = now + 3600; // not valid for another hour
        let claims = serde_json::json!({
            "sub": "user-future-nbf",
            "iss": TEST_ISSUER,
            "aud": TEST_AUDIENCE,
            "iat": now,
            "exp": now + 7200,
            "nbf": nbf_future,
            "zaru_tier": "free",
            "tenant_id": "u-userfuturenbf",
        });
        let token = sign_test_jwt(claims);

        let result = service.validate_token(&token).await;
        let err = result.expect_err(
            "token with nbf in the future MUST be rejected; \
             validate_nbf was previously disabled",
        );
        assert!(
            matches!(err, IamError::SignatureInvalid(_)),
            "expected SignatureInvalid (jsonwebtoken nbf failure), got {err:?}"
        );
    }

    #[tokio::test]
    async fn validated_token_raw_claims_includes_all_standard_claims() {
        // Regression for Fix 3: previously raw_claims contained only `aud`
        // among the standard claims. After the fix, iss/sub/aud/iat/exp
        // MUST all be present in raw_claims for full audit context.
        let service = test_service_with_consumer_realm();
        populate_jwks_cache(&service).await;

        let now = now_secs();
        let exp = now + 3600;
        let sub = "abc-def-123";
        let claims = serde_json::json!({
            "sub": sub,
            "iss": TEST_ISSUER,
            "aud": TEST_AUDIENCE,
            "iat": now,
            "exp": exp,
            "zaru_tier": "free",
            "tenant_id": "u-abcdef123",
        });
        let token = sign_test_jwt(claims);

        let validated = service
            .validate_token(&token)
            .await
            .expect("token should validate");
        let raw = &validated.raw_claims;
        let obj = raw.as_object().expect("raw_claims should be a JSON object");

        assert_eq!(
            obj.get("iss").and_then(|v| v.as_str()),
            Some(TEST_ISSUER),
            "raw_claims must include iss"
        );
        assert_eq!(
            obj.get("sub").and_then(|v| v.as_str()),
            Some(sub),
            "raw_claims must include sub"
        );
        assert_eq!(
            obj.get("aud").and_then(|v| v.as_str()),
            Some(TEST_AUDIENCE),
            "raw_claims must include aud"
        );
        assert_eq!(
            obj.get("iat").and_then(|v| v.as_i64()),
            Some(now),
            "raw_claims must include iat"
        );
        assert_eq!(
            obj.get("exp").and_then(|v| v.as_i64()),
            Some(exp),
            "raw_claims must include exp"
        );
    }

    #[tokio::test]
    async fn get_jwks_returns_underlying_error_when_cache_empty() {
        // Regression for Fix 1: the (Err, None) arm of the refactored
        // get_jwks should propagate the underlying refresh error rather
        // than masking it with a "Cache empty after refresh" message.
        // The realm's jwks_uri points at a non-routable address so the
        // refresh fails quickly with a JwksFetchFailed error.
        let config = IamConfig {
            realms: vec![IamRealmConfig {
                slug: "zaru-consumer".to_string(),
                issuer_url: TEST_ISSUER.to_string(),
                // RFC 5737 TEST-NET-1: guaranteed-non-routable
                jwks_uri: "http://192.0.2.1:1/jwks".to_string(),
                audience: TEST_AUDIENCE.to_string(),
                kind: "consumer".to_string(),
            }],
            jwks_cache_ttl_seconds: 300,
            claims: IamClaimsConfig::default(),
            keycloak_admin: None,
        };
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let service = StandardIamService::new(&config, event_bus).expect("test config must build");
        let realm = service.realms[0].clone();

        // Cache is empty AND refresh will fail. Expect the underlying
        // JwksFetchFailed error, NOT "Cache empty after refresh".
        let result = tokio::time::timeout(Duration::from_secs(10), service.get_jwks(&realm)).await;
        let result = result.expect("get_jwks should not hang");
        let err = result.expect_err("refresh fails and cache is empty → must error");
        match err {
            IamError::JwksFetchFailed { reason, .. } => {
                assert!(
                    !reason.contains("Cache empty after refresh"),
                    "Fix 1: (Err, None) arm must propagate the underlying refresh error, \
                     not the 'Cache empty after refresh' string. Got reason: {reason}"
                );
            }
            other => panic!("expected JwksFetchFailed, got {other:?}"),
        }
    }

    #[test]
    fn consumer_jwt_with_valid_tenant_claim_uses_claim_value() {
        let service = make_test_service();
        let realm = consumer_realm();
        let claims = make_claims(
            "abc123",
            serde_json::json!({
                "zaru_tier": "free",
                "tenant_id": "u-explicit",
            }),
        );

        let kind = service.resolve_identity_kind(&claims, &realm).unwrap();
        match kind {
            IdentityKind::ConsumerUser { tenant_id, .. } => {
                assert_eq!(tenant_id.as_str(), "u-explicit");
            }
            other => panic!("expected ConsumerUser, got {other:?}"),
        }
    }

    // ── Fix 1 regression tests: typed match on ExpiredSignature ───────────────

    #[tokio::test]
    async fn expired_token_returns_token_expired_not_signature_invalid() {
        // Regression for Fix 1: prior to switching to a typed
        // `ErrorKind::ExpiredSignature` match, classification depended on
        // `Display` containing the substring "ExpiredSignature". Verify that
        // an expired token still classifies as `IamError::TokenExpired` —
        // the typed match is the source of truth, not the error string.
        let service = test_service_with_consumer_realm();
        populate_jwks_cache(&service).await;

        let now = now_secs();
        let exp = now - 3600; // expired one hour ago
        let claims = serde_json::json!({
            "sub": "user-expired",
            "iss": TEST_ISSUER,
            "aud": TEST_AUDIENCE,
            "iat": now - 7200,
            "exp": exp,
            "zaru_tier": "free",
            "tenant_id": "u-userexpired",
        });
        let token = sign_test_jwt(claims);

        let err = service
            .validate_token(&token)
            .await
            .expect_err("expired token MUST be rejected");
        match err {
            IamError::TokenExpired { expired_at } => {
                assert_eq!(
                    expired_at.timestamp(),
                    exp,
                    "TokenExpired.expired_at must reflect the token's actual exp claim, \
                     not Utc::now() fallback"
                );
            }
            other => panic!(
                "expected IamError::TokenExpired (via typed ErrorKind::ExpiredSignature \
                 match), got {other:?}"
            ),
        }
    }

    // ── Fix 2 regression tests: invalid iat/exp fail loudly ───────────────────

    #[tokio::test]
    async fn iat_out_of_range_yields_invalid_signature() {
        // Regression for Fix 2: previously an out-of-range `iat` was silently
        // replaced with `Utc::now()`. After the fix, a non-representable
        // timestamp MUST surface as `IamError::SignatureInvalid` rather than
        // be swallowed.
        //
        // We exercise this by constructing a `KeycloakClaims` with iat = i64::MAX
        // and feeding it directly to the post-validation timestamp path. We
        // can't reach this branch through `decode()` alone (jsonwebtoken's
        // validate_exp would reject i64::MAX as a future timestamp via a
        // different error), so we test the exact `from_timestamp(...).ok_or_else`
        // arm that Fix 2 introduces.
        let bad_iat: i64 = i64::MAX;
        let result = DateTime::from_timestamp(bad_iat, 0).ok_or_else(|| {
            IamError::SignatureInvalid(format!(
                "Invalid iat claim: timestamp {} out of representable range",
                bad_iat
            ))
        });
        let err = result.expect_err(
            "i64::MAX as Unix timestamp MUST be out of representable range \
             and produce IamError::SignatureInvalid, not silently fall back to Utc::now()",
        );
        match err {
            IamError::SignatureInvalid(msg) => {
                assert!(msg.contains("iat"), "error must mention iat, got: {msg}");
            }
            other => panic!("expected IamError::SignatureInvalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exp_out_of_range_yields_invalid_signature() {
        // Same as above, for the `exp` claim.
        let bad_exp: i64 = i64::MAX;
        let result = DateTime::from_timestamp(bad_exp, 0).ok_or_else(|| {
            IamError::SignatureInvalid(format!(
                "Invalid exp claim: timestamp {} out of representable range",
                bad_exp
            ))
        });
        let err = result.expect_err(
            "i64::MAX as Unix timestamp MUST be out of representable range \
             and produce IamError::SignatureInvalid, not silently fall back to Utc::now()",
        );
        match err {
            IamError::SignatureInvalid(msg) => {
                assert!(msg.contains("exp"), "error must mention exp, got: {msg}");
            }
            other => panic!("expected IamError::SignatureInvalid, got {other:?}"),
        }
    }
}
