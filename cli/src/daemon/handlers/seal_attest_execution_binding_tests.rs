// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The attest route binds an attestation to the execution it names (AEGIS
//! ADR-035 — Updates, volume 2, the Update of 2026-10-21, E1 to E3).
//!
//! E1: a present, non-empty `execution_id` that is not an execution id is
//! refused 400 `invalid_request` before any tenant is resolved; it never
//! falls through to the explicit-tenant or the caller's own tenant. An
//! empty string is read as absent.
//!
//! E2 and E3: the container an attestation names is inspected through a
//! container runtime socket this file serves itself (one canned inspect
//! answer), by the daemon's own verifier (`BollardContainerVerifier`) and
//! the real attestation service; the failure is answered by the route's own
//! `attest_failure_response`. A container whose `aegis.execution_id` label
//! names another execution, or that carries none, is refused 403
//! `WORKLOAD_NOT_OF_EXECUTION`; the claimed execution's own container is
//! attested; an attestation naming no execution keeps the running check
//! alone; a container that is not running is answered 401 as before.

use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};

use aegis_orchestrator_core::application::attestation_service::AttestationServiceImpl;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::iam::{IdentityKind, RealmKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::seal_session::AttestingCredential;
use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
use aegis_orchestrator_core::domain::security_context::{SecurityContext, SecurityContextMetadata};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::docker::BollardContainerVerifier;
use aegis_orchestrator_core::infrastructure::seal::attestation::{
    AttestationRequest, AttestationResponse, AttestationService,
};
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::seal::signature::SecurityTokenIssuer;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;

use super::{resolve_attest_tenant, HttpAttestationRequest};

const CONTAINER: &str = "c0ffee000001";

// ── E1: the execution id the body names ─────────────────────────────────────

fn body() -> HttpAttestationRequest {
    HttpAttestationRequest {
        agent_id: None,
        execution_id: None,
        container_id: None,
        public_key: "k".to_string(),
        security_context: None,
        principal_subject: None,
        workload_id: None,
        tenant_id: None,
        task_summary: None,
    }
}

fn consumer(sub: &str) -> UserIdentity {
    UserIdentity {
        sub: sub.to_string(),
        realm_slug: "zaru-consumer".to_string(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Free,
            tenant_id: TenantId::for_consumer_user(sub).unwrap(),
        },
    }
}

fn service_account() -> UserIdentity {
    UserIdentity {
        sub: "svc-1".to_string(),
        realm_slug: "aegis-system".to_string(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ServiceAccount {
            client_id: "aegis-sdk".to_string(),
        },
    }
}

type Lookup = std::pin::Pin<Box<dyn std::future::Future<Output = Result<TenantId, ()>> + Send>>;

fn no_lookup() -> impl FnOnce(ExecutionId) -> Lookup {
    |_| Box::pin(async { panic!("no execution is looked up for this body") })
}

/// A body naming an execution id that is not one is refused 400
/// `invalid_request`, whoever the caller is; before the change it fell
/// through: a consumer was attested in its own tenant, a service account in
/// the tenant the body named, and neither against the execution it asked
/// for.
#[tokio::test]
async fn a_malformed_execution_id_is_answered_400_invalid_request_not_a_tenant_fallback() {
    let named = TenantId::for_consumer_user("user-target").unwrap();
    for (who, identity) in [
        ("a consumer", consumer("user-alpha")),
        ("a service account", service_account()),
    ] {
        for malformed in ["not-a-uuid", "1234", " "] {
            let mut request = body();
            request.execution_id = Some(malformed.to_string());
            request.tenant_id = Some(named.as_str().to_string());
            let resolved = resolve_attest_tenant(no_lookup(), Some(&identity), &request).await;
            let err = match resolved {
                Ok(tenant) => panic!(
                    "{who}: execution_id {malformed:?} fell through to tenant {}",
                    tenant.as_str()
                ),
                Err(err) => err,
            };
            assert_eq!(
                err.http_status(),
                axum::http::StatusCode::BAD_REQUEST,
                "{who}: {malformed:?}"
            );
            assert_eq!(
                err.message(),
                "invalid_request: invalid execution_id",
                "{who}: {malformed:?}"
            );
        }
    }
}

/// An empty `execution_id` is what the agent bootstrap sends when its
/// execution id is unset; it is read as absent, as before.
#[tokio::test]
async fn an_empty_execution_id_is_read_as_absent() {
    let identity = consumer("user-alpha");
    let mut request = body();
    request.execution_id = Some(String::new());
    let tenant = resolve_attest_tenant(no_lookup(), Some(&identity), &request)
        .await
        .unwrap_or_else(|e| panic!("refused: {}", e.message()));
    assert_eq!(tenant, TenantId::for_consumer_user("user-alpha").unwrap());
}

// ── E2 and E3: the container the attestation names ──────────────────────────

/// A container runtime socket at `<dir>/runtime.sock` that answers every
/// request with one container's inspect: `running`, and `labels` as its
/// `Config.Labels` (`null` for a container with none).
fn runtime_socket(dir: &std::path::Path, running: bool, labels: Value) -> String {
    let path = dir.join("runtime.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let inspect = json!({
        "Id": CONTAINER,
        "State": { "Running": running },
        "Config": { "Labels": labels },
    })
    .to_string();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let inspect = inspect.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut seen = Vec::new();
                let mut buf = [0u8; 4096];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n";
                let answer = format!(
                    "{head}Connection: close\r\nContent-Length: {}\r\n\r\n{inspect}",
                    inspect.len()
                );
                let _ = stream.write_all(answer.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    path.to_str().unwrap().to_string()
}

fn signing_pem() -> &'static str {
    static PEM: OnceLock<String> = OnceLock::new();
    PEM.get_or_init(|| {
        use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        key.to_pkcs1_pem(LineEnding::LF).unwrap().to_string()
    })
}

/// The real attestation service with the daemon's verifier on `socket`.
async fn service_on(socket: &str) -> AttestationServiceImpl {
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts
        .save(SecurityContext {
            name: "zaru-pro".to_string(),
            description: "consumer".to_string(),
            capabilities: vec![],
            deny_list: vec![],
            metadata: SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        })
        .await
        .unwrap();
    let issuer = Arc::new(SecurityTokenIssuer::new(signing_pem(), "aegis-orchestrator").unwrap());
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let verifier = BollardContainerVerifier::new(Some(socket)).unwrap();
    AttestationServiceImpl::new(contexts, sessions, issuer)
        .with_container_verifier(Arc::new(verifier))
}

/// An agent's attestation as the bootstrap sends it: its agent and
/// execution, and the container it runs in.
fn attestation(execution_id: Option<&ExecutionId>) -> AttestationRequest {
    let key = SigningKey::from_bytes(&[5u8; 32]);
    AttestationRequest {
        agent_id: execution_id.map(|_| uuid::Uuid::new_v4().to_string()),
        execution_id: execution_id.map(|id| id.to_string()),
        container_id: Some(CONTAINER.to_string()),
        public_key_pem: STANDARD.encode(key.verifying_key().as_bytes()),
        security_context: Some("zaru-pro".to_string()),
        principal_subject: None,
        user_id: None,
        workload_id: None,
        zaru_tier: Some("pro".to_string()),
        tenant_id: TenantId::consumer(),
        realm: RealmKind::Consumer,
        task_summary: None,
        attested_by: AttestingCredential::Interactive,
    }
}

async fn attest(
    running: bool,
    labels: Value,
    execution_id: Option<&ExecutionId>,
) -> anyhow::Result<AttestationResponse> {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = runtime_socket(dir.path(), running, labels);
    let service = service_on(&socket).await;
    service.attest(attestation(execution_id)).await
}

fn refused(result: anyhow::Result<AttestationResponse>, why: &str) -> anyhow::Error {
    match result {
        Ok(_) => panic!("attested: {why}"),
        Err(e) => e,
    }
}

#[derive(Clone, Default)]
struct Captured(Arc<StdMutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The route's answer to a failed attestation: status, body and the log
/// lines it wrote.
async fn answer(error: &anyhow::Error) -> (u16, Value, String) {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let response = tracing::subscriber::with_default(subscriber, || {
        super::attest_failure_response(error, "zaru-consumer")
    });
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    (status, body, log)
}

/// 403 `WORKLOAD_NOT_OF_EXECUTION` in the route's refusal shape, its fixed
/// message naming no id; `detail` reaches the log under the body's
/// `request_id` and never the body.
fn assert_workload_not_of_execution(status: u16, body: &Value, log: &str, detail: &[String]) {
    let error = &body["error"];
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["protocol"], "seal/v1", "{body}");
    assert_eq!(body["status"], "error", "{body}");
    assert_eq!(error["code"], "WORKLOAD_NOT_OF_EXECUTION", "{body}");
    assert_eq!(
        error["message"],
        "The container this attestation names does not belong to the execution it claims.",
        "{body}"
    );
    assert_eq!(error["context"], Value::Null, "{body}");
    assert_eq!(error["tool"], Value::Null, "{body}");
    let request_id = body["request_id"].as_str().unwrap_or_default();
    assert!(uuid::Uuid::parse_str(request_id).is_ok(), "{body}");
    assert!(log.contains(request_id), "{log}");
    let text = body.to_string();
    for d in detail {
        assert!(!text.contains(d), "the body carried {d:?}: {text}");
        assert!(log.contains(d), "the log lacks {d:?}:\n{log}");
    }
}

#[tokio::test]
async fn a_container_labelled_with_another_execution_is_refused_403_workload_not_of_execution() {
    let claimed = ExecutionId::new();
    let other = ExecutionId::new();
    let labels = json!({ "aegis.execution_id": other.to_string() });
    let result = attest(true, labels, Some(&claimed)).await;
    let e = refused(result, "another execution's container was attested");
    let (status, body, log) = answer(&e).await;
    let detail = [claimed.to_string(), other.to_string(), CONTAINER.to_string()];
    assert_workload_not_of_execution(status, &body, &log, &detail);
}

#[tokio::test]
async fn a_container_without_the_execution_label_is_refused_403_workload_not_of_execution() {
    let claimed = ExecutionId::new();
    for labels in [Value::Null, json!({}), json!({ "aegis.managed": "1" })] {
        let result = attest(true, labels, Some(&claimed)).await;
        let e = refused(result, "an unlabelled container was attested");
        let (status, body, log) = answer(&e).await;
        let detail = [claimed.to_string(), CONTAINER.to_string()];
        assert_workload_not_of_execution(status, &body, &log, &detail);
    }
}

#[tokio::test]
async fn a_container_labelled_with_the_claimed_execution_is_attested() {
    let claimed = ExecutionId::new();
    let labels = json!({ "aegis.execution_id": claimed.to_string(), "aegis.managed": "true" });
    if let Err(e) = attest(true, labels, Some(&claimed)).await {
        panic!("the execution's own container was refused: {e:#}");
    }
}

/// An attestation naming no execution checks only that the container runs,
/// whatever its labels say.
#[tokio::test]
async fn an_attestation_naming_no_execution_keeps_the_running_check_alone() {
    for labels in [
        Value::Null,
        json!({ "aegis.execution_id": ExecutionId::new().to_string() }),
    ] {
        if let Err(e) = attest(true, labels.clone(), None).await {
            panic!("refused with labels {labels}: {e:#}");
        }
    }
}

/// A container that is not running is answered as before: 401 with the
/// verifier's sentence, on the execution branch too.
#[tokio::test]
async fn a_container_not_running_is_answered_401_as_before() {
    let claimed = ExecutionId::new();
    for execution in [Some(&claimed), None] {
        let labels = json!({ "aegis.execution_id": claimed.to_string() });
        let result = attest(false, labels, execution).await;
        let e = refused(result, "a stopped container was attested");
        let (status, body, _) = answer(&e).await;
        assert_eq!(status, 401, "{body}");
        assert!(body.get("protocol").is_none(), "{body}");
        let sentence = body["error"].as_str().unwrap_or_default();
        assert!(sentence.contains("is not in a running state"), "{body}");
        let prefix = "Container identity verification failed: ";
        assert!(sentence.starts_with(prefix), "{body}");
    }
}
