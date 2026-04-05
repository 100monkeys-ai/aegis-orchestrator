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
            tier.clone(),
            keycloak_realm.clone(),
            openbao_namespace,
        );
        tenant.quotas = TenantQuotas::for_tier(&tier);

        self.tenant_repo.insert(&tenant).await?;

        // Set tenant_id attribute on Keycloak user
        self.keycloak_admin
            .set_user_attribute("zaru-consumer", user_sub, "tenant_id", &slug)
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
