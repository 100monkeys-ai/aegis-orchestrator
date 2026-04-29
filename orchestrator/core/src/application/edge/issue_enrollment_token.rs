// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C — mint a short-lived enrollment JWT.
//!
//! The token is signed by the same OpenBao Transit key path that signs
//! NodeSecurityTokens. Validity is 15 minutes; one-time-use is enforced
//! server-side by the `enrollment_tokens` table at redemption.

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::domain::secrets::SecretStore;
use crate::domain::shared_kernel::TenantId;

/// OpenBao Transit signing key name for edge enrollment tokens.
///
/// MUST match the policy granted to the `relay-coordinator` AppRole in
/// `aegis-platform-deployment/scripts/bootstrap-openbao.sh`
/// (`transit/sign/edge-enrollment-token`). Pinned as a constant so a
/// regression test can assert the wiring at compile-time references this
/// exact name and not a stale literal like `aegis-node-controller-key`.
pub const EDGE_ENROLLMENT_SIGNING_KEY: &str = "edge-enrollment-token";

/// Behavior contract for issuing edge enrollment tokens.
///
/// ADR-117 SaaS topology splits signing capability: only the
/// `relay-coordinator` AppRole has the OpenBao policy to sign on
/// `transit/sign/edge-enrollment-token`. The core orchestrator (running
/// alongside a Relay Coordinator pod) MUST proxy enrollment-token requests
/// to the Relay over the trusted in-pod network rather than attempt to
/// sign locally.
///
/// Two impls exist:
/// * [`IssueEnrollmentToken`] — local signer, used by Relay Coordinator,
///   Hybrid (single-node), and Controller-without-relay-peer deployments.
/// * [`RelayProxyEnrollmentTokenIssuer`] — internal HTTP proxy used by
///   Controller deployments configured with a `relay_coordinator_endpoint`.
#[async_trait]
pub trait EnrollmentTokenIssuer: Send + Sync {
    async fn issue(
        &self,
        tenant_id: &TenantId,
        issued_to_sub: &str,
        bearer_token: Option<&str>,
    ) -> Result<IssuedEnrollmentToken>;
}

/// Output of [`EnrollmentTokenIssuer::issue`].
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Direct signing path. Use this when this process holds the
    /// signing capability. The trait method [`EnrollmentTokenIssuer::issue`]
    /// is the recommended entry point.
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

#[async_trait]
impl EnrollmentTokenIssuer for IssueEnrollmentToken {
    async fn issue(
        &self,
        tenant_id: &TenantId,
        issued_to_sub: &str,
        _bearer_token: Option<&str>,
    ) -> Result<IssuedEnrollmentToken> {
        IssueEnrollmentToken::issue(self, tenant_id, issued_to_sub).await
    }
}

/// HTTP proxy that forwards enrollment-token requests from a Controller
/// process to a co-located Relay Coordinator.
///
/// ADR-117: only the `relay-coordinator` AppRole holds the OpenBao policy
/// `transit/sign/edge-enrollment-token`. In SaaS deployments the core
/// orchestrator pod sits alongside a separate `aegis-relay-coordinator` pod
/// reachable on the trusted Podman pod-network. The user's Bearer token is
/// forwarded so the Relay's IAM middleware can authenticate the request
/// against the same `UserIdentity`/`effective_tenant`.
pub struct RelayProxyEnrollmentTokenIssuer {
    http: reqwest::Client,
    /// Base URL of the Relay Coordinator's REST surface, e.g.
    /// `http://aegis-relay-coordinator:8088`.
    endpoint: String,
}

impl RelayProxyEnrollmentTokenIssuer {
    pub fn new(endpoint: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("build reqwest client"),
            endpoint,
        }
    }

    pub fn with_client(endpoint: String, http: reqwest::Client) -> Self {
        Self { http, endpoint }
    }
}

#[derive(Serialize)]
struct ProxyRequest<'a> {
    issued_to: &'a str,
}

#[derive(Deserialize)]
struct ProxyResponse {
    token: String,
    expires_at: String,
    controller_endpoint: String,
    qr_payload: String,
    command_hint: String,
}

#[async_trait]
impl EnrollmentTokenIssuer for RelayProxyEnrollmentTokenIssuer {
    async fn issue(
        &self,
        tenant_id: &TenantId,
        issued_to_sub: &str,
        bearer_token: Option<&str>,
    ) -> Result<IssuedEnrollmentToken> {
        let url = format!(
            "{}/v1/edge/enrollment-tokens",
            self.endpoint.trim_end_matches('/')
        );
        let mut req = self
            .http
            .post(&url)
            .json(&ProxyRequest {
                issued_to: issued_to_sub,
            })
            .header("X-Tenant-Id", tenant_id.as_str());
        if let Some(tok) = bearer_token {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("relay proxy request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("relay proxy returned {}: {}", status, body);
        }
        let body: ProxyResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("relay proxy response decode: {e}"))?;
        let expires_at = chrono::DateTime::parse_from_rfc3339(&body.expires_at)
            .map_err(|e| anyhow::anyhow!("relay proxy expires_at parse: {e}"))?
            .with_timezone(&Utc);
        Ok(IssuedEnrollmentToken {
            token: body.token,
            expires_at,
            controller_endpoint: body.controller_endpoint,
            qr_payload: body.qr_payload,
            command_hint: body.command_hint,
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

    /// Regression: ADR-117 OpenBao policy alignment. The signing key name
    /// MUST be `edge-enrollment-token` to match the policy granted to the
    /// `relay-coordinator` AppRole in `bootstrap-openbao.sh`. A previous
    /// bug used `aegis-node-controller-key`, causing OpenBao to return 403
    /// on every enrollment-token request from Zaru. This test pins the
    /// constant so any drift triggers a compile-time / test-time failure
    /// rather than a runtime 500.
    #[test]
    fn signing_key_constant_matches_openbao_policy() {
        assert_eq!(
            EDGE_ENROLLMENT_SIGNING_KEY, "edge-enrollment-token",
            "signing key name must match `transit/sign/edge-enrollment-token` \
             granted to the relay-coordinator AppRole — see \
             aegis-platform-deployment/scripts/bootstrap-openbao.sh"
        );
    }

    /// Regression: ADR-117 SaaS topology. The Relay proxy issuer MUST
    /// hit `/v1/edge/enrollment-tokens` on the configured endpoint and
    /// forward both the user's Bearer token and the resolved tenant id
    /// so the Relay can run the same IAM/tenant middleware against the
    /// same identity.
    #[tokio::test]
    async fn relay_proxy_forwards_bearer_and_tenant_to_v1_path() {
        use crate::domain::shared_kernel::TenantId;

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/edge/enrollment-tokens")
            .match_header("authorization", "Bearer caller-jwt")
            .match_header("x-tenant-id", "t-consumer")
            .match_body(mockito::Matcher::JsonString(
                r#"{"issued_to":"alice"}"#.into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "token": "proxied-token",
                    "expires_at": "2099-01-01T00:00:00Z",
                    "controller_endpoint": "relay.myzaru.com:443",
                    "qr_payload": "aegis edge enroll proxied-token",
                    "command_hint": "aegis edge enroll proxied-token"
                }"#,
            )
            .create_async()
            .await;

        let issuer = RelayProxyEnrollmentTokenIssuer::new(server.url());
        let tenant = TenantId::new("t-consumer").unwrap();
        let issued = issuer
            .issue(&tenant, "alice", Some("caller-jwt"))
            .await
            .expect("proxy must succeed");
        assert_eq!(issued.token, "proxied-token");
        assert_eq!(issued.controller_endpoint, "relay.myzaru.com:443");
        mock.assert_async().await;
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
