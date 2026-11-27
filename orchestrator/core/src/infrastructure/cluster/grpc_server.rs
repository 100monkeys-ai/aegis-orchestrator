// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::application::cluster::challenge_node::BootstrapProof;
use crate::application::cluster::{
    AttestNodeRequest as AppAttestNodeRequest, AttestNodeUseCase,
    ChallengeNodeRequest as AppChallengeNodeRequest, ChallengeNodeUseCase,
    ForwardExecutionRequest as AppForwardExecutionRequest, ForwardExecutionUseCase,
    HeartbeatRequest as AppHeartbeatRequest, HeartbeatUseCase, PushConfigUseCase,
    RegisterNodeRequest as AppRegisterNodeRequest, RegisterNodeUseCase,
    RouteExecutionRequest as AppRouteExecutionRequest, RouteExecutionUseCase, SyncConfigResult,
    SyncConfigUseCase,
};
use crate::domain::agent::AgentId;
use crate::domain::cluster::{
    NodeCapabilityAdvertisement, NodeClusterRepository, NodeId, NodePeerStatus,
    NodeRole as DomainNodeRole,
};
use crate::domain::execution::ExecutionId;
use crate::domain::volume::TenantId;
use crate::infrastructure::aegis_cluster_proto::{
    node_cluster_service_server::NodeClusterService, AttestNodeRequest, AttestNodeResponse,
    ChallengeNodeRequest, ChallengeNodeResponse, DeregisterNodeInner, DeregisterNodeRequest,
    DeregisterNodeResponse, ForwardExecutionInner, ForwardExecutionRequest, ListPeersInner,
    ListPeersRequest, ListPeersResponse, NodeCapabilities as ProtoNodeCapabilities,
    NodeHeartbeatInner, NodeHeartbeatRequest, NodeHeartbeatResponse, NodeInfo, PushConfigInner,
    PushConfigRequest, PushConfigResponse, RegisterNodeInner, RegisterNodeRequest,
    RegisterNodeResponse, RouteExecutionInner, RouteExecutionRequest, RouteExecutionResponse,
    SealNodeEnvelope as ProtoEnvelope, SyncConfigInner, SyncConfigRequest, SyncConfigResponse,
};
use prost::Message;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

use crate::application::cluster::membership::NodeMembership;
use crate::application::edge::connect_edge::ConnectEdgeService;
use crate::application::edge::issue_enrollment_token::IssueEnrollmentToken;
use crate::application::edge::rotate_edge_key::RotateEdgeKeyService;
use crate::domain::cluster_membership::NodeIncarnation;
use crate::infrastructure::aegis_cluster_proto::{
    EdgeCommand, EdgeEvent, IssueEnrollmentTokenRequest, IssueEnrollmentTokenResponse,
    RotateEdgeKeyRequest, RotateEdgeKeyResponse,
};
use crate::infrastructure::aegis_cluster_proto::{
    FenceInner, FenceRequest, FenceResponse, ProbeInner, ProbeRequest, ProbeResponse,
};
use crate::infrastructure::cluster::node_envelope::{
    ClusterNodeKeys, EnvelopeError, NodeEnvelopeVerifier,
};
use crate::infrastructure::cluster::peer_transport::member_state_to_proto;
use crate::infrastructure::edge::grpc_stream::{handle_connect_edge, handle_rotate_edge_key};
use crate::presentation::grpc::auth_interceptor::{validate_grpc_request, GrpcIamAuthInterceptor};

pub struct NodeClusterServiceHandler {
    attest_node_use_case: Arc<AttestNodeUseCase>,
    challenge_node_use_case: Arc<ChallengeNodeUseCase>,
    register_node_use_case: Arc<RegisterNodeUseCase>,
    heartbeat_use_case: Arc<HeartbeatUseCase>,
    route_execution_use_case: Arc<RouteExecutionUseCase>,
    /// ADR-117: present on nodes that host agent execution (Controller /
    /// Worker / Hybrid). Absent on RelayCoordinator (the relay never
    /// dispatches forwarded executions — it brokers edge daemon streams);
    /// calling ForwardExecution in that case returns FailedPrecondition.
    forward_execution_use_case: Option<Arc<ForwardExecutionUseCase>>,
    sync_config_use_case: Arc<SyncConfigUseCase>,
    push_config_use_case: Arc<PushConfigUseCase>,
    cluster_repo: Arc<dyn NodeClusterRepository>,
    /// Opens every node RPC's `SealNodeEnvelope`; shared with the other
    /// receivers in this process.
    envelope_verifier: Arc<NodeEnvelopeVerifier>,
    /// ADR-117: present when this node serves Controller/Hybrid/RelayCoordinator
    /// roles. None on pure-worker nodes; calling ConnectEdge in that case
    /// returns FailedPrecondition.
    connect_edge_service: Option<Arc<ConnectEdgeService>>,
    rotate_edge_key_service: Option<Arc<RotateEdgeKeyService>>,
    /// ADR-117: present on RelayCoordinator deployments — the relay holds
    /// the OpenBao policy `transit/sign/edge-enrollment-token` and is the
    /// canonical signer. Absent elsewhere; calling IssueEnrollmentToken
    /// returns FailedPrecondition.
    issue_enrollment_token_use_case: Option<Arc<IssueEnrollmentToken>>,
    /// Per-handler IAM bearer-JWT validator for the IssueEnrollmentToken
    /// RPC. The cluster gRPC server has no global interceptor (node-IAM
    /// RPCs use SealNodeEnvelope), so this validator is plumbed through
    /// the handler struct and invoked only inside the user-facing
    /// IssueEnrollmentToken handler. None when grpc_auth is disabled.
    grpc_auth: Option<GrpcIamAuthInterceptor>,
    /// ADR-152: this process's cluster membership, which answers `Probe`
    /// and takes `Fence`. None on nodes that have not joined a cluster; the
    /// RPCs then return FailedPrecondition.
    membership: Option<Arc<NodeMembership>>,
    /// The owner's side of a forwarded cancel (ADR-152 D5.4): present on a
    /// node that runs executions.
    canceller: Option<Arc<dyn crate::application::cluster::OwnedExecutionCanceller>>,
    /// The local bus whose relayed-category events this node streams to its
    /// peers (ADR-152 D3); present on a joined node.
    event_bus: Option<crate::infrastructure::event_bus::EventBus>,
    /// The leases a forwarding node must be live in (ADR-152 D5.5); present
    /// on a joined node that runs executions.
    forward_leases: Option<Arc<dyn crate::domain::cluster_membership::NodeLeaseRepository>>,
}

/// How far a forward's issue time may be from this node's clock (ADR-152
/// D5.5).
pub const FORWARD_MAX_AGE_SECS: i64 = 60;

/// How many envelopes a peer's stream may hold before it is closed and the
/// peer resynchronises (ADR-152 D3.6).
pub const RELAY_OUTBOUND_QUEUE: usize = 4096;

impl NodeClusterServiceHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        attest_node_use_case: Arc<AttestNodeUseCase>,
        challenge_node_use_case: Arc<ChallengeNodeUseCase>,
        register_node_use_case: Arc<RegisterNodeUseCase>,
        heartbeat_use_case: Arc<HeartbeatUseCase>,
        route_execution_use_case: Arc<RouteExecutionUseCase>,
        forward_execution_use_case: Option<Arc<ForwardExecutionUseCase>>,
        sync_config_use_case: Arc<SyncConfigUseCase>,
        push_config_use_case: Arc<PushConfigUseCase>,
        cluster_repo: Arc<dyn NodeClusterRepository>,
        envelope_verifier: Arc<NodeEnvelopeVerifier>,
    ) -> Self {
        Self {
            attest_node_use_case,
            challenge_node_use_case,
            register_node_use_case,
            heartbeat_use_case,
            route_execution_use_case,
            forward_execution_use_case,
            sync_config_use_case,
            push_config_use_case,
            cluster_repo,
            envelope_verifier,
            connect_edge_service: None,
            rotate_edge_key_service: None,
            issue_enrollment_token_use_case: None,
            grpc_auth: None,
            membership: None,
            canceller: None,
            event_bus: None,
            forward_leases: None,
        }
    }

    /// ADR-117: enable Edge Mode handlers. Activate only on roles
    /// Controller / Hybrid / RelayCoordinator.
    pub fn with_edge_services(
        mut self,
        connect_edge: Arc<ConnectEdgeService>,
        rotate: Arc<RotateEdgeKeyService>,
    ) -> Self {
        self.connect_edge_service = Some(connect_edge);
        self.rotate_edge_key_service = Some(rotate);
        self
    }

    /// ADR-117: enable IssueEnrollmentToken on the RelayCoordinator. The
    /// relay is the sole holder of the OpenBao Transit policy
    /// `transit/sign/edge-enrollment-token`; other deployments leave the
    /// use-case absent and the RPC returns FailedPrecondition.
    pub fn with_issue_enrollment_token(
        mut self,
        use_case: Arc<IssueEnrollmentToken>,
        grpc_auth: Option<GrpcIamAuthInterceptor>,
    ) -> Self {
        self.issue_enrollment_token_use_case = Some(use_case);
        self.grpc_auth = grpc_auth;
        self
    }

    /// ADR-152: answer `Probe` and take `Fence` for this process.
    pub fn with_execution_canceller(
        mut self,
        canceller: Arc<dyn crate::application::cluster::OwnedExecutionCanceller>,
    ) -> Self {
        self.canceller = Some(canceller);
        self
    }

    /// ADR-152 D3: stream this node's relayed-category events to its peers.
    pub fn with_event_relay(
        mut self,
        event_bus: crate::infrastructure::event_bus::EventBus,
    ) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    /// ADR-152 D5.5: take forwarded executions from peers live in `leases`.
    pub fn with_forward_acceptance(
        mut self,
        leases: Arc<dyn crate::domain::cluster_membership::NodeLeaseRepository>,
    ) -> Self {
        self.forward_leases = Some(leases);
        self
    }

    pub fn with_membership(mut self, membership: Arc<NodeMembership>) -> Self {
        self.membership = Some(membership);
        self
    }

    fn membership(&self) -> Result<&Arc<NodeMembership>, Status> {
        self.membership
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("this node has not joined a cluster"))
    }

    /// Open a SEAL node envelope (see `node_envelope`) and decode its payload.
    async fn authenticate_node<T: Message + Default>(
        &self,
        proto_envelope: Option<ProtoEnvelope>,
    ) -> Result<(NodeId, T), Status> {
        let envelope = proto_envelope.ok_or(EnvelopeError::Missing)?;
        let token = self
            .envelope_verifier
            .open(&envelope, &ClusterNodeKeys(self.cluster_repo.clone()))
            .await?;
        let inner = T::decode(&envelope.payload[..]).map_err(|e| {
            Status::invalid_argument(format!("Failed to deserialize inner payload: {}", e))
        })?;
        Ok((token.node_id, inner))
    }
}

#[tonic::async_trait]
impl NodeClusterService for NodeClusterServiceHandler {
    /// ADR-152 D1.3: which process of this node answers, in what state, and
    /// when it last renewed its lease.
    async fn probe(
        &self,
        request: Request<ProbeRequest>,
    ) -> Result<Response<ProbeResponse>, Status> {
        let membership = self.membership()?.clone();
        let (caller, inner): (NodeId, ProbeInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;
        if inner.node_id != caller.0.to_string() {
            return Err(Status::permission_denied(
                "the probe names a node other than the one that signed it",
            ));
        }
        let me = membership.process();
        Ok(Response::new(ProbeResponse {
            node_id: me.node_id.0.to_string(),
            incarnation: me.incarnation.0,
            state: member_state_to_proto(membership.state()).into(),
            last_renewed_at: membership
                .last_renewed_at()
                .map(|t| prost_types::Timestamp {
                    seconds: t.timestamp(),
                    nanos: t.timestamp_subsec_nanos() as i32,
                }),
        }))
    }

    /// ADR-152 D6: a peer fences this node's named incarnation.
    async fn fence(
        &self,
        request: Request<FenceRequest>,
    ) -> Result<Response<FenceResponse>, Status> {
        let membership = self.membership()?.clone();
        let (caller, inner): (NodeId, FenceInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;
        let me = membership.process();
        if inner.target_node_id != me.node_id.0.to_string() {
            return Err(Status::invalid_argument(
                "the fence names another node; it is sent to the node it fences",
            ));
        }
        tracing::warn!(by = %caller, reason = %inner.reason, "Fence received");
        let fenced = membership.fence(NodeIncarnation(inner.target_incarnation), &inner.reason);
        Ok(Response::new(FenceResponse {
            fenced,
            incarnation: me.incarnation.0,
        }))
    }

    async fn cancel_execution(
        &self,
        request: Request<crate::infrastructure::aegis_cluster_proto::CancelExecutionRequest>,
    ) -> Result<Response<crate::infrastructure::aegis_cluster_proto::CancelExecutionResponse>, Status>
    {
        let (caller, inner): (
            NodeId,
            crate::infrastructure::aegis_cluster_proto::CancelExecutionInner,
        ) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;
        let Some(canceller) = self.canceller.as_ref() else {
            return Err(Status::failed_precondition("this node runs no executions"));
        };
        let execution_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(&inner.execution_id)
                .map_err(|_| Status::invalid_argument("execution_id is not a UUID"))?,
        );
        let tenant_id = crate::domain::tenant::TenantId::from_string(&inner.tenant_id)
            .map_err(|_| Status::invalid_argument("tenant_id is not a tenant"))?;
        let accepted = canceller
            .cancel_if_owned(&tenant_id, execution_id)
            .await
            .map_err(|e| Status::internal(format!("cancel: {e}")))?;
        tracing::info!(by = %caller, execution_id = %execution_id, accepted, "Forwarded cancel received");
        Ok(Response::new(
            crate::infrastructure::aegis_cluster_proto::CancelExecutionResponse { accepted },
        ))
    }

    async fn attest_node(
        &self,
        request: Request<AttestNodeRequest>,
    ) -> Result<Response<AttestNodeResponse>, Status> {
        let req = request.into_inner();
        let app_req = AppAttestNodeRequest {
            node_id: NodeId::from_string(&req.node_id)
                .map_err(|_| Status::invalid_argument("Invalid NodeId"))?,
            role: match req.role() {
                crate::infrastructure::aegis_cluster_proto::NodeRole::Controller => {
                    DomainNodeRole::Controller
                }
                crate::infrastructure::aegis_cluster_proto::NodeRole::Worker => {
                    DomainNodeRole::Worker
                }
                crate::infrastructure::aegis_cluster_proto::NodeRole::Hybrid => {
                    DomainNodeRole::Hybrid
                }
                crate::infrastructure::aegis_cluster_proto::NodeRole::Edge => DomainNodeRole::Edge,
                crate::infrastructure::aegis_cluster_proto::NodeRole::RelayCoordinator => {
                    DomainNodeRole::RelayCoordinator
                }
                crate::infrastructure::aegis_cluster_proto::NodeRole::Unspecified => {
                    return Err(Status::invalid_argument(
                        "invalid NodeRole: Unspecified is not a valid attestation role",
                    ));
                }
            },
            public_key: req.public_key,
            capabilities: if let Some(cap) = req.capabilities {
                NodeCapabilityAdvertisement {
                    gpu_count: cap.gpu_count,
                    vram_gb: cap.vram_gb,
                    cpu_cores: cap.cpu_cores,
                    available_memory_gb: cap.available_memory_gb,
                    supported_runtimes: cap.supported_runtimes,
                    tags: cap.tags,
                }
            } else {
                return Err(Status::invalid_argument("Missing capabilities"));
            },
            grpc_address: req.grpc_address,
            enrolment_token: req.enrolment_token,
        };

        let resp = self
            .attest_node_use_case
            .execute(app_req)
            .await
            // Audit 002 §4.9: admission failures (missing / bad / replayed
            // enrolment token) MUST surface as PermissionDenied, not Internal.
            // The use-case layer wraps these in `anyhow!` so we string-match
            // on the canonical "Cluster admission denied" prefix.
            .map_err(|e| {
                let msg = e.to_string();
                if msg.starts_with("Cluster admission denied") {
                    Status::permission_denied(msg)
                } else {
                    Status::internal(msg)
                }
            })?;

        Ok(Response::new(AttestNodeResponse {
            challenge_nonce: resp.challenge_nonce,
            challenge_id: resp.challenge_id.to_string(),
        }))
    }

    async fn challenge_node(
        &self,
        request: Request<ChallengeNodeRequest>,
    ) -> Result<Response<ChallengeNodeResponse>, Status> {
        let req = request.into_inner();
        // ADR-117: translate the prost-generated `bootstrap_proof` oneof.
        use crate::infrastructure::aegis_cluster_proto::challenge_node_request::BootstrapProof as ProtoBootstrapProof;
        let bootstrap_proof = req.bootstrap_proof.map(|bp| match bp {
            ProtoBootstrapProof::EnrollmentToken(t) => BootstrapProof::EnrollmentToken(t.into()),
        });
        let app_req = AppChallengeNodeRequest {
            challenge_id: uuid::Uuid::parse_str(&req.challenge_id)
                .map_err(|_| Status::invalid_argument("Invalid challenge_id"))?,
            node_id: NodeId::from_string(&req.node_id)
                .map_err(|_| Status::invalid_argument("Invalid node_id"))?,
            challenge_signature: req.challenge_signature,
            bootstrap_proof,
        };

        let resp = self
            .challenge_node_use_case
            .execute(app_req)
            .await
            .map_err(|e| Status::unauthenticated(e.to_string()))?;

        Ok(Response::new(ChallengeNodeResponse {
            node_security_token: resp.node_security_token,
            expires_at: Some(prost_types::Timestamp {
                seconds: resp.expires_at.timestamp(),
                nanos: resp.expires_at.timestamp_subsec_nanos() as i32,
            }),
        }))
    }

    async fn register_node(
        &self,
        request: Request<RegisterNodeRequest>,
    ) -> Result<Response<RegisterNodeResponse>, Status> {
        let (node_id, inner): (NodeId, RegisterNodeInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;

        let app_req = AppRegisterNodeRequest {
            node_id,
            capabilities: if let Some(cap) = inner.capabilities {
                NodeCapabilityAdvertisement {
                    gpu_count: cap.gpu_count,
                    vram_gb: cap.vram_gb,
                    cpu_cores: cap.cpu_cores,
                    available_memory_gb: cap.available_memory_gb,
                    supported_runtimes: cap.supported_runtimes,
                    tags: cap.tags,
                }
            } else {
                return Err(Status::invalid_argument("Missing capabilities"));
            },
            grpc_address: inner.grpc_address,
        };

        let resp = self
            .register_node_use_case
            .execute(app_req)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(RegisterNodeResponse {
            accepted: resp.accepted,
            cluster_id: resp.cluster_id.0.to_string(),
            message: resp.message,
        }))
    }

    async fn heartbeat(
        &self,
        request: Request<NodeHeartbeatRequest>,
    ) -> Result<Response<NodeHeartbeatResponse>, Status> {
        let (node_id, inner): (NodeId, NodeHeartbeatInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;

        let app_req = AppHeartbeatRequest {
            node_id,
            status: match inner.status() {
                crate::infrastructure::aegis_cluster_proto::NodeStatus::Active => {
                    NodePeerStatus::Active
                }
                crate::infrastructure::aegis_cluster_proto::NodeStatus::Draining => {
                    NodePeerStatus::Draining
                }
                crate::infrastructure::aegis_cluster_proto::NodeStatus::Unhealthy => {
                    NodePeerStatus::Unhealthy
                }
                _ => NodePeerStatus::Active,
            },
            active_executions: inner.active_executions,
            available_memory_gb: inner.available_memory_gb,
            cpu_utilization_percent: inner.cpu_utilization_percent,
        };

        let resp = self
            .heartbeat_use_case
            .execute(app_req)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        use crate::application::cluster::NodeCommand as AppCommand;
        use crate::infrastructure::aegis_cluster_proto::{
            node_command::Command, DrainCommand, NodeCommand, ShutdownCommand,
        };

        let pending_commands = resp
            .pending_commands
            .into_iter()
            .map(|c| match c {
                AppCommand::Drain(d) => NodeCommand {
                    command: Some(Command::Drain(DrainCommand { drain: d })),
                },
                AppCommand::Shutdown(r) => NodeCommand {
                    command: Some(Command::Shutdown(ShutdownCommand { reason: r })),
                },
                AppCommand::PushConfig { version, payload } => NodeCommand {
                    command: Some(Command::PushConfig(
                        crate::infrastructure::aegis_cluster_proto::PushConfigCommand {
                            config_version: version,
                            config_payload: payload,
                        },
                    )),
                },
            })
            .collect();

        Ok(Response::new(NodeHeartbeatResponse { pending_commands }))
    }

    async fn deregister_node(
        &self,
        request: Request<DeregisterNodeRequest>,
    ) -> Result<Response<DeregisterNodeResponse>, Status> {
        let (node_id, inner): (NodeId, DeregisterNodeInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;

        self.cluster_repo
            .deregister(&node_id, &inner.reason)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(DeregisterNodeResponse { accepted: true }))
    }

    async fn route_execution(
        &self,
        request: Request<RouteExecutionRequest>,
    ) -> Result<Response<RouteExecutionResponse>, Status> {
        let (_node_id, inner): (NodeId, RouteExecutionInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;

        let app_req = AppRouteExecutionRequest {
            execution_id: ExecutionId::from_string(&inner.execution_id)
                .map_err(|_| Status::invalid_argument("Invalid execution_id"))?,
            agent_id: AgentId::from_string(&inner.agent_id)
                .map_err(|_| Status::invalid_argument("Invalid agent_id"))?,
            required_capabilities: if let Some(cap) = inner.required_capabilities {
                NodeCapabilityAdvertisement {
                    gpu_count: cap.gpu_count,
                    vram_gb: cap.vram_gb,
                    cpu_cores: cap.cpu_cores,
                    available_memory_gb: cap.available_memory_gb,
                    supported_runtimes: cap.supported_runtimes,
                    tags: cap.tags,
                }
            } else {
                return Err(Status::invalid_argument("Missing required_capabilities"));
            },
            preferred_tags: inner.preferred_tags,
            tenant_id: TenantId::from_string(&inner.tenant_id)
                .map_err(|_| Status::invalid_argument("Invalid tenant_id"))?,
        };

        let resp = self
            .route_execution_use_case
            .execute(app_req)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(RouteExecutionResponse {
            target_node_id: resp.target_node_id.0.to_string(),
            worker_grpc_address: resp.worker_grpc_address,
        }))
    }

    type StreamClusterEventsStream = Pin<
        Box<
            dyn Stream<
                    Item = Result<
                        crate::infrastructure::aegis_cluster_proto::ClusterEventEnvelope,
                        Status,
                    >,
                > + Send,
        >,
    >;

    /// ADR-152 D3: this node's relayed-category events, as it publishes
    /// them, to a peer. A full queue or a lagged bus receiver ends the
    /// stream; the peer resynchronises and subscribes again.
    async fn stream_cluster_events(
        &self,
        request: Request<crate::infrastructure::aegis_cluster_proto::StreamClusterEventsRequest>,
    ) -> Result<Response<Self::StreamClusterEventsStream>, Status> {
        let (caller, _inner): (
            NodeId,
            crate::infrastructure::aegis_cluster_proto::StreamClusterEventsInner,
        ) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;
        self.membership()?;
        let Some(bus) = self.event_bus.as_ref() else {
            return Err(Status::failed_precondition("this node relays no events"));
        };
        let mut events = bus.subscribe_origin();
        let (tx, rx) = tokio::sync::mpsc::channel(RELAY_OUTBOUND_QUEUE);
        tracing::info!(peer = %caller, "Peer subscribed to the cluster event relay");
        tokio::spawn(async move {
            loop {
                let (origin, event) = match events.recv().await {
                    Ok(next) => next,
                    Err(e) => {
                        tracing::warn!(peer = %caller, error = %e, "Closing the event relay to a peer");
                        metrics::counter!("aegis_cluster_relay_streams_closed_total", "reason" => "lagged")
                            .increment(1);
                        return;
                    }
                };
                let event_json = match serde_json::to_vec(&event) {
                    Ok(json) => json,
                    Err(e) => {
                        tracing::error!(error = %e, "An event did not serialise for the relay");
                        continue;
                    }
                };
                let envelope = crate::infrastructure::aegis_cluster_proto::ClusterEventEnvelope {
                    origin_node_id: origin.node_id.to_string(),
                    origin_incarnation: origin.incarnation,
                    origin_seq: origin.seq,
                    category: crate::infrastructure::event_bus::category_of(&event).to_string(),
                    event_json,
                };
                match tx.try_send(Ok(envelope)) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(peer = %caller, "The peer's relay queue is full; closing its stream");
                        metrics::counter!("aegis_cluster_relay_streams_closed_total", "reason" => "queue_full")
                            .increment(1);
                        return;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }

    /// ADR-152 D5.3, D5.5: a peer at capacity, or draining, starts an
    /// execution here. The forwarding node vouches for the caller: its
    /// envelope is verified, it must hold a live lease, and the forward must
    /// be no older than [`FORWARD_MAX_AGE_SECS`]. Answers once this node has
    /// claimed the execution.
    async fn forward_execution(
        &self,
        request: Request<ForwardExecutionRequest>,
    ) -> Result<
        Response<crate::infrastructure::aegis_cluster_proto::ForwardExecutionResponse>,
        Status,
    > {
        let (from, inner): (NodeId, ForwardExecutionInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;
        let me = self.membership()?.process().node_id;
        let Some(leases) = self.forward_leases.as_ref() else {
            return Err(Status::failed_precondition(
                "this node takes no forwarded executions",
            ));
        };
        let live = leases
            .leases()
            .await
            .map_err(|e| Status::unavailable(format!("reading the cluster's leases: {e}")))?
            .into_iter()
            .any(|l| l.node_id == from && l.live);
        if !live {
            return Err(Status::permission_denied(
                "the forwarding node holds no live lease in this cluster",
            ));
        }
        let age = (chrono::Utc::now().timestamp() - inner.vouched_at_unix).abs();
        if age > FORWARD_MAX_AGE_SECS {
            return Err(Status::permission_denied(format!(
                "the forward was issued {age} s from this node's clock (limit {FORWARD_MAX_AGE_SECS} s)"
            )));
        }
        let caller: Option<crate::domain::iam::UserIdentity> =
            if inner.caller_identity_json.is_empty() {
                None
            } else {
                Some(
                    serde_json::from_str(&inner.caller_identity_json).map_err(|_| {
                        Status::invalid_argument("caller_identity_json is not an identity")
                    })?,
                )
            };
        let execution_id = ExecutionId::from_string(&inner.execution_id)
            .map_err(|_| Status::invalid_argument("Invalid execution_id"))?;
        let app_req = AppForwardExecutionRequest {
            execution_id,
            agent_id: AgentId::from_string(&inner.agent_id)
                .map_err(|_| Status::invalid_argument("Invalid agent_id"))?,
            input: inner.input,
            tenant_id: TenantId::from_string(&inner.tenant_id)
                .map_err(|_| Status::invalid_argument("Invalid tenant_id"))?,
            security_context_name: inner.security_context_name,
            caller,
        };
        let forward_uc = self.forward_execution_use_case.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "ForwardExecution disabled on this node (relay-coordinator role does not host agent execution)",
            )
        })?;
        let outcome = forward_uc.execute(app_req).await.map_err(|e| {
            if e.downcast_ref::<crate::application::execution::NodeDraining>()
                .is_some()
            {
                Status::unavailable("this node is draining")
            } else {
                Status::internal(e.to_string())
            }
        })?;
        tracing::info!(
            vouched_by = %from,
            execution_id = %execution_id,
            ?outcome,
            "Forwarded execution accepted"
        );
        Ok(Response::new(
            crate::infrastructure::aegis_cluster_proto::ForwardExecutionResponse {
                owner_node_id: me.0.to_string(),
            },
        ))
    }

    async fn sync_config(
        &self,
        request: Request<SyncConfigRequest>,
    ) -> Result<Response<SyncConfigResponse>, Status> {
        let (node_id, inner): (NodeId, SyncConfigInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;

        let tenant_id = inner.tenant_id.as_deref();
        let result = self
            .sync_config_use_case
            .execute(&node_id, tenant_id, &inner.current_config_version)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        match result {
            SyncConfigResult::UpToDate => Ok(Response::new(SyncConfigResponse {
                up_to_date: true,
                latest_version: inner.current_config_version,
                config_delta: Vec::new(),
            })),
            SyncConfigResult::Updated {
                latest_version,
                config_delta,
            } => Ok(Response::new(SyncConfigResponse {
                up_to_date: false,
                latest_version,
                config_delta: config_delta.into_bytes(),
            })),
        }
    }

    async fn push_config(
        &self,
        request: Request<PushConfigRequest>,
    ) -> Result<Response<PushConfigResponse>, Status> {
        let (_node_id, inner): (NodeId, PushConfigInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;

        let target_node_id = NodeId::from_string(&inner.target_node_id)
            .map_err(|_| Status::invalid_argument("Invalid target_node_id"))?;

        let payload_str = String::from_utf8(inner.config_payload)
            .map_err(|_| Status::invalid_argument("config_payload is not valid UTF-8"))?;

        let accepted = self
            .push_config_use_case
            .execute(&target_node_id, &inner.config_version, &payload_str)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(PushConfigResponse { accepted }))
    }

    async fn list_peers(
        &self,
        request: Request<ListPeersRequest>,
    ) -> Result<Response<ListPeersResponse>, Status> {
        let (_node_id, inner): (NodeId, ListPeersInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;

        // When no filter is provided, return ALL peers (Active + Draining + Unhealthy).
        // When filter_status has values, return only peers matching those statuses.
        let peers = if inner.filter_status.is_empty() {
            self.cluster_repo
                .list_all_peers()
                .await
                .map_err(|e| Status::internal(e.to_string()))?
        } else {
            let mut collected = Vec::new();
            for s in &inner.filter_status {
                let domain_status =
                    match crate::infrastructure::aegis_cluster_proto::NodeStatus::try_from(*s) {
                        Ok(crate::infrastructure::aegis_cluster_proto::NodeStatus::Active) => {
                            NodePeerStatus::Active
                        }
                        Ok(crate::infrastructure::aegis_cluster_proto::NodeStatus::Draining) => {
                            NodePeerStatus::Draining
                        }
                        Ok(crate::infrastructure::aegis_cluster_proto::NodeStatus::Unhealthy) => {
                            NodePeerStatus::Unhealthy
                        }
                        _ => continue,
                    };
                let batch = self
                    .cluster_repo
                    .list_peers_by_status(domain_status)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;
                collected.extend(batch);
            }
            collected
        };

        let nodes = peers
            .into_iter()
            .map(|p| {
                let role: i32 = match p.role {
                    DomainNodeRole::Controller => {
                        crate::infrastructure::aegis_cluster_proto::NodeRole::Controller.into()
                    }
                    DomainNodeRole::Worker => {
                        crate::infrastructure::aegis_cluster_proto::NodeRole::Worker.into()
                    }
                    DomainNodeRole::Hybrid => {
                        crate::infrastructure::aegis_cluster_proto::NodeRole::Hybrid.into()
                    }
                    DomainNodeRole::Edge => {
                        crate::infrastructure::aegis_cluster_proto::NodeRole::Edge.into()
                    }
                    DomainNodeRole::RelayCoordinator => {
                        crate::infrastructure::aegis_cluster_proto::NodeRole::RelayCoordinator
                            .into()
                    }
                };
                let status: i32 = match p.status {
                    NodePeerStatus::Active => {
                        crate::infrastructure::aegis_cluster_proto::NodeStatus::Active.into()
                    }
                    NodePeerStatus::Draining => {
                        crate::infrastructure::aegis_cluster_proto::NodeStatus::Draining.into()
                    }
                    NodePeerStatus::Unhealthy => {
                        crate::infrastructure::aegis_cluster_proto::NodeStatus::Unhealthy.into()
                    }
                };
                NodeInfo {
                    node_id: p.node_id.0.to_string(),
                    role,
                    status,
                    capabilities: Some(ProtoNodeCapabilities {
                        gpu_count: p.capabilities.gpu_count,
                        vram_gb: p.capabilities.vram_gb,
                        cpu_cores: p.capabilities.cpu_cores,
                        available_memory_gb: p.capabilities.available_memory_gb,
                        supported_runtimes: p.capabilities.supported_runtimes,
                        tags: p.capabilities.tags,
                    }),
                    grpc_address: p.grpc_address,
                    last_heartbeat_at: Some(prost_types::Timestamp {
                        seconds: p.last_heartbeat_at.timestamp(),
                        nanos: p.last_heartbeat_at.timestamp_subsec_nanos() as i32,
                    }),
                    registered_at: Some(prost_types::Timestamp {
                        seconds: p.registered_at.timestamp(),
                        nanos: p.registered_at.timestamp_subsec_nanos() as i32,
                    }),
                }
            })
            .collect();

        Ok(Response::new(ListPeersResponse {
            nodes,
            cluster_id: "aegis-cluster".to_string(),
        }))
    }

    // ── ADR-117: Edge Mode handlers ──────────────────────────────────────

    type ConnectEdgeStream = Pin<Box<dyn Stream<Item = Result<EdgeCommand, Status>> + Send>>;

    async fn connect_edge(
        &self,
        request: Request<Streaming<EdgeEvent>>,
    ) -> Result<Response<Self::ConnectEdgeStream>, Status> {
        let svc = self
            .connect_edge_service
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("edge mode disabled on this node"))?
            .clone();
        handle_connect_edge(svc, request).await
    }

    async fn rotate_edge_key(
        &self,
        request: Request<RotateEdgeKeyRequest>,
    ) -> Result<Response<RotateEdgeKeyResponse>, Status> {
        let svc = self
            .rotate_edge_key_service
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("edge mode disabled on this node"))?
            .clone();
        handle_rotate_edge_key(svc, request).await
    }

    /// ADR-117: SaaS deployment mints edge enrollment tokens here.
    ///
    /// Auth is per-handler (the cluster gRPC server has no global
    /// interceptor): the bearer JWT is read from the `authorization`
    /// metadata key and the tenant slug from `x-aegis-tenant`, both via
    /// `validate_grpc_request`. The Relay Coordinator is the sole holder
    /// of the OpenBao policy `transit/sign/edge-enrollment-token`; on any
    /// other deployment the use-case is absent and the RPC returns
    /// FailedPrecondition.
    async fn issue_enrollment_token(
        &self,
        request: Request<IssueEnrollmentTokenRequest>,
    ) -> Result<Response<IssueEnrollmentTokenResponse>, Status> {
        let interceptor = self.grpc_auth.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "IssueEnrollmentToken requires grpc_auth on the cluster server",
            )
        })?;
        let (_identity, tenant, _scope) = validate_grpc_request(
            interceptor,
            &request,
            "/aegis.cluster.v1.NodeClusterService/IssueEnrollmentToken",
        )
        .await?
        .ok_or_else(|| {
            Status::unauthenticated("IssueEnrollmentToken cannot be marked exempt from auth")
        })?;

        let signer = self
            .issue_enrollment_token_use_case
            .as_ref()
            .ok_or_else(|| {
                Status::failed_precondition(
                    "edge enrollment-token signing is not enabled on this node",
                )
            })?;

        let inner = request.into_inner();

        let issued = signer
            .issue(&tenant, &inner.issued_to)
            .await
            .map_err(|e| Status::internal(format!("issue enrollment token: {e}")))?;

        // Sent to the operator who asked for the token; the generated type
        // holds Strings.
        Ok(Response::new(IssueEnrollmentTokenResponse {
            token: issued.token.expose_owned(),
            expires_at: issued.expires_at.to_rfc3339(),
            controller_endpoint: issued.controller_endpoint,
            qr_payload: issued.qr_payload.expose_owned(),
            command_hint: issued.command_hint.expose_owned(),
        }))
    }
}
