// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

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
    NodeRole as DomainNodeRole, NodeSecurityToken,
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
use crate::infrastructure::aegis_runtime_proto::ExecutionEvent;
use prost::Message;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

pub struct NodeClusterServiceHandler {
    attest_node_use_case: Arc<AttestNodeUseCase>,
    challenge_node_use_case: Arc<ChallengeNodeUseCase>,
    register_node_use_case: Arc<RegisterNodeUseCase>,
    heartbeat_use_case: Arc<HeartbeatUseCase>,
    route_execution_use_case: Arc<RouteExecutionUseCase>,
    forward_execution_use_case: Arc<ForwardExecutionUseCase>,
    sync_config_use_case: Arc<SyncConfigUseCase>,
    push_config_use_case: Arc<PushConfigUseCase>,
    cluster_repo: Arc<dyn NodeClusterRepository>,
}

impl NodeClusterServiceHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        attest_node_use_case: Arc<AttestNodeUseCase>,
        challenge_node_use_case: Arc<ChallengeNodeUseCase>,
        register_node_use_case: Arc<RegisterNodeUseCase>,
        heartbeat_use_case: Arc<HeartbeatUseCase>,
        route_execution_use_case: Arc<RouteExecutionUseCase>,
        forward_execution_use_case: Arc<ForwardExecutionUseCase>,
        sync_config_use_case: Arc<SyncConfigUseCase>,
        push_config_use_case: Arc<PushConfigUseCase>,
        cluster_repo: Arc<dyn NodeClusterRepository>,
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
        }
    }

    async fn authenticate_node<T: Message + Default>(
        &self,
        proto_envelope: Option<ProtoEnvelope>,
    ) -> Result<(NodeId, T), Status> {
        let proto_envelope =
            proto_envelope.ok_or_else(|| Status::unauthenticated("Missing security envelope"))?;

        // 1. Parse token and extract claims
        let token = NodeSecurityToken(proto_envelope.node_security_token.clone());
        let claims = token
            .claims()
            .map_err(|e| Status::unauthenticated(format!("Invalid token: {}", e)))?;
        let node_id = claims.node_id;

        // 2. Retrieve node's public key
        let peer = self
            .cluster_repo
            .find_peer(&node_id)
            .await
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?
            .ok_or_else(|| Status::unauthenticated("Node not registered"))?;

        // 3. Verify Ed25519 signature over inner_payload
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let verifying_key = VerifyingKey::from_bytes(
            &peer
                .public_key
                .clone()
                .try_into()
                .map_err(|_| Status::internal("Invalid stored public key length"))?,
        )
        .map_err(|e| Status::internal(format!("Invalid stored public key: {}", e)))?;

        let signature = Signature::from_slice(&proto_envelope.signature)
            .map_err(|e| Status::unauthenticated(format!("Invalid signature format: {}", e)))?;

        verifying_key
            .verify(&proto_envelope.payload, &signature)
            .map_err(|e| {
                Status::unauthenticated(format!("Signature verification failed: {}", e))
            })?;

        // 4. Deserialize payload
        let inner = T::decode(&proto_envelope.payload[..]).map_err(|e| {
            Status::invalid_argument(format!("Failed to deserialize inner payload: {}", e))
        })?;

        Ok((node_id, inner))
    }
}

#[tonic::async_trait]
impl NodeClusterService for NodeClusterServiceHandler {
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
                _ => DomainNodeRole::Worker,
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
        };

        let resp = self
            .attest_node_use_case
            .execute(app_req)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

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
        let app_req = AppChallengeNodeRequest {
            challenge_id: uuid::Uuid::parse_str(&req.challenge_id)
                .map_err(|_| Status::invalid_argument("Invalid challenge_id"))?,
            node_id: NodeId::from_string(&req.node_id)
                .map_err(|_| Status::invalid_argument("Invalid node_id"))?,
            challenge_signature: req.challenge_signature,
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

    type ForwardExecutionStream =
        Pin<Box<dyn Stream<Item = Result<ExecutionEvent, Status>> + Send>>;

    async fn forward_execution(
        &self,
        request: Request<ForwardExecutionRequest>,
    ) -> Result<Response<Self::ForwardExecutionStream>, Status> {
        let (_node_id, inner): (NodeId, ForwardExecutionInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;

        let app_req = AppForwardExecutionRequest {
            execution_id: ExecutionId::from_string(&inner.execution_id)
                .map_err(|_| Status::invalid_argument("Invalid execution_id"))?,
            agent_id: AgentId::from_string(&inner.agent_id)
                .map_err(|_| Status::invalid_argument("Invalid agent_id"))?,
            input: inner.input,
            tenant_id: TenantId::from_string(&inner.tenant_id)
                .map_err(|_| Status::invalid_argument("Invalid tenant_id"))?,
            originating_node_id: inner.originating_node_id,
            user_security_token: inner.user_security_token,
            // ADR-083: propagate security context from the proto message.
            security_context_name: inner.security_context_name,
        };

        let stream = self
            .forward_execution_use_case
            .execute(app_req)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let output_stream = tokio_stream::StreamExt::filter_map(stream, |res| match res {
            Err(e) => Some(Err(Status::internal(e.to_string()))),
            Ok(domain_ev) => super::event_mapper::domain_to_proto(domain_ev).map(Ok),
        });

        Ok(Response::new(Box::pin(output_stream)))
    }

    async fn sync_config(
        &self,
        request: Request<SyncConfigRequest>,
    ) -> Result<Response<SyncConfigResponse>, Status> {
        let (node_id, inner): (NodeId, SyncConfigInner) = self
            .authenticate_node(request.into_inner().envelope)
            .await?;

        let result = self
            .sync_config_use_case
            .execute(&node_id, &inner.current_config_version)
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
}
