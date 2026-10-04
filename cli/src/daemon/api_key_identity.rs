// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `aegis_*` API-key identity lookup (ADR-093), shared by every handler
//! that authenticates an API key itself rather than through the JWT-only
//! `iam_auth_middleware`: `/v1/seal/attest` (`handlers::seal`) and
//! `GET /v1/llm/aliases/{alias}` (`handlers::llm`, Zaru ADR-0049 D4, AEGIS
//! ADR-124's Update of 2026-10-01). One lookup, so the two routes accept and
//! refuse exactly the same keys.
//!
//! A key with no stored role that holds an active operator escalation
//! resolves to `IdentityKind::Operator` with the escalation's role (AEGIS
//! ADR-129 D14); [`resolve_api_key`] also returns the key's id, its home
//! tenant and the escalation, which `/v1/seal/attest` binds the session to
//! (the record's Update U1). A role-bearing key is unchanged (D15).

use std::sync::Arc;

use aegis_orchestrator_core::application::operator_escalation_service::OperatorEscalationService;
use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::operator_escalation::OperatorEscalation;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_api_key::ApiKeyRow;
use aegis_orchestrator_core::infrastructure::repositories::PostgresApiKeyRepository;
use uuid::Uuid;

use crate::daemon::handlers::api_keys::hash_key;

/// The two queries the lookup makes: the active, unexpired `api_keys` row
/// with this SHA-256 hash, and the active operator escalation of a key (AEGIS
/// ADR-129 D14). Implemented by [`DaemonApiKeyLookup`]; handler tests
/// substitute a table.
#[async_trait::async_trait]
pub(crate) trait ApiKeyLookup: Send + Sync {
    async fn find_active_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRow>, String>;
    async fn active_escalation(
        &self,
        api_key_id: Uuid,
    ) -> Result<Option<OperatorEscalation>, String>;
}

/// The daemon's lookup: [`PostgresApiKeyRepository`], whose
/// `find_by_key_hash` filters revoked and expired rows in SQL, and the
/// operator escalation service, absent on a node without a database.
pub(crate) struct DaemonApiKeyLookup {
    keys: Arc<PostgresApiKeyRepository>,
    escalations: Option<Arc<OperatorEscalationService>>,
}

#[async_trait::async_trait]
impl ApiKeyLookup for DaemonApiKeyLookup {
    async fn find_active_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRow>, String> {
        self.keys
            .find_by_key_hash(key_hash)
            .await
            .map_err(|e| e.to_string())
    }

    async fn active_escalation(
        &self,
        api_key_id: Uuid,
    ) -> Result<Option<OperatorEscalation>, String> {
        match &self.escalations {
            Some(service) => service
                .active_for_api_key(api_key_id)
                .await
                .map_err(|e| e.to_string()),
            None => Ok(None),
        }
    }
}

/// The daemon's lookup, from `AppState::api_key_repo` (absent when the node
/// has no database) and `AppState::operator_escalations`.
pub(crate) fn lookup_from_repo(
    repo: Option<&Arc<PostgresApiKeyRepository>>,
    escalations: Option<&Arc<OperatorEscalationService>>,
) -> Option<Arc<dyn ApiKeyLookup>> {
    repo.map(|keys| {
        Arc::new(DaemonApiKeyLookup {
            keys: keys.clone(),
            escalations: escalations.cloned(),
        }) as Arc<dyn ApiKeyLookup>
    })
}

/// An authenticated `aegis_*` key: the identity it acts as, and what the
/// operator escalation needs beside it (AEGIS ADR-129 D14; the Update's U1).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedApiKey {
    pub(crate) identity: UserIdentity,
    pub(crate) api_key_id: Uuid,
    /// `api_keys.user_id`.
    pub(crate) user_id: String,
    /// The key owner's tenant (`api_keys.tenant_id`): the escalated
    /// session's home tenant.
    pub(crate) home_tenant: String,
    /// Whether the key was stored with an `aegis_role` (D10, D15).
    pub(crate) has_stored_role: bool,
    /// The key's active escalation; only a key with no stored role holds
    /// one (D10).
    pub(crate) escalation: Option<OperatorEscalation>,
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
    resolve_api_key(repo, raw_token).await.map(|r| r.identity)
}

/// Resolve an `aegis_*` key to its identity and escalation state. A key
/// with no stored role holding an active escalation acts as
/// `IdentityKind::Operator` with the escalation's role (AEGIS ADR-129 D14);
/// a failed escalation lookup refuses the key rather than guess.
pub(crate) async fn resolve_api_key(
    repo: Option<&dyn ApiKeyLookup>,
    raw_token: &str,
) -> Option<ResolvedApiKey> {
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
    let escalation = if row.aegis_role.is_none() {
        match repo.active_escalation(row.id).await {
            Ok(escalation) => escalation,
            Err(e) => {
                tracing::warn!(
                    target: "aegis::auth::api_key",
                    key_prefix = %prefix,
                    error = %e,
                    "operator escalation lookup errored; refusing the key"
                );
                return None;
            }
        }
    } else {
        None
    };
    let realm_slug = if row.aegis_role.is_some() || escalation.is_some() {
        "aegis-system".to_string()
    } else {
        "zaru-consumer".to_string()
    };
    let identity_kind = if let Some(escalation) = &escalation {
        IdentityKind::Operator {
            aegis_role: escalation.aegis_role.clone(),
        }
    } else if let Some(role_str) = row.aegis_role.as_deref() {
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
    Some(ResolvedApiKey {
        identity: UserIdentity {
            sub: row.user_id.clone(),
            realm_slug,
            email: None,
            email_verified: false,
            name: None,
            identity_kind,
        },
        api_key_id: row.id,
        user_id: row.user_id,
        home_tenant: row.tenant_id,
        has_stored_role: row.aegis_role.is_some(),
        escalation,
    })
}

/// A key table and escalation store for handler tests: the rows
/// `find_by_key_hash` would answer, and the escalation service the daemon
/// would consult (AEGIS ADR-129).
#[cfg(test)]
pub(crate) mod test_keys {
    use super::*;

    pub(crate) struct KeyTable {
        pub(crate) rows: Vec<ApiKeyRow>,
        pub(crate) escalations: Option<Arc<OperatorEscalationService>>,
    }

    #[async_trait::async_trait]
    impl ApiKeyLookup for KeyTable {
        async fn find_active_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRow>, String> {
            Ok(self
                .rows
                .iter()
                .find(|r| r.key_hash == key_hash && r.status == "active")
                .cloned())
        }

        async fn active_escalation(
            &self,
            api_key_id: Uuid,
        ) -> Result<Option<OperatorEscalation>, String> {
            match &self.escalations {
                Some(s) => s
                    .active_for_api_key(api_key_id)
                    .await
                    .map_err(|e| e.to_string()),
                None => Ok(None),
            }
        }
    }

    /// An active row for `key`, created by `user` in `tenant`, with
    /// `aegis_role` stored when the creator was an operator identity.
    pub(crate) fn key_row(
        key: &str,
        user: &str,
        tenant: &str,
        aegis_role: Option<&str>,
    ) -> ApiKeyRow {
        ApiKeyRow {
            id: Uuid::new_v4(),
            user_id: user.to_string(),
            name: "mcp".to_string(),
            key_hash: hash_key(key),
            scopes: Vec::new(),
            expires_at: None,
            last_used_at: None,
            created_at: chrono::Utc::now(),
            status: "active".to_string(),
            tenant_id: tenant.to_string(),
            aegis_role: aegis_role.map(str::to_string),
            zaru_tier: if aegis_role.is_some() {
                None
            } else {
                Some("pro".to_string())
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_keys::{key_row, KeyTable};
    use super::*;
    use aegis_orchestrator_core::application::operator_escalation_service::RedeemingKey;
    use aegis_orchestrator_core::domain::node_config::OperatorEscalationConfig;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_operator_escalation::InMemoryOperatorEscalationRepository;

    const CONSUMER_KEY: &str = "aegis_consumer-key";
    const ROLE_KEY: &str = "aegis_role-key";
    const CONSUMER_SUB: &str = "0f1e2d3c-consumer";

    fn service() -> Arc<OperatorEscalationService> {
        Arc::new(OperatorEscalationService::new(
            Arc::new(InMemoryOperatorEscalationRepository::new()),
            OperatorEscalationConfig::default(),
        ))
    }

    fn tenant() -> String {
        TenantId::for_consumer_user(CONSUMER_SUB)
            .unwrap()
            .as_str()
            .to_string()
    }

    /// AEGIS ADR-129 D14: while the key holds an active escalation it
    /// resolves to `Operator` with the escalation's role, keeping its id and
    /// home tenant beside the identity (the Update's U1); without one it is
    /// the consumer it was created as.
    #[tokio::test]
    async fn consumer_key_resolves_operator_only_while_escalated() {
        let escalations = service();
        let row = key_row(CONSUMER_KEY, CONSUMER_SUB, &tenant(), None);
        let table = KeyTable {
            rows: vec![row.clone()],
            escalations: Some(escalations.clone()),
        };
        let before = resolve_api_key(Some(&table), CONSUMER_KEY).await.unwrap();
        assert!(matches!(
            before.identity.identity_kind,
            IdentityKind::ConsumerUser { .. }
        ));
        assert!(before.escalation.is_none());

        let minted = escalations
            .mint("system-sub", CONSUMER_SUB, AegisRole::Operator)
            .await
            .unwrap();
        escalations
            .redeem(
                &RedeemingKey {
                    api_key_id: row.id,
                    user_id: CONSUMER_SUB.to_string(),
                    has_stored_role: false,
                },
                &minted.code,
            )
            .await
            .unwrap();
        let during = resolve_api_key(Some(&table), CONSUMER_KEY).await.unwrap();
        assert_eq!(
            during.identity.identity_kind,
            IdentityKind::Operator {
                aegis_role: AegisRole::Operator
            }
        );
        assert_eq!(during.identity.realm_slug, "aegis-system");
        assert_eq!(during.api_key_id, row.id);
        assert_eq!(during.home_tenant, tenant());

        escalations
            .end_for_api_key(
                row.id,
                aegis_orchestrator_core::domain::operator_escalation::EscalationEndReason::AgentRelease,
            )
            .await
            .unwrap();
        let after = identity_from_api_key(Some(&table), CONSUMER_KEY)
            .await
            .unwrap();
        assert!(matches!(
            after.identity_kind,
            IdentityKind::ConsumerUser { .. }
        ));
    }

    /// AEGIS ADR-129 D15: a role-bearing key resolves as it always has.
    #[tokio::test]
    async fn role_bearing_key_still_resolves_operator() {
        let table = KeyTable {
            rows: vec![key_row(
                ROLE_KEY,
                "system-sub",
                "system",
                Some("aegis:admin"),
            )],
            escalations: Some(service()),
        };
        let resolved = resolve_api_key(Some(&table), ROLE_KEY).await.unwrap();
        assert_eq!(
            resolved.identity.identity_kind,
            IdentityKind::Operator {
                aegis_role: AegisRole::Admin
            }
        );
        assert!(resolved.has_stored_role);
        assert!(resolved.escalation.is_none());
    }
}
