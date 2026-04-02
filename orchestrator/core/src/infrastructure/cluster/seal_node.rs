// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL Node Verifier
//!
//! Verifies Ed25519 signatures and JWT tokens carried in `SealNodeEnvelope`s
//! for inter-node cluster communication (ADR-059).

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine;
use chrono::Utc;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::domain::cluster::{NodeClusterRepository, NodeId, NodeTokenClaims, SealNodeEnvelope};

/// Verifies the authenticity and integrity of `SealNodeEnvelope`s.
///
/// For every authenticated cluster RPC the gRPC server calls
/// `verify_envelope` which:
/// 1. Parses the JWT to extract the `node_id` claim.
/// 2. Looks up the node's registered public key via `NodeClusterRepository`.
/// 3. Verifies the Ed25519 signature over `payload`.
/// 4. Checks that the token has not expired.
pub struct SealNodeVerifier {
    cluster_repo: Arc<dyn NodeClusterRepository>,
}

impl SealNodeVerifier {
    pub fn new(cluster_repo: Arc<dyn NodeClusterRepository>) -> Self {
        Self { cluster_repo }
    }

    /// Verify the envelope's token and signature, returning the authenticated `NodeId`.
    pub async fn verify_envelope(&self, envelope: &SealNodeEnvelope) -> Result<NodeId> {
        // 1. Parse and validate token claims (expiry check included)
        let claims = Self::verify_token_claims(&envelope.node_security_token.0)?;
        let node_id = claims.node_id;

        // 2. Look up the peer's registered public key
        let peer = self
            .cluster_repo
            .find_peer(&node_id)
            .await
            .context("Failed to query cluster repository for peer")?
            .ok_or_else(|| anyhow::anyhow!("Node {} is not registered", node_id))?;

        // 3. Verify Ed25519 signature over payload
        let public_key_bytes: [u8; 32] = peer
            .public_key
            .as_slice()
            .try_into()
            .context("Stored public key is not 32 bytes")?;

        let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
            .context("Invalid stored Ed25519 public key")?;

        let sig_bytes: &[u8] = envelope.signature.as_bytes();
        let signature =
            Signature::from_slice(sig_bytes).context("Invalid Ed25519 signature format")?;

        verifying_key
            .verify(&envelope.payload, &signature)
            .context("Ed25519 signature verification failed")?;

        Ok(node_id)
    }

    /// Parse a JWT token's payload segment, deserialise to `NodeTokenClaims`,
    /// and check that the token has not expired.
    pub fn verify_token_claims(token: &str) -> Result<NodeTokenClaims> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            bail!("Invalid JWT format: expected 3 dot-separated segments");
        }

        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .context("Failed to base64url-decode JWT payload")?;

        let claims: NodeTokenClaims =
            serde_json::from_slice(&payload_bytes).context("Failed to parse JWT claims")?;

        let now = Utc::now().timestamp();
        if claims.exp <= now {
            bail!(
                "Token expired: exp={} now={} (expired {}s ago)",
                claims.exp,
                now,
                now - claims.exp
            );
        }

        Ok(claims)
    }
}
