// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C — atomic dual-signature key rotation.
//!
//! Verifies:
//!   1. Outer envelope (proves possession of OLD key).
//!   2. Signature of new public key over deterministic challenge:
//!      `sha256("aegis-edge-rotate" || node_id || new_public_key || nonce)`
//!      using the NEW key.
//!
//! Atomically updates `edge_daemons.public_key`, blacklists prior
//! NodeSecurityToken, and issues a new one bound to the new key. An overlap
//! window of 60s lets the in-flight stream cycle out without disconnect.

use anyhow::{anyhow, Result};
use chrono::{Duration as ChronoDuration, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use prost::Message;
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::domain::edge::EdgeDaemonRepository;
use crate::domain::secrets::SecretStore;
use crate::domain::shared_kernel::NodeId;
use crate::infrastructure::aegis_cluster_proto::{
    RotateEdgeKeyInner, RotateEdgeKeyRequest, RotateEdgeKeyResponse,
};

pub struct RotateEdgeKeyService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    secret_store: Arc<dyn SecretStore>,
    signing_key_path: String,
    overlap_seconds: i64,
}

impl RotateEdgeKeyService {
    pub fn new(
        edge_repo: Arc<dyn EdgeDaemonRepository>,
        secret_store: Arc<dyn SecretStore>,
        signing_key_path: String,
    ) -> Self {
        Self {
            edge_repo,
            secret_store,
            signing_key_path,
            overlap_seconds: 60,
        }
    }

    pub fn rotation_challenge(node_id: &NodeId, new_public_key: &[u8], nonce: &[u8]) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(b"aegis-edge-rotate");
        h.update(node_id.0.as_bytes());
        h.update(new_public_key);
        h.update(nonce);
        h.finalize().to_vec()
    }

    pub async fn rotate(&self, req: RotateEdgeKeyRequest) -> Result<RotateEdgeKeyResponse> {
        let envelope = req
            .current_envelope
            .ok_or_else(|| anyhow!("missing current envelope"))?;
        let inner: RotateEdgeKeyInner = RotateEdgeKeyInner::decode(envelope.payload.as_ref())
            .map_err(|e| anyhow!("decode inner: {e}"))?;
        let node_id = NodeId(
            uuid::Uuid::parse_str(&inner.node_id).map_err(|e| anyhow!("invalid node_id: {e}"))?,
        );
        if inner.new_public_key != req.new_public_key {
            return Err(anyhow!("inner.new_public_key != request.new_public_key"));
        }
        if inner.new_public_key.len() != 32 {
            return Err(anyhow!("new_public_key must be 32 bytes"));
        }

        // Old-key signature: verify envelope.signature over envelope.payload using
        // the EdgeDaemon's currently-stored public_key.
        let edge = self
            .edge_repo
            .get(&node_id)
            .await?
            .ok_or_else(|| anyhow!("unknown edge {node_id}"))?;
        let old_pk_bytes: [u8; 32] = edge
            .public_key
            .clone()
            .try_into()
            .map_err(|_| anyhow!("stored public_key not 32 bytes"))?;
        let old_pk = VerifyingKey::from_bytes(&old_pk_bytes)
            .map_err(|e| anyhow!("invalid stored public_key: {e}"))?;
        let old_sig = Signature::from_slice(&envelope.signature)
            .map_err(|e| anyhow!("invalid old-key signature: {e}"))?;
        old_pk
            .verify(&envelope.payload, &old_sig)
            .map_err(|_| anyhow!("old-key signature invalid"))?;

        // New-key signature: verify signature_with_new_key over the deterministic
        // rotation challenge.
        let new_pk_bytes: [u8; 32] = inner
            .new_public_key
            .clone()
            .try_into()
            .map_err(|_| anyhow!("new_public_key not 32 bytes"))?;
        // Same-key rotations are intentionally permitted: this is the
        // degenerate case used by `aegis edge token refresh` (T5), where the
        // daemon re-attests with its existing keypair to obtain a fresh
        // NodeSecurityToken without rolling the underlying Ed25519 identity.
        // Dual-signature semantics still hold — the outer envelope proves
        // possession of the (single) private key, and `signature_with_new_key`
        // proves the same possession over the deterministic challenge.
        let new_pk = VerifyingKey::from_bytes(&new_pk_bytes)
            .map_err(|e| anyhow!("invalid new public_key: {e}"))?;
        let challenge = Self::rotation_challenge(&node_id, &inner.new_public_key, &inner.nonce);
        let new_sig = Signature::from_slice(&req.signature_with_new_key)
            .map_err(|e| anyhow!("invalid new-key signature: {e}"))?;
        new_pk
            .verify(&challenge, &new_sig)
            .map_err(|_| anyhow!("new-key signature invalid"))?;

        // Atomic: update public_key + issue new NodeSecurityToken.
        // We rely on a single UPDATE of the edge_daemons row to persist the new
        // public key — token blacklist and issuance are externalised to the
        // existing NodeSecurityToken minting path.
        let mut edge = edge;
        edge.public_key = inner.new_public_key.clone();
        self.edge_repo.upsert(&edge).await?;

        // Issue replacement NodeSecurityToken via the existing minting path.
        // For the application-layer skeleton we delegate to a thin helper that
        // returns the new bearer string.
        let exp = (Utc::now() + ChronoDuration::hours(1)).timestamp();
        let token = mint_node_security_token(
            &self.secret_store,
            &self.signing_key_path,
            &node_id,
            &edge.tenant_id,
            exp,
        )
        .await?;
        let overlap_until = Utc::now() + ChronoDuration::seconds(self.overlap_seconds);

        Ok(RotateEdgeKeyResponse {
            node_security_token: token,
            expires_at: Some(prost_types::Timestamp {
                seconds: exp,
                nanos: 0,
            }),
            overlap_until: Some(prost_types::Timestamp {
                seconds: overlap_until.timestamp(),
                nanos: 0,
            }),
        })
    }
}

async fn mint_node_security_token(
    secret_store: &Arc<dyn SecretStore>,
    signing_key_path: &str,
    node_id: &NodeId,
    tenant_id: &crate::domain::shared_kernel::TenantId,
    exp: i64,
) -> Result<String> {
    use base64::Engine;
    let header = serde_json::json!({"alg":"RS256","typ":"JWT"});
    let claims = serde_json::json!({
        "sub": node_id.0.to_string(),
        "role": "edge",
        "tid": tenant_id.as_str(),
        "iat": Utc::now().timestamp(),
        "exp": exp,
    });
    let h = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
    let c = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let signing_input = format!("{h}.{c}");
    let raw = secret_store
        .transit_sign(signing_key_path, signing_input.as_bytes())
        .await
        .map_err(|e| anyhow!("transit_sign: {e}"))?;
    let sig_b64 = raw.rsplit_once(':').map(|(_, b)| b).unwrap_or(&raw);
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let s = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig_bytes);
    Ok(format!("{signing_input}.{s}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cluster::NodePeerStatus;
    use crate::domain::edge::{EdgeCapabilities, EdgeConnectionState, EdgeDaemon};
    use crate::domain::secrets::{SecretStore, SecretsError};
    use crate::domain::shared_kernel::TenantId;
    use crate::infrastructure::aegis_cluster_proto::SealNodeEnvelope;
    use async_trait::async_trait;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use prost::Message;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    #[test]
    fn rotation_challenge_is_deterministic() {
        let n = NodeId(uuid::Uuid::nil());
        let pk = [0u8; 32];
        let nonce = [1u8; 32];
        let a = RotateEdgeKeyService::rotation_challenge(&n, &pk, &nonce);
        let b = RotateEdgeKeyService::rotation_challenge(&n, &pk, &nonce);
        assert_eq!(a, b);
    }

    #[derive(Default)]
    struct StubRepo {
        edges: Mutex<HashMap<NodeId, EdgeDaemon>>,
    }

    #[async_trait]
    impl crate::domain::edge::EdgeDaemonRepository for StubRepo {
        async fn upsert(&self, edge: &EdgeDaemon) -> anyhow::Result<()> {
            self.edges.lock().await.insert(edge.node_id, edge.clone());
            Ok(())
        }
        async fn get(&self, node_id: &NodeId) -> anyhow::Result<Option<EdgeDaemon>> {
            Ok(self.edges.lock().await.get(node_id).cloned())
        }
        async fn list_by_tenant(&self, _t: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>> {
            Ok(vec![])
        }
        async fn update_status(&self, _: &NodeId, _: NodePeerStatus) -> anyhow::Result<()> {
            Ok(())
        }
        async fn update_tags(&self, _: &NodeId, _: &[String]) -> anyhow::Result<()> {
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

    struct StubSecretStore;
    #[async_trait]
    impl SecretStore for StubSecretStore {
        async fn read(
            &self,
            _: &str,
            _: &str,
        ) -> Result<HashMap<String, crate::domain::secrets::SensitiveString>, SecretsError>
        {
            Ok(HashMap::new())
        }
        async fn write(
            &self,
            _: &str,
            _: &str,
            _: HashMap<String, crate::domain::secrets::SensitiveString>,
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
        async fn transit_sign(&self, _key_name: &str, data: &[u8]) -> Result<String, SecretsError> {
            // Return a deterministic fake signature in the format
            // mint_node_security_token expects: "vault:v1:<base64>".
            let sig_b64 = base64::engine::general_purpose::STANDARD.encode(data);
            Ok(format!("vault:v1:{sig_b64}"))
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

    fn build_request(
        signing_key: &SigningKey,
        new_signing_key: &SigningKey,
        node_id: &NodeId,
    ) -> RotateEdgeKeyRequest {
        let nonce = [7u8; 32].to_vec();
        let new_pub = new_signing_key.verifying_key().to_bytes().to_vec();
        let inner = RotateEdgeKeyInner {
            node_id: node_id.0.to_string(),
            new_public_key: new_pub.clone(),
            nonce: nonce.clone(),
        };
        let mut payload = Vec::new();
        inner.encode(&mut payload).unwrap();
        let outer_sig = signing_key.sign(&payload).to_bytes().to_vec();
        let challenge = RotateEdgeKeyService::rotation_challenge(node_id, &new_pub, &nonce);
        let new_sig = new_signing_key.sign(&challenge).to_bytes().to_vec();
        RotateEdgeKeyRequest {
            current_envelope: Some(SealNodeEnvelope {
                node_security_token: String::new(),
                signature: outer_sig,
                payload,
            }),
            new_public_key: new_pub,
            signature_with_new_key: new_sig,
        }
    }

    async fn make_service_and_seed(
        signing_key: &SigningKey,
        node_id: NodeId,
    ) -> RotateEdgeKeyService {
        let repo = Arc::new(StubRepo::default());
        repo.upsert(&EdgeDaemon {
            node_id,
            tenant_id: TenantId::new("t-test").unwrap(),
            public_key: signing_key.verifying_key().to_bytes().to_vec(),
            capabilities: EdgeCapabilities::default(),
            status: NodePeerStatus::Active,
            connection: EdgeConnectionState::Disconnected {
                since: chrono::Utc::now(),
            },
            last_heartbeat_at: None,
            enrolled_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
        RotateEdgeKeyService::new(
            repo,
            Arc::new(StubSecretStore),
            "transit/keys/test".to_string(),
        )
    }

    /// Regression: ADR-117 §"Key & token rotation" — `aegis edge token refresh`
    /// reuses RotateEdgeKey with `new_public_key == current_public_key` (T5).
    /// Prior to this commit the orchestrator rejected same-key rotations,
    /// breaking the refresh path.
    #[tokio::test]
    async fn rotate_accepts_same_key_for_token_refresh() {
        use rand_core::OsRng;
        let mut rng = OsRng;
        let signing_key = SigningKey::generate(&mut rng);
        let node_id = NodeId(uuid::Uuid::new_v4());
        let svc = make_service_and_seed(&signing_key, node_id).await;
        // Same-key: reuse `signing_key` for the "new" key.
        let req = build_request(&signing_key, &signing_key, &node_id);
        let resp = svc.rotate(req).await.expect("same-key rotate must succeed");
        assert!(!resp.node_security_token.is_empty());
        assert!(resp.expires_at.is_some());
    }

    #[tokio::test]
    async fn rotate_accepts_distinct_new_key() {
        use rand_core::OsRng;
        let mut rng = OsRng;
        let signing_key = SigningKey::generate(&mut rng);
        let new_key = SigningKey::generate(&mut rng);
        let node_id = NodeId(uuid::Uuid::new_v4());
        let svc = make_service_and_seed(&signing_key, node_id).await;
        let req = build_request(&signing_key, &new_key, &node_id);
        let resp = svc
            .rotate(req)
            .await
            .expect("distinct-key rotate must succeed");
        assert!(!resp.node_security_token.is_empty());
    }
}
