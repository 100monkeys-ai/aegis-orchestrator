// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C steps 2-4 — wire-side handshake for `aegis edge enroll`.
//!
//! After the local bootstrap step has persisted a fresh Ed25519 keypair and
//! the enrollment JWT to disk, this module:
//!
//! 1. Decodes the `cep` (controller endpoint) and `tid` (tenant id) claims
//!    from the enrollment JWT (no signature verification — the server does
//!    that).
//! 2. Dials the controller's `NodeClusterService`.
//! 3. Calls `AttestNode` anonymously with `role = NODE_ROLE_EDGE` and the
//!    daemon's freshly-generated public key. The server rate-limits this
//!    call to 5/min per source.
//! 4. Signs the returned challenge nonce with the daemon's private key and
//!    calls `ChallengeNode` with the enrollment JWT attached as
//!    `bootstrap_proof`. On success, the server atomically redeems the JWT,
//!    persists the `EdgeDaemon` row binding `node_id ↔ tenant_id`, and
//!    returns a `NodeSecurityToken`.
//!
//! Persisting the returned token belongs to the caller (`enroll::run`).
//! Keeping the token write outside this module preserves the "wire flow ↔
//! disk flow" separation and lets the caller decide retry / output policy.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use tonic::{Request, Status};

use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    node_cluster_service_client::NodeClusterServiceClient, AttestNodeRequest, ChallengeNodeRequest,
    NodeCapabilities, NodeRole,
};

use super::grpc::connect_controller;

/// Decoded result of a successful attest+challenge handshake.
#[derive(Debug, Clone)]
pub struct HandshakeOutcome {
    /// Node UUID (the `sub` claim of the issued NodeSecurityToken). Equal to
    /// the `sub` claim of the enrollment JWT — the server pins the binding
    /// during ChallengeNode.
    pub node_id: String,
    /// Tenant id the daemon is bound to (the `tid` claim of the enrollment
    /// JWT, echoed in the issued NodeSecurityToken).
    pub tenant_id: String,
    /// Controller endpoint the handshake targeted (the `cep` claim of the
    /// enrollment JWT).
    pub controller_endpoint: String,
    /// Raw NodeSecurityToken JWT (RS256). Caller persists this to
    /// `<state_dir>/node.token`.
    pub node_security_token: String,
    /// RFC-3339 timestamp at which the issued token expires. `None` when the
    /// server omitted the field (older controllers).
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Subset of enrollment-JWT claims the CLI needs to drive the handshake.
/// Sourced directly from the JWT payload segment without signature
/// verification — the server validates `iss`, `aud`, `exp`, `nbf`, and the
/// signature during ChallengeNode.
#[derive(Debug, Clone)]
pub struct EnrollmentClaims {
    /// `sub` — node UUID. The server pins `node_id ↔ public_key ↔ tenant_id`
    /// at ChallengeNode time. We use this as the AttestNode `node_id`.
    pub sub: String,
    /// `tid` — tenant id binding.
    pub tid: String,
    /// `cep` — controller endpoint to dial.
    pub cep: String,
}

/// Decode the enrollment JWT's payload without verifying the signature.
/// The server is the trust boundary for signature/audience/expiry validation.
pub fn decode_enrollment_claims(jwt: &str) -> Result<EnrollmentClaims> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        anyhow::bail!("enrollment token is not a well-formed JWT");
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .context("base64-decode enrollment-JWT payload")?;
    #[derive(serde::Deserialize)]
    struct C {
        sub: String,
        tid: String,
        cep: String,
    }
    let c: C =
        serde_json::from_slice(&payload).context("parse enrollment-JWT claims (sub/tid/cep)")?;
    Ok(EnrollmentClaims {
        sub: c.sub,
        tid: c.tid,
        cep: c.cep,
    })
}

/// Decode the issued NodeSecurityToken's payload without verifying the
/// signature. Used to surface the bound tenant + expiry in CLI output.
fn decode_issued_token(jwt: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        anyhow::bail!("issued NodeSecurityToken is not a well-formed JWT");
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .context("base64-decode issued-token payload")?;
    #[derive(serde::Deserialize)]
    struct C {
        sub: String,
        #[serde(default)]
        tid: String,
    }
    let c: C = serde_json::from_slice(&payload).context("parse issued-token claims (sub/tid)")?;
    Ok((c.sub, c.tid))
}

/// Run the AttestNode → ChallengeNode handshake against `controller_endpoint`,
/// presenting `enrollment_jwt` as `bootstrap_proof`. On success, returns the
/// issued `NodeSecurityToken` and decoded metadata.
///
/// The function does NOT touch disk — persisting the issued token is the
/// caller's responsibility.
pub async fn run_attest_and_challenge(
    controller_endpoint: &str,
    signing_key: &SigningKey,
    enrollment_jwt: &str,
) -> Result<HandshakeOutcome> {
    let claims = decode_enrollment_claims(enrollment_jwt)?;

    // Step 1: dial the controller. Surface a helpful "could not reach
    // controller at <endpoint>" message rather than a bare tonic transport
    // error so the operator can immediately see the target.
    let channel = connect_controller(controller_endpoint)
        .await
        .with_context(|| format!("could not reach controller at {controller_endpoint}"))?;
    let mut client = NodeClusterServiceClient::new(channel);

    // Step 2: AttestNode (anonymous, role = EDGE). The server records the
    // `(node_id, public_key)` candidate and returns a nonce we must sign.
    // Edge daemons advertise hardware capabilities later via the ConnectEdge
    // Hello frame; AttestNode only needs identity + role here. The
    // `enrolment_token` field is the worker-only cluster admission token —
    // edge daemons leave it empty and present the JWT on ChallengeNode
    // instead.
    let attest_req = AttestNodeRequest {
        node_id: claims.sub.clone(),
        role: NodeRole::Edge.into(),
        public_key: signing_key.verifying_key().to_bytes().to_vec(),
        capabilities: Some(NodeCapabilities::default()),
        grpc_address: String::new(),
        enrolment_token: String::new(),
    };
    let attest_resp = client
        .attest_node(Request::new(attest_req))
        .await
        .map_err(map_attest_status)?
        .into_inner();

    // Step 3: ChallengeNode with the enrollment-JWT bootstrap_proof. The
    // server validates the JWT (sig/aud/exp/nbf), atomically redeems the
    // `jti`, persists the `EdgeDaemon` row binding `node_id ↔ tenant_id`,
    // and issues a `NodeSecurityToken` whose claims include `tid` and `cep`.
    use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::challenge_node_request::BootstrapProof;
    let signature = signing_key
        .sign(&attest_resp.challenge_nonce)
        .to_bytes()
        .to_vec();
    let challenge_req = ChallengeNodeRequest {
        challenge_id: attest_resp.challenge_id,
        node_id: claims.sub.clone(),
        challenge_signature: signature,
        bootstrap_proof: Some(BootstrapProof::EnrollmentToken(enrollment_jwt.to_string())),
    };
    let challenge_resp = client
        .challenge_node(Request::new(challenge_req))
        .await
        .map_err(map_challenge_status)?
        .into_inner();

    // The issued token's `sub` MUST match the enrollment JWT's `sub` (the
    // server pins the binding). If they ever drift, surface the mismatch
    // rather than silently accepting it — a corrupted server response would
    // otherwise persist an unusable token.
    let (issued_sub, issued_tid) = decode_issued_token(&challenge_resp.node_security_token)?;
    if issued_sub != claims.sub {
        anyhow::bail!(
            "issued NodeSecurityToken sub claim '{}' does not match enrollment-JWT sub '{}'",
            issued_sub,
            claims.sub
        );
    }
    let tenant_id = if !issued_tid.is_empty() {
        issued_tid
    } else {
        claims.tid.clone()
    };

    let expires_at = challenge_resp.expires_at.and_then(|ts| {
        chrono::DateTime::<chrono::Utc>::from_timestamp(ts.seconds, u32::try_from(ts.nanos).ok()?)
    });

    Ok(HandshakeOutcome {
        node_id: claims.sub,
        tenant_id,
        controller_endpoint: claims.cep,
        node_security_token: challenge_resp.node_security_token,
        expires_at,
    })
}

/// Translate a tonic `Status` returned by `AttestNode` into a user-friendly
/// `anyhow::Error`. The 5/min rate-limit per ADR-117 §C is the most common
/// operator-visible failure mode here, so we name it explicitly.
fn map_attest_status(status: Status) -> anyhow::Error {
    match status.code() {
        tonic::Code::ResourceExhausted => anyhow!(
            "AttestNode rate-limited by controller (5/min per source per ADR-117): {}",
            status.message()
        ),
        _ => anyhow!(
            "AttestNode RPC failed ({:?}): {}",
            status.code(),
            status.message()
        ),
    }
}

/// Translate a tonic `Status` returned by `ChallengeNode` into a
/// user-friendly `anyhow::Error`. Bootstrap-proof validation failures
/// (expired token, already-redeemed jti, audience mismatch) surface as
/// `InvalidArgument`; we forward the server's message so the operator can
/// distinguish the cases without re-examining server logs.
fn map_challenge_status(status: Status) -> anyhow::Error {
    match status.code() {
        tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => anyhow!(
            "ChallengeNode rejected bootstrap_proof ({:?}): {}",
            status.code(),
            status.message()
        ),
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => anyhow!(
            "ChallengeNode rejected enrollment token ({:?}): {}",
            status.code(),
            status.message()
        ),
        _ => anyhow!(
            "ChallengeNode RPC failed ({:?}): {}",
            status.code(),
            status.message()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(payload_json: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header}.{payload}.{sig}")
    }

    #[test]
    fn decode_enrollment_claims_extracts_sub_tid_cep() {
        let jwt = make_jwt(
            r#"{"sub":"00000000-0000-0000-0000-000000000001","tid":"tenant-a","cep":"controller.example:443","jti":"x","iss":"y","aud":"edge-enrollment","exp":9999999999,"nbf":0}"#,
        );
        let c = decode_enrollment_claims(&jwt).expect("decode");
        assert_eq!(c.sub, "00000000-0000-0000-0000-000000000001");
        assert_eq!(c.tid, "tenant-a");
        assert_eq!(c.cep, "controller.example:443");
    }

    #[test]
    fn decode_enrollment_claims_rejects_missing_required_field() {
        // No `tid` claim — must fail rather than silently default.
        let jwt = make_jwt(r#"{"sub":"x","cep":"y"}"#);
        let err = decode_enrollment_claims(&jwt).expect_err("missing tid must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("tid") || msg.contains("missing"), "{msg}");
    }

    #[test]
    fn decode_enrollment_claims_rejects_malformed_jwt() {
        let err = decode_enrollment_claims("not-a-jwt").expect_err("must reject non-JWT");
        let msg = format!("{err:#}");
        assert!(msg.contains("JWT") || msg.contains("well-formed"), "{msg}");
    }

    #[test]
    fn map_attest_status_names_rate_limit() {
        let err = map_attest_status(Status::resource_exhausted("5/min"));
        let msg = format!("{err:#}");
        assert!(msg.contains("rate-limited"), "must name rate limit: {msg}");
    }

    #[test]
    fn map_challenge_status_names_bootstrap_proof_rejection() {
        let err = map_challenge_status(Status::invalid_argument("enrollment token expired"));
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bootstrap_proof") && msg.contains("expired"),
            "must surface server detail: {msg}"
        );
    }
}
