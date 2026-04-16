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
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
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
    pub fn new(config: &IamConfig, event_bus: Arc<EventBus>) -> Self {
        let realms: Vec<IdentityRealm> = config.realms.iter().map(realm_from_config).collect();

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

        Self {
            realms,
            issuer_to_realm,
            jwks_cache: Arc::new(RwLock::new(HashMap::new())),
            http_client: reqwest::Client::new(),
            claims_config: config.claims.clone(),
            event_bus,
            cache_ttl: Duration::from_secs(config.jwks_cache_ttl_seconds),
        }
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
        match self.refresh_jwks(realm).await {
            Ok(()) => {
                let cache = self.jwks_cache.read().await;
                cache
                    .get(&realm.realm_slug)
                    .map(|c| c.keys.clone())
                    .ok_or_else(|| IamError::JwksFetchFailed {
                        realm: realm.realm_slug.clone(),
                        reason: "Cache empty after refresh".to_string(),
                    })
            }
            Err(e) => {
                let cache = self.jwks_cache.read().await;
                if let Some(stale) = cache.get(&realm.realm_slug) {
                    warn!(
                        realm = %realm.realm_slug,
                        error = %e,
                        "JWKS refresh failed, serving stale keys"
                    );
                    Ok(stale.keys.clone())
                } else {
                    Err(e)
                }
            }
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

                // Extract per-user tenant_id from custom claim (ADR-097)
                let tenant_id_str = claims
                    .extra
                    .get(&self.claims_config.tenant_id)
                    .and_then(|v| v.as_str())
                    .unwrap_or("zaru-consumer");
                let tenant_id = crate::domain::tenant::TenantId::from_string(tenant_id_str)
                    .unwrap_or_else(|_| crate::domain::tenant::TenantId::consumer());

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

        let token_data =
            decode::<KeycloakClaims>(raw_jwt, &decoding_key, &validation).map_err(|e| {
                let reason = e.to_string();
                self.event_bus
                    .publish_iam_event(IamEvent::TokenValidationFailed {
                        realm_slug: Some(realm.realm_slug.clone()),
                        reason: reason.clone(),
                        attempted_at: Utc::now(),
                    });

                if reason.contains("ExpiredSignature") {
                    IamError::TokenExpired {
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

        let issued_at = DateTime::from_timestamp(claims.iat, 0).unwrap_or_else(Utc::now);
        let expires_at = DateTime::from_timestamp(claims.exp, 0).unwrap_or_else(Utc::now);

        // Reconstruct raw claims including standard + custom claims for audit trail
        let mut raw_claims = serde_json::to_value(&claims.extra).unwrap_or_default();
        if let Some(obj) = raw_claims.as_object_mut() {
            obj.insert("aud".to_string(), claims.aud.clone());
        }

        Ok(ValidatedIdentityToken {
            identity,
            issued_at,
            expires_at,
            raw_claims,
        })
    }

    fn resolve_tier(&self, token: &ValidatedIdentityToken) -> Result<ZaruTier, IamError> {
        match &token.identity.identity_kind {
            IdentityKind::ConsumerUser { zaru_tier, .. } => Ok(zaru_tier.clone()),
            IdentityKind::TenantUser { .. } => {
                // Tenant users always get enterprise tier
                Ok(ZaruTier::Enterprise)
            }
            _ => {
                // Try extracting from raw claims as a fallback
                token
                    .raw_claims
                    .get(&self.claims_config.zaru_tier)
                    .and_then(|v| v.as_str())
                    .and_then(ZaruTier::from_claim)
                    .ok_or(IamError::MissingClaim {
                        claim: self.claims_config.zaru_tier.clone(),
                    })
            }
        }
    }

    fn resolve_role(&self, token: &ValidatedIdentityToken) -> Result<AegisRole, IamError> {
        match &token.identity.identity_kind {
            IdentityKind::Operator { aegis_role } => Ok(aegis_role.clone()),
            _ => {
                // Try extracting from raw claims as a fallback
                token
                    .raw_claims
                    .get(&self.claims_config.aegis_role)
                    .and_then(|v| v.as_str())
                    .and_then(AegisRole::from_claim)
                    .ok_or(IamError::MissingClaim {
                        claim: self.claims_config.aegis_role.clone(),
                    })
            }
        }
    }

    fn known_realms(&self) -> Vec<IdentityRealm> {
        self.realms.clone()
    }
}

/// Convert a `IamRealmConfig` from YAML to a `IdentityRealm` domain value.
fn realm_from_config(config: &IamRealmConfig) -> IdentityRealm {
    let realm_kind = match config.kind.as_str() {
        "system" => RealmKind::System,
        "consumer" => RealmKind::Consumer,
        _ => {
            // Assume "tenant" kind — extract slug from realm slug
            let slug = config
                .slug
                .strip_prefix("tenant-")
                .unwrap_or(&config.slug)
                .to_string();
            RealmKind::Tenant { slug }
        }
    };

    IdentityRealm {
        realm_slug: config.slug.clone(),
        issuer_url: config.issuer_url.clone(),
        jwks_uri: config.jwks_uri.clone(),
        audience: config.audience.clone(),
        realm_kind,
    }
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
        let realm = realm_from_config(&config);
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
        let realm = realm_from_config(&config);
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
        let realm = realm_from_config(&config);
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
        let service = StandardIamService::new(&config, event_bus);

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
        let service = StandardIamService::new(&config, event_bus);

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
        let service = StandardIamService::new(&config, event_bus);

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
}
