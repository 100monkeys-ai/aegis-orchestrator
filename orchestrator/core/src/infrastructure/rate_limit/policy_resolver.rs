// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Hierarchical Policy Resolver (ADR-072)
//!
//! Resolves the effective [`RateLimitPolicy`] for a request by walking the
//! three-level override hierarchy:
//!
//! 1. **User override** — `rate_limit_overrides WHERE user_id = ?`
//! 2. **Tenant override** — `rate_limit_overrides WHERE tenant_id = ?`
//! 3. **Tier default** — [`tier_defaults`] for the resolved [`ZaruTier`]
//!
//! The first level that produces at least one row wins for that resource type.
//!
//! A consumer user's tier is read from the store the `zaru_tier` claim is
//! written from ([`EffectiveTierService::compute_effective_tier`]), through
//! the [`ConsumerTierSource`] port, because the writers rebuild the
//! identity they pass (an execution's initiating user, a schedule's stored
//! owner) and the claim it carries is not the subscriber's live tier.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{PgPool, Row};

use crate::application::effective_tier_service::{
    EffectiveTierError, EffectiveTierService, StandardEffectiveTierService, TierSyncPort,
    UserClaims,
};
use crate::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use crate::domain::rate_limit::{
    tier_defaults, ConsumerTierSource, RateLimitBucket, RateLimitError, RateLimitPolicy,
    RateLimitPolicyResolver, RateLimitResourceType, RateLimitWindow,
};
use crate::domain::team::MembershipRepository;
use crate::domain::tenant::TenantId;
use crate::infrastructure::repositories::{
    BillingRepository, PgMembershipRepository, PgTeamRepository, PostgresBillingRepository,
};

/// A consumer's tier from the orchestrator's database: the effective tier
/// [`EffectiveTierService::compute_effective_tier`] computes, the value the
/// Keycloak `zaru_tier` claim is written from. A user the store holds
/// nothing for (no subscription row, no active membership) answers `None`.
pub struct EffectiveTierSource {
    tiers: Arc<dyn EffectiveTierService>,
    billing: Arc<dyn BillingRepository>,
    memberships: Arc<dyn MembershipRepository>,
}

impl EffectiveTierSource {
    /// The store in `pool`, read only.
    pub fn from_pool(pool: PgPool) -> Self {
        let billing: Arc<dyn BillingRepository> =
            Arc::new(PostgresBillingRepository::new(pool.clone()));
        let memberships: Arc<dyn MembershipRepository> =
            Arc::new(PgMembershipRepository::new(pool.clone()));
        let tiers = Arc::new(StandardEffectiveTierService::new(
            Arc::new(PgTeamRepository::new(pool)),
            memberships.clone(),
            billing.clone(),
            Arc::new(NoClaimWrites),
        ));
        Self {
            tiers,
            billing,
            memberships,
        }
    }
}

/// The resolver computes tiers and writes no claim: the tier service it
/// reads through is given a sync port that refuses every write.
struct NoClaimWrites;

#[async_trait]
impl TierSyncPort for NoClaimWrites {
    async fn set_claims(&self, _: &str, _: &UserClaims) -> Result<(), EffectiveTierError> {
        Err(EffectiveTierError::TierSync(
            "the rate-limit resolver reads tiers and writes no claim".into(),
        ))
    }
}

#[async_trait]
impl ConsumerTierSource for EffectiveTierSource {
    async fn consumer_tier(&self, sub: &str) -> Result<Option<ZaruTier>, RateLimitError> {
        let failed = |e: String| RateLimitError::PolicyResolutionFailed(e);
        let Ok(personal_tenant) = TenantId::for_consumer_user(sub) else {
            return Ok(None);
        };
        let subscribed = self
            .billing
            .get_subscription(&personal_tenant)
            .await
            .map_err(|e| failed(format!("get_subscription: {e}")))?
            .is_some();
        if !subscribed
            && self
                .memberships
                .find_active_for_user(sub)
                .await
                .map_err(|e| failed(format!("find_active_for_user: {e}")))?
                .is_empty()
        {
            return Ok(None);
        }
        let tier = self
            .tiers
            .compute_effective_tier(sub)
            .await
            .map_err(|e| failed(e.to_string()))?;
        Ok(ZaruTier::from_claim(tier.as_keycloak_str()))
    }
}

/// Resolves rate-limit policies using the override hierarchy:
/// user → tenant → tier defaults.
pub struct HierarchicalPolicyResolver {
    pool: PgPool,
    consumer_tiers: Arc<dyn ConsumerTierSource>,
}

impl HierarchicalPolicyResolver {
    /// Overrides from `pool`, and a consumer's tier from the effective tier
    /// the same database holds ([`EffectiveTierSource`]).
    pub fn new(pool: PgPool) -> Self {
        let consumer_tiers = Arc::new(EffectiveTierSource::from_pool(pool.clone()));
        Self {
            pool,
            consumer_tiers,
        }
    }

    /// Read a consumer's tier from `source` instead.
    pub fn with_consumer_tiers(mut self, source: Arc<dyn ConsumerTierSource>) -> Self {
        self.consumer_tiers = source;
        self
    }

    /// The tier a policy's defaults are taken from: for a consumer user the
    /// effective tier the store holds, or the identity's claim when the store
    /// holds nothing for the user or cannot be read; for every other
    /// identity kind, [`Self::resolve_tier`].
    async fn tier_for(&self, identity: &UserIdentity) -> ZaruTier {
        let claimed = Self::resolve_tier(identity);
        if !matches!(identity.identity_kind, IdentityKind::ConsumerUser { .. }) {
            return claimed;
        }
        match self.consumer_tiers.consumer_tier(&identity.sub).await {
            Ok(Some(tier)) => tier,
            Ok(None) => claimed,
            Err(e) => {
                tracing::warn!(
                    user_id = %identity.sub,
                    error = %e,
                    "effective tier unreadable; the identity's claim stands"
                );
                claimed
            }
        }
    }

    /// Derive the [`ZaruTier`] from the authenticated identity.
    ///
    /// Consumer users carry their tier in the JWT claim; all other identity
    /// kinds (operators, service accounts, tenant users) are treated as
    /// Enterprise for rate-limiting purposes.
    fn resolve_tier(identity: &UserIdentity) -> ZaruTier {
        match &identity.identity_kind {
            IdentityKind::ConsumerUser { zaru_tier, .. } => zaru_tier.clone(),
            IdentityKind::TenantUser { .. }
            | IdentityKind::Operator { .. }
            | IdentityKind::ServiceAccount { .. } => ZaruTier::Enterprise,
        }
    }

    /// Serialize a [`RateLimitResourceType`] to the string representation
    /// stored in the `rate_limit_overrides.resource_type` column.
    fn resource_type_to_db(resource_type: &RateLimitResourceType) -> String {
        match resource_type {
            RateLimitResourceType::SealToolCall { tool_pattern } => {
                format!("seal_tool:{tool_pattern}")
            }
            RateLimitResourceType::AgentExecution => "agent_execution".into(),
            RateLimitResourceType::WorkflowExecution => "workflow_execution".into(),
            RateLimitResourceType::LlmCall => "llm_call".into(),
            RateLimitResourceType::LlmToken => "llm_token".into(),
        }
    }

    /// Parse a `rate_limit_overrides.bucket` column value into a
    /// [`RateLimitBucket`].
    fn bucket_from_db(s: &str) -> Option<RateLimitBucket> {
        match s {
            "per_minute" => Some(RateLimitBucket::PerMinute),
            "hourly" => Some(RateLimitBucket::Hourly),
            "daily" => Some(RateLimitBucket::Daily),
            "weekly" => Some(RateLimitBucket::Weekly),
            "monthly" => Some(RateLimitBucket::Monthly),
            _ => None,
        }
    }

    /// Load override rows for a given user, returning `None` if no rows match.
    async fn load_user_overrides(
        &self,
        user_id: &str,
        resource_type_str: &str,
    ) -> Result<Option<HashMap<RateLimitBucket, RateLimitWindow>>, RateLimitError> {
        let rows = sqlx::query(
            r#"
            SELECT bucket, limit_value, burst_value
            FROM rate_limit_overrides
            WHERE user_id = $1 AND resource_type = $2
            "#,
        )
        .bind(user_id)
        .bind(resource_type_str)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RateLimitError::PolicyResolutionFailed(e.to_string()))?;

        Self::rows_to_windows(&rows)
    }

    /// Load override rows for a given tenant, returning `None` if no rows match.
    async fn load_tenant_overrides(
        &self,
        tenant_id: &str,
        resource_type_str: &str,
    ) -> Result<Option<HashMap<RateLimitBucket, RateLimitWindow>>, RateLimitError> {
        let rows = sqlx::query(
            r#"
            SELECT bucket, limit_value, burst_value
            FROM rate_limit_overrides
            WHERE tenant_id = $1 AND resource_type = $2
            "#,
        )
        .bind(tenant_id)
        .bind(resource_type_str)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RateLimitError::PolicyResolutionFailed(e.to_string()))?;

        Self::rows_to_windows(&rows)
    }

    /// Convert database rows into a bucket→window map.
    /// Returns `None` when the row set is empty or contains no valid buckets.
    fn rows_to_windows(
        rows: &[sqlx::postgres::PgRow],
    ) -> Result<Option<HashMap<RateLimitBucket, RateLimitWindow>>, RateLimitError> {
        if rows.is_empty() {
            return Ok(None);
        }

        let mut windows = HashMap::new();
        for row in rows {
            let bucket_str: String = row.get("bucket");
            let limit_value: i64 = row.get("limit_value");
            let burst_value: Option<i64> = row.get("burst_value");

            if let Some(bucket) = Self::bucket_from_db(&bucket_str) {
                windows.insert(
                    bucket,
                    RateLimitWindow {
                        limit: limit_value as u64,
                        window_seconds: bucket.window_seconds(),
                        burst: burst_value.map(|v| v as u64),
                    },
                );
            }
        }

        if windows.is_empty() {
            Ok(None)
        } else {
            Ok(Some(windows))
        }
    }
}

#[async_trait]
impl RateLimitPolicyResolver for HierarchicalPolicyResolver {
    async fn resolve_policy(
        &self,
        identity: &UserIdentity,
        tenant_id: &TenantId,
        resource_type: &RateLimitResourceType,
    ) -> Result<RateLimitPolicy, RateLimitError> {
        let resource_str = Self::resource_type_to_db(resource_type);

        // 1. User-level override (highest priority)
        if let Some(windows) = self
            .load_user_overrides(&identity.sub, &resource_str)
            .await?
        {
            return Ok(RateLimitPolicy {
                resource_type: resource_type.clone(),
                windows,
            });
        }

        // 2. Tenant-level override
        if let Some(windows) = self
            .load_tenant_overrides(tenant_id.as_str(), &resource_str)
            .await?
        {
            return Ok(RateLimitPolicy {
                resource_type: resource_type.clone(),
                windows,
            });
        }

        // 3. Tier defaults (lowest priority)
        let tier = self.tier_for(identity).await;
        let defaults = tier_defaults(&tier);
        let policy = defaults
            .into_iter()
            .find(|p| p.resource_type == *resource_type)
            .unwrap_or_else(|| RateLimitPolicy {
                resource_type: resource_type.clone(),
                windows: HashMap::new(),
            });

        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_tier_consumer_free() {
        let identity = UserIdentity {
            sub: "u-1".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: crate::domain::tenant::TenantId::consumer(),
            },
        };
        assert_eq!(
            HierarchicalPolicyResolver::resolve_tier(&identity),
            ZaruTier::Free
        );
    }

    #[test]
    fn resolve_tier_consumer_pro() {
        let identity = UserIdentity {
            sub: "u-2".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Pro,
                tenant_id: crate::domain::tenant::TenantId::consumer(),
            },
        };
        assert_eq!(
            HierarchicalPolicyResolver::resolve_tier(&identity),
            ZaruTier::Pro
        );
    }

    #[test]
    fn resolve_tier_tenant_user_is_enterprise() {
        let identity = UserIdentity {
            sub: "u-3".into(),
            realm_slug: "tenant-acme".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "acme".into(),
            },
        };
        assert_eq!(
            HierarchicalPolicyResolver::resolve_tier(&identity),
            ZaruTier::Enterprise
        );
    }

    #[test]
    fn resolve_tier_operator_is_enterprise() {
        let identity = UserIdentity {
            sub: "u-4".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: crate::domain::iam::AegisRole::Admin,
            },
        };
        assert_eq!(
            HierarchicalPolicyResolver::resolve_tier(&identity),
            ZaruTier::Enterprise
        );
    }

    #[test]
    fn resolve_tier_service_account_is_enterprise() {
        let identity = UserIdentity {
            sub: "sa-1".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ServiceAccount {
                client_id: "aegis-sdk".into(),
            },
        };
        assert_eq!(
            HierarchicalPolicyResolver::resolve_tier(&identity),
            ZaruTier::Enterprise
        );
    }

    /// A tier store answering one value for every user.
    struct FixedTiers(Result<Option<ZaruTier>, ()>);

    #[async_trait]
    impl ConsumerTierSource for FixedTiers {
        async fn consumer_tier(&self, _: &str) -> Result<Option<ZaruTier>, RateLimitError> {
            self.0
                .clone()
                .map_err(|_| RateLimitError::PolicyResolutionFailed("unreachable".into()))
        }
    }

    /// A resolver whose pool is never connected: `tier_for` reads only the
    /// tier store.
    fn resolver_over(tiers: FixedTiers) -> HierarchicalPolicyResolver {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused")
            .unwrap();
        HierarchicalPolicyResolver::new(pool).with_consumer_tiers(Arc::new(tiers))
    }

    fn consumer_claiming(zaru_tier: ZaruTier) -> UserIdentity {
        UserIdentity {
            sub: "u-5".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier,
                tenant_id: crate::domain::tenant::TenantId::consumer(),
            },
        }
    }

    /// A consumer rebuilt at Free whose stored tier is Pro resolves Pro.
    #[tokio::test]
    async fn a_consumers_stored_tier_replaces_the_rebuilt_claim() {
        let resolver = resolver_over(FixedTiers(Ok(Some(ZaruTier::Pro))));
        assert_eq!(
            resolver.tier_for(&consumer_claiming(ZaruTier::Free)).await,
            ZaruTier::Pro
        );
    }

    /// With nothing stored, or the store unreadable, the claim stands.
    #[tokio::test]
    async fn a_consumer_with_no_stored_tier_or_an_unreadable_store_keeps_the_claim() {
        for tiers in [FixedTiers(Ok(None)), FixedTiers(Err(()))] {
            let resolver = resolver_over(tiers);
            assert_eq!(
                resolver.tier_for(&consumer_claiming(ZaruTier::Pro)).await,
                ZaruTier::Pro
            );
        }
    }

    /// The store is read for consumers only: an operator stays Enterprise.
    #[tokio::test]
    async fn an_operator_is_not_read_from_the_tier_store() {
        let resolver = resolver_over(FixedTiers(Ok(Some(ZaruTier::Free))));
        let operator = UserIdentity {
            sub: "u-6".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: crate::domain::iam::AegisRole::Admin,
            },
        };
        assert_eq!(resolver.tier_for(&operator).await, ZaruTier::Enterprise);
    }

    #[test]
    fn resource_type_to_db_variants() {
        assert_eq!(
            HierarchicalPolicyResolver::resource_type_to_db(&RateLimitResourceType::AgentExecution),
            "agent_execution"
        );
        assert_eq!(
            HierarchicalPolicyResolver::resource_type_to_db(
                &RateLimitResourceType::WorkflowExecution
            ),
            "workflow_execution"
        );
        assert_eq!(
            HierarchicalPolicyResolver::resource_type_to_db(&RateLimitResourceType::LlmCall),
            "llm_call"
        );
        assert_eq!(
            HierarchicalPolicyResolver::resource_type_to_db(&RateLimitResourceType::LlmToken),
            "llm_token"
        );
        assert_eq!(
            HierarchicalPolicyResolver::resource_type_to_db(&RateLimitResourceType::SealToolCall {
                tool_pattern: "fs_*".into()
            }),
            "seal_tool:fs_*"
        );
    }

    #[test]
    fn bucket_from_db_roundtrip() {
        assert_eq!(
            HierarchicalPolicyResolver::bucket_from_db("per_minute"),
            Some(RateLimitBucket::PerMinute)
        );
        assert_eq!(
            HierarchicalPolicyResolver::bucket_from_db("hourly"),
            Some(RateLimitBucket::Hourly)
        );
        assert_eq!(
            HierarchicalPolicyResolver::bucket_from_db("daily"),
            Some(RateLimitBucket::Daily)
        );
        assert_eq!(
            HierarchicalPolicyResolver::bucket_from_db("weekly"),
            Some(RateLimitBucket::Weekly)
        );
        assert_eq!(
            HierarchicalPolicyResolver::bucket_from_db("monthly"),
            Some(RateLimitBucket::Monthly)
        );
        assert_eq!(HierarchicalPolicyResolver::bucket_from_db("unknown"), None);
    }
}
