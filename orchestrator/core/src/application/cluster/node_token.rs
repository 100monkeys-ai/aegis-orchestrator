// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # NodeSecurityToken issuance (ADR-059, ADR-117, ADR-152)
//!
//! A `NodeSecurityToken` is an RS256 JWT signed by one OpenBao Transit key,
//! reachable from every node of a cluster (ADR-152 Context). The
//! controller issues one to a node it attests (`ChallengeNode`); in the
//! peer topology a node that has joined issues its own, to sign the
//! envelopes of the membership RPCs it sends its peers (`Probe`, `Fence`).
//! The token never authorises by itself: every receiver also verifies the
//! envelope's Ed25519 signature against the public key `cluster_nodes`
//! holds for the token's node.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use base64::Engine;

use crate::domain::cluster::NodeTokenClaims;
use crate::domain::secrets::SecretStore;

/// How long an issued token is valid.
pub const NODE_TOKEN_TTL: chrono::Duration = chrono::Duration::hours(1);

/// Signs `NodeSecurityToken`s with the cluster's Transit key.
#[derive(Clone)]
pub struct NodeTokenIssuer {
    secret_store: Arc<dyn SecretStore>,
    signing_key_path: String,
}

impl NodeTokenIssuer {
    pub fn new(secret_store: Arc<dyn SecretStore>, signing_key_path: String) -> Self {
        Self {
            secret_store,
            signing_key_path,
        }
    }

    pub fn signing_key_path(&self) -> &str {
        &self.signing_key_path
    }

    /// The signed JWT for `claims`.
    pub async fn issue(&self, claims: &NodeTokenClaims) -> Result<String> {
        let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
        let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&header)?);
        let claims_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_string(claims)?);
        let signing_input = format!("{header_b64}.{claims_b64}");
        let raw_sig = self
            .secret_store
            .transit_sign(&self.signing_key_path, signing_input.as_bytes())
            .await
            .map_err(|e| anyhow!("transit_sign failed: {e}"))?;
        // Vault Transit returns `vault:v<version>:<base64sig>`; the shared
        // strict parser handles every version number and refuses a
        // malformed envelope, as `IssueEnrollmentToken` does.
        let signature_b64 = crate::application::edge::transit::parse_vault_signature(&raw_sig)
            .map_err(|e| anyhow!("transit_sign parse: {e}"))?;
        Ok(format!("{signing_input}.{signature_b64}"))
    }
}
