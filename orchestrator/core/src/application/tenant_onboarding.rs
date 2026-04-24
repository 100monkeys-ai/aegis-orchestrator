// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # TenantOnboardingService (ADR-056 §Tenant Lifecycle)
//!
//! Provisions enterprise tenants with a 5-step atomic sequence:
//! 1. Validate slug uniqueness
//! 2. Create Keycloak realm
//! 3. Create OpenBao namespace
//! 4. Insert into tenants table
//! 5. Publish TenantProvisioned domain event
//!
//! Steps 2–4 include compensating rollback actions on failure so that no
//! partially-provisioned state is left behind.
//!
//! This is distinct from [`crate::application::tenant_provisioning::TenantProvisioningService`],
//! which handles per-user consumer tenants at signup (ADR-097).

use std::sync::Arc;

use chrono::Utc;
use tracing::warn;

use crate::domain::events::TenantEvent;
use crate::domain::repository::{RepositoryError, TenantRepository};
use crate::domain::tenancy::{Tenant, TenantQuotas, TenantTier};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::iam::keycloak_admin_client::{KeycloakAdminClient, KeycloakAdminError};
use crate::infrastructure::secrets_manager::SecretsManager;

#[derive(Debug, thiserror::Error)]
pub enum TenantOnboardingError {
    #[error("invalid slug: {0}")]
    SlugInvalid(String),
    #[error("tenant slug already exists: {0}")]
    SlugConflict(String),
    #[error("keycloak error: {0}")]
    Keycloak(#[from] KeycloakAdminError),
    #[error("openbao error: {0}")]
    OpenBao(String),
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),
}

pub struct TenantOnboardingService {
    tenant_repo: Arc<dyn TenantRepository>,
    keycloak_admin: Arc<KeycloakAdminClient>,
    secrets_manager: Arc<SecretsManager>,
    event_bus: Arc<EventBus>,
}

impl TenantOnboardingService {
    pub fn new(
        tenant_repo: Arc<dyn TenantRepository>,
        keycloak_admin: Arc<KeycloakAdminClient>,
        secrets_manager: Arc<SecretsManager>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            tenant_repo,
            keycloak_admin,
            secrets_manager,
            event_bus,
        }
    }

    /// Provision a new enterprise tenant with 5-step atomic sequence (ADR-056 §Tenant Lifecycle).
    ///
    /// Returns the fully-provisioned `Tenant` aggregate on success.
    /// On failure after partial completion, compensating actions are executed to
    /// ensure no half-provisioned resources are left behind.
    pub async fn provision(
        &self,
        slug: TenantId,
        display_name: String,
        tier: TenantTier,
        quotas: TenantQuotas,
    ) -> Result<Tenant, TenantOnboardingError> {
        // Step 1: Validate slug — check for conflicts in the tenants table.
        if self.tenant_repo.find_by_slug(&slug).await?.is_some() {
            return Err(TenantOnboardingError::SlugConflict(
                slug.as_str().to_string(),
            ));
        }

        let slug_str = slug.as_str().to_string();
        let keycloak_realm = format!("tenant-{}", slug_str);
        let openbao_namespace = format!("tenant-{}/", slug_str);

        // Step 2: Create Keycloak realm.
        self.keycloak_admin.create_realm(&keycloak_realm).await?;

        // Step 3: Create OpenBao namespace.
        if let Err(e) = self
            .secrets_manager
            .create_namespace(&format!("tenant-{}", slug_str))
            .await
        {
            // Rollback: delete Keycloak realm.
            if let Err(rollback_err) = self.keycloak_admin.delete_realm(&keycloak_realm).await {
                warn!(
                    slug = %slug_str,
                    error = %rollback_err,
                    "Failed to rollback Keycloak realm after OpenBao namespace creation failure"
                );
            }
            return Err(TenantOnboardingError::OpenBao(e.to_string()));
        }

        // Step 4: Insert into tenants table. `tier` and `quotas` are retained
        // on the command signature for callers (and as inputs to the provisioning
        // event) but are NOT persisted on the tenants row — tier lives
        // exclusively on `tenant_subscriptions.tier` and quotas derive from it
        // dynamically via `TenantQuotas::for_tier`.
        let tenant = Tenant::new(
            slug.clone(),
            display_name,
            keycloak_realm.clone(),
            openbao_namespace.clone(),
        );
        let _ = quotas; // intentionally dropped — see doc comment above.

        if let Err(e) = self.tenant_repo.insert(&tenant).await {
            // Rollback: delete OpenBao namespace and Keycloak realm.
            if let Err(rollback_err) = self
                .secrets_manager
                .delete_namespace(&format!("tenant-{}", slug_str))
                .await
            {
                warn!(
                    slug = %slug_str,
                    error = %rollback_err,
                    "Failed to rollback OpenBao namespace after DB insert failure"
                );
            }
            if let Err(rollback_err) = self.keycloak_admin.delete_realm(&keycloak_realm).await {
                warn!(
                    slug = %slug_str,
                    error = %rollback_err,
                    "Failed to rollback Keycloak realm after DB insert failure"
                );
            }
            return Err(TenantOnboardingError::Repository(e));
        }

        // Step 5: Publish TenantProvisioned event (best-effort; failure is logged, not fatal).
        // Tier is sourced from the caller's `tier` argument — it is no longer
        // persisted on the tenants row.
        self.event_bus
            .publish_tenant_event(TenantEvent::TenantProvisioned {
                tenant_slug: slug_str.clone(),
                tier: tier.as_keycloak_str().to_string(),
                keycloak_realm,
                openbao_namespace,
                provisioned_at: Utc::now(),
            });

        Ok(tenant)
    }
}
