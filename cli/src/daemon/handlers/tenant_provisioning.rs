// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Keycloak Webhook Handler — Tenant Provisioning (ADR-097)
//!
//! Handles Keycloak Events SPI webhooks. On `REGISTER` events, provisions a
//! per-user tenant via [`TenantProvisioningService`].

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use std::sync::Arc;

use crate::daemon::state::AppState;

/// Keycloak Events SPI webhook payload (ADR-097).
#[derive(Debug, Deserialize)]
pub(crate) struct KeycloakEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "realmId")]
    pub realm_id: String,
}

/// Handle Keycloak event webhooks. Provisions a per-user tenant on REGISTER events.
pub(crate) async fn keycloak_event_handler(
    State(state): State<Arc<AppState>>,
    Json(event): Json<KeycloakEvent>,
) -> impl IntoResponse {
    if event.event_type != "REGISTER" {
        return StatusCode::OK;
    }

    let provisioning_service = match &state.tenant_provisioning_service {
        Some(svc) => svc,
        None => {
            tracing::warn!(
                user_sub = %event.user_id,
                "Keycloak REGISTER event received but tenant provisioning is not configured"
            );
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    };

    match provisioning_service
        .provision_user_tenant(
            &event.user_id,
            &aegis_orchestrator_core::domain::iam::ZaruTier::Free,
        )
        .await
    {
        Ok(tenant) => {
            tracing::info!(
                tenant_slug = %tenant.slug,
                user_sub = %event.user_id,
                "Per-user tenant provisioned"
            );
            StatusCode::CREATED
        }
        Err(e) => {
            tracing::error!(
                user_sub = %event.user_id,
                error = %e,
                "Per-user tenant provisioning failed"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
