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
use std::fs;
use std::path::Path;
use tonic::{Request, Status};

use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    node_cluster_service_client::NodeClusterServiceClient, AttestNodeRequest, ChallengeNodeRequest,
    NodeCapabilities, NodeRole,
};

use super::grpc::connect_controller;

/// Decoded result of a successful attest+challenge handshake.
#[derive(Debug, Clone)]
pub struct HandshakeOutcome {
    /// Node UUID — the `sub` claim of the issued `NodeSecurityToken`. This is
    /// the daemon's identity, minted client-side at bootstrap time and
    /// persisted in `aegis-config.yaml` (`spec.node.id`). It is unrelated to
    /// the enrollment JWT's `sub` claim, which is operator display metadata.
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
///
/// The JWT's `sub` claim (operator display label, e.g. "BEASTLY1" or
/// `edge-<short>`) is intentionally NOT decoded here. It is consumed only
/// server-side as audit metadata at redemption time
/// (`enroll_edge::redeem(... &claims.sub ...)`); the daemon's identity is the
/// UUID minted client-side at bootstrap and stored in `aegis-config.yaml`
/// `spec.node.id`. Surfacing the JWT `sub` in the CLI would invite the same
/// "use sub as node_id" confusion this module exists to prevent.
#[derive(Debug, Clone)]
pub struct EnrollmentClaims {
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
        tid: String,
        cep: String,
    }
    let c: C = serde_json::from_slice(&payload).context("parse enrollment-JWT claims (tid/cep)")?;
    Ok(EnrollmentClaims {
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
    node_id: &str,
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
        node_id: node_id.to_string(),
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
        node_id: node_id.to_string(),
        challenge_signature: signature,
        bootstrap_proof: Some(BootstrapProof::EnrollmentToken(enrollment_jwt.to_string())),
    };
    let challenge_resp = client
        .challenge_node(Request::new(challenge_req))
        .await
        .map_err(map_challenge_status)?
        .into_inner();

    // The issued token's `sub` IS the daemon's node_id (the UUID we minted at
    // bootstrap and just presented on the wire). Decode it to surface the
    // bound tenant + node identity in the CLI output. We deliberately do NOT
    // compare against `claims.sub` from the enrollment JWT: those are
    // unrelated values by contract — the JWT `sub` is operator display
    // metadata, the issued token `sub` is the daemon's node UUID.
    let (issued_sub, issued_tid) = decode_issued_token(&challenge_resp.node_security_token)?;
    let tenant_id = if !issued_tid.is_empty() {
        issued_tid
    } else {
        claims.tid.clone()
    };

    let expires_at = challenge_resp.expires_at.and_then(|ts| {
        chrono::DateTime::<chrono::Utc>::from_timestamp(ts.seconds, u32::try_from(ts.nanos).ok()?)
    });

    Ok(HandshakeOutcome {
        node_id: issued_sub,
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

/// Persist the handshake outcome into `aegis-config.yaml` (BUG 1 fix).
///
/// After a successful enrollment the daemon's local config must reflect the
/// tenant binding minted by the controller — without this, the daemon starts
/// with `tenant_id: null` and dispatches that fail tenant-ownership checks
/// surface as silent inertness ("enrolled but does nothing").
///
/// Updates two fields:
///
/// * `spec.cluster.edge.tenant_id` ← outcome's `tenant_id` (the `tid` claim
///   of the issued NodeSecurityToken, falling back to the enrollment JWT's
///   `tid` if the issued token omits it).
/// * `spec.cluster.controller.endpoint` ← outcome's `controller_endpoint`
///   (the `cep` claim of the enrollment JWT). Bootstrap already set this from
///   the same source, but we re-write it here to make the post-handshake
///   step idempotent on re-enrollment.
///
/// Writes are atomic (same-directory tempfile + `rename`) and re-runnable —
/// running enroll a second time replaces the values rather than duplicating
/// them.
pub fn persist_handshake_outcome_to_config(
    state_dir: &Path,
    outcome: &HandshakeOutcome,
) -> Result<()> {
    let cfg_path = state_dir.join("aegis-config.yaml");
    let body =
        fs::read_to_string(&cfg_path).with_context(|| format!("read {}", cfg_path.display()))?;
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(&body).with_context(|| format!("parse {}", cfg_path.display()))?;

    // Walk to spec.cluster, creating intermediate maps if they're absent.
    // The bootstrap step writes a complete document so the maps should
    // already exist; this is defensive in case an operator hand-edits the
    // config and removes a section.
    let root = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("aegis-config.yaml root is not a mapping"))?;
    let spec = ensure_child_mapping(root, "spec")?;
    let cluster = ensure_child_mapping(spec, "cluster")?;

    // controller.endpoint — set the endpoint on a freshly-resolved mapping
    // ref. Re-resolve from `cluster` between the two writes so we never hold
    // overlapping mutable borrows of the same parent.
    {
        let controller = ensure_child_mapping(cluster, "controller")?;
        controller.insert(
            serde_yaml::Value::String("endpoint".to_string()),
            serde_yaml::Value::String(outcome.controller_endpoint.clone()),
        );
    }

    // edge.tenant_id
    {
        let edge = ensure_child_mapping(cluster, "edge")?;
        edge.insert(
            serde_yaml::Value::String("tenant_id".to_string()),
            serde_yaml::Value::String(outcome.tenant_id.clone()),
        );
    }

    let new_body = serde_yaml::to_string(&doc)
        .with_context(|| format!("serialize updated {}", cfg_path.display()))?;
    super::grpc::atomic_write_secret(&cfg_path, new_body.as_bytes()).with_context(|| {
        format!(
            "atomically rewrite {} with handshake outcome",
            cfg_path.display()
        )
    })?;
    Ok(())
}

/// Mutate-or-create a child entry on a YAML mapping, returning a mutable
/// reference to its value as a mapping. Errors if `key` exists but is not a
/// mapping (which would indicate a hand-edit that violated the schema —
/// surface rather than silently overwrite).
fn ensure_child_mapping<'a>(
    parent: &'a mut serde_yaml::Mapping,
    key: &str,
) -> Result<&'a mut serde_yaml::Mapping> {
    let k = serde_yaml::Value::String(key.to_string());
    if !parent.contains_key(&k) {
        parent.insert(
            k.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let val = parent.get_mut(&k).expect("just inserted if missing");
    val.as_mapping_mut()
        .ok_or_else(|| anyhow!("'{key}' exists but is not a mapping; refusing to overwrite"))
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
    fn decode_enrollment_claims_extracts_tid_cep_and_ignores_sub() {
        // The JWT carries an operator display label ("BEASTLY1") in `sub` —
        // this is NOT a node identifier and the CLI must not surface it.
        // Only `tid` + `cep` are needed to drive the handshake; the daemon's
        // node_id comes from `aegis-config.yaml` `spec.node.id`.
        let jwt = make_jwt(
            r#"{"sub":"BEASTLY1","tid":"tenant-a","cep":"controller.example:443","jti":"x","iss":"y","aud":"edge-enrollment","exp":9999999999,"nbf":0}"#,
        );
        let c = decode_enrollment_claims(&jwt).expect("decode");
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

    /// Build a representative `aegis-config.yaml` matching what `bootstrap.rs`
    /// emits, with `tenant_id: null` and a placeholder controller endpoint —
    /// this is exactly the state BUG 1 leaves on disk before the fix runs.
    fn write_pre_handshake_config(dir: &Path, controller_endpoint: &str) {
        let body = format!(
            "spec:\n  cluster:\n    controller:\n      endpoint: \"{controller_endpoint}\"\n    edge:\n      tenant_id: null\n      capabilities:\n        local_tools: [shell]\n"
        );
        std::fs::write(dir.join("aegis-config.yaml"), body).unwrap();
    }

    fn make_outcome(tenant_id: &str, controller_endpoint: &str) -> HandshakeOutcome {
        HandshakeOutcome {
            node_id: "00000000-0000-0000-0000-000000000001".to_string(),
            tenant_id: tenant_id.to_string(),
            controller_endpoint: controller_endpoint.to_string(),
            node_security_token: "header.payload.sig".to_string(),
            expires_at: None,
        }
    }

    #[test]
    fn persist_handshake_outcome_writes_tenant_id_and_endpoint() {
        // Regression for BUG 1: post-handshake the daemon's config must carry
        // the JWT's tid claim and the resolved controller endpoint. Before
        // the fix, the bootstrap-time `tenant_id: null` was never replaced
        // and every InvokeTool dispatch failed the tenant-ownership check.
        let tmp = tempfile::tempdir().unwrap();
        write_pre_handshake_config(tmp.path(), "controller.example:443");
        let outcome = make_outcome(
            "u-d7f8170035d349b6b237c391ccc19035",
            "controller.example:443",
        );
        persist_handshake_outcome_to_config(tmp.path(), &outcome)
            .expect("persist must succeed on a well-formed config");

        let body = std::fs::read_to_string(tmp.path().join("aegis-config.yaml")).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
        let tid = doc
            .get("spec")
            .and_then(|v| v.get("cluster"))
            .and_then(|v| v.get("edge"))
            .and_then(|v| v.get("tenant_id"))
            .and_then(|v| v.as_str())
            .expect("tenant_id must be a string after persist");
        assert_eq!(tid, "u-d7f8170035d349b6b237c391ccc19035");

        let endpoint = doc
            .get("spec")
            .and_then(|v| v.get("cluster"))
            .and_then(|v| v.get("controller"))
            .and_then(|v| v.get("endpoint"))
            .and_then(|v| v.as_str())
            .expect("controller.endpoint must be present");
        assert_eq!(endpoint, "controller.example:443");
    }

    #[test]
    fn persist_handshake_outcome_is_idempotent_on_re_enroll() {
        // Pin re-enrollment safety: running the persist step twice with the
        // same outcome must produce the same final document, not append a
        // duplicate `tenant_id` entry or grow the file.
        let tmp = tempfile::tempdir().unwrap();
        write_pre_handshake_config(tmp.path(), "controller.example:443");
        let outcome = make_outcome("u-abc", "controller.example:443");
        persist_handshake_outcome_to_config(tmp.path(), &outcome).unwrap();
        let after_first = std::fs::read_to_string(tmp.path().join("aegis-config.yaml")).unwrap();
        persist_handshake_outcome_to_config(tmp.path(), &outcome).unwrap();
        let after_second = std::fs::read_to_string(tmp.path().join("aegis-config.yaml")).unwrap();
        assert_eq!(
            after_first, after_second,
            "re-running persist must be a no-op when the outcome is unchanged"
        );

        // And applying a *different* outcome must replace, not append.
        let outcome_2 = make_outcome("u-def", "new-controller.example:8443");
        persist_handshake_outcome_to_config(tmp.path(), &outcome_2).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("aegis-config.yaml")).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
        let tid = doc
            .get("spec")
            .and_then(|v| v.get("cluster"))
            .and_then(|v| v.get("edge"))
            .and_then(|v| v.get("tenant_id"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(tid, "u-def", "tenant_id must be replaced, not appended");
        let endpoint = doc
            .get("spec")
            .and_then(|v| v.get("cluster"))
            .and_then(|v| v.get("controller"))
            .and_then(|v| v.get("endpoint"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(endpoint, "new-controller.example:8443");
    }

    #[test]
    fn persist_handshake_outcome_creates_missing_intermediate_maps() {
        // Defensive: an operator hand-edit might have removed the `edge`
        // section. The persist step must re-create the path rather than
        // panic, so the daemon has a sane post-handshake config.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("aegis-config.yaml"),
            "spec:\n  cluster:\n    role: edge\n",
        )
        .unwrap();
        let outcome = make_outcome("u-xyz", "controller.example:443");
        persist_handshake_outcome_to_config(tmp.path(), &outcome).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("aegis-config.yaml")).unwrap();
        assert!(body.contains("u-xyz"));
        assert!(body.contains("controller.example:443"));
    }

    #[test]
    fn persist_handshake_outcome_preserves_other_fields() {
        // Pin that the rewrite preserves unrelated fields — capabilities,
        // backoff, custom_labels — so an operator's edits aren't clobbered
        // by enrollment.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("aegis-config.yaml"),
            "spec:\n  cluster:\n    controller:\n      endpoint: \"old:443\"\n      tls:\n        enabled: true\n    edge:\n      tenant_id: null\n      capabilities:\n        os: linux\n        local_tools:\n          - shell\n          - docker\n        custom_labels:\n          region: home\n",
        )
        .unwrap();
        let outcome = make_outcome("u-pqr", "new:443");
        persist_handshake_outcome_to_config(tmp.path(), &outcome).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("aegis-config.yaml")).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
        // Updated values
        assert_eq!(
            doc["spec"]["cluster"]["controller"]["endpoint"].as_str(),
            Some("new:443")
        );
        assert_eq!(
            doc["spec"]["cluster"]["edge"]["tenant_id"].as_str(),
            Some("u-pqr")
        );
        // Preserved values
        assert_eq!(
            doc["spec"]["cluster"]["controller"]["tls"]["enabled"].as_bool(),
            Some(true)
        );
        let tools = doc["spec"]["cluster"]["edge"]["capabilities"]["local_tools"]
            .as_sequence()
            .expect("local_tools sequence preserved");
        assert_eq!(tools.len(), 2);
        assert_eq!(
            doc["spec"]["cluster"]["edge"]["capabilities"]["custom_labels"]["region"].as_str(),
            Some("home")
        );
    }
}
