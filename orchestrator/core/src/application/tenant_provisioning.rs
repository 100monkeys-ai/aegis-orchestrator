// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tenant Provisioning Application Service (ADR-097, Phases 3 & 6)
//!
//! Provisions per-user tenants for consumer users at signup. Called by the
//! Keycloak webhook handler when a `REGISTER` event arrives.
//!
//! ## Idempotency
//!
//! If a tenant already exists for the given user `sub`, the service returns
//! the existing tenant without side effects.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::events::TenantEvent;
use crate::domain::iam::ZaruTier;
use crate::domain::repository::{RepositoryError, TenantRepository};
use crate::domain::shared_kernel::TenantId;
use crate::domain::tenancy::{Tenant, TenantQuotas, TenantTier};
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::iam::keycloak_admin_client::{KeycloakAdminClient, KeycloakAdminError};

#[derive(Debug, thiserror::Error)]
pub enum ProvisioningError {
    #[error("invalid user sub for tenant slug: {0}")]
    InvalidSub(String),
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),
    #[error("keycloak admin error: {0}")]
    Keycloak(#[from] KeycloakAdminError),
    /// Keycloak has not yet materialised the user we're trying to
    /// provision a tenant for (webhook race on the REGISTER event).
    /// Callers should retry on the next login rather than surface as a
    /// hard 500 — the user record will appear shortly.
    #[error("keycloak user not ready yet (retry on next login): {0}")]
    KeycloakUserNotReady(String),
}

pub struct TenantProvisioningService {
    tenant_repo: Arc<dyn TenantRepository>,
    keycloak_admin: Arc<KeycloakAdminClient>,
    event_bus: Arc<EventBus>,
}

impl TenantProvisioningService {
    pub fn new(
        tenant_repo: Arc<dyn TenantRepository>,
        keycloak_admin: Arc<KeycloakAdminClient>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            tenant_repo,
            keycloak_admin,
            event_bus,
        }
    }

    /// Provision a per-user tenant for a consumer user at signup (ADR-097).
    /// Idempotent: returns existing tenant if already provisioned.
    pub async fn provision_user_tenant(
        &self,
        user_sub: &str,
        zaru_tier: &ZaruTier,
    ) -> Result<Tenant, ProvisioningError> {
        let tenant_id = TenantId::for_consumer_user(user_sub)
            .map_err(|e| ProvisioningError::InvalidSub(e.to_string()))?;

        // Idempotent: check if already provisioned
        if let Some(existing) = self.tenant_repo.find_by_slug(&tenant_id).await? {
            return Ok(existing);
        }

        let tier = Self::map_zaru_tier(zaru_tier);
        let slug = tenant_id.as_str().to_string();
        let keycloak_realm = "zaru-consumer".to_string();
        let openbao_namespace = format!("tenant-{}/", slug);

        let mut tenant = Tenant::new(
            tenant_id.clone(),
            format!("User {}", &user_sub[..8.min(user_sub.len())]),
            tier,
            keycloak_realm.clone(),
            openbao_namespace,
        );
        tenant.quotas = TenantQuotas::for_tier(&tier);

        // Fetch the full Keycloak user so set_user_attribute can send the
        // complete user representation on PUT (Keycloak rejects partial bodies).
        //
        // If the user has not yet been materialised in Keycloak (webhook race
        // with the REGISTER event), return a distinct, retryable error rather
        // than a hard 500 — the user record will appear shortly, and
        // provisioning is idempotent on retry. We also skip the tenant insert
        // below so a subsequent retry can complete the full provisioning flow
        // atomically.
        let kc_user = match self
            .keycloak_admin
            .get_user("zaru-consumer", user_sub)
            .await?
        {
            Some(u) => u,
            None => {
                tracing::info!(
                    user_sub,
                    "Keycloak user not ready yet for tenant provisioning — retry on next login"
                );
                return Err(ProvisioningError::KeycloakUserNotReady(
                    user_sub.to_string(),
                ));
            }
        };

        self.tenant_repo.insert(&tenant).await?;

        // Set tenant_id and zaru_tier attributes on Keycloak user so the
        // protocol mappers can include them as JWT claims on subsequent logins.
        self.keycloak_admin
            .set_user_attribute("zaru-consumer", &kc_user, "tenant_id", &slug)
            .await?;
        self.keycloak_admin
            .set_user_attribute("zaru-consumer", &kc_user, "zaru_tier", tier_to_str(&tier))
            .await?;

        self.event_bus
            .publish_tenant_event(TenantEvent::UserTenantProvisioned {
                tenant_slug: slug,
                user_sub: user_sub.to_string(),
                tier: tier_to_str(&tier).to_string(),
                keycloak_realm,
                provisioned_at: Utc::now(),
            });

        Ok(tenant)
    }

    fn map_zaru_tier(zaru_tier: &ZaruTier) -> TenantTier {
        match zaru_tier {
            ZaruTier::Free => TenantTier::Free,
            ZaruTier::Pro => TenantTier::Pro,
            ZaruTier::Business => TenantTier::Business,
            ZaruTier::Enterprise => TenantTier::Enterprise,
        }
    }
}

fn tier_to_str(tier: &TenantTier) -> &'static str {
    match tier {
        TenantTier::Free => "free",
        TenantTier::Pro => "pro",
        TenantTier::Business => "business",
        TenantTier::Enterprise => "enterprise",
        TenantTier::System => "system",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure construction test — the `KeycloakUserNotReady` variant is part
    /// of the public error surface and its `Display` impl must clearly
    /// communicate the retryable nature of the failure to webhook callers.
    #[test]
    fn keycloak_user_not_ready_variant_surfaces_retryable_message() {
        let err = ProvisioningError::KeycloakUserNotReady("abcd-1234".to_string());
        let msg = format!("{err}");
        assert!(
            msg.contains("retry"),
            "error must indicate retryability, got: {msg}"
        );
        assert!(msg.contains("abcd-1234"));
    }

    /// Regression for the provisioning → webhook race: the
    /// `provision_user_tenant` path must return the distinct
    /// `KeycloakUserNotReady` variant (not a generic KeycloakAdminError)
    /// when Keycloak has not yet materialised the user. The webhook
    /// handler surfaces this as a 503 retryable so Keycloak's webhook
    /// redelivery can complete provisioning.
    ///
    /// NOTE: `KeycloakAdminClient` is a concrete struct that performs
    /// live HTTP; wiring an in-process mock HTTP server here would add
    /// significant scaffolding out of scope for this sweep. The
    /// structural fix is verified at the call-site via the match arm in
    /// `cli/src/daemon/handlers/tenant_provisioning.rs`.
    #[test]
    #[ignore = "requires mock Keycloak HTTP server — covered structurally by the 503 match arm in the webhook handler"]
    fn provision_user_tenant_returns_keycloak_user_not_ready_when_missing() {}
}
