// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Billing Domain Model
//!
//! Represents a tenant's Stripe subscription state. This domain entity bridges
//! Stripe's subscription lifecycle with the AEGIS tenant tier system.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::tenancy::TenantTier;
use crate::domain::tenant::TenantId;

/// Persistent record linking a tenant to their Stripe subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantSubscription {
    pub tenant_id: TenantId,
    pub stripe_customer_id: String,
    pub stripe_subscription_id: Option<String>,
    pub tier: TenantTier,
    pub status: SubscriptionStatus,
    pub current_period_end: Option<DateTime<Utc>>,
    pub cancel_at_period_end: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Number of paid seats on this subscription (ADR-111).
    ///
    /// For consumer (per-user) subscriptions this is always `1`. For
    /// [`TenantKind::Team`](crate::domain::tenancy::TenantKind::Team)
    /// subscriptions, this tracks the active membership count and is kept in
    /// sync with Stripe via `BillingService::sync_seats`.
    #[serde(default = "default_seat_count")]
    pub seat_count: u32,
}

fn default_seat_count() -> u32 {
    1
}

/// Stripe subscription lifecycle status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    PastDue,
    Canceled,
    Incomplete,
    Trialing,
    /// No Stripe subscription exists (free tier).
    None,
}

impl SubscriptionStatus {
    /// Parse from Stripe's subscription status string.
    pub fn from_stripe(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "past_due" => Self::PastDue,
            "canceled" => Self::Canceled,
            "incomplete" | "incomplete_expired" => Self::Incomplete,
            "trialing" => Self::Trialing,
            _ => Self::None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::PastDue => "past_due",
            Self::Canceled => "canceled",
            Self::Incomplete => "incomplete",
            Self::Trialing => "trialing",
            Self::None => "none",
        }
    }

    /// `true` iff the subscription is in a state that entitles the tenant to
    /// the paid tier's benefits — i.e. `Active` or `Trialing`. Used by
    /// `EffectiveTierService` (ADR-111 Phase 3) to decide whether the personal
    /// subscription's tier should contribute to the effective-tier `max()`.
    pub fn is_active_or_trialing(&self) -> bool {
        matches!(self, Self::Active | Self::Trialing)
    }
}

impl std::fmt::Display for SubscriptionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_status_from_stripe_active() {
        assert_eq!(
            SubscriptionStatus::from_stripe("active"),
            SubscriptionStatus::Active
        );
    }

    #[test]
    fn subscription_status_from_stripe_past_due() {
        assert_eq!(
            SubscriptionStatus::from_stripe("past_due"),
            SubscriptionStatus::PastDue
        );
    }

    #[test]
    fn subscription_status_from_stripe_canceled() {
        assert_eq!(
            SubscriptionStatus::from_stripe("canceled"),
            SubscriptionStatus::Canceled
        );
    }

    #[test]
    fn subscription_status_from_stripe_trialing() {
        assert_eq!(
            SubscriptionStatus::from_stripe("trialing"),
            SubscriptionStatus::Trialing
        );
    }

    #[test]
    fn subscription_status_from_stripe_incomplete() {
        assert_eq!(
            SubscriptionStatus::from_stripe("incomplete"),
            SubscriptionStatus::Incomplete
        );
    }

    #[test]
    fn subscription_status_from_stripe_incomplete_expired() {
        assert_eq!(
            SubscriptionStatus::from_stripe("incomplete_expired"),
            SubscriptionStatus::Incomplete
        );
    }

    #[test]
    fn subscription_status_from_stripe_unknown_defaults_to_none() {
        assert_eq!(
            SubscriptionStatus::from_stripe("bogus"),
            SubscriptionStatus::None
        );
    }

    #[test]
    fn subscription_status_roundtrip() {
        let statuses = [
            SubscriptionStatus::Active,
            SubscriptionStatus::PastDue,
            SubscriptionStatus::Canceled,
            SubscriptionStatus::Incomplete,
            SubscriptionStatus::Trialing,
            SubscriptionStatus::None,
        ];
        for status in &statuses {
            assert_eq!(SubscriptionStatus::from_stripe(status.as_str()), *status);
        }
    }

    #[test]
    fn tenant_subscription_serialization_roundtrip() {
        let sub = TenantSubscription {
            tenant_id: TenantId::consumer(),
            stripe_customer_id: "cus_test123".into(),
            stripe_subscription_id: Some("sub_test456".into()),
            tier: TenantTier::Pro,
            status: SubscriptionStatus::Active,
            current_period_end: Some(Utc::now()),
            cancel_at_period_end: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            seat_count: 1,
        };
        let json = serde_json::to_string(&sub).unwrap();
        let deserialized: TenantSubscription = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.stripe_customer_id, "cus_test123");
        assert_eq!(deserialized.status, SubscriptionStatus::Active);
    }
}
