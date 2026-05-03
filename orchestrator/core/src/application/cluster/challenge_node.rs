// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::application::edge::enroll_edge::{EnrollEdgeError, EnrollEdgeService};
use crate::domain::cluster::{
    NodeChallengeRepository, NodeClusterRepository, NodeId, NodePeer, NodePeerStatus, NodeRole,
    NodeTokenClaims,
};
use crate::domain::secrets::SecretStore;
use anyhow::{anyhow, Result};
use base64::Engine;
use chrono::Utc;
use std::sync::Arc;

/// ADR-117: bootstrap proof carried alongside the challenge signature for
/// roles that require auxiliary attestation. Currently only `EnrollmentToken`
/// (used by `NODE_ROLE_EDGE`) is defined — mirrors the proto `oneof
/// bootstrap_proof { string enrollment_token = 4; }`.
#[derive(Debug, Clone)]
pub enum BootstrapProof {
    EnrollmentToken(String),
}

pub struct ChallengeNodeRequest {
    pub challenge_id: uuid::Uuid,
    pub node_id: NodeId,
    pub challenge_signature: Vec<u8>,
    /// ADR-117: optional bootstrap proof — required when `role == Edge`.
    pub bootstrap_proof: Option<BootstrapProof>,
}

pub struct ChallengeNodeResponse {
    pub node_security_token: String,
    pub expires_at: chrono::DateTime<Utc>,
}

pub struct ChallengeNodeUseCase {
    challenge_repo: Arc<dyn NodeChallengeRepository>,
    cluster_repo: Arc<dyn NodeClusterRepository>,
    secret_store: Arc<dyn SecretStore>,
    /// OpenBao Transit signing key path used to sign the issued
    /// `NodeSecurityToken`. ADR-117 §120 prescribes that the enrollment JWT
    /// and the `NodeSecurityToken` share the same Transit key — callers wire
    /// this from `EDGE_ENROLLMENT_SIGNING_KEY` so the relay-coordinator
    /// AppRole's `transit/sign/<key>` policy grant covers both signatures.
    signing_key_path: String,
    /// ADR-117: when present, edge daemons supplying a `BootstrapProof::
    /// EnrollmentToken` are validated and persisted via this service before
    /// minting their NodeSecurityToken. None on pure-worker controllers that
    /// do not host the edge enrollment surface.
    enroll_edge_service: Option<Arc<EnrollEdgeService>>,
}

impl ChallengeNodeUseCase {
    pub fn new(
        challenge_repo: Arc<dyn NodeChallengeRepository>,
        cluster_repo: Arc<dyn NodeClusterRepository>,
        secret_store: Arc<dyn SecretStore>,
        signing_key_path: String,
    ) -> Self {
        Self {
            challenge_repo,
            cluster_repo,
            secret_store,
            signing_key_path,
            enroll_edge_service: None,
        }
    }

    /// ADR-117: enable edge enrollment processing on this controller.
    pub fn with_enroll_edge_service(mut self, svc: Arc<EnrollEdgeService>) -> Self {
        self.enroll_edge_service = Some(svc);
        self
    }

    pub async fn execute(&self, req: ChallengeNodeRequest) -> Result<ChallengeNodeResponse> {
        // 1. Retrieve challenge
        let challenge = self
            .challenge_repo
            .get_challenge(&req.challenge_id)
            .await?
            .ok_or_else(|| anyhow!("Challenge not found or expired"))?;

        if challenge.node_id != req.node_id {
            return Err(anyhow!("Node ID mismatch"));
        }

        if challenge.is_expired() {
            return Err(anyhow!("Challenge expired"));
        }

        // 2. Verify Ed25519 signature
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let verifying_key = VerifyingKey::from_bytes(
            &challenge
                .public_key
                .clone()
                .try_into()
                .map_err(|_| anyhow!("Invalid public key length"))?,
        )
        .map_err(|e| anyhow!("Invalid public key: {}", e))?;

        let signature = Signature::from_slice(&req.challenge_signature)
            .map_err(|e| anyhow!("Invalid signature: {}", e))?;

        verifying_key
            .verify(&challenge.nonce, &signature)
            .map_err(|e| anyhow!("Signature verification failed: {}", e))?;

        // 3. ADR-117: for edge daemons, validate the enrollment JWT and
        //    persist the EdgeDaemon row before minting the NodeSecurityToken
        //    so the token can carry the binding `tid` / `cep` claims.
        let (edge_tid, edge_cep) = if challenge.role == NodeRole::Edge {
            let proof = req
                .bootstrap_proof
                .as_ref()
                .ok_or_else(|| anyhow!("Edge role requires bootstrap_proof::enrollment_token"))?;
            let BootstrapProof::EnrollmentToken(jwt) = proof;
            let svc = self
                .enroll_edge_service
                .as_ref()
                .ok_or_else(|| anyhow!("Edge enrollment not enabled on this controller"))?;

            // The capability hash on the challenge encodes the worker-style
            // NodeCapabilityAdvertisement; for edge daemons we accept the same
            // shape (no additional EdgeCapabilities are sent on the handshake;
            // they arrive on the first ConnectEdge HelloEvent).
            let edge_caps = crate::domain::edge::EdgeCapabilities::default();

            match svc
                .enroll(jwt, req.node_id, challenge.public_key.clone(), edge_caps)
                .await
            {
                Ok(claims) => (Some(claims.tid), Some(claims.cep)),
                Err(EnrollEdgeError::AlreadyRedeemed) => {
                    return Err(anyhow!("Enrollment token already redeemed"));
                }
                Err(EnrollEdgeError::Expired) => {
                    return Err(anyhow!("Enrollment token expired"));
                }
                Err(EnrollEdgeError::AudienceMismatch) => {
                    return Err(anyhow!("Enrollment token audience mismatch"));
                }
                Err(EnrollEdgeError::InvalidSignature) => {
                    return Err(anyhow!("Enrollment token signature invalid"));
                }
                Err(EnrollEdgeError::InvalidTokenFormat) => {
                    return Err(anyhow!("Enrollment token malformed"));
                }
                Err(EnrollEdgeError::InvalidTenant(e)) => {
                    return Err(anyhow!("Enrollment token tenant invalid: {e}"));
                }
                Err(EnrollEdgeError::Other(e)) => {
                    return Err(anyhow!("Edge enrollment failed: {e}"));
                }
            }
        } else {
            (None, None)
        };

        // 4. Issue NodeSecurityToken (RS256 JWT)
        // ADR-059: Signed by controller's OpenBao Transit key.
        let exp = (Utc::now() + chrono::Duration::hours(1)).timestamp();
        let iat = Utc::now().timestamp();

        let claims = NodeTokenClaims {
            node_id: req.node_id,
            role: challenge.role,
            capabilities_hash: challenge.capabilities.hash(),
            iat,
            exp,
            tid: edge_tid,
            cep: edge_cep,
        };

        // Construct JWT header and payload
        let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
        let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&header)?);
        let claims_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&claims)?);
        let signing_input = format!("{}.{}", header_b64, claims_b64);

        // Sign with OpenBao Transit. ADR-117: shares the
        // `EDGE_ENROLLMENT_SIGNING_KEY` Transit key with the enrollment JWT
        // so a single relay-coordinator AppRole policy grant covers both.
        let raw_sig = self
            .secret_store
            .transit_sign(&self.signing_key_path, signing_input.as_bytes())
            .await
            .map_err(|e| anyhow!("transit_sign failed: {e}"))?;
        // Vault Transit returns `vault:v<version>:<base64sig>` — use the
        // shared strict parser so arbitrary version numbers and malformed
        // envelopes are handled identically to `IssueEnrollmentToken`.
        let signature_b64 = crate::application::edge::transit::parse_vault_signature(&raw_sig)
            .map_err(|e| anyhow!("transit_sign parse: {e}"))?
            .to_string();

        let token = format!("{}.{}", signing_input, signature_b64);

        // 5. Cleanup challenge
        self.challenge_repo
            .delete_challenge(&req.challenge_id)
            .await?;

        // 6. UPSERT node peer in repository (it's now attested). Edge daemons
        //    are persisted in EdgeDaemonRepository by EnrollEdgeService above
        //    and intentionally never appear in the worker NodePeer set.
        if challenge.role != NodeRole::Edge {
            let peer = NodePeer {
                node_id: req.node_id,
                role: challenge.role,
                public_key: challenge.public_key,
                capabilities: challenge.capabilities,
                grpc_address: challenge.grpc_address,
                status: NodePeerStatus::Active,
                last_heartbeat_at: Utc::now(),
                registered_at: Utc::now(),
            };
            self.cluster_repo.upsert_peer(&peer).await?;
        }

        Ok(ChallengeNodeResponse {
            node_security_token: token,
            expires_at: Utc::now() + chrono::Duration::hours(1),
        })
    }
}

#[cfg(test)]
mod tests {
    //! Regression tests for ADR-117 OpenBao policy alignment on the
    //! `NodeSecurityToken` signing key. A prior implementation hardcoded
    //! `transit_sign("aegis-node-controller-key", ...)` — a Transit key that
    //! does not exist in OpenBao and is not granted by any AppRole policy.
    //! Every `ChallengeNode` RPC against a real OpenBao backend therefore
    //! returned 403, breaking edge enrollment and worker join. These tests
    //! pin the contract that the signing key flows through the constructor
    //! so future regressions fail the build instead of silently 403-ing in
    //! production.
    use super::*;
    use crate::application::edge::issue_enrollment_token::EDGE_ENROLLMENT_SIGNING_KEY;
    use crate::domain::cluster::{
        NodeCapabilityAdvertisement, NodeChallenge, NodeChallengeRepository, NodeClusterRepository,
        NodePeer, NodePeerStatus, NodeRole, ResourceSnapshot,
    };
    use crate::domain::secrets::{SecretStore, SecretsError, SensitiveString};
    use async_trait::async_trait;
    use chrono::Utc;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// Captures the `key_path` argument passed to `transit_sign` so the
    /// test can assert the use case forwards its configured signing key
    /// rather than re-hardcoding a literal.
    struct CapturingSecretStore {
        captured_key: Mutex<Option<String>>,
    }

    impl CapturingSecretStore {
        fn new() -> Self {
            Self {
                captured_key: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl SecretStore for CapturingSecretStore {
        async fn read(
            &self,
            _: &str,
            _: &str,
        ) -> Result<HashMap<String, SensitiveString>, SecretsError> {
            unimplemented!()
        }
        async fn write(
            &self,
            _: &str,
            _: &str,
            _: HashMap<String, SensitiveString>,
        ) -> Result<(), SecretsError> {
            unimplemented!()
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
            unimplemented!()
        }
        async fn revoke_lease(&self, _: &str) -> Result<(), SecretsError> {
            unimplemented!()
        }
        async fn transit_sign(&self, key: &str, _: &[u8]) -> Result<String, SecretsError> {
            *self.captured_key.lock().unwrap() = Some(key.to_string());
            // Return a well-formed Vault transit envelope so
            // `parse_vault_signature` succeeds and the use case proceeds.
            Ok("vault:v1:c2lnbmF0dXJl".to_string())
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

    /// In-memory `NodeChallengeRepository` storing one challenge.
    struct InMemoryChallengeRepo {
        challenge: Mutex<Option<NodeChallenge>>,
    }

    #[async_trait]
    impl NodeChallengeRepository for InMemoryChallengeRepo {
        async fn save_challenge(&self, c: &NodeChallenge) -> anyhow::Result<()> {
            *self.challenge.lock().unwrap() = Some(c.clone());
            Ok(())
        }
        async fn get_challenge(&self, id: &Uuid) -> anyhow::Result<Option<NodeChallenge>> {
            let c = self.challenge.lock().unwrap().clone();
            Ok(c.filter(|c| &c.challenge_id == id))
        }
        async fn delete_challenge(&self, _: &Uuid) -> anyhow::Result<()> {
            *self.challenge.lock().unwrap() = None;
            Ok(())
        }
    }

    /// `NodeClusterRepository` accepting all upserts.
    struct InMemoryClusterRepo;

    #[async_trait]
    impl NodeClusterRepository for InMemoryClusterRepo {
        async fn upsert_peer(&self, _: &NodePeer) -> anyhow::Result<()> {
            Ok(())
        }
        async fn find_peer(&self, _: &NodeId) -> anyhow::Result<Option<NodePeer>> {
            Ok(None)
        }
        async fn list_peers_by_status(&self, _: NodePeerStatus) -> anyhow::Result<Vec<NodePeer>> {
            Ok(vec![])
        }
        async fn record_heartbeat(&self, _: &NodeId, _: ResourceSnapshot) -> anyhow::Result<()> {
            Ok(())
        }
        async fn mark_unhealthy(&self, _: &NodeId) -> anyhow::Result<()> {
            Ok(())
        }
        async fn start_drain(&self, _: &NodeId) -> anyhow::Result<()> {
            Ok(())
        }
        async fn deregister(&self, _: &NodeId, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_config_version(&self, _: &NodeId) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        async fn record_config_version(&self, _: &NodeId, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_all_peers(&self) -> anyhow::Result<Vec<NodePeer>> {
            Ok(vec![])
        }
        async fn count_by_status(&self) -> anyhow::Result<HashMap<NodePeerStatus, usize>> {
            Ok(HashMap::new())
        }
    }

    /// Regression: `ChallengeNodeUseCase` MUST forward its constructor-supplied
    /// `signing_key_path` to `transit_sign`. The prior implementation hardcoded
    /// `"aegis-node-controller-key"` — a key that does not exist in OpenBao and
    /// is not granted by any AppRole policy. This test pins the wiring so the
    /// "let's just hardcode it" regression fails CI.
    #[tokio::test]
    async fn challenge_node_signs_with_configured_key() {
        // Build a Worker-role challenge (Edge would require an
        // EnrollEdgeService stub; Worker exercises the same signing path).
        let signing_key = SigningKey::generate(&mut rand_core::OsRng);
        let verifying_key = signing_key.verifying_key();
        let challenge_id = Uuid::new_v4();
        let node_id = NodeId::new();
        let nonce: Vec<u8> = (0..32u8).collect();

        let challenge = NodeChallenge {
            challenge_id,
            node_id,
            nonce: nonce.clone(),
            public_key: verifying_key.to_bytes().to_vec(),
            role: NodeRole::Worker,
            capabilities: NodeCapabilityAdvertisement::default(),
            grpc_address: "127.0.0.1:50050".to_string(),
            created_at: Utc::now(),
        };

        let challenge_repo: Arc<dyn NodeChallengeRepository> = Arc::new(InMemoryChallengeRepo {
            challenge: Mutex::new(Some(challenge.clone())),
        });
        let cluster_repo: Arc<dyn NodeClusterRepository> = Arc::new(InMemoryClusterRepo);
        let capturing = Arc::new(CapturingSecretStore::new());
        let secret_store: Arc<dyn SecretStore> = capturing.clone();

        let configured_key = EDGE_ENROLLMENT_SIGNING_KEY.to_string();
        let uc = ChallengeNodeUseCase::new(
            challenge_repo,
            cluster_repo,
            secret_store,
            configured_key.clone(),
        );

        // Sign the challenge nonce with the matching ed25519 private key.
        let signature = signing_key.sign(&nonce);

        let resp = uc
            .execute(ChallengeNodeRequest {
                challenge_id,
                node_id,
                challenge_signature: signature.to_bytes().to_vec(),
                bootstrap_proof: None,
            })
            .await
            .expect("challenge must succeed for happy-path Worker role");

        assert!(
            !resp.node_security_token.is_empty(),
            "issued token must be non-empty"
        );

        let captured = capturing
            .captured_key
            .lock()
            .unwrap()
            .clone()
            .expect("transit_sign must have been called");
        assert_eq!(
            captured, configured_key,
            "transit_sign must receive the constructor-supplied signing_key_path \
             (ADR-117: NodeSecurityToken and enrollment JWT share the same Transit key)"
        );
        assert_ne!(
            captured, "aegis-node-controller-key",
            "must NOT regress to the hardcoded literal — that key is not provisioned in OpenBao"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // ADR-117 EnrollEdgeService wiring regression
    //
    // `ChallengeNodeUseCase::with_enroll_edge_service` was a builder method
    // that no production boot path ever called. Both the relay-coordinator
    // (`cli/src/daemon/relay_server.rs`) and the controller/hybrid path
    // (`cli/src/daemon/server.rs`) constructed the use case without it, so
    // every ChallengeNode(role=Edge) RPC fell into the
    // `enroll_edge_service.is_some()` guard at line 122 and returned
    // `"Edge enrollment not enabled on this controller"`. The
    // `edge_daemons` row was therefore never written and the host stayed
    // invisible to Zaru and to every tenant-scoped /v1/edge/* API.
    //
    // These tests pin two contracts:
    //   1. When wired with an `EnrollEdgeService`, a successful Edge
    //      ChallengeNode persists the `EdgeDaemon` row via
    //      `EdgeDaemonRepository::upsert` AND skips the worker
    //      `NodePeer` upsert (Edge daemons live in `edge_daemons`, not
    //      `node_peers` — ADR-117 §C).
    //   2. The use case returns the JTI-already-redeemed error when the
    //      enrollment-token repo refuses the second redemption attempt;
    //      the upstream gRPC adapter maps this onto a non-OK status.
    // ────────────────────────────────────────────────────────────────────
    use crate::application::edge::enroll_edge::EnrollEdgeService;
    use crate::domain::edge::{
        EdgeCapabilities, EdgeDaemon, EdgeDaemonRepository, EnrollmentTokenError,
        EnrollmentTokenRepository,
    };
    use crate::domain::shared_kernel::TenantId;
    use chrono::DateTime;

    /// EdgeDaemonRepository capturing the most recent upsert so the test
    /// can assert that the fix actually persisted the daemon.
    struct CapturingEdgeRepo {
        upserted: Mutex<Option<EdgeDaemon>>,
    }

    impl CapturingEdgeRepo {
        fn new() -> Self {
            Self {
                upserted: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl EdgeDaemonRepository for CapturingEdgeRepo {
        async fn upsert(&self, edge: &EdgeDaemon) -> anyhow::Result<()> {
            *self.upserted.lock().unwrap() = Some(edge.clone());
            Ok(())
        }
        async fn get(&self, _: &NodeId) -> anyhow::Result<Option<EdgeDaemon>> {
            Ok(None)
        }
        async fn list_by_tenant(&self, _: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>> {
            Ok(vec![])
        }
        async fn update_status(&self, _: &NodeId, _: NodePeerStatus) -> anyhow::Result<()> {
            Ok(())
        }
        async fn record_heartbeat(&self, _: &NodeId) -> anyhow::Result<()> {
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

    /// EnrollmentTokenRepository that succeeds the first redeem and refuses
    /// every subsequent attempt with `AlreadyRedeemed`. Captures call count.
    struct OneShotTokenRepo {
        calls: Mutex<u32>,
    }

    impl OneShotTokenRepo {
        fn new() -> Self {
            Self {
                calls: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl EnrollmentTokenRepository for OneShotTokenRepo {
        async fn redeem(
            &self,
            _: uuid::Uuid,
            _: &TenantId,
            _: &str,
            _: DateTime<Utc>,
        ) -> Result<(), EnrollmentTokenError> {
            let mut n = self.calls.lock().unwrap();
            *n += 1;
            if *n == 1 {
                Ok(())
            } else {
                Err(EnrollmentTokenError::AlreadyRedeemed)
            }
        }
    }

    /// SecretStore for the Edge happy-path: `transit_verify` returns true
    /// (we are not testing OpenBao here), `transit_sign` returns a
    /// well-formed Vault transit envelope.
    struct EdgeStubSecretStore;
    #[async_trait]
    impl SecretStore for EdgeStubSecretStore {
        async fn read(
            &self,
            _: &str,
            _: &str,
        ) -> Result<HashMap<String, SensitiveString>, SecretsError> {
            unimplemented!()
        }
        async fn write(
            &self,
            _: &str,
            _: &str,
            _: HashMap<String, SensitiveString>,
        ) -> Result<(), SecretsError> {
            unimplemented!()
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
            unimplemented!()
        }
        async fn revoke_lease(&self, _: &str) -> Result<(), SecretsError> {
            unimplemented!()
        }
        async fn transit_sign(&self, _: &str, _: &[u8]) -> Result<String, SecretsError> {
            Ok("vault:v1:c2lnbmF0dXJl".to_string())
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

    /// Build a fake edge enrollment JWT whose payload deserializes into
    /// `EnrollmentTokenClaims`. Header and signature are placeholder bytes —
    /// `EdgeStubSecretStore::transit_verify` accepts them unconditionally.
    fn fake_edge_jwt(issuer: &str, tenant: &str) -> String {
        let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
        let now = Utc::now().timestamp();
        let claims = serde_json::json!({
            "tid": tenant,
            "sub": "operator-sub",
            "jti": uuid::Uuid::new_v4(),
            "exp": now + 3600,
            "nbf": now - 60,
            "aud": "edge-enrollment",
            "iss": issuer,
            "cep": "relay.example:50056",
        });
        let h = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&header).unwrap());
        let p = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&claims).unwrap());
        // Signature byte payload is irrelevant — `transit_verify` is stubbed.
        let s = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"fake-signature");
        format!("{h}.{p}.{s}")
    }

    /// Regression: with `with_enroll_edge_service` wired, a successful Edge
    /// ChallengeNode MUST upsert into `EdgeDaemonRepository`. This is the
    /// exact wiring that production was missing — the use case had the
    /// builder, but no boot path called it.
    #[tokio::test]
    async fn challenge_node_edge_role_persists_edge_daemon_via_enroll_service() {
        let signing_key = SigningKey::generate(&mut rand_core::OsRng);
        let verifying_key = signing_key.verifying_key();
        let challenge_id = Uuid::new_v4();
        let node_id = NodeId::new();
        let nonce: Vec<u8> = (0..32u8).collect();

        let challenge = NodeChallenge {
            challenge_id,
            node_id,
            nonce: nonce.clone(),
            public_key: verifying_key.to_bytes().to_vec(),
            role: NodeRole::Edge,
            capabilities: NodeCapabilityAdvertisement::default(),
            grpc_address: String::new(),
            created_at: Utc::now(),
        };

        let challenge_repo: Arc<dyn NodeChallengeRepository> = Arc::new(InMemoryChallengeRepo {
            challenge: Mutex::new(Some(challenge.clone())),
        });
        let cluster_repo: Arc<dyn NodeClusterRepository> = Arc::new(InMemoryClusterRepo);
        let secret_store: Arc<dyn SecretStore> = Arc::new(EdgeStubSecretStore);

        let edge_repo_concrete = Arc::new(CapturingEdgeRepo::new());
        let edge_repo: Arc<dyn EdgeDaemonRepository> = edge_repo_concrete.clone();
        let token_repo: Arc<dyn EnrollmentTokenRepository> = Arc::new(OneShotTokenRepo::new());

        let issuer = "https://relay.example".to_string();
        let enroll_edge_service = Arc::new(EnrollEdgeService::new(
            edge_repo,
            token_repo,
            secret_store.clone(),
            EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
            issuer.clone(),
        ));

        let uc = ChallengeNodeUseCase::new(
            challenge_repo,
            cluster_repo,
            secret_store,
            EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
        )
        .with_enroll_edge_service(enroll_edge_service);

        let signature = signing_key.sign(&nonce);
        let jwt = fake_edge_jwt(&issuer, "tenant-abc");

        let resp = uc
            .execute(ChallengeNodeRequest {
                challenge_id,
                node_id,
                challenge_signature: signature.to_bytes().to_vec(),
                bootstrap_proof: Some(BootstrapProof::EnrollmentToken(jwt)),
            })
            .await
            .expect("Edge ChallengeNode must succeed when EnrollEdgeService is wired");

        assert!(
            !resp.node_security_token.is_empty(),
            "issued NodeSecurityToken must be non-empty"
        );

        let upserted = edge_repo_concrete.upserted.lock().unwrap().clone().expect(
            "EdgeDaemonRepository::upsert MUST be called for a successful Edge \
                 ChallengeNode — this is the wiring the fix restores",
        );
        assert_eq!(
            upserted.node_id, node_id,
            "persisted EdgeDaemon must carry the challenge's node_id"
        );
        assert_eq!(
            upserted.tenant_id.as_str(),
            "tenant-abc",
            "persisted EdgeDaemon must carry the JWT's `tid` claim"
        );
    }

    /// Regression: a wired `ChallengeNodeUseCase` MUST refuse to mint a
    /// NodeSecurityToken when the enrollment token has already been
    /// redeemed — the `EnrollmentTokenRepository::redeem` call surfaces
    /// `AlreadyRedeemed` and the use case returns it as a domain error
    /// rather than silently re-issuing.
    #[tokio::test]
    async fn challenge_node_edge_role_rejects_replayed_enrollment_token() {
        let signing_key = SigningKey::generate(&mut rand_core::OsRng);
        let verifying_key = signing_key.verifying_key();
        let challenge_id = Uuid::new_v4();
        let node_id = NodeId::new();
        let nonce: Vec<u8> = (0..32u8).collect();

        let challenge = NodeChallenge {
            challenge_id,
            node_id,
            nonce: nonce.clone(),
            public_key: verifying_key.to_bytes().to_vec(),
            role: NodeRole::Edge,
            capabilities: NodeCapabilityAdvertisement::default(),
            grpc_address: String::new(),
            created_at: Utc::now(),
        };

        let challenge_repo: Arc<dyn NodeChallengeRepository> = Arc::new(InMemoryChallengeRepo {
            challenge: Mutex::new(Some(challenge)),
        });
        let cluster_repo: Arc<dyn NodeClusterRepository> = Arc::new(InMemoryClusterRepo);
        let secret_store: Arc<dyn SecretStore> = Arc::new(EdgeStubSecretStore);

        // Token repo that ALWAYS reports AlreadyRedeemed: simulates a JWT
        // whose JTI is already in the ledger.
        struct ReplayedTokenRepo;
        #[async_trait]
        impl EnrollmentTokenRepository for ReplayedTokenRepo {
            async fn redeem(
                &self,
                _: uuid::Uuid,
                _: &TenantId,
                _: &str,
                _: DateTime<Utc>,
            ) -> Result<(), EnrollmentTokenError> {
                Err(EnrollmentTokenError::AlreadyRedeemed)
            }
        }

        let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(CapturingEdgeRepo::new());
        let token_repo: Arc<dyn EnrollmentTokenRepository> = Arc::new(ReplayedTokenRepo);

        let issuer = "https://relay.example".to_string();
        let enroll_edge_service = Arc::new(EnrollEdgeService::new(
            edge_repo,
            token_repo,
            secret_store.clone(),
            EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
            issuer.clone(),
        ));

        let uc = ChallengeNodeUseCase::new(
            challenge_repo,
            cluster_repo,
            secret_store,
            EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
        )
        .with_enroll_edge_service(enroll_edge_service);

        let signature = signing_key.sign(&nonce);
        let jwt = fake_edge_jwt(&issuer, "tenant-abc");

        let result = uc
            .execute(ChallengeNodeRequest {
                challenge_id,
                node_id,
                challenge_signature: signature.to_bytes().to_vec(),
                bootstrap_proof: Some(BootstrapProof::EnrollmentToken(jwt)),
            })
            .await;
        let err = match result {
            Ok(_) => panic!("replayed JWT must fail ChallengeNode"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already redeemed"),
            "error must surface AlreadyRedeemed, got: {msg}"
        );
    }
}
