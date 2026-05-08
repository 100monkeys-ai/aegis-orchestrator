// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 SEV-3-A — in-process tonic round-trip for the ConnectEdge bidi
//! stream.
//!
//! Strategy: stand up the orchestrator-side `ConnectEdgeService` behind a
//! tonic server bound to a loopback TCP port, connect a tonic `Channel` to
//! that port, and drive Hello → InvokeTool → CommandResult through the real
//! wire format. The server stub implements only the `connect_edge` RPC; all
//! other methods on `NodeClusterService` return `Status::unimplemented`.
//!
//! The previous edge-mode integration test (`edge_mode.rs`) drives the
//! ConnectEdgeService directly without ever touching tonic. This file
//! complements it by asserting that the proto encoding round-trips cleanly
//! — catches regressions in canonical envelope payloads, stream framing,
//! and the `EdgeConnectionRegistry` semantics that are invisible to the
//! in-memory composition test.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use chrono::Utc;
use prost_types::Struct;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::Stream;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

use aegis_orchestrator_core::application::edge::connect_edge::ConnectEdgeService;
use aegis_orchestrator_core::domain::cluster::NodePeerStatus;
use aegis_orchestrator_core::domain::edge::{
    EdgeCapabilities, EdgeConnectionState, EdgeDaemon, EdgeDaemonRepository,
};
use aegis_orchestrator_core::domain::shared_kernel::{NodeId, TenantId};
use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    edge_command::Command as OutCmd,
    edge_event::Event as InEv,
    node_cluster_service_client::NodeClusterServiceClient,
    node_cluster_service_server::{NodeClusterService, NodeClusterServiceServer},
    AttestNodeRequest, AttestNodeResponse, ChallengeNodeRequest, ChallengeNodeResponse,
    DeregisterNodeRequest, DeregisterNodeResponse, EdgeCapabilities as ProtoEdgeCapabilities,
    EdgeCommand, EdgeEvent, EdgeResult, ForwardExecutionRequest, HelloEvent, InvokeToolCommand,
    IssueEnrollmentTokenRequest, IssueEnrollmentTokenResponse, ListPeersRequest, ListPeersResponse,
    NodeCapabilities as ProtoNodeCapabilities, NodeHeartbeatRequest, NodeHeartbeatResponse,
    NodeRole as ProtoNodeRole, PushConfigRequest, PushConfigResponse, RegisterNodeRequest,
    RegisterNodeResponse, RotateEdgeKeyRequest, RotateEdgeKeyResponse, RouteExecutionRequest,
    RouteExecutionResponse, SealEnvelope, SealNodeEnvelope, SyncConfigRequest, SyncConfigResponse,
};
use aegis_orchestrator_core::infrastructure::aegis_runtime_proto::ExecutionEvent;
use aegis_orchestrator_core::infrastructure::edge::EdgeConnectionRegistry;

/// In-memory `EdgeDaemonRepository` test double for the in-process tonic
/// round-trip suite; mutations fake persistence behind a `Mutex<HashMap>`.
#[derive(Default)]
struct StubEdgeRepo {
    edges: Mutex<HashMap<NodeId, EdgeDaemon>>,
}

#[async_trait]
impl EdgeDaemonRepository for StubEdgeRepo {
    async fn upsert(&self, edge: &EdgeDaemon) -> anyhow::Result<()> {
        self.edges.lock().await.insert(edge.node_id, edge.clone());
        Ok(())
    }
    async fn get(&self, node_id: &NodeId) -> anyhow::Result<Option<EdgeDaemon>> {
        Ok(self.edges.lock().await.get(node_id).cloned())
    }
    async fn list_by_tenant(&self, tenant_id: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>> {
        Ok(self
            .edges
            .lock()
            .await
            .values()
            .filter(|e| &e.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    async fn list_all(&self) -> anyhow::Result<Vec<EdgeDaemon>> {
        Ok(self.edges.lock().await.values().cloned().collect())
    }
    async fn update_status(&self, node_id: &NodeId, status: NodePeerStatus) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.status = status;
        }
        Ok(())
    }
    async fn record_heartbeat(&self, node_id: &NodeId) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.status = NodePeerStatus::Active;
        }
        Ok(())
    }
    async fn update_tags(&self, node_id: &NodeId, tags: &[String]) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.capabilities.tags = tags.to_vec();
        }
        Ok(())
    }
    async fn update_display_name(
        &self,
        node_id: &NodeId,
        display_name: &str,
    ) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.display_name = display_name.to_string();
        }
        Ok(())
    }
    async fn update_capabilities(
        &self,
        node_id: &NodeId,
        capabilities: &EdgeCapabilities,
    ) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.capabilities = capabilities.clone();
        }
        Ok(())
    }
    async fn delete(&self, node_id: &NodeId) -> anyhow::Result<()> {
        self.edges.lock().await.remove(node_id);
        Ok(())
    }
}

/// Minimal `NodeClusterService` impl: only `connect_edge` is wired; every
/// other RPC returns `Status::unimplemented`. Sufficient for round-trip
/// coverage of the edge mode bidi surface — accidental coupling to other
/// RPCs in tests will surface as an `unimplemented` status.
struct EdgeOnlyClusterService {
    connect_edge: Arc<ConnectEdgeService>,
}

#[async_trait]
impl NodeClusterService for EdgeOnlyClusterService {
    async fn attest_node(
        &self,
        _r: Request<AttestNodeRequest>,
    ) -> Result<Response<AttestNodeResponse>, Status> {
        Err(Status::unimplemented("attest_node"))
    }
    async fn challenge_node(
        &self,
        _r: Request<ChallengeNodeRequest>,
    ) -> Result<Response<ChallengeNodeResponse>, Status> {
        Err(Status::unimplemented("challenge_node"))
    }
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
        request: Request<Streaming<EdgeEvent>>,
    ) -> Result<Response<Self::ConnectEdgeStream>, Status> {
        let svc = self.connect_edge.clone();
        aegis_orchestrator_core::infrastructure::edge::grpc_stream::handle_connect_edge(
            svc, request,
        )
        .await
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
        Err(Status::unimplemented(
            "test mock does not implement issue_enrollment_token",
        ))
    }
}

/// Mint a minimal JWT-shaped string with `sub` set to `node_id`. The
/// orchestrator side base64url-decodes the payload segment looking for
/// `sub`; the header and signature segments are placeholders.
fn fake_node_jwt(node_id: NodeId) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let payload_json = format!("{{\"sub\":\"{}\"}}", node_id.0);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
    format!("{header}.{payload}.{sig}")
}

/// Builds a minimal `EdgeDaemon` for the in-process tonic round-trip
/// suite — same shape as the `make_edge` helper in `edge_mode.rs` but
/// kept local (and named differently to reflect the different signature)
/// to avoid a `tests/common` module just for two integration files.
fn make_test_edge(node_id: NodeId, tenant: &TenantId) -> EdgeDaemon {
    EdgeDaemon {
        node_id,
        tenant_id: tenant.clone(),
        public_key: vec![0; 32],
        capabilities: EdgeCapabilities {
            os: "linux".into(),
            arch: "x86_64".into(),
            local_tools: vec!["shell".into()],
            mount_points: vec![],
            custom_labels: Default::default(),
            tags: vec![],
        },
        status: NodePeerStatus::Active,
        connection: EdgeConnectionState::Disconnected { since: Utc::now() },
        last_heartbeat_at: None,
        enrolled_at: Utc::now(),
        display_name: String::new(),
    }
}

/// Bind a loopback listener (IPv4 with IPv6 fallback) and serve only the
/// `connect_edge` RPC. Returns the URL and a shutdown sender.
async fn spawn_edge_only_server(
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    registry: EdgeConnectionRegistry,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(v4_err) => match TcpListener::bind("[::1]:0").await {
            Ok(l) => l,
            Err(v6_err) => panic!("failed to bind 127.0.0.1:0 ({v4_err}) and [::1]:0 ({v6_err})"),
        },
    };
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let connect_svc = Arc::new(ConnectEdgeService::new(edge_repo, registry));
    tokio::spawn(async move {
        let incoming = TcpListenerStream::new(listener);
        let _ = Server::builder()
            .add_service(NodeClusterServiceServer::new(EdgeOnlyClusterService {
                connect_edge: connect_svc,
            }))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = rx.await;
            })
            .await;
    });
    (url, tx)
}

/// Test 1 — Hello → InvokeTool → CommandResult round-trip.
///
/// Asserts:
/// * The daemon-side Hello frame parses on the server.
/// * The server's `EdgeConnectionRegistry::register` records the daemon.
/// * Dispatching an `InvokeToolCommand` via the registered sender reaches
///   the client over the real tonic Channel.
/// * The client-emitted `CommandResultEvent` resolves the pending oneshot
///   on the server, completing the round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_invoke_tool_command_result_round_trip() {
    let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubEdgeRepo::default());
    let registry = EdgeConnectionRegistry::new();
    let tenant = TenantId::new("tenant-a").unwrap();
    let node_id = NodeId::new();
    edge_repo
        .upsert(&make_test_edge(node_id, &tenant))
        .await
        .unwrap();

    let (url, _shutdown) = spawn_edge_only_server(edge_repo.clone(), registry.clone()).await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    // Outbound stream from daemon → server.
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<EdgeEvent>(8);
    let outbound = tokio_stream::wrappers::ReceiverStream::new(out_rx);

    let token = fake_node_jwt(node_id);
    let hello = EdgeEvent {
        event: Some(InEv::Hello(HelloEvent {
            envelope: Some(SealNodeEnvelope {
                node_security_token: token.clone(),
                signature: vec![0; 64],
                payload: b"hello-payload".to_vec(),
            }),
            capabilities: Some(ProtoEdgeCapabilities {
                os: "linux".into(),
                arch: "x86_64".into(),
                local_tools: vec!["shell".into()],
                mount_points: vec![],
                custom_labels: Default::default(),
                tags: vec![],
                node_capabilities: None,
            }),
            stream_id: Uuid::new_v4().to_string(),
            last_seen_command_id: None,
        })),
    };
    out_tx.send(hello).await.expect("send hello");

    let response = client
        .connect_edge(tonic::Request::new(outbound))
        .await
        .expect("connect_edge RPC accepted");
    let mut inbound_cmds = response.into_inner();

    // Wait for the server to register the daemon (Hello processing is async
    // inside handle_connect_edge → ConnectEdgeService::handle_stream).
    let mut sender = None;
    for _ in 0..50 {
        if let Some(s) = registry.get(&node_id) {
            sender = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let sender = sender.expect("server registered the edge connection");

    // Dispatch an InvokeTool through the registry. This is the path the
    // orchestrator's DispatchToEdgeService uses; we exercise it directly so
    // we can verify the wire format flows through tonic without depending on
    // the full dispatch use-case.
    let command_id = Uuid::new_v4();
    let pending = registry.pending().clone();
    let pending_rx = pending.register(command_id, node_id);
    let invoke = EdgeCommand {
        command: Some(OutCmd::InvokeTool(InvokeToolCommand {
            command_id: command_id.to_string(),
            security_context_name: "ctx".into(),
            seal_envelope: Some(SealEnvelope {
                user_security_token: "a.b.c".into(),
                tenant_id: tenant.as_str().to_string(),
                security_context_name: "ctx".into(),
                payload: Some(Struct::default()),
                signature: vec![],
            }),
            tool_name: "cmd.run".into(),
            args: Some(Struct::default()),
            deadline: Some(prost_types::Duration {
                seconds: 5,
                nanos: 0,
            }),
        })),
    };
    sender
        .send(invoke)
        .await
        .expect("server enqueued InvokeTool");

    // Daemon side: receive the InvokeTool over the tonic stream.
    let received = tokio::time::timeout(Duration::from_secs(2), inbound_cmds.message())
        .await
        .expect("received within 2s")
        .expect("no transport error")
        .expect("non-empty message");
    let cmd_id_received = match received.command {
        Some(OutCmd::InvokeTool(i)) => i.command_id,
        other => panic!("expected InvokeTool, got {other:?}"),
    };
    assert_eq!(
        cmd_id_received,
        command_id.to_string(),
        "InvokeTool command_id round-trips through the real wire encoding"
    );

    // Daemon → server CommandResult (also exercising the wire format).
    let result_event = EdgeEvent {
        event: Some(InEv::CommandResult(
            aegis_orchestrator_core::infrastructure::aegis_cluster_proto::CommandResultEvent {
                envelope: Some(SealNodeEnvelope {
                    node_security_token: token.clone(),
                    signature: vec![0; 64],
                    payload: command_id.to_string().into_bytes(),
                }),
                command_id: command_id.to_string(),
                result: Some(EdgeResult {
                    ok: true,
                    exit_code: 0,
                    stdout: b"ok\n".to_vec(),
                    stderr: vec![],
                    structured_result: None,
                    error_kind: String::new(),
                    error_message: String::new(),
                }),
            },
        )),
    };
    out_tx.send(result_event).await.expect("send CommandResult");

    // Server side: pending registry must resolve.
    let resolved = tokio::time::timeout(Duration::from_secs(2), pending_rx)
        .await
        .expect("resolved within 2s")
        .expect("oneshot recv ok");
    let result = resolved.expect("server-side dispatch result Ok");
    assert!(result.ok);
    assert_eq!(result.stdout, b"ok\n");
}

/// Test 2 — drop-guard semantics when the daemon disconnects.
///
/// Dropping the daemon-side `EdgeConnectionGuard` must:
///   1. Remove the registered sender from `EdgeConnectionRegistry`.
///   2. Resolve every pending `oneshot` registered against that node with
///      `EdgeRouterError::EdgeDisconnected`.
///
/// Implemented directly against `EdgeConnectionRegistry` /
/// `PendingEdgeCalls` (no tonic round-trip required), since the guard's
/// invariants are independent of the gRPC layer — they only depend on the
/// `Drop` impl firing once the daemon-side sender is dropped.
#[tokio::test]
async fn drop_guard_resolves_pending_with_edge_disconnected() {
    use aegis_orchestrator_core::domain::edge::EdgeRouterError;
    use tokio::sync::mpsc;

    let registry = EdgeConnectionRegistry::new();
    let node_id = NodeId(Uuid::new_v4());
    let (tx, _rx) = mpsc::channel(8);

    // Register the connection — this hands back the drop-guard.
    let guard = registry.register(node_id, tx);
    assert!(
        registry.get(&node_id).is_some(),
        "registry must contain node after register"
    );

    // Register a pending RPC against the node.
    let cmd_id = Uuid::new_v4();
    let pending_rx = registry.pending().register(cmd_id, node_id);

    // Dropping the guard must (a) remove the registry entry and
    // (b) resolve every pending oneshot with EdgeDisconnected.
    drop(guard);
    assert!(
        registry.get(&node_id).is_none(),
        "registry entry must be removed on drop"
    );

    let resolved = tokio::time::timeout(Duration::from_secs(2), pending_rx)
        .await
        .expect("pending oneshot must resolve within 2s")
        .expect("oneshot recv ok");
    match resolved {
        Err(EdgeRouterError::EdgeDisconnected) => {}
        other => panic!("expected EdgeDisconnected, got {other:?}"),
    }
}

/// Test 3 — server rejects Hello with mismatched signature.
///
/// The orchestrator-side ConnectEdge handler delegates outer envelope
/// verification to a SEAL middleware that wraps the gRPC layer (see
/// `application::edge::connect_edge::handle_stream` source comment). In an
/// isolated tonic test that middleware is not present, so this case is
/// deferred until we can stand up the middleware in-process.
#[tokio::test]
#[ignore = "TODO(adr-117): expand in-process round-trip suite — needs SEAL middleware"]
async fn mismatched_signature_rejected() {}

// ─────────────────────────────────────────────────────────────────────────────
// Production-handler wiring regression: ADR-117 §B/§C
//
// These tests pin the wiring of `ConnectEdgeService` / `RotateEdgeKeyService`
// into the *production* `NodeClusterServiceHandler` (not the
// `EdgeOnlyClusterService` stub used above). Both RPCs must dispatch when
// `with_edge_services` has been called and must return
// `Status::failed_precondition("edge mode disabled on this node")` otherwise.
//
// Background: prior to this commit, `with_edge_services` was never called
// from `cli/src/daemon/server.rs`; both RPCs returned FailedPrecondition in
// production even though the use cases existed. The negative-path tests
// also pin the absent-services fallback so the guard cannot be silently
// removed in a future refactor.
// ─────────────────────────────────────────────────────────────────────────────

mod prod_handler_stubs {
    //! Minimal no-op trait stubs sufficient to construct the production
    //! `NodeClusterServiceHandler`. `connect_edge` / `rotate_edge_key` only
    //! read the edge-service fields — none of the cluster use cases or their
    //! dependencies are reached, so every method here is `unimplemented!`.

    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;

    use aegis_orchestrator_core::application::cluster::{
        AttestNodeUseCase, ChallengeNodeUseCase, ForwardExecutionUseCase, HeartbeatUseCase,
        PushConfigUseCase, RegisterNodeUseCase, RouteExecutionUseCase, SyncConfigUseCase,
    };
    use aegis_orchestrator_core::domain::cluster::{
        ClusterEnrolmentTokenError, ClusterEnrolmentTokenRepository, ConfigLayerRepository,
        ConfigScope, ConfigSnapshot, ConfigType, MergedConfig, NodeCapabilityAdvertisement,
        NodeChallenge, NodeChallengeRepository, NodeCluster, NodeClusterRepository,
        NodeConfigAssignment, NodePeer, NodePeerStatus, NodeRegistryRepository, NodeRouter,
        NodeRouterError, RegisteredNode, ResourceSnapshot, RuntimeRegistryAssignment,
    };
    use aegis_orchestrator_core::domain::secrets::{SecretStore, SecretsError};
    use aegis_orchestrator_core::domain::shared_kernel::NodeId;

    pub struct NoOpClusterRepo;

    #[async_trait]
    impl NodeClusterRepository for NoOpClusterRepo {
        async fn upsert_peer(&self, _peer: &NodePeer) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn find_peer(&self, _node_id: &NodeId) -> anyhow::Result<Option<NodePeer>> {
            unimplemented!("test stub")
        }
        async fn list_peers_by_status(
            &self,
            _status: NodePeerStatus,
        ) -> anyhow::Result<Vec<NodePeer>> {
            unimplemented!("test stub")
        }
        async fn record_heartbeat(
            &self,
            _node_id: &NodeId,
            _snapshot: ResourceSnapshot,
        ) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn mark_unhealthy(&self, _node_id: &NodeId) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn start_drain(&self, _node_id: &NodeId) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn deregister(&self, _node_id: &NodeId, _reason: &str) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn get_config_version(&self, _node_id: &NodeId) -> anyhow::Result<Option<String>> {
            unimplemented!("test stub")
        }
        async fn record_config_version(
            &self,
            _node_id: &NodeId,
            _hash: &str,
        ) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn list_all_peers(&self) -> anyhow::Result<Vec<NodePeer>> {
            unimplemented!("test stub")
        }
        async fn count_by_status(&self) -> anyhow::Result<HashMap<NodePeerStatus, usize>> {
            unimplemented!("test stub")
        }
    }

    pub struct NoOpChallengeRepo;
    #[async_trait]
    impl NodeChallengeRepository for NoOpChallengeRepo {
        async fn save_challenge(&self, _c: &NodeChallenge) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn get_challenge(&self, _id: &uuid::Uuid) -> anyhow::Result<Option<NodeChallenge>> {
            unimplemented!("test stub")
        }
        async fn delete_challenge(&self, _id: &uuid::Uuid) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
    }

    pub struct NoOpEnrolmentTokenRepo;
    #[async_trait]
    impl ClusterEnrolmentTokenRepository for NoOpEnrolmentTokenRepo {
        async fn redeem(
            &self,
            _token: &str,
            _presented_node_id: &NodeId,
        ) -> Result<NodeId, ClusterEnrolmentTokenError> {
            unimplemented!("test stub")
        }
    }

    pub struct NoOpRegistryRepo;
    #[async_trait]
    impl NodeRegistryRepository for NoOpRegistryRepo {
        async fn find_registered_node(
            &self,
            _node_id: &NodeId,
        ) -> anyhow::Result<Option<RegisteredNode>> {
            unimplemented!("test stub")
        }
        async fn list_registered_nodes(&self) -> anyhow::Result<Vec<RegisteredNode>> {
            unimplemented!("test stub")
        }
        async fn upsert_registered_node(&self, _node: &RegisteredNode) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn assign_config(&self, _assignment: &NodeConfigAssignment) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn get_config_assignments(
            &self,
            _node_id: &NodeId,
        ) -> anyhow::Result<Vec<NodeConfigAssignment>> {
            unimplemented!("test stub")
        }
        async fn assign_runtime_registry(
            &self,
            _assignment: &RuntimeRegistryAssignment,
        ) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn get_runtime_registry_assignments(
            &self,
            _node_id: &NodeId,
        ) -> anyhow::Result<Vec<RuntimeRegistryAssignment>> {
            unimplemented!("test stub")
        }
    }

    pub struct NoOpRouter;
    impl NodeRouter for NoOpRouter {
        fn select_worker(
            &self,
            _required: &NodeCapabilityAdvertisement,
            _cluster: &NodeCluster,
        ) -> Result<aegis_orchestrator_core::domain::cluster::ExecutionRoute, NodeRouterError>
        {
            unimplemented!("test stub")
        }
    }

    pub struct NoOpConfigRepo;
    #[async_trait]
    impl ConfigLayerRepository for NoOpConfigRepo {
        async fn get_layer(
            &self,
            _scope: &ConfigScope,
            _scope_key: &str,
            _config_type: &ConfigType,
        ) -> anyhow::Result<Option<ConfigSnapshot>> {
            unimplemented!("test stub")
        }
        async fn upsert_layer(
            &self,
            _scope: &ConfigScope,
            _scope_key: &str,
            _config_type: &ConfigType,
            _payload: serde_json::Value,
        ) -> anyhow::Result<ConfigSnapshot> {
            unimplemented!("test stub")
        }
        async fn get_merged_config(
            &self,
            _node_id: &NodeId,
            _tenant_id: Option<&str>,
            _config_type: &ConfigType,
        ) -> anyhow::Result<MergedConfig> {
            unimplemented!("test stub")
        }
        async fn list_layers(
            &self,
            _config_type: &ConfigType,
        ) -> anyhow::Result<Vec<ConfigSnapshot>> {
            unimplemented!("test stub")
        }
        async fn delete_layer(
            &self,
            _scope: &ConfigScope,
            _scope_key: &str,
            _config_type: &ConfigType,
        ) -> anyhow::Result<bool> {
            unimplemented!("test stub")
        }
    }

    pub struct NoOpSecretStore;
    #[async_trait]
    impl SecretStore for NoOpSecretStore {
        async fn read(
            &self,
            _: &str,
            _: &str,
        ) -> Result<
            HashMap<String, aegis_orchestrator_core::domain::secrets::SensitiveString>,
            SecretsError,
        > {
            unimplemented!("test stub")
        }
        async fn write(
            &self,
            _: &str,
            _: &str,
            _: HashMap<String, aegis_orchestrator_core::domain::secrets::SensitiveString>,
        ) -> Result<(), SecretsError> {
            unimplemented!("test stub")
        }
        async fn generate_dynamic(
            &self,
            _: &str,
            _: &str,
        ) -> Result<aegis_orchestrator_core::domain::secrets::DomainDynamicSecret, SecretsError>
        {
            unimplemented!("test stub")
        }
        async fn renew_lease(
            &self,
            _: &str,
            _: std::time::Duration,
        ) -> Result<std::time::Duration, SecretsError> {
            unimplemented!("test stub")
        }
        async fn revoke_lease(&self, _: &str) -> Result<(), SecretsError> {
            unimplemented!("test stub")
        }
        async fn transit_sign(&self, _: &str, _: &[u8]) -> Result<String, SecretsError> {
            unimplemented!("test stub")
        }
        async fn transit_verify(&self, _: &str, _: &[u8], _: &str) -> Result<bool, SecretsError> {
            unimplemented!("test stub")
        }
        async fn transit_encrypt(&self, _: &str, _: &[u8]) -> Result<String, SecretsError> {
            unimplemented!("test stub")
        }
        async fn transit_decrypt(&self, _: &str, _: &str) -> Result<Vec<u8>, SecretsError> {
            unimplemented!("test stub")
        }
    }

    /// Tuple of stubbed cluster use cases produced by [`build_use_cases`].
    /// Aliased to keep the function signature within clippy's type-complexity
    /// threshold; the production handler consumes these in fixed order.
    pub type ClusterUseCaseBundle = (
        Arc<AttestNodeUseCase>,
        Arc<ChallengeNodeUseCase>,
        Arc<RegisterNodeUseCase>,
        Arc<HeartbeatUseCase>,
        Arc<RouteExecutionUseCase>,
        Arc<ForwardExecutionUseCase>,
        Arc<SyncConfigUseCase>,
        Arc<PushConfigUseCase>,
        Arc<dyn NodeClusterRepository>,
    );

    /// Build a fully-stubbed bundle of cluster use cases for the production
    /// handler. None of them are reachable from the edge RPCs under test.
    pub fn build_use_cases() -> ClusterUseCaseBundle {
        let cluster_repo: Arc<dyn NodeClusterRepository> = Arc::new(NoOpClusterRepo);
        let challenge_repo: Arc<dyn NodeChallengeRepository> = Arc::new(NoOpChallengeRepo);
        let enrolment_token_repo: Arc<dyn ClusterEnrolmentTokenRepository> =
            Arc::new(NoOpEnrolmentTokenRepo);
        let registry_repo: Arc<dyn NodeRegistryRepository> = Arc::new(NoOpRegistryRepo);
        let router: Arc<dyn NodeRouter> = Arc::new(NoOpRouter);
        let config_repo: Arc<dyn ConfigLayerRepository> = Arc::new(NoOpConfigRepo);
        let secret_store: Arc<dyn SecretStore> = Arc::new(NoOpSecretStore);
        let controller_node_id = NodeId::new();

        let attest_uc = Arc::new(AttestNodeUseCase::new(
            challenge_repo.clone(),
            enrolment_token_repo,
        ));
        let challenge_uc = Arc::new(ChallengeNodeUseCase::new(
            challenge_repo,
            cluster_repo.clone(),
            secret_store,
            "edge-enrollment-token".to_string(),
        ));
        let register_uc = Arc::new(RegisterNodeUseCase::new(
            cluster_repo.clone(),
            registry_repo,
            controller_node_id,
        ));
        let heartbeat_uc = Arc::new(HeartbeatUseCase::new(cluster_repo.clone()));
        let route_uc = Arc::new(RouteExecutionUseCase::new(
            cluster_repo.clone(),
            router,
            controller_node_id,
        ));
        // ForwardExecutionUseCase needs an ExecutionService — we never reach
        // it from connect_edge / rotate_edge_key, but constructing the
        // handler still requires a value. Use a no-op stub.
        let exec_svc: Arc<dyn aegis_orchestrator_core::application::execution::ExecutionService> =
            Arc::new(NoOpExecutionService);
        let forward_uc = Arc::new(ForwardExecutionUseCase::new(exec_svc));
        let sync_config_uc = Arc::new(SyncConfigUseCase::new(
            Arc::new(NoOpConfigRepo),
            cluster_repo.clone(),
        ));
        let push_config_uc = Arc::new(PushConfigUseCase::new(config_repo));

        (
            attest_uc,
            challenge_uc,
            register_uc,
            heartbeat_uc,
            route_uc,
            forward_uc,
            sync_config_uc,
            push_config_uc,
            cluster_repo,
        )
    }

    pub struct NoOpExecutionService;
    #[async_trait]
    impl aegis_orchestrator_core::application::execution::ExecutionService for NoOpExecutionService {
        async fn start_execution(
            &self,
            _agent_id: aegis_orchestrator_core::domain::agent::AgentId,
            _input: aegis_orchestrator_core::domain::execution::ExecutionInput,
            _security_context_name: String,
            _identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::ExecutionId> {
            unimplemented!("test stub")
        }
        async fn start_execution_with_id(
            &self,
            _execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
            _agent_id: aegis_orchestrator_core::domain::agent::AgentId,
            _input: aegis_orchestrator_core::domain::execution::ExecutionInput,
            _security_context_name: String,
            _identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::ExecutionId> {
            unimplemented!("test stub")
        }
        async fn start_child_execution(
            &self,
            _agent_id: aegis_orchestrator_core::domain::agent::AgentId,
            _input: aegis_orchestrator_core::domain::execution::ExecutionInput,
            _parent_execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::ExecutionId> {
            unimplemented!("test stub")
        }
        async fn get_execution_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::shared_kernel::TenantId,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::Execution> {
            unimplemented!("test stub")
        }
        async fn get_execution_unscoped(
            &self,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::Execution> {
            unimplemented!("test stub")
        }
        async fn get_iterations_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::shared_kernel::TenantId,
            _exec_id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<Vec<aegis_orchestrator_core::domain::execution::Iteration>> {
            unimplemented!("test stub")
        }
        async fn cancel_execution_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::shared_kernel::TenantId,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn stream_execution(
            &self,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<
            std::pin::Pin<
                Box<
                    dyn tokio_stream::Stream<
                            Item = anyhow::Result<
                                aegis_orchestrator_core::domain::events::ExecutionEvent,
                            >,
                        > + Send,
                >,
            >,
        > {
            unimplemented!("test stub")
        }
        async fn stream_agent_events(
            &self,
            _id: aegis_orchestrator_core::domain::agent::AgentId,
        ) -> anyhow::Result<
            std::pin::Pin<
                Box<
                    dyn tokio_stream::Stream<
                            Item = anyhow::Result<
                                aegis_orchestrator_core::infrastructure::event_bus::DomainEvent,
                            >,
                        > + Send,
                >,
            >,
        > {
            unimplemented!("test stub")
        }
        async fn list_executions_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::shared_kernel::TenantId,
            _agent_id: Option<aegis_orchestrator_core::domain::agent::AgentId>,
            _workflow_id: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
            _limit: usize,
        ) -> anyhow::Result<Vec<aegis_orchestrator_core::domain::execution::Execution>> {
            unimplemented!("test stub")
        }
        async fn delete_execution_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::shared_kernel::TenantId,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn record_llm_interaction(
            &self,
            _execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
            _iteration: u8,
            _interaction: aegis_orchestrator_core::domain::execution::LlmInteraction,
        ) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
        async fn store_iteration_trajectory(
            &self,
            _execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
            _iteration: u8,
            _trajectory: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
        ) -> anyhow::Result<()> {
            unimplemented!("test stub")
        }
    }
}

/// Build a production `NodeClusterServiceHandler` with all use cases stubbed.
/// Only `connect_edge` / `rotate_edge_key` are exercised by the regression
/// tests below — the stubs panic for any other code path.
fn build_prod_handler(
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    registry: EdgeConnectionRegistry,
    wire_edge: bool,
) -> aegis_orchestrator_core::infrastructure::cluster::grpc_server::NodeClusterServiceHandler {
    use aegis_orchestrator_core::application::edge::connect_edge::ConnectEdgeService;
    use aegis_orchestrator_core::application::edge::rotate_edge_key::RotateEdgeKeyService;
    use aegis_orchestrator_core::infrastructure::cluster::grpc_server::NodeClusterServiceHandler;

    let (
        attest_uc,
        challenge_uc,
        register_uc,
        heartbeat_uc,
        route_uc,
        forward_uc,
        sync_config_uc,
        push_config_uc,
        cluster_repo,
    ) = prod_handler_stubs::build_use_cases();

    let mut handler = NodeClusterServiceHandler::new(
        attest_uc,
        challenge_uc,
        register_uc,
        heartbeat_uc,
        route_uc,
        Some(forward_uc),
        sync_config_uc,
        push_config_uc,
        cluster_repo,
    );
    if wire_edge {
        let connect_svc = Arc::new(ConnectEdgeService::new(edge_repo.clone(), registry));
        let rotate_svc = Arc::new(RotateEdgeKeyService::with_default_ttl(
            edge_repo,
            Arc::new(prod_handler_stubs::NoOpSecretStore),
            "edge-enrollment-token".to_string(),
        ));
        handler = handler.with_edge_services(connect_svc, rotate_svc);
    }
    handler
}

/// Spawn the *production* `NodeClusterServiceHandler` (not the
/// `EdgeOnlyClusterService` stub) bound to a loopback TCP port. Returns the
/// URL and a shutdown sender.
async fn spawn_prod_handler_server(
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    registry: EdgeConnectionRegistry,
    wire_edge: bool,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::node_cluster_service_server::NodeClusterServiceServer;

    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(v4_err) => match TcpListener::bind("[::1]:0").await {
            Ok(l) => l,
            Err(v6_err) => panic!("failed to bind 127.0.0.1:0 ({v4_err}) and [::1]:0 ({v6_err})"),
        },
    };
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handler = build_prod_handler(edge_repo, registry, wire_edge);
    tokio::spawn(async move {
        let incoming = TcpListenerStream::new(listener);
        let _ = Server::builder()
            .add_service(NodeClusterServiceServer::new(handler))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = rx.await;
            })
            .await;
    });
    (url, tx)
}

/// Regression: ConnectEdge dispatches to `ConnectEdgeService` when
/// `with_edge_services` has been called. Prior to this commit the production
/// handler was constructed in `cli/src/daemon/server.rs` without ever
/// calling `with_edge_services`, so this RPC always returned
/// `Status::failed_precondition("edge mode disabled on this node")`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_edge_dispatches_when_edge_services_wired() {
    let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubEdgeRepo::default());
    let registry = EdgeConnectionRegistry::new();
    let tenant = TenantId::new("tenant-a").unwrap();
    let node_id = NodeId::new();
    edge_repo
        .upsert(&make_test_edge(node_id, &tenant))
        .await
        .unwrap();

    let (url, _shutdown) =
        spawn_prod_handler_server(edge_repo, registry.clone(), /* wire_edge */ true).await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<EdgeEvent>(8);
    let outbound = tokio_stream::wrappers::ReceiverStream::new(out_rx);

    let token = fake_node_jwt(node_id);
    out_tx
        .send(EdgeEvent {
            event: Some(InEv::Hello(HelloEvent {
                envelope: Some(SealNodeEnvelope {
                    node_security_token: token,
                    signature: vec![0; 64],
                    payload: b"hello-payload".to_vec(),
                }),
                capabilities: Some(ProtoEdgeCapabilities {
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    local_tools: vec!["shell".into()],
                    mount_points: vec![],
                    custom_labels: Default::default(),
                    tags: vec![],
                    node_capabilities: None,
                }),
                stream_id: Uuid::new_v4().to_string(),
                last_seen_command_id: None,
            })),
        })
        .await
        .expect("send hello");

    // The RPC must be accepted (i.e. NOT FailedPrecondition). The bidi
    // stream then proceeds asynchronously inside the server.
    let response = client
        .connect_edge(tonic::Request::new(outbound))
        .await
        .expect("ConnectEdge accepted when edge services are wired");
    drop(response);

    // Verify the registry observed the daemon — proves we dispatched into
    // ConnectEdgeService::handle_stream and not into the FailedPrecondition
    // fallback.
    let mut registered = false;
    for _ in 0..50 {
        if registry.get(&node_id).is_some() {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        registered,
        "production handler must register the daemon when with_edge_services is called"
    );
}

/// Regression: ConnectEdge returns `Status::failed_precondition` when
/// `with_edge_services` has NOT been called. Pins the existing fallback so a
/// future contributor can't accidentally remove the guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_edge_returns_failed_precondition_without_edge_services() {
    let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubEdgeRepo::default());
    let registry = EdgeConnectionRegistry::new();

    let (url, _shutdown) =
        spawn_prod_handler_server(edge_repo, registry, /* wire_edge */ false).await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<EdgeEvent>(1);
    let outbound = tokio_stream::wrappers::ReceiverStream::new(out_rx);
    drop(out_tx); // close immediately — we never get past handler entry.

    let err = client
        .connect_edge(tonic::Request::new(outbound))
        .await
        .expect_err("ConnectEdge must fail when edge services are not wired");
    assert_eq!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "expected FailedPrecondition, got {err:?}"
    );
    assert_eq!(err.message(), "edge mode disabled on this node");
}

/// Regression: RotateEdgeKey dispatches to `RotateEdgeKeyService` when
/// `with_edge_services` has been called. We do not assert success of the
/// rotation itself (the stub `SecretStore` panics on `transit_sign`); we
/// only assert that the call is dispatched past the FailedPrecondition
/// guard. Any non-FailedPrecondition error proves the wiring is correct.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotate_edge_key_dispatches_when_edge_services_wired() {
    let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubEdgeRepo::default());
    let registry = EdgeConnectionRegistry::new();

    let (url, _shutdown) =
        spawn_prod_handler_server(edge_repo, registry, /* wire_edge */ true).await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    let req = RotateEdgeKeyRequest {
        current_envelope: None,
        new_public_key: vec![0; 32],
        signature_with_new_key: vec![0; 64],
    };
    let res = client.rotate_edge_key(tonic::Request::new(req)).await;
    match res {
        Ok(_) => {} // surprise success — also fine, wiring is in place.
        Err(status) => {
            assert_ne!(
                status.code(),
                tonic::Code::FailedPrecondition,
                "RotateEdgeKey must dispatch past FailedPrecondition when wired; got {status:?}"
            );
        }
    }
}

/// Regression: RotateEdgeKey returns `Status::failed_precondition` when
/// `with_edge_services` has NOT been called.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotate_edge_key_returns_failed_precondition_without_edge_services() {
    let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubEdgeRepo::default());
    let registry = EdgeConnectionRegistry::new();

    let (url, _shutdown) =
        spawn_prod_handler_server(edge_repo, registry, /* wire_edge */ false).await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    let req = RotateEdgeKeyRequest {
        current_envelope: None,
        new_public_key: vec![0; 32],
        signature_with_new_key: vec![0; 64],
    };
    let err = client
        .rotate_edge_key(tonic::Request::new(req))
        .await
        .expect_err("RotateEdgeKey must fail when edge services are not wired");
    assert_eq!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "expected FailedPrecondition, got {err:?}"
    );
    assert_eq!(err.message(), "edge mode disabled on this node");
}

// ─────────────────────────────────────────────────────────────────────────────
// node_id strict-UUID contract regression coverage
//
// `NodeClusterServiceHandler::attest_node` / `challenge_node` parse `node_id`
// via `NodeId::from_string` → `Uuid::parse_str` and reject anything that
// isn't a UUID with `Status::invalid_argument`. Pinning the contract here
// prevents a future contributor from loosening the parse to accept arbitrary
// strings (which would re-enable the CLI bug where the operator's friendly
// name like "BEASTLY1" was sent verbatim as `node_id`).
// ─────────────────────────────────────────────────────────────────────────────

/// AttestNode rejects a non-UUID `node_id` with InvalidArgument before any
/// use-case dispatch. Operator-supplied display labels (Zaru "BEASTLY1",
/// auto-generated "edge-abc12345") are NOT identifiers and must not pass
/// the parse boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attest_node_rejects_non_uuid_node_id() {
    let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubEdgeRepo::default());
    let registry = EdgeConnectionRegistry::new();
    let (url, _shutdown) =
        spawn_prod_handler_server(edge_repo, registry, /* wire_edge */ false).await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    let req = AttestNodeRequest {
        node_id: "BEASTLY1".to_string(),
        role: ProtoNodeRole::Edge as i32,
        public_key: vec![0u8; 32],
        capabilities: Some(ProtoNodeCapabilities::default()),
        grpc_address: String::new(),
        enrolment_token: String::new(),
    };
    let err = client
        .attest_node(tonic::Request::new(req))
        .await
        .expect_err("AttestNode must reject a non-UUID node_id");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "expected InvalidArgument, got {err:?}"
    );
    assert!(
        err.message().contains("Invalid NodeId"),
        "error must surface 'Invalid NodeId', got: {}",
        err.message()
    );
}

/// ChallengeNode rejects a non-UUID `node_id` with InvalidArgument. Mirrors
/// the AttestNode guard — both handlers are gates on the same UUID contract
/// and the pair must never drift.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn challenge_node_rejects_non_uuid_node_id() {
    let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubEdgeRepo::default());
    let registry = EdgeConnectionRegistry::new();
    let (url, _shutdown) =
        spawn_prod_handler_server(edge_repo, registry, /* wire_edge */ false).await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    let req = ChallengeNodeRequest {
        challenge_id: Uuid::new_v4().to_string(),
        node_id: "edge-abc12345".to_string(),
        challenge_signature: vec![0u8; 64],
        bootstrap_proof: None,
    };
    let err = client
        .challenge_node(tonic::Request::new(req))
        .await
        .expect_err("ChallengeNode must reject a non-UUID node_id");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "expected InvalidArgument, got {err:?}"
    );
    assert!(
        err.message().contains("Invalid node_id"),
        "error must surface 'Invalid node_id', got: {}",
        err.message()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// NodeRole proto-to-domain mapping regression coverage
//
// `NodeClusterServiceHandler::attest_node` translates the inbound proto
// `NodeRole` to the domain enum before constructing `AppAttestNodeRequest`.
// Prior to this commit the `match` had explicit arms only for
// Controller / Worker / Hybrid; Edge, RelayCoordinator, and Unspecified all
// fell through `_ => DomainNodeRole::Worker`. That silently rewrote Edge to
// Worker, so the admission gate at `attest_node.rs:49-53` (which exempts
// Edge from the enrolment-token requirement per ADR-117) never saw the real
// role and rejected legitimate edge daemons with PermissionDenied.
//
// These tests pin all three paths: Edge passes the gate without an enrolment
// token, Worker still requires one, and Unspecified is rejected at the
// adapter boundary.
// ─────────────────────────────────────────────────────────────────────────────

mod node_role_mapping_stubs {
    //! Variant of `prod_handler_stubs` that swaps in working in-memory
    //! `NodeChallengeRepository` and `ClusterEnrolmentTokenRepository`
    //! implementations, so `AttestNodeUseCase::execute` can run end-to-end
    //! without panicking on a `NoOpChallengeRepo`.

    use async_trait::async_trait;
    use parking_lot::Mutex as ParkingMutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    use aegis_orchestrator_core::application::cluster::{
        AttestNodeUseCase, ChallengeNodeUseCase, ForwardExecutionUseCase, HeartbeatUseCase,
        PushConfigUseCase, RegisterNodeUseCase, RouteExecutionUseCase, SyncConfigUseCase,
    };
    use aegis_orchestrator_core::domain::cluster::{
        ClusterEnrolmentTokenError, ClusterEnrolmentTokenRepository, NodeChallenge,
        NodeChallengeRepository, NodeClusterRepository,
    };
    use aegis_orchestrator_core::domain::shared_kernel::NodeId;

    use super::prod_handler_stubs;

    /// In-memory challenge repo: stores challenges in a `Mutex<HashMap>`.
    /// Required for the AttestNode happy-path tests because the use case
    /// calls `save_challenge` once admission passes.
    pub struct InMemoryChallengeRepo {
        pub saved: ParkingMutex<HashMap<uuid::Uuid, NodeChallenge>>,
    }

    impl Default for InMemoryChallengeRepo {
        fn default() -> Self {
            Self {
                saved: ParkingMutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl NodeChallengeRepository for InMemoryChallengeRepo {
        async fn save_challenge(&self, challenge: &NodeChallenge) -> anyhow::Result<()> {
            self.saved
                .lock()
                .insert(challenge.challenge_id, challenge.clone());
            Ok(())
        }
        async fn get_challenge(
            &self,
            challenge_id: &uuid::Uuid,
        ) -> anyhow::Result<Option<NodeChallenge>> {
            Ok(self.saved.lock().get(challenge_id).cloned())
        }
        async fn delete_challenge(&self, challenge_id: &uuid::Uuid) -> anyhow::Result<()> {
            self.saved.lock().remove(challenge_id);
            Ok(())
        }
    }

    /// In-memory enrolment-token repo. Only the AttestNode Worker-path test
    /// invokes it indirectly; the empty-token branch in the use case
    /// short-circuits before touching this repo so `redeem` always returns
    /// `NotFound`.
    pub struct InMemoryEnrolmentRepo;

    #[async_trait]
    impl ClusterEnrolmentTokenRepository for InMemoryEnrolmentRepo {
        async fn redeem(
            &self,
            _token: &str,
            _presented_node_id: &NodeId,
        ) -> Result<NodeId, ClusterEnrolmentTokenError> {
            Err(ClusterEnrolmentTokenError::NotFound)
        }
    }

    /// Aggregate of cluster use cases plus the underlying in-memory challenge
    /// repo handle, returned from `build_use_cases_with_real_attest`. Modeled
    /// as a struct rather than a tuple to keep clippy's `type_complexity`
    /// lint happy.
    pub struct RealAttestUseCases {
        pub attest_uc: Arc<AttestNodeUseCase>,
        pub challenge_uc: Arc<ChallengeNodeUseCase>,
        pub register_uc: Arc<RegisterNodeUseCase>,
        pub heartbeat_uc: Arc<HeartbeatUseCase>,
        pub route_uc: Arc<RouteExecutionUseCase>,
        pub forward_uc: Arc<ForwardExecutionUseCase>,
        pub sync_config_uc: Arc<SyncConfigUseCase>,
        pub push_config_uc: Arc<PushConfigUseCase>,
        pub cluster_repo: Arc<dyn NodeClusterRepository>,
        pub challenge_repo: Arc<InMemoryChallengeRepo>,
    }

    /// Build a fully-wired bundle of cluster use cases where AttestNode has
    /// a working challenge repo + enrolment-token repo. Other use cases keep
    /// the no-op stubs from `prod_handler_stubs` since they are never
    /// reached by the NodeRole-mapping tests.
    pub fn build_use_cases_with_real_attest() -> RealAttestUseCases {
        use aegis_orchestrator_core::domain::cluster::{
            ConfigLayerRepository, NodeRegistryRepository, NodeRouter,
        };
        use aegis_orchestrator_core::domain::secrets::SecretStore;

        let cluster_repo: Arc<dyn NodeClusterRepository> =
            Arc::new(prod_handler_stubs::NoOpClusterRepo);
        let challenge_repo_concrete: Arc<InMemoryChallengeRepo> =
            Arc::new(InMemoryChallengeRepo::default());
        let challenge_repo: Arc<dyn NodeChallengeRepository> = challenge_repo_concrete.clone();
        let enrolment_token_repo: Arc<dyn ClusterEnrolmentTokenRepository> =
            Arc::new(InMemoryEnrolmentRepo);
        let registry_repo: Arc<dyn NodeRegistryRepository> =
            Arc::new(prod_handler_stubs::NoOpRegistryRepo);
        let router: Arc<dyn NodeRouter> = Arc::new(prod_handler_stubs::NoOpRouter);
        let config_repo: Arc<dyn ConfigLayerRepository> =
            Arc::new(prod_handler_stubs::NoOpConfigRepo);
        let secret_store: Arc<dyn SecretStore> = Arc::new(prod_handler_stubs::NoOpSecretStore);
        let controller_node_id = NodeId::new();

        let attest_uc = Arc::new(AttestNodeUseCase::new(
            challenge_repo.clone(),
            enrolment_token_repo,
        ));
        let challenge_uc = Arc::new(ChallengeNodeUseCase::new(
            challenge_repo,
            cluster_repo.clone(),
            secret_store,
            "edge-enrollment-token".to_string(),
        ));
        let register_uc = Arc::new(RegisterNodeUseCase::new(
            cluster_repo.clone(),
            registry_repo,
            controller_node_id,
        ));
        let heartbeat_uc = Arc::new(HeartbeatUseCase::new(cluster_repo.clone()));
        let route_uc = Arc::new(RouteExecutionUseCase::new(
            cluster_repo.clone(),
            router,
            controller_node_id,
        ));
        let exec_svc: Arc<dyn aegis_orchestrator_core::application::execution::ExecutionService> =
            Arc::new(prod_handler_stubs::NoOpExecutionService);
        let forward_uc = Arc::new(ForwardExecutionUseCase::new(exec_svc));
        let sync_config_uc = Arc::new(SyncConfigUseCase::new(
            Arc::new(prod_handler_stubs::NoOpConfigRepo),
            cluster_repo.clone(),
        ));
        let push_config_uc = Arc::new(PushConfigUseCase::new(config_repo));

        RealAttestUseCases {
            attest_uc,
            challenge_uc,
            register_uc,
            heartbeat_uc,
            route_uc,
            forward_uc,
            sync_config_uc,
            push_config_uc,
            cluster_repo,
            challenge_repo: challenge_repo_concrete,
        }
    }
}

/// Build a `NodeClusterServiceHandler` with a real in-memory challenge repo
/// behind `AttestNodeUseCase`. Returns the handler plus a handle to the
/// challenge repo so tests can assert on persisted challenges.
fn build_handler_with_real_attest() -> (
    aegis_orchestrator_core::infrastructure::cluster::grpc_server::NodeClusterServiceHandler,
    Arc<node_role_mapping_stubs::InMemoryChallengeRepo>,
) {
    use aegis_orchestrator_core::infrastructure::cluster::grpc_server::NodeClusterServiceHandler;

    let bundle = node_role_mapping_stubs::build_use_cases_with_real_attest();

    let handler = NodeClusterServiceHandler::new(
        bundle.attest_uc,
        bundle.challenge_uc,
        bundle.register_uc,
        bundle.heartbeat_uc,
        bundle.route_uc,
        Some(bundle.forward_uc),
        bundle.sync_config_uc,
        bundle.push_config_uc,
        bundle.cluster_repo,
    );
    (handler, bundle.challenge_repo)
}

async fn spawn_attest_server() -> (
    String,
    tokio::sync::oneshot::Sender<()>,
    Arc<node_role_mapping_stubs::InMemoryChallengeRepo>,
) {
    use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::node_cluster_service_server::NodeClusterServiceServer;

    let (handler, challenge_repo) = build_handler_with_real_attest();
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(v4_err) => match TcpListener::bind("[::1]:0").await {
            Ok(l) => l,
            Err(v6_err) => panic!("failed to bind 127.0.0.1:0 ({v4_err}) and [::1]:0 ({v6_err})"),
        },
    };
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}");
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let incoming = TcpListenerStream::new(listener);
        let _ = Server::builder()
            .add_service(NodeClusterServiceServer::new(handler))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = rx.await;
            })
            .await;
    });
    (url, tx, challenge_repo)
}

/// Regression: an Edge daemon hitting `AttestNode` with an empty
/// enrolment_token must NOT be rejected by the cluster admission gate. Prior
/// to this commit, `grpc_server.rs:197-208` translated proto NodeRole::Edge
/// to DomainNodeRole::Worker via a catch-all arm; the admission gate at
/// `attest_node.rs:49-53` exempts Edge but never saw the real role and
/// rejected the request with PermissionDenied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attest_node_accepts_edge_role_without_enrolment_token() {
    let (url, _shutdown, challenge_repo) = spawn_attest_server().await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    let node_id = NodeId::new();
    let req = AttestNodeRequest {
        node_id: node_id.0.to_string(),
        role: ProtoNodeRole::Edge as i32,
        public_key: vec![0u8; 32],
        capabilities: Some(ProtoNodeCapabilities::default()),
        grpc_address: String::new(),
        enrolment_token: String::new(),
    };
    let resp = client
        .attest_node(tonic::Request::new(req))
        .await
        .expect("Edge attest must succeed without enrolment_token");
    let inner = resp.into_inner();
    assert_eq!(
        inner.challenge_nonce.len(),
        32,
        "challenge_nonce must be 32 bytes (two UUIDv4 concatenated)"
    );
    assert!(
        !challenge_repo.saved.lock().is_empty(),
        "AttestNode must persist a challenge for Edge role"
    );
}

/// Regression: pins the worker admission gate. A Worker-role attest with an
/// empty enrolment_token must surface `PermissionDenied("missing
/// enrolment_token")`. Prior to the inbound-mapping fix this path was
/// already correct; the test exists to ensure the gate stays in place after
/// the fix and that fixing the Edge mapping did not loosen the Worker path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attest_node_rejects_worker_role_without_enrolment_token() {
    let (url, _shutdown, _challenge_repo) = spawn_attest_server().await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    let node_id = NodeId::new();
    let req = AttestNodeRequest {
        node_id: node_id.0.to_string(),
        role: ProtoNodeRole::Worker as i32,
        public_key: vec![0u8; 32],
        capabilities: Some(ProtoNodeCapabilities::default()),
        grpc_address: String::new(),
        enrolment_token: String::new(),
    };
    let err = client
        .attest_node(tonic::Request::new(req))
        .await
        .expect_err("Worker attest must reject empty enrolment_token");
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "expected PermissionDenied, got {err:?}"
    );
    assert!(
        err.message().contains("missing enrolment_token"),
        "error must surface 'missing enrolment_token', got: {}",
        err.message()
    );
}

/// Regression: the gRPC adapter must reject `NodeRole::Unspecified` at the
/// proto-to-domain mapping boundary with InvalidArgument. Replaces the
/// pre-fix silent fallback that mapped Unspecified to DomainNodeRole::Worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attest_node_rejects_unspecified_role() {
    let (url, _shutdown, _challenge_repo) = spawn_attest_server().await;
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .expect("connect to in-process server");
    let mut client = NodeClusterServiceClient::new(channel);

    let node_id = NodeId::new();
    let req = AttestNodeRequest {
        node_id: node_id.0.to_string(),
        role: ProtoNodeRole::Unspecified as i32,
        public_key: vec![0u8; 32],
        capabilities: Some(ProtoNodeCapabilities::default()),
        grpc_address: String::new(),
        enrolment_token: String::new(),
    };
    let err = client
        .attest_node(tonic::Request::new(req))
        .await
        .expect_err("Unspecified role must be rejected at the adapter boundary");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "expected InvalidArgument, got {err:?}"
    );
    assert!(
        err.message().contains("invalid NodeRole"),
        "error must surface 'invalid NodeRole', got: {}",
        err.message()
    );
}
