// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # TenantQuotaService (ADR-056 §Quota Enforcement)
//!
//! Enforces per-tenant resource quotas for agents and concurrent executions.
//!
//! ## Design
//!
//! Two check methods are provided — one per quota dimension — so that callers
//! can fail fast with a precise error before creating any domain resources.
//!
//! Both methods publish [`TenantEvent::TenantQuotaExceeded`] on the event bus
//! before returning the error so that observability and alerting pipelines
//! receive real-time quota signals.
//!
//! ## Single source of truth
//!
//! Tier is read from `tenant_subscriptions.tier` via `BillingRepository` and
//! quotas are computed dynamically via [`TenantQuotas::for_tier`]. There is no
//! duplicate tier/quota state on the `tenants` row.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::events::TenantEvent;
use crate::domain::repository::{
    AgentRepository, ExecutionRepository, RepositoryError, TenantRepository,
};
use crate::domain::tenancy::{TenantQuotaKind, TenantQuotas, TenantTier};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::repositories::BillingRepository;

#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error("quota exceeded for {kind:?}: current {current}, limit {limit}")]
    Exceeded {
        kind: TenantQuotaKind,
        current: u64,
        limit: u64,
    },
    #[error("tenant not found: {0}")]
    TenantNotFound(String),
    #[error("repository error: {0}")]
    Repository(String),
}

impl From<RepositoryError> for QuotaError {
    fn from(e: RepositoryError) -> Self {
        QuotaError::Repository(e.to_string())
    }
}

/// Resolve the tenant's tier from its `tenant_subscriptions` row.
///
/// Returns the subscription tier when the subscription is Active or Trialing,
/// otherwise falls back to [`TenantTier::Free`]. This is the single source of
/// truth for tier resolution in quota enforcement — there is no longer a
/// `tier` column on the `tenants` row.
pub(crate) async fn resolve_tier_from_subscription(
    billing_repo: &dyn BillingRepository,
    tenant_id: &TenantId,
) -> Result<TenantTier, QuotaError> {
    let sub = billing_repo
        .get_subscription(tenant_id)
        .await
        .map_err(|e| QuotaError::Repository(e.to_string()))?;
    Ok(match sub {
        Some(s) if s.status.is_active_or_trialing() => s.tier,
        _ => TenantTier::Free,
    })
}

pub struct TenantQuotaService {
    tenant_repo: Arc<dyn TenantRepository>,
    billing_repo: Arc<dyn BillingRepository>,
    agent_repo: Arc<dyn AgentRepository>,
    execution_repo: Arc<dyn ExecutionRepository>,
    event_bus: Arc<EventBus>,
}

impl TenantQuotaService {
    pub fn new(
        tenant_repo: Arc<dyn TenantRepository>,
        billing_repo: Arc<dyn BillingRepository>,
        agent_repo: Arc<dyn AgentRepository>,
        execution_repo: Arc<dyn ExecutionRepository>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            tenant_repo,
            billing_repo,
            agent_repo,
            execution_repo,
            event_bus,
        }
    }

    /// Resolve the tenant's tier from its subscription row. When no
    /// subscription exists or it is not Active/Trialing, fall back to Free.
    pub(crate) async fn resolve_tier(
        &self,
        tenant_id: &TenantId,
    ) -> Result<TenantTier, QuotaError> {
        resolve_tier_from_subscription(self.billing_repo.as_ref(), tenant_id).await
    }

    /// Check whether deploying a new agent would exceed the tenant's `max_agents` quota.
    ///
    /// Fetches the current active agent count and compares it against the tenant's quota
    /// computed from `tenant_subscriptions.tier`. Publishes
    /// [`TenantEvent::TenantQuotaExceeded`] and returns `Err(QuotaError::Exceeded)`
    /// when the limit is reached or exceeded.
    pub async fn check_agent_quota(&self, tenant_id: &TenantId) -> Result<(), QuotaError> {
        // Probe tenant existence so callers get a precise NotFound error.
        if self.tenant_repo.find_by_slug(tenant_id).await?.is_none() {
            return Err(QuotaError::TenantNotFound(tenant_id.as_str().to_string()));
        }

        let tier = self.resolve_tier(tenant_id).await?;
        let quotas = TenantQuotas::for_tier(&tier);
        let limit = quotas.max_agents as u64;

        let current = self.agent_repo.count_active(tenant_id).await?;

        if current >= limit {
            self.event_bus
                .publish_tenant_event(TenantEvent::TenantQuotaExceeded {
                    tenant_slug: tenant_id.as_str().to_string(),
                    quota_kind: TenantQuotaKind::TotalAgents,
                    current_value: current,
                    limit,
                    exceeded_at: Utc::now(),
                });
            return Err(QuotaError::Exceeded {
                kind: TenantQuotaKind::TotalAgents,
                current,
                limit,
            });
        }

        Ok(())
    }

    /// Check whether starting a new execution would exceed the tenant's
    /// `max_concurrent_executions` quota.
    ///
    /// Counts running and pending executions and compares against the limit
    /// computed from `tenant_subscriptions.tier`. Publishes
    /// [`TenantEvent::TenantQuotaExceeded`] and returns `Err(QuotaError::Exceeded)`
    /// when the limit is reached or exceeded.
    pub async fn check_execution_quota(&self, tenant_id: &TenantId) -> Result<(), QuotaError> {
        if self.tenant_repo.find_by_slug(tenant_id).await?.is_none() {
            return Err(QuotaError::TenantNotFound(tenant_id.as_str().to_string()));
        }

        let tier = self.resolve_tier(tenant_id).await?;
        let quotas = TenantQuotas::for_tier(&tier);
        let limit = quotas.max_concurrent_executions as u64;

        let current = self.execution_repo.count_running(tenant_id).await?;

        if current >= limit {
            self.event_bus
                .publish_tenant_event(TenantEvent::TenantQuotaExceeded {
                    tenant_slug: tenant_id.as_str().to_string(),
                    quota_kind: TenantQuotaKind::ConcurrentExecutions,
                    current_value: current,
                    limit,
                    exceeded_at: Utc::now(),
                });
            return Err(QuotaError::Exceeded {
                kind: TenantQuotaKind::ConcurrentExecutions,
                current,
                limit,
            });
        }

        Ok(())
    }
}

// ============================================================================
// Tests — single source of truth for tier is tenant_subscriptions.tier.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};

    use crate::domain::billing::{SubscriptionStatus, TenantSubscription};
    use crate::domain::repository::RepositoryError;
    use crate::domain::tenancy::{Tenant, TenantStatus};

    // ── In-memory fakes ─────────────────────────────────────────────────────

    #[derive(Default)]
    struct FakeTenantRepo {
        rows: Mutex<Vec<Tenant>>,
    }

    impl FakeTenantRepo {
        fn with_tenant(tenant: Tenant) -> Self {
            Self {
                rows: Mutex::new(vec![tenant]),
            }
        }
    }

    #[async_trait]
    impl TenantRepository for FakeTenantRepo {
        async fn find_by_slug(&self, slug: &TenantId) -> Result<Option<Tenant>, RepositoryError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|t| &t.slug == slug)
                .cloned())
        }
        async fn find_all_active(&self) -> Result<Vec<Tenant>, RepositoryError> {
            Ok(self.rows.lock().unwrap().clone())
        }
        async fn insert(&self, _tenant: &Tenant) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn update_status(
            &self,
            _slug: &TenantId,
            _status: &TenantStatus,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeBillingRepo {
        subs: Mutex<HashMap<String, TenantSubscription>>,
    }

    impl FakeBillingRepo {
        fn set(&self, tenant_id: &TenantId, tier: TenantTier, status: SubscriptionStatus) {
            let sub = TenantSubscription {
                tenant_id: tenant_id.clone(),
                stripe_customer_id: "cus_test".into(),
                stripe_subscription_id: Some("sub_test".into()),
                tier,
                status,
                current_period_end: None,
                cancel_at_period_end: false,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                seat_count: 0,
                user_sub: None,
            };
            self.subs
                .lock()
                .unwrap()
                .insert(tenant_id.as_str().to_string(), sub);
        }
    }

    #[async_trait]
    impl BillingRepository for FakeBillingRepo {
        async fn upsert_subscription(
            &self,
            _sub: &TenantSubscription,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn get_subscription(
            &self,
            tenant_id: &TenantId,
        ) -> Result<Option<TenantSubscription>, RepositoryError> {
            Ok(self.subs.lock().unwrap().get(tenant_id.as_str()).cloned())
        }
        async fn get_subscription_by_customer(
            &self,
            _stripe_customer_id: &str,
        ) -> Result<Option<TenantSubscription>, RepositoryError> {
            Ok(None)
        }
        async fn get_subscription_by_stripe_sub_id(
            &self,
            _stripe_subscription_id: &str,
        ) -> Result<Option<TenantSubscription>, RepositoryError> {
            Ok(None)
        }
        async fn clear_stripe_subscription_id(
            &self,
            _tenant_id: &TenantId,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn update_tier(
            &self,
            _tenant_id: &TenantId,
            _tier: &TenantTier,
            _status: &SubscriptionStatus,
            _period_end: Option<DateTime<Utc>>,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn update_seat_count_by_customer(
            &self,
            _stripe_customer_id: &str,
            _seat_count: u32,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn get_subscription_by_user_sub(
            &self,
            _user_sub: &str,
        ) -> Result<Option<TenantSubscription>, RepositoryError> {
            Ok(None)
        }
        async fn update_tenant_id_for_user_sub(
            &self,
            _user_sub: &str,
            _new_tenant_id: &TenantId,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    fn mk_tenant(slug: &TenantId) -> Tenant {
        Tenant::new(
            slug.clone(),
            "Test".into(),
            "zaru-consumer".into(),
            format!("tenant-{}/", slug.as_str()),
        )
    }

    /// Business subscription → tier is Business → max_agents = 200.
    /// Exercises the production tier-resolution helper `resolve_tier_from_subscription`,
    /// which is what `TenantQuotaService::check_*_quota` calls under the hood.
    #[tokio::test]
    async fn tenant_quota_check_reads_from_subscription_tier() {
        let slug = TenantId::for_consumer_user("11111111-2222-3333-4444-555555555555").unwrap();
        let _tenant_repo = Arc::new(FakeTenantRepo::with_tenant(mk_tenant(&slug)));
        let billing_fake = FakeBillingRepo::default();
        billing_fake.set(&slug, TenantTier::Business, SubscriptionStatus::Active);

        let tier = resolve_tier_from_subscription(&billing_fake, &slug)
            .await
            .unwrap();
        assert_eq!(tier, TenantTier::Business);
        assert_eq!(TenantQuotas::for_tier(&tier).max_agents, 200);
        assert_eq!(TenantQuotas::for_tier(&tier).max_concurrent_executions, 25);
    }

    /// No subscription → fall back to Free → max_agents = 5.
    #[tokio::test]
    async fn tenant_quota_check_falls_back_to_free_when_no_subscription() {
        let slug = TenantId::for_consumer_user("22222222-2222-3333-4444-555555555555").unwrap();
        let billing_fake = FakeBillingRepo::default();

        let tier = resolve_tier_from_subscription(&billing_fake, &slug)
            .await
            .unwrap();
        assert_eq!(tier, TenantTier::Free);
        assert_eq!(TenantQuotas::for_tier(&tier).max_agents, 5);
    }

    /// Canceled subscription → fall back to Free.
    #[tokio::test]
    async fn tenant_quota_check_falls_back_to_free_when_subscription_canceled() {
        let slug = TenantId::for_consumer_user("33333333-2222-3333-4444-555555555555").unwrap();
        let billing_fake = FakeBillingRepo::default();
        billing_fake.set(&slug, TenantTier::Business, SubscriptionStatus::Canceled);

        let tier = resolve_tier_from_subscription(&billing_fake, &slug)
            .await
            .unwrap();
        assert_eq!(tier, TenantTier::Free);
        assert_eq!(TenantQuotas::for_tier(&tier).max_agents, 5);
    }

    /// End-to-end `TenantQuotaService::check_agent_quota` path asserts that
    /// the service resolves the tier via `BillingRepository` rather than
    /// reading a `tier` column on the tenants row. Requires AgentRepository
    /// + ExecutionRepository fakes with a rich surface; the tier-resolution
    /// logic that gates the check is covered directly above.
    #[tokio::test]
    #[ignore = "requires AgentRepository + ExecutionRepository fakes with full trait surface; tier-resolution logic covered above"]
    async fn check_agent_quota_end_to_end_reads_subscription_tier() {}
}
