// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `aegis_*` API-key identity lookup (ADR-093), shared by every handler
//! that authenticates an API key itself rather than through the JWT-only
//! `iam_auth_middleware`: `/v1/seal/attest` (`handlers::seal`) and
//! `GET /v1/llm/aliases/{alias}` (`handlers::llm`, Zaru ADR-0049 D4, AEGIS
//! ADR-124's Update of 2026-10-01). One lookup, so the two routes accept and
//! refuse exactly the same keys.

use std::sync::Arc;

use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_api_key::ApiKeyRow;
use aegis_orchestrator_core::infrastructure::repositories::PostgresApiKeyRepository;

use crate::daemon::handlers::api_keys::hash_key;

/// The one query the lookup makes: the active, unexpired `api_keys` row with
/// this SHA-256 hash. Implemented by [`PostgresApiKeyRepository`], whose
/// `find_by_key_hash` filters revoked and expired rows in SQL; handler tests
/// substitute a table.
#[async_trait::async_trait]
pub(crate) trait ApiKeyLookup: Send + Sync {
    async fn find_active_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRow>, String>;
}

#[async_trait::async_trait]
impl ApiKeyLookup for PostgresApiKeyRepository {
    async fn find_active_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRow>, String> {
        self.find_by_key_hash(key_hash)
            .await
            .map_err(|e| e.to_string())
    }
}

/// The daemon's API-key repository as the lookup, from
/// `AppState::api_key_repo` (absent when the node has no database).
pub(crate) fn lookup_from_repo(
    repo: Option<&Arc<PostgresApiKeyRepository>>,
) -> Option<Arc<dyn ApiKeyLookup>> {
    repo.map(|r| r.clone() as Arc<dyn ApiKeyLookup>)
}

/// Returns the first 12 hex chars of the SHA-256 hash of an API key for
/// safe correlation in logs. Never log the raw key, never log the full
/// hash — this prefix is enough to grep DB rows or correlate across
/// services without leaking material that could be used to reconstruct
/// the key.
pub(crate) fn key_hash_prefix(raw_token: &str) -> String {
    let h = hash_key(raw_token);
    h.chars().take(12).collect()
}

/// Synthesize a [`UserIdentity`] from an `aegis_*` API key row by looking
/// the key up in the api_keys repository. Returns `None` if the key is
/// unknown / revoked / expired, or if no repository is configured.
pub(crate) async fn identity_from_api_key(
    repo: Option<&dyn ApiKeyLookup>,
    raw_token: &str,
) -> Option<UserIdentity> {
    let prefix = key_hash_prefix(raw_token);
    let repo = match repo {
        Some(r) => r,
        None => {
            tracing::warn!(
                target: "aegis::auth::api_key",
                key_prefix = %prefix,
                "api_key_repo not configured; cannot validate API key"
            );
            return None;
        }
    };
    let hash = hash_key(raw_token);
    let row = match repo.find_active_by_hash(&hash).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::warn!(
                target: "aegis::auth::api_key",
                key_prefix = %prefix,
                "API key hash not found in repository"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                target: "aegis::auth::api_key",
                key_prefix = %prefix,
                error = %e,
                "API key repository lookup errored"
            );
            return None;
        }
    };
    tracing::info!(
        target: "aegis::auth::api_key",
        key_prefix = %prefix,
        user_id = %row.user_id,
        tenant_id = %row.tenant_id,
        has_aegis_role = row.aegis_role.is_some(),
        has_zaru_tier = row.zaru_tier.is_some(),
        "API key row found"
    );
    let realm_slug = if row.aegis_role.is_some() {
        "aegis-system".to_string()
    } else {
        "zaru-consumer".to_string()
    };
    let identity_kind = if let Some(role_str) = row.aegis_role.as_deref() {
        let role = match AegisRole::from_claim(role_str) {
            Some(r) => r,
            None => {
                tracing::warn!(
                    target: "aegis::auth::api_key",
                    key_prefix = %prefix,
                    aegis_role = %role_str,
                    "AegisRole::from_claim returned None for stored aegis_role"
                );
                return None;
            }
        };
        IdentityKind::Operator { aegis_role: role }
    } else {
        let tenant_id = match TenantId::from_realm_slug(&row.tenant_id) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    target: "aegis::auth::api_key",
                    key_prefix = %prefix,
                    stored_tenant_id = %row.tenant_id,
                    error = %e,
                    "TenantId::from_realm_slug failed; cannot synthesize ConsumerUser identity"
                );
                return None;
            }
        };
        let zaru_tier = row
            .zaru_tier
            .as_deref()
            .and_then(ZaruTier::from_claim)
            .unwrap_or(ZaruTier::Free);
        IdentityKind::ConsumerUser {
            zaru_tier,
            tenant_id,
        }
    };
    Some(UserIdentity {
        sub: row.user_id,
        realm_slug,
        email: None,
        email_verified: false,
        name: None,
        identity_kind,
    })
}
