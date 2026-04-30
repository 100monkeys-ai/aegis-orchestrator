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
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use crate::domain::secrets::SecretStore;
use crate::domain::shared_kernel::TenantId;
use crate::infrastructure::aegis_cluster_proto::{
    node_cluster_service_client::NodeClusterServiceClient, IssueEnrollmentTokenRequest,
};

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
/// * [`RelayGrpcEnrollmentTokenIssuer`] — gRPC client used by Controller
///   deployments configured with a `relay_coordinator_endpoint`. Calls
///   `NodeClusterService.IssueEnrollmentToken` on the Relay over the
///   trusted in-pod network.
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

/// gRPC client that forwards enrollment-token requests from a Controller
/// process to a co-located Relay Coordinator via
/// `NodeClusterService.IssueEnrollmentToken`.
///
/// ADR-117: only the `relay-coordinator` AppRole holds the OpenBao policy
/// `transit/sign/edge-enrollment-token`. In SaaS deployments the core
/// orchestrator pod sits alongside a separate `aegis-relay-coordinator` pod
/// reachable on the trusted Podman pod-network at port 50056. The user's
/// Bearer token is forwarded in the `authorization` metadata key and the
/// resolved tenant in `x-aegis-tenant` so the Relay's per-handler IAM
/// validation authenticates against the same `UserIdentity` /
/// `effective_tenant`.
pub struct RelayGrpcEnrollmentTokenIssuer {
    client: NodeClusterServiceClient<Channel>,
}

impl RelayGrpcEnrollmentTokenIssuer {
    /// Build a lazy-connected client against `endpoint` (e.g.
    /// `http://aegis-relay-coordinator:50056`). The TCP/HTTP2 connection is
    /// established on first RPC; constructor does not block on the network.
    pub fn connect_lazy(endpoint: impl Into<String>) -> Result<Self> {
        let ep = Endpoint::from_shared(endpoint.into())
            .map_err(|e| anyhow::anyhow!("invalid relay endpoint: {e}"))?
            .timeout(std::time::Duration::from_secs(10))
            .connect_timeout(std::time::Duration::from_secs(5));
        let channel = ep.connect_lazy();
        Ok(Self {
            client: NodeClusterServiceClient::new(channel),
        })
    }

    /// Construct directly from an existing channel — primarily for tests
    /// using in-process tonic transports.
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            client: NodeClusterServiceClient::new(channel),
        }
    }
}

#[async_trait]
impl EnrollmentTokenIssuer for RelayGrpcEnrollmentTokenIssuer {
    async fn issue(
        &self,
        tenant_id: &TenantId,
        issued_to_sub: &str,
        bearer_token: Option<&str>,
    ) -> Result<IssuedEnrollmentToken> {
        let mut req = tonic::Request::new(IssueEnrollmentTokenRequest {
            issued_to: issued_to_sub.to_string(),
        });
        if let Some(tok) = bearer_token {
            let header = format!("Bearer {tok}");
            let value: MetadataValue<_> = header
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid bearer token for metadata: {e}"))?;
            req.metadata_mut().insert("authorization", value);
        }
        let tenant_value: MetadataValue<_> = tenant_id
            .as_str()
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid tenant id for metadata: {e}"))?;
        req.metadata_mut().insert("x-aegis-tenant", tenant_value);

        let resp = self
            .client
            .clone()
            .issue_enrollment_token(req)
            .await
            .map_err(|s| {
                anyhow::anyhow!(
                    "relay IssueEnrollmentToken failed ({:?}): {}",
                    s.code(),
                    s.message()
                )
            })?
            .into_inner();

        let expires_at = chrono::DateTime::parse_from_rfc3339(&resp.expires_at)
            .map_err(|e| anyhow::anyhow!("relay expires_at parse: {e}"))?
            .with_timezone(&Utc);
        Ok(IssuedEnrollmentToken {
            token: resp.token,
            expires_at,
            controller_endpoint: resp.controller_endpoint,
            qr_payload: resp.qr_payload,
            command_hint: resp.command_hint,
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

    /// Regression: ADR-117 SaaS topology over gRPC. The Relay client MUST
    /// invoke `NodeClusterService.IssueEnrollmentToken` and forward the
    /// caller's Bearer token in the `authorization` metadata key plus the
    /// resolved tenant slug in `x-aegis-tenant` — these are the metadata
    /// keys the Relay's per-handler `validate_grpc_request` reads.
    ///
    /// Replaces the prior REST-proxy test
    /// `relay_proxy_forwards_bearer_and_tenant_to_v1_path`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_grpc_client_forwards_bearer_and_tenant() {
        use crate::infrastructure::aegis_cluster_proto::{
            node_cluster_service_server::{NodeClusterService, NodeClusterServiceServer},
            AttestNodeRequest, AttestNodeResponse, ChallengeNodeRequest, ChallengeNodeResponse,
            DeregisterNodeRequest, DeregisterNodeResponse, EdgeCommand, EdgeEvent,
            ForwardExecutionRequest, IssueEnrollmentTokenResponse, ListPeersRequest,
            ListPeersResponse, NodeHeartbeatRequest, NodeHeartbeatResponse, PushConfigRequest,
            PushConfigResponse, RegisterNodeRequest, RegisterNodeResponse, RotateEdgeKeyRequest,
            RotateEdgeKeyResponse, RouteExecutionRequest, RouteExecutionResponse,
            SyncConfigRequest, SyncConfigResponse,
        };
        use crate::infrastructure::aegis_runtime_proto::ExecutionEvent;
        use std::pin::Pin;
        use std::sync::Mutex;
        use tokio_stream::Stream;
        use tonic::{transport::Server, Request, Response, Status, Streaming};

        struct CapturingService {
            captured:
                Arc<Mutex<Option<(Option<String>, Option<String>, IssueEnrollmentTokenRequest)>>>,
        }
        #[tonic::async_trait]
        impl NodeClusterService for CapturingService {
            async fn issue_enrollment_token(
                &self,
                request: Request<IssueEnrollmentTokenRequest>,
            ) -> Result<Response<IssueEnrollmentTokenResponse>, Status> {
                let auth = request
                    .metadata()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let tenant = request
                    .metadata()
                    .get("x-aegis-tenant")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let inner = request.into_inner();
                *self.captured.lock().unwrap() = Some((auth, tenant, inner));
                Ok(Response::new(IssueEnrollmentTokenResponse {
                    token: "stub-token".into(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    controller_endpoint: "relay.myzaru.com:443".into(),
                    qr_payload: "aegis edge enroll stub-token".into(),
                    command_hint: "aegis edge enroll stub-token".into(),
                }))
            }
            // Unused RPCs — return Unimplemented.
            async fn attest_node(
                &self,
                _: Request<AttestNodeRequest>,
            ) -> Result<Response<AttestNodeResponse>, Status> {
                Err(Status::unimplemented(""))
            }
            async fn challenge_node(
                &self,
                _: Request<ChallengeNodeRequest>,
            ) -> Result<Response<ChallengeNodeResponse>, Status> {
                Err(Status::unimplemented(""))
            }
            async fn register_node(
                &self,
                _: Request<RegisterNodeRequest>,
            ) -> Result<Response<RegisterNodeResponse>, Status> {
                Err(Status::unimplemented(""))
            }
            async fn heartbeat(
                &self,
                _: Request<NodeHeartbeatRequest>,
            ) -> Result<Response<NodeHeartbeatResponse>, Status> {
                Err(Status::unimplemented(""))
            }
            async fn deregister_node(
                &self,
                _: Request<DeregisterNodeRequest>,
            ) -> Result<Response<DeregisterNodeResponse>, Status> {
                Err(Status::unimplemented(""))
            }
            async fn route_execution(
                &self,
                _: Request<RouteExecutionRequest>,
            ) -> Result<Response<RouteExecutionResponse>, Status> {
                Err(Status::unimplemented(""))
            }
            type ForwardExecutionStream =
                Pin<Box<dyn Stream<Item = Result<ExecutionEvent, Status>> + Send>>;
            async fn forward_execution(
                &self,
                _: Request<ForwardExecutionRequest>,
            ) -> Result<Response<Self::ForwardExecutionStream>, Status> {
                Err(Status::unimplemented(""))
            }
            async fn sync_config(
                &self,
                _: Request<SyncConfigRequest>,
            ) -> Result<Response<SyncConfigResponse>, Status> {
                Err(Status::unimplemented(""))
            }
            async fn push_config(
                &self,
                _: Request<PushConfigRequest>,
            ) -> Result<Response<PushConfigResponse>, Status> {
                Err(Status::unimplemented(""))
            }
            async fn list_peers(
                &self,
                _: Request<ListPeersRequest>,
            ) -> Result<Response<ListPeersResponse>, Status> {
                Err(Status::unimplemented(""))
            }
            type ConnectEdgeStream =
                Pin<Box<dyn Stream<Item = Result<EdgeCommand, Status>> + Send>>;
            async fn connect_edge(
                &self,
                _: Request<Streaming<EdgeEvent>>,
            ) -> Result<Response<Self::ConnectEdgeStream>, Status> {
                Err(Status::unimplemented(""))
            }
            async fn rotate_edge_key(
                &self,
                _: Request<RotateEdgeKeyRequest>,
            ) -> Result<Response<RotateEdgeKeyResponse>, Status> {
                Err(Status::unimplemented(""))
            }
        }

        // Loopback TCP transport — see tests/edge_mode_grpc.rs for the
        // canonical in-process tonic pattern. We bind 127.0.0.1:0 (with a
        // ::1 fallback) so concurrent test runs don't fight over a port.
        let captured = Arc::new(Mutex::new(None));
        let svc = CapturingService {
            captured: captured.clone(),
        };

        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(_) => tokio::net::TcpListener::bind("[::1]:0")
                .await
                .expect("loopback bind"),
        };
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("http://{addr}");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            let _ = Server::builder()
                .add_service(NodeClusterServiceServer::new(svc))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let channel = Channel::from_shared(url)
            .unwrap()
            .connect()
            .await
            .expect("connect to in-process server");

        let issuer = RelayGrpcEnrollmentTokenIssuer::from_channel(channel);
        let tenant = TenantId::new("t-consumer").unwrap();
        let issued = issuer
            .issue(&tenant, "alice", Some("caller-jwt"))
            .await
            .expect("rpc must succeed");

        assert_eq!(issued.token, "stub-token");
        assert_eq!(issued.controller_endpoint, "relay.myzaru.com:443");

        let (auth, tenant_meta, inner) = captured
            .lock()
            .unwrap()
            .clone()
            .expect("server must have observed the call");
        assert_eq!(
            auth.as_deref(),
            Some("Bearer caller-jwt"),
            "Bearer token must propagate over the metadata `authorization` key"
        );
        assert_eq!(
            tenant_meta.as_deref(),
            Some("t-consumer"),
            "tenant slug must propagate over `x-aegis-tenant`"
        );
        assert_eq!(inner.issued_to, "alice");

        let _ = shutdown_tx.send(());
    }
}
