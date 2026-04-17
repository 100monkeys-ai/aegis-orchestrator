// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Billing Application Service (ADR-111)
//!
//! Trait boundary for Stripe-integrated billing operations used by the
//! team-tenancy workflow (see [`team_service`](super::team_service)) to keep
//! the Stripe subscription quantity aligned with the active membership count.
//!
//! ## Layering
//!
//! The trait lives in `aegis-orchestrator-core` so `team_service` can depend
//! on it without pulling the Stripe SDK into core. The concrete
//! `StripeBillingService` implementation lives in the CLI crate alongside the
//! existing handler-level Stripe integration, keeping the `async-stripe`
//! dependency confined to the binary.
//!
//! ## Scope
//!
//! This service is **additive** — the existing `cli/src/daemon/handlers/billing.rs`
//! handlers remain the consumer-facing Stripe entry point for checkout,
//! portal, and self-service seat adjustments. [`BillingService`] is the
//! internal primitive that team orchestration calls to create a team
//! Customer, persist the `TenantSubscription` row, and reprorate seats when
//! membership changes.

use async_trait::async_trait;

use crate::domain::team::TeamId;
use crate::domain::tenancy::TenantTier;
use crate::domain::tenant::TenantId;

// ============================================================================
// Errors
// ============================================================================

/// Error type returned by [`BillingService`] methods (ADR-111 §Billing Model).
#[derive(Debug, thiserror::Error)]
pub enum BillingServiceError {
    /// Stripe integration is not configured on this node. Callers should map
    /// this to HTTP 501 Not Implemented to match the existing
    /// `cli/src/daemon/handlers/billing.rs` contract.
    #[error("billing is not configured")]
    NotConfigured,
    /// The subscription for this tenant does not exist. Team seat sync requires
    /// a prior call to [`BillingService::provision_team_customer`].
    #[error("no subscription for tenant: {0}")]
    NoSubscription(String),
    /// The persisted subscription has no `stripe_subscription_id` — the tenant
    /// has a Stripe Customer but has not yet completed checkout. Seat sync is
    /// a no-op in this state.
    #[error("tenant has no active stripe subscription")]
    NoActiveSubscription,
    /// Stripe SDK returned an error.
    #[error("stripe error: {0}")]
    Stripe(String),
    /// Repository-layer error.
    #[error("repository error: {0}")]
    Repository(String),
    /// Invalid Stripe customer or subscription id encountered when talking to
    /// Stripe.
    #[error("invalid stripe id: {0}")]
    InvalidStripeId(String),
}

// ============================================================================
// Service Trait
// ============================================================================

/// Application service for Stripe-backed billing operations used by
/// team-tenancy orchestration (ADR-111 §Billing Model).
#[async_trait]
pub trait BillingService: Send + Sync {
    /// Set the seat quantity on a team's Stripe subscription to
    /// `active_member_count` and persist the new `seat_count` on the
    /// [`TenantSubscription`](crate::domain::billing::TenantSubscription) row.
    ///
    /// Returns the previous `seat_count` so the caller can publish a
    /// `SeatCountChanged` event when the value actually changed.
    async fn sync_seats(
        &self,
        team_id: TeamId,
        tenant_id: &TenantId,
        active_member_count: u32,
    ) -> Result<u32, BillingServiceError>;

    /// Create a Stripe Customer for a new team tenant and insert a fresh
    /// [`TenantSubscription`](crate::domain::billing::TenantSubscription) row
    /// with `seat_count = 1`. Returns the newly minted `stripe_customer_id`.
    ///
    /// The subscription starts in
    /// [`SubscriptionStatus::None`](crate::domain::billing::SubscriptionStatus::None)
    /// with no `stripe_subscription_id` — the owner must complete the Stripe
    /// Checkout flow to attach a paid subscription to the Customer.
    async fn provision_team_customer(
        &self,
        team_id: TeamId,
        owner_email: String,
        tenant_id: &TenantId,
        tier: TenantTier,
    ) -> Result<String, BillingServiceError>;

    /// Cancel the Stripe subscription attached to a team tenant (if any) and
    /// transition the persisted subscription to
    /// [`SubscriptionStatus::Canceled`](crate::domain::billing::SubscriptionStatus::Canceled).
    /// Idempotent — calling on a tenant with no active subscription is a no-op.
    async fn cancel_team_subscription(
        &self,
        tenant_id: &TenantId,
    ) -> Result<(), BillingServiceError>;
}
