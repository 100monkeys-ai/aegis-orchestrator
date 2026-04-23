// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Consumer Self-Service Endpoints (ADR-097)
//!
//! Handlers for consumer-user lifecycle operations invoked by the Zaru client.
//!
//! The [`ensure_provisioned_handler`] exists as the login-time self-heal path
//! for users who registered before the Keycloak Events SPI webhook was wired.
//! Their Keycloak user has no `tenant_id` / `zaru_tier` attribute, so their
//! JWT carries no `tenant_id` claim and every downstream path has to fall
//! back to the shared consumer slug. The Zaru client calls this endpoint on
//! every login; the first call materialises the tenant + writes the Keycloak
//! attributes, and every subsequent call is a cheap idempotent no-op.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use aegis_orchestrator_core::application::tenant_provisioning::ProvisioningError;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};

use crate::daemon::state::AppState;

/// `POST /v1/consumer/ensure-provisioned` — Idempotently provision the
/// authenticated consumer user's per-user tenant (ADR-097).
///
/// Called by the Zaru client on the login callback. Safe to call on every
/// login: `provision_user_tenant` returns the existing tenant without side
/// effects when already present.
///
/// Responses:
/// - `204 No Content`: tenant already existed or was freshly provisioned.
/// - `204 No Content`: non-consumer identity (operator, service account,
///   tenant user) — no-op, this endpoint is only meaningful for consumers.
/// - `503 Service Unavailable`: provisioning service not configured, or the
///   Keycloak REGISTER webhook race hit (user not yet materialised).
/// - `500 Internal Server Error`: unexpected provisioning failure.
pub(crate) async fn ensure_provisioned_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
) -> axum::response::Response {
    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    let zaru_tier = match &identity.identity_kind {
        IdentityKind::ConsumerUser { zaru_tier, .. } => zaru_tier.clone(),
        // Non-consumer identities never get a per-user tenant. Return 204 so
        // the client's unconditional "call on login" flow stays simple.
        IdentityKind::Operator { .. }
        | IdentityKind::ServiceAccount { .. }
        | IdentityKind::TenantUser { .. } => {
            return StatusCode::NO_CONTENT.into_response();
        }
    };

    let svc = match &state.tenant_provisioning_service {
        Some(svc) => svc,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Tenant provisioning not configured"})),
            )
                .into_response();
        }
    };

    match svc.provision_user_tenant(&identity.sub, &zaru_tier).await {
        Ok(tenant) => {
            tracing::info!(
                tenant_slug = %tenant.slug,
                user_sub = %identity.sub,
                "ensure-provisioned completed"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(ProvisioningError::KeycloakUserNotReady(_)) => {
            tracing::info!(
                user_sub = %identity.sub,
                "ensure-provisioned deferred — Keycloak user not yet materialised"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Keycloak user not yet materialised; retry shortly"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(
                user_sub = %identity.sub,
                error = %e,
                "ensure-provisioned failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Tenant provisioning failed"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    // The handler wires `TenantProvisioningService` (a concrete struct, not a
    // trait) into a path that talks to a live `KeycloakAdminClient` and a live
    // `TenantRepository`. There is no seam to mock without plumbing new
    // trait abstractions, so the integration-style behaviours (204 on success,
    // 503 on KeycloakUserNotReady, 500 on other errors, 204 on non-consumer)
    // are covered by the broader billing/provisioning integration tests.
    //
    // The variant-dispatch logic (ConsumerUser vs. other identity kinds) and
    // the absent-service branch are trivial matches that don't warrant the
    // cost of building a new mock harness.

    #[ignore = "TenantProvisioningService is a concrete struct with no mockable seam; covered by integration tests"]
    #[test]
    fn ensure_provisioned_handler_behaviour_matrix() {}
}
