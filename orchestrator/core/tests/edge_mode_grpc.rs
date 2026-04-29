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
    ListPeersRequest, ListPeersResponse, NodeHeartbeatRequest, NodeHeartbeatResponse,
    PushConfigRequest, PushConfigResponse, RegisterNodeRequest, RegisterNodeResponse,
    RotateEdgeKeyRequest, RotateEdgeKeyResponse, RouteExecutionRequest, RouteExecutionResponse,
    SealEnvelope, SealNodeEnvelope, SyncConfigRequest, SyncConfigResponse,
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
    async fn update_status(&self, node_id: &NodeId, status: NodePeerStatus) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.status = status;
        }
        Ok(())
    }
    async fn update_tags(&self, node_id: &NodeId, tags: &[String]) -> anyhow::Result<()> {
        if let Some(e) = self.edges.lock().await.get_mut(node_id) {
            e.capabilities.tags = tags.to_vec();
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
