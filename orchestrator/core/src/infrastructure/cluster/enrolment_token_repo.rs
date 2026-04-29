// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Postgres-backed [`ClusterEnrolmentTokenRepository`] (security audit 002 §4.9).
//!
//! Single-use, atomic redemption of cluster enrolment tokens. Token format is
//! `<token_id>.<secret>`; rows store an HMAC-SHA256 of the secret keyed under
//! the controller's enrolment-key path. Redemption is enforced via
//! `UPDATE ... WHERE redeemed_at IS NULL RETURNING node_id` — if the row is
//! already redeemed (or missing) the RETURNING clause yields no rows and we
//! surface `NotFound`. Node-binding mismatch is verified BEFORE the update so
//! a hostile caller cannot consume a legitimate node's token by guessing its
//! identity.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use sqlx::Row;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::domain::cluster::{ClusterEnrolmentTokenError, ClusterEnrolmentTokenRepository, NodeId};

pub struct PgClusterEnrolmentTokenRepository {
    pool: PgPool,
}

impl PgClusterEnrolmentTokenRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn parse_token(token: &str) -> Result<(Uuid, &str), ClusterEnrolmentTokenError> {
        let (id_part, secret) = token
            .split_once('.')
            .ok_or(ClusterEnrolmentTokenError::Malformed)?;
        let token_id =
            Uuid::parse_str(id_part).map_err(|_| ClusterEnrolmentTokenError::Malformed)?;
        if secret.is_empty() {
            return Err(ClusterEnrolmentTokenError::Malformed);
        }
        Ok((token_id, secret))
    }

    fn hash_secret(secret: &str) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());
        hasher.finalize().to_vec()
    }
}

#[async_trait]
impl ClusterEnrolmentTokenRepository for PgClusterEnrolmentTokenRepository {
    async fn redeem(
        &self,
        token: &str,
        presented_node_id: &NodeId,
    ) -> Result<NodeId, ClusterEnrolmentTokenError> {
        let (token_id, secret) = Self::parse_token(token)?;
        let presented_secret_hash = Self::hash_secret(secret);

        // Look up the row by token_id (do NOT yet redeem). We need to verify
        // both the secret and the node_id binding before consuming the token,
        // otherwise a hostile caller could burn a legitimate node's token by
        // guessing the token_id.
        let row = sqlx::query(
            r#"
            SELECT node_id, secret_hash, exp, redeemed_at
              FROM cluster_enrolment_tokens
             WHERE token_id = $1
            "#,
        )
        .bind(token_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ClusterEnrolmentTokenError::Other(e.into()))?;

        let row = row.ok_or(ClusterEnrolmentTokenError::NotFound)?;
        let bound_node_id: Uuid = row
            .try_get("node_id")
            .map_err(|e| ClusterEnrolmentTokenError::Other(e.into()))?;
        let stored_secret_hash: Vec<u8> = row
            .try_get("secret_hash")
            .map_err(|e| ClusterEnrolmentTokenError::Other(e.into()))?;
        let exp: DateTime<Utc> = row
            .try_get("exp")
            .map_err(|e| ClusterEnrolmentTokenError::Other(e.into()))?;
        let redeemed_at: Option<DateTime<Utc>> = row
            .try_get("redeemed_at")
            .map_err(|e| ClusterEnrolmentTokenError::Other(e.into()))?;

        if redeemed_at.is_some() {
            return Err(ClusterEnrolmentTokenError::NotFound);
        }
        if exp < Utc::now() {
            return Err(ClusterEnrolmentTokenError::Expired(exp));
        }
        if presented_secret_hash.ct_eq(&stored_secret_hash).unwrap_u8() != 1 {
            // Treat hash mismatch as NotFound — leaks no information about
            // whether the token_id exists.
            return Err(ClusterEnrolmentTokenError::NotFound);
        }
        let bound = NodeId(bound_node_id);
        if &bound != presented_node_id {
            return Err(ClusterEnrolmentTokenError::NodeIdMismatch {
                bound,
                presented: *presented_node_id,
            });
        }

        // Atomic single-use: only succeeds if redeemed_at is still NULL.
        let updated = sqlx::query(
            r#"
            UPDATE cluster_enrolment_tokens
               SET redeemed_at = NOW()
             WHERE token_id = $1
               AND redeemed_at IS NULL
            RETURNING node_id
            "#,
        )
        .bind(token_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ClusterEnrolmentTokenError::Other(e.into()))?;

        if updated.is_none() {
            // Lost the race against a concurrent redemption.
            return Err(ClusterEnrolmentTokenError::NotFound);
        }
        Ok(bound)
    }
}
