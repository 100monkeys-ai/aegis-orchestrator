// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL node envelopes (ADR-059, ADR-117, ADR-152)
//!
//! Every authenticated node RPC carries a `SealNodeEnvelope`. This module
//! is the one place that seals one and the one place that opens one; every
//! receiver (the cluster service, remote storage, `ConnectEdge`) opens
//! through [`NodeEnvelopeVerifier`], so the checks cannot drift apart.
//!
//! An envelope is accepted only when, in this order:
//!
//! 1. its `issued_at_ms` lies within [`FRESHNESS_WINDOW`] of this node's
//!    clock and its nonce is at least [`NONCE_LEN`] bytes;
//! 2. its `NodeSecurityToken` is an RS256 JWT whose signature the cluster's
//!    OpenBao Transit key verifies, and it has not expired;
//! 3. its Ed25519 signature over [`signed_bytes`] verifies against the key
//!    the token's node registered;
//! 4. this process has not already accepted that node's nonce.
//!
//! The nonce is remembered only after the signature verifies, so a forged
//! envelope cannot fill the memory, and only for as long as an envelope
//! bearing it could still pass step 1.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::domain::cluster::{NodeClusterRepository, NodeId};
use crate::domain::edge::EdgeDaemonRepository;
use crate::domain::secrets::SecretStore;
use crate::infrastructure::aegis_cluster_proto::SealNodeEnvelope;

/// Prefixes the signed bytes, so a node's signature over an envelope can
/// never be presented as its signature over anything else.
pub const SIGNED_BYTES_DOMAIN: &[u8] = b"aegis-seal-node/v1";

/// How far an envelope's `issued_at_ms` may lie from the receiver's clock,
/// either way.
pub const FRESHNESS_WINDOW: Duration = Duration::seconds(60);

/// The length of the nonce a sender draws for each envelope.
pub const NONCE_LEN: usize = 16;

/// The bytes the Ed25519 signature covers:
/// `domain || issued_at_ms (i64 BE) || len(nonce) (u32 BE) || nonce || payload`.
pub fn signed_bytes(issued_at_ms: i64, nonce: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(SIGNED_BYTES_DOMAIN.len() + 8 + 4 + nonce.len() + payload.len());
    out.extend_from_slice(SIGNED_BYTES_DOMAIN);
    out.extend_from_slice(&issued_at_ms.to_be_bytes());
    out.extend_from_slice(&(nonce.len() as u32).to_be_bytes());
    out.extend_from_slice(nonce);
    out.extend_from_slice(payload);
    out
}

/// Seal `payload` for sending: stamp it now, draw a fresh nonce, and sign.
pub fn seal(
    signing_key: &SigningKey,
    node_security_token: String,
    payload: Vec<u8>,
) -> SealNodeEnvelope {
    seal_at(signing_key, node_security_token, payload, Utc::now())
}

fn seal_at(
    signing_key: &SigningKey,
    node_security_token: String,
    payload: Vec<u8>,
    at: DateTime<Utc>,
) -> SealNodeEnvelope {
    let issued_at_ms = at.timestamp_millis();
    let mut nonce = vec![0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let signature = signing_key
        .sign(&signed_bytes(issued_at_ms, &nonce, &payload))
        .to_bytes()
        .to_vec();
    SealNodeEnvelope {
        node_security_token,
        signature,
        payload,
        issued_at_ms,
        nonce,
    }
}

/// Why an envelope was refused.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("missing security envelope")]
    Missing,
    #[error("envelope issued outside the freshness window")]
    Stale,
    #[error("envelope nonce shorter than {NONCE_LEN} bytes")]
    ShortNonce,
    #[error("malformed node security token: {0}")]
    Malformed(&'static str),
    #[error("node security token expired")]
    Expired,
    #[error("node security token signature does not verify")]
    TokenSignature,
    #[error("node is not registered")]
    UnknownNode,
    #[error("registered public key is invalid")]
    BadStoredKey,
    #[error("envelope signature does not verify")]
    Signature,
    #[error("envelope nonce already used")]
    Replayed,
    /// The secret store or a repository could not answer; the caller may retry.
    #[error("cannot verify now: {0}")]
    Unavailable(String),
}

impl From<EnvelopeError> for tonic::Status {
    fn from(e: EnvelopeError) -> Self {
        match e {
            EnvelopeError::Unavailable(_) => tonic::Status::unavailable(e.to_string()),
            EnvelopeError::BadStoredKey => tonic::Status::internal(e.to_string()),
            _ => tonic::Status::unauthenticated(e.to_string()),
        }
    }
}

/// What a verified token says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedNodeToken {
    pub node_id: NodeId,
    pub exp: i64,
    /// ADR-117: the tenant an edge daemon's token binds it to.
    pub tenant_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct TokenHeader {
    alg: String,
    kid: String,
}

#[derive(serde::Deserialize)]
struct TokenClaims {
    sub: NodeId,
    exp: i64,
    #[serde(default)]
    tid: Option<String>,
}

/// Verifies `NodeSecurityToken`s against the cluster's Transit key. A token
/// that verified is remembered until it expires, so Transit is asked once
/// per token rather than once per RPC.
pub struct NodeTokenVerifier {
    secret_store: Arc<dyn SecretStore>,
    signing_key_path: String,
    verified: Mutex<HashMap<[u8; 32], VerifiedNodeToken>>,
}

impl NodeTokenVerifier {
    pub fn new(secret_store: Arc<dyn SecretStore>, signing_key_path: String) -> Self {
        Self {
            secret_store,
            signing_key_path,
            verified: Mutex::new(HashMap::new()),
        }
    }

    pub async fn verify(&self, token: &str) -> Result<VerifiedNodeToken, EnvelopeError> {
        self.verify_at(token, Utc::now()).await
    }

    async fn verify_at(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<VerifiedNodeToken, EnvelopeError> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let cached = self
            .verified
            .lock()
            .expect("token cache")
            .get(&digest)
            .cloned();
        if let Some(known) = cached {
            if now.timestamp() >= known.exp {
                return Err(EnvelopeError::Expired);
            }
            return Ok(known);
        }

        let mut parts = token.split('.');
        let (Some(header_b64), Some(claims_b64), Some(sig_b64), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(EnvelopeError::Malformed("not three segments"));
        };
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header: TokenHeader = b64
            .decode(header_b64)
            .ok()
            .and_then(|h| serde_json::from_slice(&h).ok())
            .ok_or(EnvelopeError::Malformed("header"))?;
        if header.alg != "RS256" {
            return Err(EnvelopeError::Malformed("alg is not RS256"));
        }
        let version_ok = header
            .kid
            .strip_prefix('v')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !version_ok {
            return Err(EnvelopeError::Malformed("kid is not a Transit key version"));
        }
        let claims: TokenClaims = b64
            .decode(claims_b64)
            .ok()
            .and_then(|c| serde_json::from_slice(&c).ok())
            .ok_or(EnvelopeError::Malformed("claims"))?;
        if now.timestamp() >= claims.exp {
            return Err(EnvelopeError::Expired);
        }
        let signature = b64
            .decode(sig_b64)
            .map_err(|_| EnvelopeError::Malformed("signature"))?;
        let transit_signature = format!(
            "vault:{}:{}",
            header.kid,
            base64::engine::general_purpose::STANDARD.encode(signature)
        );
        let signing_input = format!("{header_b64}.{claims_b64}");
        let ok = self
            .secret_store
            .transit_verify(
                &self.signing_key_path,
                signing_input.as_bytes(),
                &transit_signature,
            )
            .await
            .map_err(|e| EnvelopeError::Unavailable(format!("transit_verify: {e}")))?;
        if !ok {
            return Err(EnvelopeError::TokenSignature);
        }

        let known = VerifiedNodeToken {
            node_id: claims.sub,
            exp: claims.exp,
            tenant_id: claims.tid,
        };
        let mut verified = self.verified.lock().expect("token cache");
        let now_s = now.timestamp();
        verified.retain(|_, t| t.exp > now_s);
        verified.insert(digest, known.clone());
        Ok(known)
    }
}

/// The Ed25519 key a node registered.
#[async_trait]
pub trait NodeKeys: Send + Sync {
    async fn public_key(&self, node_id: &NodeId) -> anyhow::Result<Option<Vec<u8>>>;
}

/// Cluster nodes: the key in `cluster_nodes`.
pub struct ClusterNodeKeys(pub Arc<dyn NodeClusterRepository>);

#[async_trait]
impl NodeKeys for ClusterNodeKeys {
    async fn public_key(&self, node_id: &NodeId) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.0.find_peer(node_id).await?.map(|p| p.public_key))
    }
}

/// Edge daemons (ADR-117): the key in `edge_daemons`.
pub struct EdgeDaemonKeys(pub Arc<dyn EdgeDaemonRepository>);

#[async_trait]
impl NodeKeys for EdgeDaemonKeys {
    async fn public_key(&self, node_id: &NodeId) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.0.get(node_id).await?.map(|e| e.public_key))
    }
}

/// The nonces this process has accepted, each kept until an envelope
/// bearing it could no longer be fresh.
#[derive(Default)]
struct NonceGuard {
    state: Mutex<NonceState>,
}

/// A node's nonce.
type NonceKey = (NodeId, Vec<u8>);

#[derive(Default)]
struct NonceState {
    seen: HashSet<NonceKey>,
    /// In the order accepted, with when to forget each; the forget time
    /// rises monotonically.
    order: VecDeque<(DateTime<Utc>, NonceKey)>,
}

impl NonceGuard {
    /// True when `nonce` is new for `node_id`; it is then remembered.
    fn admit(&self, node_id: NodeId, nonce: &[u8], now: DateTime<Utc>) -> bool {
        let mut state = self.state.lock().expect("nonce guard");
        while state
            .order
            .front()
            .is_some_and(|(forget_at, _)| *forget_at <= now)
        {
            if let Some((_, key)) = state.order.pop_front() {
                state.seen.remove(&key);
            }
        }
        let key = (node_id, nonce.to_vec());
        if !state.seen.insert(key.clone()) {
            return false;
        }
        // An envelope accepted now was issued no later than now + window, so
        // it stops being fresh by now + 2 * window.
        state.order.push_back((now + FRESHNESS_WINDOW * 2, key));
        true
    }
}

/// Opens `SealNodeEnvelope`s. One per process, shared by every receiver,
/// so a nonce accepted by one service is refused by all.
pub struct NodeEnvelopeVerifier {
    tokens: NodeTokenVerifier,
    nonces: NonceGuard,
}

impl NodeEnvelopeVerifier {
    pub fn new(secret_store: Arc<dyn SecretStore>, signing_key_path: String) -> Self {
        Self {
            tokens: NodeTokenVerifier::new(secret_store, signing_key_path),
            nonces: NonceGuard::default(),
        }
    }

    /// Verify `envelope` and return what its token says. Only then may the
    /// caller decode `envelope.payload`.
    pub async fn open(
        &self,
        envelope: &SealNodeEnvelope,
        keys: &dyn NodeKeys,
    ) -> Result<VerifiedNodeToken, EnvelopeError> {
        self.open_at(envelope, keys, Utc::now()).await
    }

    async fn open_at(
        &self,
        envelope: &SealNodeEnvelope,
        keys: &dyn NodeKeys,
        now: DateTime<Utc>,
    ) -> Result<VerifiedNodeToken, EnvelopeError> {
        let issued_at = DateTime::<Utc>::from_timestamp_millis(envelope.issued_at_ms)
            .ok_or(EnvelopeError::Stale)?;
        if (now - issued_at).abs() > FRESHNESS_WINDOW {
            return Err(EnvelopeError::Stale);
        }
        if envelope.nonce.len() < NONCE_LEN {
            return Err(EnvelopeError::ShortNonce);
        }

        let token = self
            .tokens
            .verify_at(&envelope.node_security_token, now)
            .await?;

        let public_key = keys
            .public_key(&token.node_id)
            .await
            .map_err(|e| EnvelopeError::Unavailable(format!("node key lookup: {e}")))?
            .ok_or(EnvelopeError::UnknownNode)?;
        let public_key: [u8; 32] = public_key
            .as_slice()
            .try_into()
            .map_err(|_| EnvelopeError::BadStoredKey)?;
        let verifying_key =
            VerifyingKey::from_bytes(&public_key).map_err(|_| EnvelopeError::BadStoredKey)?;
        let signature =
            Signature::from_slice(&envelope.signature).map_err(|_| EnvelopeError::Signature)?;
        verifying_key
            .verify(
                &signed_bytes(envelope.issued_at_ms, &envelope.nonce, &envelope.payload),
                &signature,
            )
            .map_err(|_| EnvelopeError::Signature)?;

        if !self.nonces.admit(token.node_id, &envelope.nonce, now) {
            return Err(EnvelopeError::Replayed);
        }
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::cluster::node_token::NodeTokenIssuer;
    use crate::domain::secrets::{DomainDynamicSecret, SecretsError, SensitiveString};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const KEY: &str = "node-token-key";

    /// A Transit stand-in: signs with a keyed digest under a fixed key
    /// version, and verifies only that version's signatures.
    struct FakeTransit {
        version: u32,
        verifies: AtomicUsize,
    }

    impl FakeTransit {
        fn new(version: u32) -> Arc<Self> {
            Arc::new(Self {
                version,
                verifies: AtomicUsize::new(0),
            })
        }
        fn mac(key: &str, data: &[u8]) -> Vec<u8> {
            Sha256::new()
                .chain_update(key.as_bytes())
                .chain_update(data)
                .finalize()
                .to_vec()
        }
    }

    #[async_trait]
    impl SecretStore for FakeTransit {
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
        ) -> Result<DomainDynamicSecret, SecretsError> {
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
        async fn transit_sign(&self, key: &str, data: &[u8]) -> Result<String, SecretsError> {
            Ok(format!(
                "vault:v{}:{}",
                self.version,
                base64::engine::general_purpose::STANDARD.encode(Self::mac(key, data))
            ))
        }
        async fn transit_verify(
            &self,
            key: &str,
            data: &[u8],
            signature: &str,
        ) -> Result<bool, SecretsError> {
            self.verifies.fetch_add(1, Ordering::SeqCst);
            Ok(signature == self.transit_sign(key, data).await?)
        }
        async fn transit_encrypt(&self, _: &str, _: &[u8]) -> Result<String, SecretsError> {
            unimplemented!()
        }
        async fn transit_decrypt(&self, _: &str, _: &str) -> Result<Vec<u8>, SecretsError> {
            unimplemented!()
        }
    }

    struct OneKey(NodeId, Vec<u8>);

    #[async_trait]
    impl NodeKeys for OneKey {
        async fn public_key(&self, node_id: &NodeId) -> anyhow::Result<Option<Vec<u8>>> {
            Ok((*node_id == self.0).then(|| self.1.clone()))
        }
    }

    struct Fixture {
        transit: Arc<FakeTransit>,
        verifier: NodeEnvelopeVerifier,
        node: NodeId,
        signing_key: SigningKey,
        keys: OneKey,
        token: String,
    }

    async fn token_for(transit: Arc<FakeTransit>, node: NodeId, exp: i64) -> String {
        let claims = serde_json::json!({
            "sub": node.0.to_string(), "role": "worker", "cap_hash": "",
            "iat": Utc::now().timestamp(), "exp": exp,
        });
        NodeTokenIssuer::new(transit, KEY.to_string())
            .issue(&claims)
            .await
            .unwrap()
    }

    async fn fixture() -> Fixture {
        let transit = FakeTransit::new(3);
        let node = NodeId(uuid::Uuid::new_v4());
        let signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let keys = OneKey(node, signing_key.verifying_key().to_bytes().to_vec());
        let token = token_for(transit.clone(), node, Utc::now().timestamp() + 3600).await;
        Fixture {
            verifier: NodeEnvelopeVerifier::new(transit.clone(), KEY.to_string()),
            transit,
            node,
            signing_key,
            keys,
            token,
        }
    }

    #[tokio::test]
    async fn a_sealed_envelope_opens_and_names_its_node() {
        let f = fixture().await;
        let env = seal(&f.signing_key, f.token.clone(), b"inner".to_vec());
        let token = f.verifier.open(&env, &f.keys).await.unwrap();
        assert_eq!(token.node_id, f.node);
    }

    #[tokio::test]
    async fn the_same_envelope_is_refused_the_second_time() {
        let f = fixture().await;
        let env = seal(&f.signing_key, f.token.clone(), b"probe".to_vec());
        f.verifier.open(&env, &f.keys).await.unwrap();
        assert!(matches!(
            f.verifier.open(&env, &f.keys).await,
            Err(EnvelopeError::Replayed)
        ));
        // Two envelopes over the same payload are both accepted: the nonce,
        // not the payload, identifies a request.
        let again = seal(&f.signing_key, f.token.clone(), b"probe".to_vec());
        f.verifier.open(&again, &f.keys).await.unwrap();
    }

    #[tokio::test]
    async fn an_envelope_outside_the_window_is_refused() {
        let f = fixture().await;
        let now = Utc::now();
        for at in [now - Duration::seconds(61), now + Duration::seconds(61)] {
            let env = seal_at(&f.signing_key, f.token.clone(), b"x".to_vec(), at);
            assert!(matches!(
                f.verifier.open_at(&env, &f.keys, now).await,
                Err(EnvelopeError::Stale)
            ));
        }
        let env = seal_at(
            &f.signing_key,
            f.token.clone(),
            b"x".to_vec(),
            now - Duration::seconds(59),
        );
        f.verifier.open_at(&env, &f.keys, now).await.unwrap();
    }

    #[tokio::test]
    async fn altering_the_timestamp_nonce_or_payload_breaks_the_signature() {
        let f = fixture().await;
        let base = seal(&f.signing_key, f.token.clone(), b"payload".to_vec());
        let mut later = base.clone();
        later.issued_at_ms += 1;
        let mut renonced = base.clone();
        renonced.nonce[0] ^= 1;
        let mut repayloaded = base.clone();
        repayloaded.payload = b"payloae".to_vec();
        for env in [later, renonced, repayloaded] {
            assert!(matches!(
                f.verifier.open(&env, &f.keys).await,
                Err(EnvelopeError::Signature)
            ));
        }
    }

    #[tokio::test]
    async fn a_token_not_signed_by_transit_is_refused() {
        let f = fixture().await;
        // Same claims, signed by a different Transit key version: the
        // header's kid no longer matches the signature.
        let forged = token_for(FakeTransit::new(4), f.node, Utc::now().timestamp() + 3600).await;
        let env = seal(&f.signing_key, forged.clone(), b"x".to_vec());
        assert!(matches!(
            f.verifier.open(&env, &f.keys).await,
            Err(EnvelopeError::TokenSignature)
        ));
        // Hand-made claims with a copied signature.
        let mut segs: Vec<&str> = f.token.split('.').collect();
        let other = NodeId(uuid::Uuid::new_v4());
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({"sub": other.0.to_string(), "exp": Utc::now().timestamp() + 3600})
                .to_string(),
        );
        segs[1] = &claims;
        let tampered = segs.join(".");
        let env = seal(&f.signing_key, tampered, b"x".to_vec());
        assert!(matches!(
            f.verifier.open(&env, &f.keys).await,
            Err(EnvelopeError::TokenSignature)
        ));
    }

    #[tokio::test]
    async fn an_expired_token_is_refused_even_once_cached() {
        let f = fixture().await;
        let short = token_for(f.transit.clone(), f.node, Utc::now().timestamp() + 30).await;
        let env = seal(&f.signing_key, short.clone(), b"x".to_vec());
        f.verifier.open(&env, &f.keys).await.unwrap();
        let later = Utc::now() + Duration::seconds(31);
        let env = seal_at(&f.signing_key, short, b"x".to_vec(), later);
        assert!(matches!(
            f.verifier.open_at(&env, &f.keys, later).await,
            Err(EnvelopeError::Expired)
        ));
    }

    #[tokio::test]
    async fn transit_is_asked_once_per_token() {
        let f = fixture().await;
        for _ in 0..5 {
            let env = seal(&f.signing_key, f.token.clone(), b"x".to_vec());
            f.verifier.open(&env, &f.keys).await.unwrap();
        }
        assert_eq!(f.transit.verifies.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_key_other_than_the_registered_one_is_refused() {
        let f = fixture().await;
        let stranger = SigningKey::from_bytes(&[1u8; 32]);
        let env = seal(&stranger, f.token.clone(), b"x".to_vec());
        assert!(matches!(
            f.verifier.open(&env, &f.keys).await,
            Err(EnvelopeError::Signature)
        ));
        // A token for a node with no registered key.
        let unknown = token_for(
            f.transit.clone(),
            NodeId(uuid::Uuid::new_v4()),
            Utc::now().timestamp() + 60,
        )
        .await;
        let env = seal(&f.signing_key, unknown, b"x".to_vec());
        assert!(matches!(
            f.verifier.open(&env, &f.keys).await,
            Err(EnvelopeError::UnknownNode)
        ));
    }

    #[tokio::test]
    async fn a_short_nonce_is_refused() {
        let f = fixture().await;
        let mut env = seal(&f.signing_key, f.token.clone(), b"x".to_vec());
        env.nonce.truncate(NONCE_LEN - 1);
        assert!(matches!(
            f.verifier.open(&env, &f.keys).await,
            Err(EnvelopeError::ShortNonce)
        ));
    }

    #[test]
    fn nonces_are_forgotten_only_after_they_could_no_longer_be_fresh() {
        let guard = NonceGuard::default();
        let node = NodeId(uuid::Uuid::new_v4());
        let t0 = Utc::now();
        assert!(guard.admit(node, b"n", t0));
        assert!(!guard.admit(
            node,
            b"n",
            t0 + FRESHNESS_WINDOW * 2 - Duration::milliseconds(1)
        ));
        assert!(guard.admit(node, b"n", t0 + FRESHNESS_WINDOW * 2));
        // Another node's identical nonce is its own.
        assert!(guard.admit(NodeId(uuid::Uuid::new_v4()), b"n", t0));
    }
}
