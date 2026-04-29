// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Postgres-backed [`EnrollmentTokenRepository`] (ADR-117).
//!
//! `redeem` uses `INSERT ... ON CONFLICT (jti) DO NOTHING RETURNING jti` to
//! enforce one-time use atomically. If the row already exists the RETURNING
//! clause yields no rows and we surface `AlreadyRedeemed`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use uuid::Uuid;

use crate::domain::edge::{EnrollmentTokenError, EnrollmentTokenRepository};
use crate::domain::shared_kernel::TenantId;

pub struct PgEnrollmentTokenRepository {
    pool: PgPool,
}

impl PgEnrollmentTokenRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl EnrollmentTokenRepository for PgEnrollmentTokenRepository {
    async fn redeem(
        &self,
        jti: Uuid,
        tenant_id: &TenantId,
        issued_to: &str,
        exp: DateTime<Utc>,
    ) -> Result<(), EnrollmentTokenError> {
        let row: Option<(Uuid,)> = sqlx::query_as(
            r#"
            INSERT INTO enrollment_tokens (jti, tenant_id, issued_to, exp, redeemed_at)
            VALUES ($1, $2, $3, $4, NOW())
            ON CONFLICT (jti) DO NOTHING
            RETURNING jti
            "#,
        )
        .bind(jti)
        .bind(tenant_id.as_str())
        .bind(issued_to)
        .bind(exp)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| EnrollmentTokenError::Other(e.into()))?;

        if row.is_none() {
            return Err(EnrollmentTokenError::AlreadyRedeemed);
        }
        Ok(())
    }
}
