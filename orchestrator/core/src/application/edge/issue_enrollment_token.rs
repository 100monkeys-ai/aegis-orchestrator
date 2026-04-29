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
        let sig_b64 = super::transit::parse_vault_signature(&raw_sig)
            .map_err(|e| anyhow::anyhow!("transit_sign parse: {e}"))?;
        // Vault transit's documented signature format is standard base64.
        // We decode it and re-encode as URL_SAFE_NO_PAD for the JWT signature
        // segment. We tolerate URL_SAFE_NO_PAD on input as a forward-compat
        // fallback in case a future transit version standardises on it.
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig_b64))
            .map_err(|e| {
                anyhow::anyhow!(
                    "decode transit signature (tried STANDARD and URL_SAFE_NO_PAD): {e}"
                )
            })?;
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
    use super::*;
    use crate::domain::secrets::{SecretStore, SecretsError, SensitiveString};
    use async_trait::async_trait;
    use std::collections::HashMap;

    struct MalformedTransitStore {
        raw: &'static str,
    }
    #[async_trait]
    impl SecretStore for MalformedTransitStore {
        async fn read(
            &self,
            _: &str,
            _: &str,
        ) -> Result<HashMap<String, SensitiveString>, SecretsError> {
            Ok(HashMap::new())
        }
        async fn write(
            &self,
            _: &str,
            _: &str,
            _: HashMap<String, SensitiveString>,
        ) -> Result<(), SecretsError> {
            Ok(())
        }
        async fn generate_dynamic(
            &self,
            _: &str,
            _: &str,
        ) -> Result<crate::domain::secrets::DomainDynamicSecret, SecretsError> {
            unimplemented!()
        }
        async fn renew_lease(
            &self,
            _: &str,
            _: std::time::Duration,
        ) -> Result<std::time::Duration, SecretsError> {
            Ok(std::time::Duration::from_secs(0))
        }
        async fn revoke_lease(&self, _: &str) -> Result<(), SecretsError> {
            Ok(())
        }
        async fn transit_sign(&self, _: &str, _: &[u8]) -> Result<String, SecretsError> {
            Ok(self.raw.to_string())
        }
        async fn transit_verify(&self, _: &str, _: &[u8], _: &str) -> Result<bool, SecretsError> {
            Ok(true)
        }
        async fn transit_encrypt(&self, _: &str, _: &[u8]) -> Result<String, SecretsError> {
            Ok(String::new())
        }
        async fn transit_decrypt(&self, _: &str, _: &str) -> Result<Vec<u8>, SecretsError> {
            Ok(vec![])
        }
    }

    /// Regression: ADR-117 audit pass 3 SEV-2-C — `IssueEnrollmentToken::issue`
    /// must reject malformed Vault transit envelopes. Prior code silently
    /// fell through to base64-decode garbage.
    #[tokio::test]
    async fn issue_rejects_malformed_transit_signature() {
        use crate::domain::shared_kernel::TenantId;
        use std::sync::Arc;

        for malformed in ["justbase64", "vault::abc", "vault:notv:abc", "vault:v1:"] {
            let svc = IssueEnrollmentToken::new(
                Arc::new(MalformedTransitStore { raw: malformed }),
                "issuer".to_string(),
                "endpoint".to_string(),
                "transit/keys/test".to_string(),
            );
            let tenant = TenantId::new("t-test").unwrap();
            let err = svc
                .issue(&tenant, "operator")
                .await
                .expect_err(&format!("expected error for malformed input {malformed:?}"));
            let msg = format!("{err}");
            assert!(
                msg.contains("transit_sign parse")
                    || msg.contains("unexpected transit_sign format"),
                "unexpected error for {malformed:?}: {msg}"
            );
        }
    }
}
