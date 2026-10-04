// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Fixtures for handler tests that drive routes through the daemon's real
//! request-authentication stack (`router::apply_request_auth_layers`):
//! `iam_auth_middleware` resolving a bearer token to a `UserIdentity` and its
//! JWT scopes, then `tenant_context_middleware` resolving the tenant, served
//! on a loopback listener and called over HTTP.

use std::collections::HashMap;
use std::sync::Arc;

use aegis_orchestrator_core::domain::iam::{
    AegisRole, IamError, IdentityKind, IdentityProvider, IdentityRealm, UserIdentity,
    ValidatedIdentityToken, ZaruTier,
};
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use aegis_orchestrator_core::domain::team::MembershipRepository;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::presentation::tenant_middleware::TenantMiddlewareState;
use axum::Router;

use crate::daemon::router::apply_request_auth_layers;

/// Resolves a bearer token to a fixed identity, `scope` claim and any further
/// raw claims (`azp`, `consumer_sub`), the way `StandardIamService` resolves
/// a validated JWT. Unknown tokens fail validation.
pub(crate) struct TokenTableIdentityProvider {
    entries: HashMap<String, (UserIdentity, String, serde_json::Value)>,
}

#[async_trait::async_trait]
impl IdentityProvider for TokenTableIdentityProvider {
    async fn validate_token(&self, raw_jwt: &str) -> Result<ValidatedIdentityToken, IamError> {
        let (identity, scopes, extra) =
            self.entries
                .get(raw_jwt)
                .cloned()
                .ok_or_else(|| IamError::MissingClaim {
                    claim: "sub".to_string(),
                })?;
        let mut raw_claims = serde_json::json!({ "scope": scopes });
        if let (Some(raw), Some(extra)) = (raw_claims.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                raw.insert(k.clone(), v.clone());
            }
        }
        Ok(ValidatedIdentityToken {
            identity,
            issued_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
            raw_claims,
        })
    }
    fn resolve_tier(&self, _token: &ValidatedIdentityToken) -> Result<ZaruTier, IamError> {
        Ok(ZaruTier::Free)
    }
    fn resolve_role(&self, _token: &ValidatedIdentityToken) -> Result<AegisRole, IamError> {
        Err(IamError::MissingClaim {
            claim: "aegis_role".to_string(),
        })
    }
    fn known_realms(&self) -> Vec<IdentityRealm> {
        Vec::new()
    }
}

/// A provider for `(bearer token, identity, space-separated scopes)` rows.
pub(crate) fn identity_provider(
    entries: &[(&str, UserIdentity, &str)],
) -> Arc<dyn IdentityProvider> {
    Arc::new(TokenTableIdentityProvider {
        entries: entries
            .iter()
            .map(|(token, id, scopes)| {
                (
                    token.to_string(),
                    (id.clone(), scopes.to_string(), serde_json::Value::Null),
                )
            })
            .collect(),
    })
}

/// A provider for `(bearer token, identity, space-separated scopes, further
/// raw claims)` rows: the claims a real token carries beside its identity,
/// such as `azp` and `consumer_sub` (AEGIS ADR-129 D13).
pub(crate) fn identity_provider_with_claims(
    entries: &[(&str, UserIdentity, &str, serde_json::Value)],
) -> Arc<dyn IdentityProvider> {
    Arc::new(TokenTableIdentityProvider {
        entries: entries
            .iter()
            .map(|(token, id, scopes, extra)| {
                (
                    token.to_string(),
                    (id.clone(), scopes.to_string(), extra.clone()),
                )
            })
            .collect(),
    })
}

pub(crate) fn operator(role: AegisRole) -> UserIdentity {
    UserIdentity {
        sub: format!("op-{}", role.as_claim_str()),
        realm_slug: "aegis-system".into(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::Operator { aegis_role: role },
    }
}

/// A Free-tier consumer whose per-user tenant is `u-{sub without dashes}`.
pub(crate) fn consumer(sub: &str) -> UserIdentity {
    UserIdentity {
        sub: sub.into(),
        realm_slug: "zaru-consumer".into(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Free,
            tenant_id: TenantId::for_consumer_user(sub).expect("per-user tenant id"),
        },
    }
}

pub(crate) fn tenant_user(sub: &str, tenant_slug: &str) -> UserIdentity {
    UserIdentity {
        sub: sub.into(),
        realm_slug: format!("tenant-{tenant_slug}"),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::TenantUser {
            tenant_slug: tenant_slug.into(),
        },
    }
}

pub(crate) fn service_account() -> UserIdentity {
    UserIdentity {
        sub: "svc-sub".into(),
        realm_slug: "aegis-system".into(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ServiceAccount {
            client_id: "aegis-temporal-worker".into(),
        },
    }
}

/// Serve `router` beneath the daemon's authentication stack on a loopback
/// port and return its base URL. `iam` of `None` reproduces a node
/// configured without `spec.iam`, where no authentication layer is mounted.
pub(crate) async fn serve(
    router: Router,
    iam: Option<Arc<dyn IdentityProvider>>,
    membership_repo: Option<Arc<dyn MembershipRepository>>,
) -> String {
    let app = apply_request_auth_layers(
        router,
        TenantMiddlewareState {
            team_repo: None,
            membership_repo,
            event_bus: Arc::new(EventBus::new(16)),
        },
        iam,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve router under test");
    });
    format!("http://{addr}")
}

/// Send one request and return its status and JSON body (`Null` when the
/// body is not JSON).
pub(crate) async fn send(
    base: &str,
    method: &reqwest::Method,
    path: &str,
    body: &Option<serde_json::Value>,
    bearer: Option<&str>,
) -> (u16, serde_json::Value) {
    let mut req = reqwest::Client::new().request(method.clone(), format!("{base}{path}"));
    if let Some(token) = bearer {
        req = req.bearer_auth(token);
    }
    if let Some(json) = body {
        req = req.json(json);
    }
    let resp = req.send().await.expect("loopback request");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}
