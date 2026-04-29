// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C — mint a short-lived enrollment JWT.
//!
//! The token is signed by the same OpenBao Transit key path that signs
//! NodeSecurityTokens. Validity is 15 minutes; one-time-use is enforced
//! server-side by the `enrollment_tokens` table at redemption.

use anyhow::Result;
use base64::Engine;
use chrono::{Duration, Utc};
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::domain::secrets::SecretStore;
use crate::domain::shared_kernel::TenantId;

/// Output of `IssueEnrollmentToken`.
#[derive(Debug, Clone, Serialize)]
pub struct IssuedEnrollmentToken {
    pub token: String,
    pub expires_at: chrono::DateTime<Utc>,
    /// Endpoint the daemon should connect to (host:port). Echoed in the JWT
    /// `cep` claim so the CLI doesn't need a separate `--endpoint` flag.
    pub controller_endpoint: String,
    /// Pre-rendered QR payload (`aegis edge enroll <token>`).
    pub qr_payload: String,
    /// Copy-pasteable shell command for the operator.
    pub command_hint: String,
}

#[derive(Serialize)]
struct Claims {
    tid: String,
    sub: String,
    jti: Uuid,
    exp: i64,
    nbf: i64,
    aud: &'static str,
    iss: String,
    cep: String,
}

#[derive(Serialize)]
struct JwtHeader {
    alg: &'static str,
    typ: &'static str,
}

pub struct IssueEnrollmentToken {
    secret_store: Arc<dyn SecretStore>,
    /// JWT issuer (controller URL or SaaS issuer URL).
    issuer: String,
    /// Endpoint advertised in the token's `cep` claim.
    controller_endpoint: String,
    /// OpenBao Transit signing key path used for NodeSecurityTokens.
    signing_key_path: String,
}

impl IssueEnrollmentToken {
    pub fn new(
        secret_store: Arc<dyn SecretStore>,
        issuer: String,
        controller_endpoint: String,
        signing_key_path: String,
    ) -> Self {
        Self {
            secret_store,
            issuer,
            controller_endpoint,
            signing_key_path,
        }
    }

    pub async fn issue(
        &self,
        tenant_id: &TenantId,
        issued_to_sub: &str,
    ) -> Result<IssuedEnrollmentToken> {
        let now = Utc::now();
        let expires_at = now + Duration::minutes(15);
        let claims = Claims {
            tid: tenant_id.as_str().to_string(),
            sub: issued_to_sub.to_string(),
            jti: Uuid::new_v4(),
            exp: expires_at.timestamp(),
            nbf: now.timestamp(),
            aud: "edge-enrollment",
            iss: self.issuer.clone(),
            cep: self.controller_endpoint.clone(),
        };
        let header = JwtHeader {
            alg: "RS256",
            typ: "JWT",
        };
        let h_json = serde_json::to_vec(&header)?;
        let c_json = serde_json::to_vec(&claims)?;
        let h_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&h_json);
        let c_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&c_json);
        let signing_input = format!("{h_b64}.{c_b64}");

        // OpenBao Transit returns `vault:vN:<base64>` — extract the signature
        // payload and re-encode as URL-safe-no-pad for the JWT third segment.
        let raw_sig = self
            .secret_store
            .transit_sign(&self.signing_key_path, signing_input.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("transit_sign failed: {e}"))?;
        let sig_b64 = raw_sig
            .rsplit_once(':')
            .map(|(_, b)| b.to_string())
            .unwrap_or(raw_sig);
        // OpenBao returns standard base64; convert to URL-safe-no-pad for JWT.
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(&sig_b64)
            .map_err(|e| anyhow::anyhow!("decode transit signature: {e}"))?;
        let s_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&sig_bytes);
        let token = format!("{signing_input}.{s_b64}");

        let cmd = format!("aegis edge enroll {token}");
        Ok(IssuedEnrollmentToken {
            token,
            expires_at,
            controller_endpoint: self.controller_endpoint.clone(),
            qr_payload: cmd.clone(),
            command_hint: cmd,
        })
    }
}

#[cfg(test)]
mod tests {
    // Round-trip is exercised in integration tests where the real SecretStore
    // signing key is available; here we keep coverage to construction only.
    #[test]
    fn module_compiles() {}
}
