// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Billing Repository
//!
//! Persistence for `TenantSubscription` — the Stripe ↔ tenant billing link.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;

use crate::domain::billing::{SubscriptionStatus, TenantSubscription};
use crate::domain::repository::RepositoryError;
use crate::domain::tenancy::TenantTier;
use crate::domain::tenant::TenantId;

/// Repository trait for tenant subscription persistence.
#[async_trait]
pub trait BillingRepository: Send + Sync {
    /// Upsert a subscription record (insert or update on conflict).
    async fn upsert_subscription(&self, sub: &TenantSubscription) -> Result<(), RepositoryError>;

    /// Look up subscription by tenant ID.
    async fn get_subscription(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Option<TenantSubscription>, RepositoryError>;

    /// Look up subscription by Stripe customer ID.
    async fn get_subscription_by_customer(
        &self,
        stripe_customer_id: &str,
    ) -> Result<Option<TenantSubscription>, RepositoryError>;

    /// Update tier, status, and period end for an existing subscription.
    async fn update_tier(
        &self,
        tenant_id: &TenantId,
        tier: &TenantTier,
        status: &SubscriptionStatus,
        period_end: Option<DateTime<Utc>>,
    ) -> Result<(), RepositoryError>;
}

pub struct PostgresBillingRepository {
    pool: PgPool,
}

impl PostgresBillingRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn parse_tier(s: &str) -> TenantTier {
    match s {
        "free" => TenantTier::Free,
        "pro" => TenantTier::Pro,
        "business" => TenantTier::Business,
        "enterprise" => TenantTier::Enterprise,
        "system" => TenantTier::System,
        _ => TenantTier::Free,
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

fn row_to_subscription(row: &sqlx::postgres::PgRow) -> Result<TenantSubscription, RepositoryError> {
    let tenant_id_str: String = row.get("tenant_id");
    let tenant_id = TenantId::from_string(&tenant_id_str)
        .map_err(|e| RepositoryError::Serialization(format!("Invalid tenant id: {e}")))?;

    Ok(TenantSubscription {
        tenant_id,
        stripe_customer_id: row.get("stripe_customer_id"),
        stripe_subscription_id: row.get("stripe_subscription_id"),
        tier: parse_tier(row.get::<String, _>("tier").as_str()),
        status: SubscriptionStatus::from_stripe(row.get::<String, _>("status").as_str()),
        current_period_end: row.get("current_period_end"),
        cancel_at_period_end: row.get("cancel_at_period_end"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        seat_count: row.get::<i32, _>("seat_count") as u32,
    })
}

#[async_trait]
impl BillingRepository for PostgresBillingRepository {
    async fn upsert_subscription(&self, sub: &TenantSubscription) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            INSERT INTO tenant_subscriptions (
                tenant_id, stripe_customer_id, stripe_subscription_id,
                tier, status, current_period_end, cancel_at_period_end,
                created_at, updated_at, seat_count
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT (tenant_id) DO UPDATE SET
                stripe_customer_id = EXCLUDED.stripe_customer_id,
                stripe_subscription_id = EXCLUDED.stripe_subscription_id,
                tier = EXCLUDED.tier,
                status = EXCLUDED.status,
                current_period_end = EXCLUDED.current_period_end,
                cancel_at_period_end = EXCLUDED.cancel_at_period_end,
                updated_at = EXCLUDED.updated_at,
                seat_count = EXCLUDED.seat_count
            "#,
        )
        .bind(sub.tenant_id.as_str())
        .bind(&sub.stripe_customer_id)
        .bind(&sub.stripe_subscription_id)
        .bind(tier_to_str(&sub.tier))
        .bind(sub.status.as_str())
        .bind(sub.current_period_end)
        .bind(sub.cancel_at_period_end)
        .bind(sub.created_at)
        .bind(sub.updated_at)
        .bind(sub.seat_count as i32)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to upsert subscription: {e}")))?;

        Ok(())
    }

    async fn get_subscription(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Option<TenantSubscription>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT tenant_id, stripe_customer_id, stripe_subscription_id,
                   tier, status, current_period_end, cancel_at_period_end,
                   created_at, updated_at, seat_count
            FROM tenant_subscriptions
            WHERE tenant_id = $1
            "#,
        )
        .bind(tenant_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        match row {
            Some(ref r) => Ok(Some(row_to_subscription(r)?)),
            None => Ok(None),
        }
    }

    async fn get_subscription_by_customer(
        &self,
        stripe_customer_id: &str,
    ) -> Result<Option<TenantSubscription>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT tenant_id, stripe_customer_id, stripe_subscription_id,
                   tier, status, current_period_end, cancel_at_period_end,
                   created_at, updated_at, seat_count
            FROM tenant_subscriptions
            WHERE stripe_customer_id = $1
            "#,
        )
        .bind(stripe_customer_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        match row {
            Some(ref r) => Ok(Some(row_to_subscription(r)?)),
            None => Ok(None),
        }
    }

    async fn update_tier(
        &self,
        tenant_id: &TenantId,
        tier: &TenantTier,
        status: &SubscriptionStatus,
        period_end: Option<DateTime<Utc>>,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            UPDATE tenant_subscriptions
            SET tier = $2, status = $3, current_period_end = $4, updated_at = NOW()
            WHERE tenant_id = $1
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(tier_to_str(tier))
        .bind(status.as_str())
        .bind(period_end)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("Failed to update subscription tier: {e}"))
        })?;

        Ok(())
    }
}
