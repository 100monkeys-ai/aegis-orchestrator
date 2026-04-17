// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Stripe-backed Billing Service (ADR-111)
//!
//! Production implementation of
//! [`BillingService`](aegis_orchestrator_core::application::billing_service::BillingService)
//! wired to the `async-stripe` client and the `BillingRepository`. Lives in
//! the CLI crate to keep the Stripe SDK dependency confined to the binary.

use std::sync::Arc;

use aegis_orchestrator_core::application::billing_service::{BillingService, BillingServiceError};
use aegis_orchestrator_core::domain::billing::{SubscriptionStatus, TenantSubscription};
use aegis_orchestrator_core::domain::node_config::{resolve_env_value, BillingConfig};
use aegis_orchestrator_core::domain::team::TeamId;
use aegis_orchestrator_core::domain::tenancy::TenantTier;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::BillingRepository;
use async_trait::async_trait;
use chrono::Utc;
use tracing::{info, warn};

/// Stripe-backed implementation of
/// [`BillingService`](aegis_orchestrator_core::application::billing_service::BillingService)
/// (ADR-111).
pub struct StripeBillingService {
    config: BillingConfig,
    subscription_repo: Arc<dyn BillingRepository>,
}

impl StripeBillingService {
    pub fn new(config: BillingConfig, subscription_repo: Arc<dyn BillingRepository>) -> Self {
        Self {
            config,
            subscription_repo,
        }
    }

    fn stripe_client(&self) -> Result<stripe::Client, BillingServiceError> {
        let key = resolve_env_value(&self.config.stripe_secret_key)
            .map_err(|_| BillingServiceError::NotConfigured)?;
        if key.is_empty() {
            return Err(BillingServiceError::NotConfigured);
        }
        Ok(stripe::Client::new(key))
    }
}

#[async_trait]
impl BillingService for StripeBillingService {
    async fn sync_seats(
        &self,
        _team_id: TeamId,
        tenant_id: &TenantId,
        active_member_count: u32,
    ) -> Result<u32, BillingServiceError> {
        let stripe_client = self.stripe_client()?;

        let mut sub = self
            .subscription_repo
            .get_subscription(tenant_id)
            .await
            .map_err(|e| BillingServiceError::Repository(e.to_string()))?
            .ok_or_else(|| BillingServiceError::NoSubscription(tenant_id.as_str().to_string()))?;

        let previous_count = sub.seat_count;

        let stripe_sub_id = match &sub.stripe_subscription_id {
            Some(id) => id.clone(),
            None => return Err(BillingServiceError::NoActiveSubscription),
        };
        let parsed_sub_id: stripe::SubscriptionId =
            stripe_sub_id.parse().map_err(|e: stripe::ParseIdError| {
                BillingServiceError::InvalidStripeId(e.to_string())
            })?;

        // Retrieve the subscription so we can target the first line item by
        // id — Stripe requires item-level updates to reference the item id.
        let remote = stripe::Subscription::retrieve(&stripe_client, &parsed_sub_id, &[])
            .await
            .map_err(|e| BillingServiceError::Stripe(e.to_string()))?;

        let item =
            remote.items.data.first().ok_or_else(|| {
                BillingServiceError::Stripe("subscription has no items".to_string())
            })?;

        let update = stripe::UpdateSubscription {
            items: Some(vec![stripe::UpdateSubscriptionItems {
                id: Some(item.id.to_string()),
                quantity: Some(active_member_count as u64),
                ..Default::default()
            }]),
            ..Default::default()
        };

        stripe::Subscription::update(&stripe_client, &parsed_sub_id, update)
            .await
            .map_err(|e| BillingServiceError::Stripe(e.to_string()))?;

        sub.seat_count = active_member_count;
        sub.updated_at = Utc::now();
        self.subscription_repo
            .upsert_subscription(&sub)
            .await
            .map_err(|e| BillingServiceError::Repository(e.to_string()))?;

        info!(
            tenant_id = %tenant_id,
            previous_count,
            new_count = active_member_count,
            "Team seat count synchronized with Stripe"
        );

        Ok(previous_count)
    }

    async fn provision_team_customer(
        &self,
        team_id: TeamId,
        owner_email: String,
        tenant_id: &TenantId,
        tier: TenantTier,
    ) -> Result<String, BillingServiceError> {
        let stripe_client = self.stripe_client()?;

        let mut params = stripe::CreateCustomer::new();
        params.email = Some(&owner_email);
        let metadata: std::collections::HashMap<String, String> = [
            ("tenant_id".to_string(), tenant_id.as_str().to_string()),
            ("team_id".to_string(), team_id.to_string()),
            ("tenant_kind".to_string(), "team".to_string()),
        ]
        .into_iter()
        .collect();
        params.metadata = Some(metadata);

        let customer = stripe::Customer::create(&stripe_client, params)
            .await
            .map_err(|e| BillingServiceError::Stripe(e.to_string()))?;
        let customer_id = customer.id.to_string();

        let now = Utc::now();
        let sub = TenantSubscription {
            tenant_id: tenant_id.clone(),
            stripe_customer_id: customer_id.clone(),
            stripe_subscription_id: None,
            tier,
            status: SubscriptionStatus::None,
            current_period_end: None,
            cancel_at_period_end: false,
            created_at: now,
            updated_at: now,
            seat_count: 1,
        };
        self.subscription_repo
            .upsert_subscription(&sub)
            .await
            .map_err(|e| BillingServiceError::Repository(e.to_string()))?;

        info!(
            tenant_id = %tenant_id,
            team_id = %team_id,
            stripe_customer_id = %customer_id,
            "Provisioned team Stripe customer"
        );

        Ok(customer_id)
    }

    async fn cancel_team_subscription(
        &self,
        tenant_id: &TenantId,
    ) -> Result<(), BillingServiceError> {
        let stripe_client = self.stripe_client()?;

        let mut sub = match self
            .subscription_repo
            .get_subscription(tenant_id)
            .await
            .map_err(|e| BillingServiceError::Repository(e.to_string()))?
        {
            Some(s) => s,
            None => return Ok(()),
        };

        if let Some(stripe_sub_id) = sub.stripe_subscription_id.clone() {
            let parsed: stripe::SubscriptionId =
                stripe_sub_id.parse().map_err(|e: stripe::ParseIdError| {
                    BillingServiceError::InvalidStripeId(e.to_string())
                })?;
            if let Err(e) = stripe::Subscription::cancel(
                &stripe_client,
                &parsed,
                stripe::CancelSubscription::default(),
            )
            .await
            {
                warn!(error = %e, "Failed to cancel Stripe subscription");
                return Err(BillingServiceError::Stripe(e.to_string()));
            }
        }

        sub.status = SubscriptionStatus::Canceled;
        sub.updated_at = Utc::now();
        self.subscription_repo
            .upsert_subscription(&sub)
            .await
            .map_err(|e| BillingServiceError::Repository(e.to_string()))?;
        Ok(())
    }
}
