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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use base64::Engine;
use serde::Serialize;

use crate::domain::secrets::SecretStore;

/// How long an issued token is valid.
pub const NODE_TOKEN_TTL: chrono::Duration = chrono::Duration::hours(1);

/// The Transit key version each key last signed with. The header names the
/// version (`kid`) and is itself signed, so it is written before Transit
/// answers: from this guess, and again when Transit answers with another
/// version because the key was rotated.
fn last_versions() -> &'static Mutex<HashMap<String, String>> {
    static LAST: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

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

    /// The signed JWT for `claims`: header `{alg: RS256, typ: JWT, kid:
    /// v<Transit key version>}`, every segment base64url without padding,
    /// as `NodeTokenVerifier` reads it.
    pub async fn issue<C: Serialize + ?Sized>(&self, claims: &C) -> Result<String> {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let claims_b64 = b64.encode(serde_json::to_vec(claims)?);
        let mut kid = last_versions()
            .lock()
            .expect("transit versions")
            .get(&self.signing_key_path)
            .cloned()
            .unwrap_or_else(|| "v1".to_string());
        // Twice at most: a rotation changes the version once.
        for _ in 0..2 {
            let header = serde_json::json!({ "alg": "RS256", "typ": "JWT", "kid": kid });
            let signing_input =
                format!("{}.{claims_b64}", b64.encode(serde_json::to_vec(&header)?));
            let raw_sig = self
                .secret_store
                .transit_sign(&self.signing_key_path, signing_input.as_bytes())
                .await
                .map_err(|e| anyhow!("transit_sign failed: {e}"))?;
            let (version, signature_b64) =
                crate::application::edge::transit::parse_vault_signature_versioned(&raw_sig)
                    .map_err(|e| anyhow!("transit_sign parse: {e}"))?;
            if version != kid {
                kid = version.to_string();
                last_versions()
                    .lock()
                    .expect("transit versions")
                    .insert(self.signing_key_path.clone(), kid.clone());
                continue;
            }
            // Transit returns standard base64; a JWT segment is base64url.
            let signature = base64::engine::general_purpose::STANDARD
                .decode(signature_b64)
                .map_err(|e| anyhow!("transit signature is not base64: {e}"))?;
            return Ok(format!("{signing_input}.{}", b64.encode(signature)));
        }
        Err(anyhow!(
            "transit key {} changed version twice while signing",
            self.signing_key_path
        ))
    }
}
