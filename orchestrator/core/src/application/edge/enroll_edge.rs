// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C — validate enrollment JWT, redeem `jti`, persist EdgeDaemon.
//!
//! Invoked from `ChallengeNode` when `bootstrap_proof::EnrollmentToken` is
//! present and the role is `Edge`. Returns the parsed claims so the caller
//! can mint a `NodeSecurityToken` carrying `tid`/`cep`.

use anyhow::{anyhow, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use std::sync::Arc;

use crate::domain::cluster::NodePeerStatus;
use crate::domain::edge::{
    EdgeCapabilities, EdgeConnectionState, EdgeDaemon, EdgeDaemonRepository, EnrollmentTokenClaims,
    EnrollmentTokenError, EnrollmentTokenRepository,
};
use crate::domain::secrets::SecretStore;
use crate::domain::shared_kernel::{NodeId, TenantId};

#[derive(thiserror::Error, Debug)]
pub enum EnrollEdgeError {
    #[error("invalid enrollment token format")]
    InvalidTokenFormat,
    #[error("token signature invalid")]
    InvalidSignature,
    #[error("token expired or not yet valid")]
    Expired,
    #[error("token audience mismatch (expected edge-enrollment)")]
    AudienceMismatch,
    #[error("token already redeemed")]
    AlreadyRedeemed,
    #[error("invalid tenant id: {0}")]
    InvalidTenant(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct EnrollEdgeService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    token_repo: Arc<dyn EnrollmentTokenRepository>,
    secret_store: Arc<dyn SecretStore>,
    signing_key_path: String,
    expected_issuer: String,
}

impl EnrollEdgeService {
    pub fn new(
        edge_repo: Arc<dyn EdgeDaemonRepository>,
        token_repo: Arc<dyn EnrollmentTokenRepository>,
        secret_store: Arc<dyn SecretStore>,
        signing_key_path: String,
        expected_issuer: String,
    ) -> Self {
        Self {
            edge_repo,
            token_repo,
            secret_store,
            signing_key_path,
            expected_issuer,
        }
    }

    /// Validate the JWT, atomically redeem its `jti`, persist a fresh
    /// EdgeDaemon row, and return the parsed claims.
    pub async fn enroll(
        &self,
        token: &str,
        node_id: NodeId,
        public_key: Vec<u8>,
        capabilities: EdgeCapabilities,
    ) -> Result<EnrollmentTokenClaims, EnrollEdgeError> {
        let claims = self.verify_and_parse(token).await?;

        // Audience check.
        if claims.aud != "edge-enrollment" {
            return Err(EnrollEdgeError::AudienceMismatch);
        }
        // Issuer check.
        if claims.iss != self.expected_issuer {
            return Err(EnrollEdgeError::Other(anyhow!(
                "issuer mismatch: got {}, expected {}",
                claims.iss,
                self.expected_issuer
            )));
        }
        // Time window.
        let now = Utc::now().timestamp();
        if claims.exp <= now || claims.nbf > now {
            return Err(EnrollEdgeError::Expired);
        }

        let tenant_id = TenantId::new(claims.tid.clone())
            .map_err(|e| EnrollEdgeError::InvalidTenant(e.to_string()))?;

        // One-time use: atomic INSERT.
        let exp_at: DateTime<Utc> = DateTime::from_timestamp(claims.exp, 0)
            .ok_or_else(|| EnrollEdgeError::Other(anyhow!("invalid exp timestamp")))?;
        match self
            .token_repo
            .redeem(claims.jti, &tenant_id, &claims.sub, exp_at)
            .await
        {
            Ok(()) => {}
            Err(EnrollmentTokenError::AlreadyRedeemed) => {
                return Err(EnrollEdgeError::AlreadyRedeemed)
            }
            Err(EnrollmentTokenError::NotFound) => {
                return Err(EnrollEdgeError::Other(anyhow!("token not found")))
            }
            Err(EnrollmentTokenError::Other(e)) => return Err(EnrollEdgeError::Other(e)),
        }

        // The friendly name supplied at enrollment-token issuance is the
        // sole legitimate use of `claims.sub` here: Zaru's "Add Edge Host"
        // dialog feeds the operator-typed label through `issued_to` →
        // `Claims.sub`, where it survives a single decode and lands on the
        // EdgeDaemon row as the operator-facing display name. After this
        // point the JWT is consumed and the row owns the label.
        let display_name = claims.sub.clone();
        let edge = EdgeDaemon {
            node_id,
            tenant_id: tenant_id.clone(),
            public_key,
            capabilities,
            status: NodePeerStatus::Active,
            connection: EdgeConnectionState::Disconnected { since: Utc::now() },
            last_heartbeat_at: None,
            enrolled_at: Utc::now(),
            display_name,
        };
        self.edge_repo.upsert(&edge).await?;
        Ok(claims)
    }

    async fn verify_and_parse(
        &self,
        token: &str,
    ) -> Result<EnrollmentTokenClaims, EnrollEdgeError> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err(EnrollEdgeError::InvalidTokenFormat);
        }
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|_| EnrollEdgeError::InvalidTokenFormat)?;
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig_bytes);
        let vault_sig = format!("vault:v1:{sig_b64}");

        let ok = self
            .secret_store
            .transit_verify(&self.signing_key_path, signing_input.as_bytes(), &vault_sig)
            .await
            .map_err(|e| EnrollEdgeError::Other(anyhow!("transit_verify: {e}")))?;
        if !ok {
            return Err(EnrollEdgeError::InvalidSignature);
        }

        let claims_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|_| EnrollEdgeError::InvalidTokenFormat)?;
        let claims: EnrollmentTokenClaims = serde_json::from_slice(&claims_json)
            .map_err(|_| EnrollEdgeError::InvalidTokenFormat)?;
        Ok(claims)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cluster::NodePeerStatus;
    use crate::domain::edge::EdgeCapabilities;
    use crate::domain::secrets::{SecretStore, SecretsError, SensitiveString};
    use async_trait::async_trait;
    use base64::Engine;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Mutex;

    /// Captures the last upserted EdgeDaemon so the test can assert
    /// `display_name` was populated from `claims.sub`.
    #[derive(Default)]
    struct CapturingEdgeRepo {
        last: Mutex<Option<crate::domain::edge::EdgeDaemon>>,
    }

    #[async_trait]
    impl crate::domain::edge::EdgeDaemonRepository for CapturingEdgeRepo {
        async fn upsert(&self, edge: &crate::domain::edge::EdgeDaemon) -> anyhow::Result<()> {
            *self.last.lock().await = Some(edge.clone());
            Ok(())
        }
        async fn get(
            &self,
            _node_id: &NodeId,
        ) -> anyhow::Result<Option<crate::domain::edge::EdgeDaemon>> {
            Ok(None)
        }
        async fn list_by_tenant(
            &self,
            _tenant_id: &TenantId,
        ) -> anyhow::Result<Vec<crate::domain::edge::EdgeDaemon>> {
            Ok(vec![])
        }
        async fn update_status(&self, _: &NodeId, _: NodePeerStatus) -> anyhow::Result<()> {
            Ok(())
        }
        async fn update_tags(&self, _: &NodeId, _: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn update_display_name(&self, _: &NodeId, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn update_capabilities(
            &self,
            _: &NodeId,
            _: &EdgeCapabilities,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &NodeId) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct AcceptOnceTokenRepo {
        called: StdMutex<bool>,
    }

    #[async_trait]
    impl crate::domain::edge::EnrollmentTokenRepository for AcceptOnceTokenRepo {
        async fn redeem(
            &self,
            _jti: uuid::Uuid,
            _tenant: &TenantId,
            _issued_to: &str,
            _exp: chrono::DateTime<chrono::Utc>,
        ) -> Result<(), crate::domain::edge::EnrollmentTokenError> {
            let mut c = self.called.lock().unwrap();
            if *c {
                return Err(crate::domain::edge::EnrollmentTokenError::AlreadyRedeemed);
            }
            *c = true;
            Ok(())
        }
    }

    /// Always-accept verifier: the unit test focuses on the post-verify
    /// assignment of `display_name`, not the JWS path covered elsewhere.
    struct AlwaysVerifyStore;
    #[async_trait]
    impl SecretStore for AlwaysVerifyStore {
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
            Ok(String::new())
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

    fn make_jwt(claims: &serde_json::Value) -> String {
        let header_b = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let claims_b = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(claims).unwrap());
        // Signature segment is opaque under the AlwaysVerifyStore; supply
        // a valid base64 byte to satisfy decode.
        let sig_b = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header_b}.{claims_b}.{sig_b}")
    }

    /// Regression: ADR-117 friendly name persistence. Zaru's "Add Edge
    /// Host" dialog feeds the operator-typed label through `issued_to` →
    /// JWT `sub`. Before this fix the orchestrator decoded `sub`,
    /// validated it once, then dropped it on the floor — so the hosts
    /// list had no label to display. This test enrolls a daemon and
    /// asserts the constructed `EdgeDaemon` carries the JWT `sub` as
    /// `display_name`.
    #[tokio::test]
    async fn enroll_populates_display_name_from_jwt_sub() {
        let edge_repo = std::sync::Arc::new(CapturingEdgeRepo::default());
        let token_repo = std::sync::Arc::new(AcceptOnceTokenRepo {
            called: StdMutex::new(false),
        });
        let svc = EnrollEdgeService::new(
            edge_repo.clone(),
            token_repo,
            std::sync::Arc::new(AlwaysVerifyStore),
            "transit/keys/test".into(),
            "issuer.test".into(),
        );

        let now = chrono::Utc::now();
        let claims = serde_json::json!({
            "tid": "t-test",
            "sub": "home-laptop",
            "jti": uuid::Uuid::new_v4(),
            "exp": (now + chrono::Duration::minutes(5)).timestamp(),
            "nbf": (now - chrono::Duration::minutes(1)).timestamp(),
            "aud": "edge-enrollment",
            "iss": "issuer.test",
            "cep": "controller.test:443",
        });
        let jwt = make_jwt(&claims);

        let result = svc
            .enroll(
                &jwt,
                NodeId::new(),
                vec![0u8; 32],
                EdgeCapabilities::default(),
            )
            .await
            .expect("enroll must succeed under always-verify stub");

        assert_eq!(
            result.sub, "home-laptop",
            "claims.sub must round-trip through verify_and_parse"
        );

        let stored = edge_repo
            .last
            .lock()
            .await
            .clone()
            .expect("EnrollEdgeService must upsert exactly one EdgeDaemon");
        assert_eq!(
            stored.display_name, "home-laptop",
            "display_name must be populated from claims.sub at enrollment \
             — Zaru's hosts list and rename UX depend on this field"
        );
    }
}
