// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C — `aegis edge enroll` end-to-end coverage.
//!
//! Stands up an in-process tonic server hosting a stub `NodeClusterService`
//! whose `attest_node` / `challenge_node` handlers are scripted per test.
//! Drives `enroll::run` directly (not via clap) and asserts the on-disk +
//! wire-flow contract pinned by the ADR:
//!
//! 1. Local bootstrap writes node.key, node.key.pub, enrollment.jwt, and
//!    aegis-config.yaml.
//! 2. AttestNode is called anonymously with role = NODE_ROLE_EDGE.
//! 3. ChallengeNode is called with `bootstrap_proof = enrollment_token(jwt)`.
//! 4. The returned NodeSecurityToken is persisted to `<state_dir>/node.token`
//!    at mode 0600 — and ONLY after the handshake succeeds (a failed
//!    challenge must not leave a half-written token on disk).
//!
//! Why the file lives in `cli/tests/` (not `orchestrator/core/tests/`):
//! these tests are CLI-driven — the system under test is the
//! `aegis_orchestrator::commands::edge::enroll` flow, not the orchestrator's
//! application services. The complementary file
//! `orchestrator/core/tests/edge_mode_grpc.rs` covers the server side of
//! ConnectEdge; this file covers the operator side of enrollment.

use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::Stream;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

use aegis_orchestrator::commands::edge::enroll::{run as enroll_run, EnrollArgs};
use aegis_orchestrator::output::OutputFormat;
use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    node_cluster_service_server::{NodeClusterService, NodeClusterServiceServer},
    AttestNodeRequest, AttestNodeResponse, ChallengeNodeRequest, ChallengeNodeResponse,
    DeregisterNodeRequest, DeregisterNodeResponse, EdgeCommand, EdgeEvent, ForwardExecutionRequest,
    IssueEnrollmentTokenRequest, IssueEnrollmentTokenResponse, ListPeersRequest, ListPeersResponse,
    NodeHeartbeatRequest, NodeHeartbeatResponse, NodeRole, PushConfigRequest, PushConfigResponse,
    RegisterNodeRequest, RegisterNodeResponse, RotateEdgeKeyRequest, RotateEdgeKeyResponse,
    RouteExecutionRequest, RouteExecutionResponse, SyncConfigRequest, SyncConfigResponse,
};
use aegis_orchestrator_core::infrastructure::aegis_runtime_proto::ExecutionEvent;

// ---------------------------------------------------------------------------
// Scripted NodeClusterService stub
// ---------------------------------------------------------------------------

/// Records calls made by the CLI under test, and lets each test script the
/// `attest_node` / `challenge_node` outcomes independently.
#[derive(Default)]
struct StubState {
    attest_calls: usize,
    challenge_calls: usize,
    last_attest_role: Option<i32>,
    last_attest_public_key: Option<Vec<u8>>,
    last_attest_node_id: Option<String>,
    last_challenge_proof: Option<String>,
    last_challenge_node_id: Option<String>,
    last_challenge_signature_len: Option<usize>,
    attest_response: Option<Result<AttestNodeResponse, Status>>,
    challenge_response: Option<Result<ChallengeNodeResponse, Status>>,
}

#[derive(Clone, Default)]
struct ScriptedStub {
    state: Arc<Mutex<StubState>>,
}

impl ScriptedStub {
    fn snapshot(&self) -> StubSnapshot {
        let s = self.state.lock();
        StubSnapshot {
            attest_calls: s.attest_calls,
            challenge_calls: s.challenge_calls,
            last_attest_role: s.last_attest_role,
            last_attest_public_key: s.last_attest_public_key.clone(),
            last_attest_node_id: s.last_attest_node_id.clone(),
            last_challenge_proof: s.last_challenge_proof.clone(),
            last_challenge_node_id: s.last_challenge_node_id.clone(),
            last_challenge_signature_len: s.last_challenge_signature_len,
        }
    }

    fn script_attest(&self, resp: Result<AttestNodeResponse, Status>) {
        self.state.lock().attest_response = Some(resp);
    }

    fn script_challenge(&self, resp: Result<ChallengeNodeResponse, Status>) {
        self.state.lock().challenge_response = Some(resp);
    }
}

struct StubSnapshot {
    attest_calls: usize,
    challenge_calls: usize,
    last_attest_role: Option<i32>,
    last_attest_public_key: Option<Vec<u8>>,
    last_attest_node_id: Option<String>,
    last_challenge_proof: Option<String>,
    last_challenge_node_id: Option<String>,
    last_challenge_signature_len: Option<usize>,
}

#[async_trait]
impl NodeClusterService for ScriptedStub {
    async fn attest_node(
        &self,
        request: Request<AttestNodeRequest>,
    ) -> Result<Response<AttestNodeResponse>, Status> {
        let req = request.into_inner();
        // Pin the production server contract: AttestNode `node_id` MUST parse
        // as a UUID — `grpc_server::attest_node` returns
        // `Status::invalid_argument("Invalid NodeId")` otherwise. If the CLI
        // ever leaks the enrollment JWT's `sub` (an operator display label
        // like "BEASTLY1") into `node_id` again, this guard turns the
        // regression into a loud test failure instead of a silent server-side
        // 400 in production.
        if Uuid::parse_str(&req.node_id).is_err() {
            return Err(Status::invalid_argument(format!(
                "Invalid NodeId: '{}' (must be a UUID — JWT sub claim must NOT be used as node_id)",
                req.node_id
            )));
        }
        let scripted = {
            let mut s = self.state.lock();
            s.attest_calls += 1;
            s.last_attest_role = Some(req.role);
            s.last_attest_public_key = Some(req.public_key.clone());
            s.last_attest_node_id = Some(req.node_id.clone());
            s.attest_response.take()
        };
        match scripted {
            Some(Ok(r)) => Ok(Response::new(r)),
            Some(Err(e)) => Err(e),
            None => Err(Status::failed_precondition("attest stub not scripted")),
        }
    }

    async fn challenge_node(
        &self,
        request: Request<ChallengeNodeRequest>,
    ) -> Result<Response<ChallengeNodeResponse>, Status> {
        let req = request.into_inner();
        // Same strict-UUID guard as attest_node — pins the production
        // server's `Status::invalid_argument("Invalid node_id")` contract.
        if Uuid::parse_str(&req.node_id).is_err() {
            return Err(Status::invalid_argument(format!(
                "Invalid node_id: '{}' (must be a UUID)",
                req.node_id
            )));
        }
        let scripted = {
            let mut s = self.state.lock();
            s.challenge_calls += 1;
            s.last_challenge_node_id = Some(req.node_id.clone());
            s.last_challenge_signature_len = Some(req.challenge_signature.len());
            s.last_challenge_proof = req.bootstrap_proof.map(|bp| match bp {
                aegis_orchestrator_core::infrastructure::aegis_cluster_proto::challenge_node_request::BootstrapProof::EnrollmentToken(t) => t,
            });
            s.challenge_response.take()
        };
        match scripted {
            Some(Ok(r)) => Ok(Response::new(r)),
            Some(Err(e)) => Err(e),
            None => Err(Status::failed_precondition("challenge stub not scripted")),
        }
    }

    // ----- unused RPCs return unimplemented -----
    async fn register_node(
        &self,
        _r: Request<RegisterNodeRequest>,
    ) -> Result<Response<RegisterNodeResponse>, Status> {
        Err(Status::unimplemented("register_node"))
    }
    async fn heartbeat(
        &self,
        _r: Request<NodeHeartbeatRequest>,
    ) -> Result<Response<NodeHeartbeatResponse>, Status> {
        Err(Status::unimplemented("heartbeat"))
    }
    async fn deregister_node(
        &self,
        _r: Request<DeregisterNodeRequest>,
    ) -> Result<Response<DeregisterNodeResponse>, Status> {
        Err(Status::unimplemented("deregister_node"))
    }
    async fn route_execution(
        &self,
        _r: Request<RouteExecutionRequest>,
    ) -> Result<Response<RouteExecutionResponse>, Status> {
        Err(Status::unimplemented("route_execution"))
    }
    type ForwardExecutionStream =
        Pin<Box<dyn Stream<Item = Result<ExecutionEvent, Status>> + Send>>;
    async fn forward_execution(
        &self,
        _r: Request<ForwardExecutionRequest>,
    ) -> Result<Response<Self::ForwardExecutionStream>, Status> {
        Err(Status::unimplemented("forward_execution"))
    }
    async fn sync_config(
        &self,
        _r: Request<SyncConfigRequest>,
    ) -> Result<Response<SyncConfigResponse>, Status> {
        Err(Status::unimplemented("sync_config"))
    }
    async fn push_config(
        &self,
        _r: Request<PushConfigRequest>,
    ) -> Result<Response<PushConfigResponse>, Status> {
        Err(Status::unimplemented("push_config"))
    }
    async fn list_peers(
        &self,
        _r: Request<ListPeersRequest>,
    ) -> Result<Response<ListPeersResponse>, Status> {
        Err(Status::unimplemented("list_peers"))
    }
    type ConnectEdgeStream = Pin<Box<dyn Stream<Item = Result<EdgeCommand, Status>> + Send>>;
    async fn connect_edge(
        &self,
        _r: Request<Streaming<EdgeEvent>>,
    ) -> Result<Response<Self::ConnectEdgeStream>, Status> {
        Err(Status::unimplemented("connect_edge"))
    }
    async fn rotate_edge_key(
        &self,
        _r: Request<RotateEdgeKeyRequest>,
    ) -> Result<Response<RotateEdgeKeyResponse>, Status> {
        Err(Status::unimplemented("rotate_edge_key"))
    }
    async fn issue_enrollment_token(
        &self,
        _r: Request<IssueEnrollmentTokenRequest>,
    ) -> Result<Response<IssueEnrollmentTokenResponse>, Status> {
        Err(Status::unimplemented("issue_enrollment_token"))
    }
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

async fn spawn_scripted_server(stub: ScriptedStub) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    let endpoint = format!("127.0.0.1:{}", addr.port());
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let incoming = TcpListenerStream::new(listener);
        let _ = Server::builder()
            .add_service(NodeClusterServiceServer::new(stub))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = rx.await;
            })
            .await;
    });
    (endpoint, tx)
}

/// Build a minimal JWT-shaped string carrying the claims `enroll::run` (and
/// the handshake module) consume: `sub`, `tid`, `cep`. Other claims are
/// validated server-side and are not relevant to the client's behavior.
fn make_enrollment_jwt(sub: &str, tid: &str, cep: &str) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let payload_json = format!(
        r#"{{"sub":"{sub}","tid":"{tid}","cep":"{cep}","aud":"edge-enrollment","iss":"test","jti":"{}","exp":9999999999,"nbf":0}}"#,
        Uuid::new_v4()
    );
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
    format!("{header}.{payload}.{sig}")
}

/// Build a NodeSecurityToken-shaped JWT; only `sub` and `tid` are decoded
/// client-side. Other claims (role, capabilities_hash, iat, exp, …) are
/// validated by the daemon's connect_edge call, not here.
fn make_node_security_token(sub: &str, tid: &str) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
    let payload_json = format!(r#"{{"sub":"{sub}","tid":"{tid}"}}"#);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
    format!("{header}.{payload}.{sig}")
}

fn enroll_args(token: String, state_dir: &Path) -> EnrollArgs {
    EnrollArgs {
        token,
        state_dir: Some(state_dir.to_path_buf()),
        non_interactive: true,
        force: false,
        keep_existing: false,
        dry_run: false,
        minimal: false,
    }
}

#[cfg(unix)]
fn assert_mode_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::metadata(path)
        .expect("stat node.token")
        .permissions();
    assert_eq!(
        perms.mode() & 0o777,
        0o600,
        "node.token must be mode 0600, got {:o}",
        perms.mode() & 0o777
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy path: scripted attest+challenge succeeds, node.token is persisted
/// with mode 0600, and AttestNode was called with `role = NODE_ROLE_EDGE`
/// and the daemon's freshly-generated public key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_writes_node_token_after_successful_handshake() {
    let stub = ScriptedStub::default();
    // Use a non-UUID JWT `sub` — operator-supplied display label like the one
    // Zaru's `AddEdgeHostDialog` actually emits. The CLI must NOT use this as
    // `node_id`; it must mint a fresh UUID at bootstrap and present that on
    // the wire instead. The strict-UUID stub guard above turns any regression
    // (JWT-sub leaking into node_id) into a hard test failure.
    let jwt_sub = "BEASTLY1";
    // The issued NodeSecurityToken's `sub` is the minted UUID (the daemon's
    // node_id). We don't know the minted UUID up front; pre-script a stable
    // UUID here and assert it round-trips into the persisted token. The
    // handshake decodes this value and uses it as `outcome.node_id`.
    let issued_node_id = Uuid::new_v4().to_string();
    let issued = make_node_security_token(&issued_node_id, "tenant-a");
    stub.script_attest(Ok(AttestNodeResponse {
        challenge_nonce: vec![7u8; 32],
        challenge_id: Uuid::new_v4().to_string(),
    }));
    stub.script_challenge(Ok(ChallengeNodeResponse {
        node_security_token: issued.clone(),
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800, // 2100-01-01
            nanos: 0,
        }),
    }));
    let (endpoint, _shutdown) = spawn_scripted_server(stub.clone()).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let token = make_enrollment_jwt(jwt_sub, "tenant-a", &endpoint);
    enroll_run(enroll_args(token, tmp.path()), OutputFormat::Text)
        .await
        .expect("enroll must succeed");

    let token_path = tmp.path().join("node.token");
    assert!(token_path.exists(), "node.token must be persisted");
    let on_disk = std::fs::read_to_string(&token_path).expect("read node.token");
    assert_eq!(
        on_disk.trim(),
        issued.trim(),
        "persisted token must match the issued NodeSecurityToken"
    );
    #[cfg(unix)]
    assert_mode_0600(&token_path);

    // The CLI must mint a UUID at bootstrap and persist it in
    // `aegis-config.yaml` `spec.node.id`. Read it back and assert the wire
    // calls used THAT value — not the operator-supplied JWT `sub`.
    let minted_node_id = read_persisted_node_id(tmp.path());
    assert!(
        Uuid::parse_str(&minted_node_id).is_ok(),
        "spec.node.id must be a UUID, got '{minted_node_id}'"
    );
    assert_ne!(
        minted_node_id, jwt_sub,
        "minted node_id must not be the JWT sub claim"
    );

    // Pin the wire contract: edge daemons attest as NODE_ROLE_EDGE and present
    // the enrollment JWT as bootstrap_proof on ChallengeNode.
    let snap = stub.snapshot();
    assert_eq!(snap.attest_calls, 1);
    assert_eq!(snap.challenge_calls, 1);
    assert_eq!(snap.last_attest_role, Some(NodeRole::Edge as i32));
    assert_eq!(
        snap.last_attest_public_key.as_ref().map(|k| k.len()),
        Some(32),
        "AttestNode public_key must be a raw 32-byte Ed25519 key"
    );
    assert_eq!(
        snap.last_challenge_signature_len,
        Some(64),
        "ChallengeNode signature must be a 64-byte Ed25519 signature"
    );
    assert!(
        snap.last_challenge_proof.is_some(),
        "ChallengeNode must carry an enrollment_token bootstrap_proof"
    );
    assert_eq!(
        snap.last_attest_node_id.as_deref(),
        Some(minted_node_id.as_str()),
        "AttestNode must use the minted UUID, not the JWT sub"
    );
    assert_eq!(
        snap.last_challenge_node_id.as_deref(),
        Some(minted_node_id.as_str()),
        "ChallengeNode must use the minted UUID, not the JWT sub"
    );
}

/// Read the `spec.node.id` field that bootstrap minted into
/// `aegis-config.yaml`. Fails the test if the field is absent or not a string.
fn read_persisted_node_id(state_dir: &Path) -> String {
    let body = std::fs::read_to_string(state_dir.join("aegis-config.yaml"))
        .expect("read aegis-config.yaml");
    let doc: serde_yaml::Value = serde_yaml::from_str(&body).expect("parse aegis-config.yaml");
    doc.get("spec")
        .and_then(|v| v.get("node"))
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .expect("spec.node.id must be present in aegis-config.yaml after bootstrap")
}

/// AttestNode rate-limit must surface a user-friendly error mentioning the
/// rate limit (so the operator immediately sees why and waits 1 minute
/// rather than retrying in a tight loop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_surfaces_attest_rate_limit_error() {
    let stub = ScriptedStub::default();
    stub.script_attest(Err(Status::resource_exhausted("5/min")));
    let (endpoint, _shutdown) = spawn_scripted_server(stub.clone()).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let token = make_enrollment_jwt("BEASTLY1", "tenant-a", &endpoint);
    let err = enroll_run(enroll_args(token, tmp.path()), OutputFormat::Text)
        .await
        .expect_err("attest rate-limit must propagate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("rate-limited") || msg.contains("ResourceExhausted"),
        "error must name the rate limit: {msg}"
    );

    // Atomicity: rate-limit at AttestNode means we never wrote node.token.
    assert!(
        !tmp.path().join("node.token").exists(),
        "no node.token may exist when AttestNode failed"
    );
}

/// ChallengeNode rejection (e.g. expired enrollment token) must surface the
/// server's reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_surfaces_challenge_invalid_token_error() {
    let stub = ScriptedStub::default();
    stub.script_attest(Ok(AttestNodeResponse {
        challenge_nonce: vec![1u8; 32],
        challenge_id: Uuid::new_v4().to_string(),
    }));
    stub.script_challenge(Err(Status::invalid_argument("enrollment token expired")));
    let (endpoint, _shutdown) = spawn_scripted_server(stub.clone()).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let token = make_enrollment_jwt("edge-abc12345", "tenant-a", &endpoint);
    let err = enroll_run(enroll_args(token, tmp.path()), OutputFormat::Text)
        .await
        .expect_err("challenge must propagate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("expired")
            && (msg.contains("bootstrap_proof") || msg.contains("InvalidArgument")),
        "error must surface the server's reason: {msg}"
    );
}

/// No server listening at all → "could not reach controller" with the
/// endpoint named.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_surfaces_network_unreachable_error() {
    // Bind a port, then drop the listener so nothing is listening.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let endpoint = format!("127.0.0.1:{port}");

    let tmp = tempfile::tempdir().expect("tempdir");
    let token = make_enrollment_jwt("BEASTLY1", "tenant-a", &endpoint);
    let err = enroll_run(enroll_args(token, tmp.path()), OutputFormat::Text)
        .await
        .expect_err("must fail when no controller listens");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("could not reach controller") && msg.contains(&endpoint),
        "error must name the endpoint: {msg}"
    );
    assert!(
        !tmp.path().join("node.token").exists(),
        "no node.token may exist when controller is unreachable"
    );
}

/// Atomicity: a ChallengeNode failure must NOT leave a partially-written
/// node.token on disk — the daemon would later happily try to use it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_does_not_write_node_token_on_handshake_failure() {
    let stub = ScriptedStub::default();
    stub.script_attest(Ok(AttestNodeResponse {
        challenge_nonce: vec![2u8; 32],
        challenge_id: Uuid::new_v4().to_string(),
    }));
    stub.script_challenge(Err(Status::permission_denied("jti already redeemed")));
    let (endpoint, _shutdown) = spawn_scripted_server(stub.clone()).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let token = make_enrollment_jwt("BEASTLY1", "tenant-a", &endpoint);
    let _ = enroll_run(enroll_args(token, tmp.path()), OutputFormat::Text)
        .await
        .expect_err("challenge denied");
    assert!(
        !tmp.path().join("node.token").exists(),
        "no node.token may exist after a failed challenge"
    );
    // But the bootstrap-stage artifacts MUST exist — we want a re-run to
    // skip keypair generation (the same key proved POP at attest, and the
    // server's challenge rejection is independent of the key material).
    assert!(
        tmp.path().join("node.key").exists(),
        "node.key persists across handshake retries"
    );
    assert!(
        tmp.path().join("enrollment.jwt").exists(),
        "enrollment.jwt persists across retries"
    );
}

/// `--dry-run` must skip the wire flow entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_dry_run_skips_network() {
    let stub = ScriptedStub::default();
    let (endpoint, _shutdown) = spawn_scripted_server(stub.clone()).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let token = make_enrollment_jwt("BEASTLY1", "tenant-a", &endpoint);
    let mut args = enroll_args(token, tmp.path());
    args.dry_run = true;
    enroll_run(args, OutputFormat::Text)
        .await
        .expect("dry-run must succeed without scripting RPCs");

    let snap = stub.snapshot();
    assert_eq!(snap.attest_calls, 0, "dry-run must not call AttestNode");
    assert_eq!(
        snap.challenge_calls, 0,
        "dry-run must not call ChallengeNode"
    );
    assert!(
        !tmp.path().join("node.token").exists(),
        "dry-run must not persist node.token"
    );
}

/// `--keep-existing` short-circuits the wire flow when a valid identity is
/// already on disk. Re-running enrollment after a successful first run must
/// not burn another attestation slot (and would fail server-side since the
/// jti is already redeemed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_keep_existing_skips_handshake_when_node_token_present() {
    let stub = ScriptedStub::default();
    // No scripting — any RPC the CLI accidentally makes will fail-fast.
    let (endpoint, _shutdown) = spawn_scripted_server(stub.clone()).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    // Pre-seed the disk to look like a previously-successful enrollment:
    //   node.key (32 bytes), node.token (a fake NodeSecurityToken), and the
    //   aegis-config.yaml whose controller endpoint matches the new token.
    std::fs::write(tmp.path().join("node.key"), [3u8; 32]).unwrap();
    let node_id = Uuid::new_v4().to_string();
    let preexisting_token = make_node_security_token(&node_id, "tenant-a");
    std::fs::write(tmp.path().join("node.token"), &preexisting_token).unwrap();
    let cfg = format!("spec:\n  cluster:\n    controller:\n      endpoint: \"{endpoint}\"\n");
    std::fs::write(tmp.path().join("aegis-config.yaml"), cfg).unwrap();

    // JWT `sub` is operator display metadata, NOT the node_id. Use a
    // non-UUID label here to prove the keep-existing path doesn't conflate
    // them either: outcome.node_id is read from the persisted node.token's
    // `sub` claim (the minted UUID), not from the enrollment JWT.
    let _persisted_uuid = node_id; // shown for clarity — same UUID inside node.token
    let token = make_enrollment_jwt("BEASTLY1", "tenant-a", &endpoint);
    let mut args = enroll_args(token, tmp.path());
    args.keep_existing = true;
    args.non_interactive = false; // keep_existing is the explicit flag
    enroll_run(args, OutputFormat::Text)
        .await
        .expect("keep-existing must succeed when artifacts match");

    let snap = stub.snapshot();
    assert_eq!(
        snap.attest_calls, 0,
        "keep-existing with valid node.token must NOT call AttestNode"
    );
    assert_eq!(
        snap.challenge_calls, 0,
        "keep-existing with valid node.token must NOT call ChallengeNode"
    );
    let on_disk = std::fs::read_to_string(tmp.path().join("node.token")).unwrap();
    assert_eq!(
        on_disk.trim(),
        preexisting_token.trim(),
        "keep-existing must NOT replace the existing node.token"
    );
}

/// JSON output: shape must include node_id, tenant_id, controller_endpoint,
/// node_token_path, expires_at. Captured via stdout — we redirect via a
/// child process to keep the assertion robust against any future test-time
/// concurrency. (For the in-process variant we just exercise the success
/// path here and trust the JSON serializer; shape is pinned by the
/// serde_json::json! literal in `enroll.rs`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_json_output_succeeds_and_persists_token() {
    let stub = ScriptedStub::default();
    let issued_node_id = Uuid::new_v4().to_string();
    let issued = make_node_security_token(&issued_node_id, "tenant-x");
    stub.script_attest(Ok(AttestNodeResponse {
        challenge_nonce: vec![9u8; 32],
        challenge_id: Uuid::new_v4().to_string(),
    }));
    stub.script_challenge(Ok(ChallengeNodeResponse {
        node_security_token: issued.clone(),
        expires_at: None,
    }));
    let (endpoint, _shutdown) = spawn_scripted_server(stub).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let token = make_enrollment_jwt("BEASTLY1", "tenant-x", &endpoint);
    enroll_run(enroll_args(token, tmp.path()), OutputFormat::Json)
        .await
        .expect("json enroll must succeed");
    let on_disk = std::fs::read_to_string(tmp.path().join("node.token")).unwrap();
    assert_eq!(on_disk.trim(), issued.trim());
}

// ---------------------------------------------------------------------------
// node_id contract regression coverage (ADR-117 §C)
//
// These tests pin the post-fix contract: the daemon mints a UUID at bootstrap
// and presents it on the wire. The enrollment JWT's `sub` claim is operator
// display metadata only and must NEVER be sent as `node_id`.
// ---------------------------------------------------------------------------

/// Bootstrap on a fresh state-dir must populate `aegis-config.yaml`
/// `spec.node.id` with a parseable UUID. The CLI mints this client-side; it
/// is unrelated to the enrollment JWT's `sub`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_writes_node_id_to_config_on_first_run() {
    let stub = ScriptedStub::default();
    let issued_node_id = Uuid::new_v4().to_string();
    let issued = make_node_security_token(&issued_node_id, "tenant-a");
    stub.script_attest(Ok(AttestNodeResponse {
        challenge_nonce: vec![5u8; 32],
        challenge_id: Uuid::new_v4().to_string(),
    }));
    stub.script_challenge(Ok(ChallengeNodeResponse {
        node_security_token: issued,
        expires_at: None,
    }));
    let (endpoint, _shutdown) = spawn_scripted_server(stub.clone()).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let token = make_enrollment_jwt("BEASTLY1", "tenant-a", &endpoint);
    enroll_run(enroll_args(token, tmp.path()), OutputFormat::Text)
        .await
        .expect("first-run enroll must succeed");

    let cfg = tmp.path().join("aegis-config.yaml");
    assert!(cfg.exists(), "aegis-config.yaml must exist after first run");
    let persisted = read_persisted_node_id(tmp.path());
    Uuid::parse_str(&persisted).unwrap_or_else(|_| {
        panic!("spec.node.id must be a UUID, got '{persisted}'");
    });
}

/// Operator-supplied JWT `sub` (Zaru's friendly name like "BEASTLY1") is NOT
/// a UUID. Enrollment must succeed — the CLI mints its own UUID `node_id`
/// and presents it on the wire. The strict-UUID stub guard turns any
/// regression into a hard test failure here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_with_non_uuid_jwt_sub_succeeds_when_node_id_is_minted_uuid() {
    let stub = ScriptedStub::default();
    let issued_node_id = Uuid::new_v4().to_string();
    let issued = make_node_security_token(&issued_node_id, "tenant-a");
    stub.script_attest(Ok(AttestNodeResponse {
        challenge_nonce: vec![6u8; 32],
        challenge_id: Uuid::new_v4().to_string(),
    }));
    stub.script_challenge(Ok(ChallengeNodeResponse {
        node_security_token: issued,
        expires_at: None,
    }));
    let (endpoint, _shutdown) = spawn_scripted_server(stub.clone()).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    // The exact label Zaru's `AddEdgeHostDialog` puts in `issued_to` — never
    // a UUID. The bug this test pins: prior CLI sent this string verbatim as
    // `node_id`, so the server's strict UUID parse rejected the request.
    let token = make_enrollment_jwt("BEASTLY1", "tenant-a", &endpoint);
    enroll_run(enroll_args(token, tmp.path()), OutputFormat::Text)
        .await
        .expect("enroll with non-UUID JWT sub must succeed (CLI mints its own UUID)");

    let persisted = read_persisted_node_id(tmp.path());
    Uuid::parse_str(&persisted).unwrap_or_else(|_| {
        panic!("spec.node.id must be a UUID, got '{persisted}'");
    });
    assert_ne!(
        persisted, "BEASTLY1",
        "the operator label must NEVER be persisted as node_id"
    );

    // Wire contract: AttestNode + ChallengeNode were called with the minted
    // UUID, not with "BEASTLY1". (The strict-UUID stub guard would have
    // rejected the call otherwise; assert here for explicitness.)
    let snap = stub.snapshot();
    assert_eq!(
        snap.last_attest_node_id.as_deref(),
        Some(persisted.as_str())
    );
    assert_eq!(
        snap.last_challenge_node_id.as_deref(),
        Some(persisted.as_str())
    );
}

/// Persistence contract: a re-enroll on the same host MUST reuse the
/// previously-minted `node_id`. The bootstrap step reads back the existing
/// `spec.node.id` rather than minting a new one — so the daemon's identity
/// is stable across re-enrollments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_persisted_node_id_survives_reenroll() {
    let stub = ScriptedStub::default();
    let (endpoint, _shutdown) = spawn_scripted_server(stub.clone()).await;

    let tmp = tempfile::tempdir().expect("tempdir");

    // Run 1 — fresh state-dir, mint a UUID.
    let issued_1 = make_node_security_token(&Uuid::new_v4().to_string(), "tenant-a");
    stub.script_attest(Ok(AttestNodeResponse {
        challenge_nonce: vec![1u8; 32],
        challenge_id: Uuid::new_v4().to_string(),
    }));
    stub.script_challenge(Ok(ChallengeNodeResponse {
        node_security_token: issued_1,
        expires_at: None,
    }));
    let token_1 = make_enrollment_jwt("BEASTLY1", "tenant-a", &endpoint);
    enroll_run(enroll_args(token_1, tmp.path()), OutputFormat::Text)
        .await
        .expect("first enroll must succeed");
    let minted = read_persisted_node_id(tmp.path());
    let snap_1 = stub.snapshot();
    assert_eq!(snap_1.last_attest_node_id.as_deref(), Some(minted.as_str()));

    // Run 2 — same state-dir, --force to re-run the wire flow. The minted
    // UUID must be reused, not regenerated.
    let issued_2 = make_node_security_token(&Uuid::new_v4().to_string(), "tenant-a");
    stub.script_attest(Ok(AttestNodeResponse {
        challenge_nonce: vec![2u8; 32],
        challenge_id: Uuid::new_v4().to_string(),
    }));
    stub.script_challenge(Ok(ChallengeNodeResponse {
        node_security_token: issued_2,
        expires_at: None,
    }));
    let token_2 = make_enrollment_jwt("BEASTLY1", "tenant-a", &endpoint);
    let mut args_2 = enroll_args(token_2, tmp.path());
    args_2.force = true;
    args_2.non_interactive = false;
    enroll_run(args_2, OutputFormat::Text)
        .await
        .expect("re-enroll with --force must succeed");

    let after = read_persisted_node_id(tmp.path());
    assert_eq!(
        after, minted,
        "persisted node_id must survive re-enroll: was '{minted}', now '{after}'"
    );
    let snap_2 = stub.snapshot();
    assert_eq!(
        snap_2.last_attest_node_id.as_deref(),
        Some(minted.as_str()),
        "re-enroll must present the SAME minted UUID on the wire"
    );
}
