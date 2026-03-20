// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use std::pin::Pin;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tokio_stream::Stream;
use prost::Message;
use crate::application::cluster::{
    AttestNodeUseCase, ChallengeNodeUseCase, RegisterNodeUseCase, HeartbeatUseCase,
    RouteExecutionUseCase, ForwardExecutionUseCase, AttestNodeRequest as AppAttestNodeRequest,
    ChallengeNodeRequest as AppChallengeNodeRequest, RegisterNodeRequest as AppRegisterNodeRequest,
    HeartbeatRequest as AppHeartbeatRequest, RouteExecutionRequest as AppRouteExecutionRequest,
    ForwardExecutionRequest as AppForwardExecutionRequest,
};
use crate::domain::cluster::{
    NodeId, NodeCapabilityAdvertisement, NodePeerStatus, NodeRole as DomainNodeRole, 
    NodeClusterRepository, NodeSecurityToken
};
use crate::domain::execution::ExecutionId;
use crate::domain::agent::AgentId;
use crate::domain::volume::TenantId;
use crate::infrastructure::aegis_cluster_proto::{
    node_cluster_service_server::NodeClusterService,
    AttestNodeRequest, AttestNodeResponse,
    ChallengeNodeRequest, ChallengeNodeResponse,
    RegisterNodeRequest, RegisterNodeResponse,
    NodeHeartbeatRequest, NodeHeartbeatResponse,
    DeregisterNodeRequest, DeregisterNodeResponse,
    RouteExecutionRequest, RouteExecutionResponse,
    ForwardExecutionRequest, SyncConfigRequest, SyncConfigResponse,
    PushConfigRequest, PushConfigResponse,
    ListPeersRequest, ListPeersResponse,
    RegisterNodeInner, NodeHeartbeatInner, RouteExecutionInner, ForwardExecutionInner,
    SmcpNodeEnvelope as ProtoEnvelope,
    DeregisterNodeInner,
};
use crate::infrastructure::aegis_runtime_proto::ExecutionEvent;

pub struct NodeClusterServiceHandler {
    attest_node_use_case: Arc<AttestNodeUseCase>,
    challenge_node_use_case: Arc<ChallengeNodeUseCase>,
    register_node_use_case: Arc<RegisterNodeUseCase>,
    heartbeat_use_case: Arc<HeartbeatUseCase>,
    route_execution_use_case: Arc<RouteExecutionUseCase>,
    forward_execution_use_case: Arc<ForwardExecutionUseCase>,
    cluster_repo: Arc<dyn NodeClusterRepository>,
}

impl NodeClusterServiceHandler {
    pub fn new(
        attest_node_use_case: Arc<AttestNodeUseCase>,
        challenge_node_use_case: Arc<ChallengeNodeUseCase>,
        register_node_use_case: Arc<RegisterNodeUseCase>,
        heartbeat_use_case: Arc<HeartbeatUseCase>,
        route_execution_use_case: Arc<RouteExecutionUseCase>,
        forward_execution_use_case: Arc<ForwardExecutionUseCase>,
        cluster_repo: Arc<dyn NodeClusterRepository>,
    ) -> Self {
        Self {
            attest_node_use_case,
            challenge_node_use_case,
            register_node_use_case,
            heartbeat_use_case,
            route_execution_use_case,
            forward_execution_use_case,
            cluster_repo,
        }
    }

    async fn authenticate_node<T: Message + Default>(&self, proto_envelope: Option<ProtoEnvelope>) -> Result<(NodeId, T), Status> {
        let proto_envelope = proto_envelope.ok_or_else(|| Status::unauthenticated("Missing security envelope"))?;
        
        // 1. Parse token and extract claims
        let token = NodeSecurityToken(proto_envelope.node_security_token.clone());
        let claims = token.claims().map_err(|e| Status::unauthenticated(format!("Invalid token: {}", e)))?;
        let node_id = claims.node_id;

        // 2. Retrieve node's public key
        let peer = self.cluster_repo.find_peer(&node_id).await
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?
            .ok_or_else(|| Status::unauthenticated("Node not registered"))?;

        // 3. Verify Ed25519 signature over inner_payload
        use ed25519_dalek::{VerifyingKey, Signature, Verifier};
        let verifying_key = VerifyingKey::from_bytes(&peer.public_key.clone().try_into().map_err(|_| Status::internal("Invalid stored public key length"))?)
            .map_err(|e| Status::internal(format!("Invalid stored public key: {}", e)))?;
        
        let signature = Signature::from_slice(&proto_envelope.signature)
            .map_err(|e| Status::unauthenticated(format!("Invalid signature format: {}", e)))?;

        verifying_key.verify(&proto_envelope.inner_payload, &signature)
            .map_err(|e| Status::unauthenticated(format!("Signature verification failed: {}", e)))?;

        // 4. Deserialize inner_payload
        let inner = T::decode(&proto_envelope.inner_payload[..])
            .map_err(|e| Status::invalid_argument(format!("Failed to deserialize inner payload: {}", e)))?;

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
            node_id: NodeId::from_string(&req.node_id).map_err(|_| Status::invalid_argument("Invalid NodeId"))?,
            role: match req.role() {
                crate::infrastructure::aegis_cluster_proto::NodeRole::Controller => DomainNodeRole::Controller,
                crate::infrastructure::aegis_cluster_proto::NodeRole::Worker => DomainNodeRole::Worker,
                crate::infrastructure::aegis_cluster_proto::NodeRole::Hybrid => DomainNodeRole::Hybrid,
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

        let resp = self.attest_node_use_case.execute(app_req).await
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
            challenge_id: uuid::Uuid::parse_str(&req.challenge_id).map_err(|_| Status::invalid_argument("Invalid challenge_id"))?,
            node_id: NodeId::from_string(&req.node_id).map_err(|_| Status::invalid_argument("Invalid node_id"))?,
            challenge_signature: req.challenge_signature,
        };

        let resp = self.challenge_node_use_case.execute(app_req).await
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
        let (node_id, inner): (NodeId, RegisterNodeInner) = self.authenticate_node(request.into_inner().envelope).await?;
        
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

        let resp = self.register_node_use_case.execute(app_req).await
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
        let (node_id, inner): (NodeId, NodeHeartbeatInner) = self.authenticate_node(request.into_inner().envelope).await?;
        
        let app_req = AppHeartbeatRequest {
            node_id,
            status: match inner.status() {
                crate::infrastructure::aegis_cluster_proto::NodeStatus::NodeStatusActive => NodePeerStatus::Active,
                crate::infrastructure::aegis_cluster_proto::NodeStatus::NodeStatusDraining => NodePeerStatus::Draining,
                crate::infrastructure::aegis_cluster_proto::NodeStatus::NodeStatusUnhealthy => NodePeerStatus::Unhealthy,
                _ => NodePeerStatus::Active,
            },
            active_executions: inner.active_executions,
            available_memory_gb: inner.available_memory_gb,
            cpu_utilization_percent: inner.cpu_utilization_percent,
        };

        let resp = self.heartbeat_use_case.execute(app_req).await
            .map_err(|e| Status::internal(e.to_string()))?;

        use crate::application::cluster::NodeCommand as AppCommand;
        use crate::infrastructure::aegis_cluster_proto::{NodeCommand, DrainCommand, ShutdownCommand, node_command::Command};

        let pending_commands = resp.pending_commands.into_iter().map(|c| match c {
            AppCommand::Drain(d) => NodeCommand { command: Some(Command::Drain(DrainCommand { drain: d })) },
            AppCommand::Shutdown(r) => NodeCommand { command: Some(Command::Shutdown(ShutdownCommand { reason: r })) },
            AppCommand::PushConfig { version, payload } => NodeCommand { command: Some(Command::PushConfig(crate::infrastructure::aegis_cluster_proto::PushConfigCommand { config_version: version, config_payload: payload })) },
        }).collect();

        Ok(Response::new(NodeHeartbeatResponse {
            pending_commands,
        }))
    }

    async fn deregister_node(
        &self,
        request: Request<DeregisterNodeRequest>,
    ) -> Result<Response<DeregisterNodeResponse>, Status> {
        let (node_id, _inner): (NodeId, DeregisterNodeInner) = self.authenticate_node(request.into_inner().envelope).await?;
        
        // Mark node as inactive
        self.cluster_repo.record_heartbeat(&node_id, crate::domain::cluster::ResourceSnapshot {
            cpu_utilization: 0.0,
            gpu_utilization: 0.0,
            active_executions: 0,
        }).await.map_err(|e| Status::internal(e.to_string()))?;
        
        Ok(Response::new(DeregisterNodeResponse {
            success: true,
            message: "Node deregistered successfully".to_string(),
        }))
    }

    async fn route_execution(
        &self,
        request: Request<RouteExecutionRequest>,
    ) -> Result<Response<RouteExecutionResponse>, Status> {
        let (_node_id, inner): (NodeId, RouteExecutionInner) = self.authenticate_node(request.into_inner().envelope).await?;
        
        let app_req = AppRouteExecutionRequest {
            execution_id: ExecutionId::from_string(&inner.execution_id).map_err(|_| Status::invalid_argument("Invalid execution_id"))?,
            agent_id: inner.agent_id,
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
            tenant_id: TenantId::from_string(&inner.tenant_id).map_err(|_| Status::invalid_argument("Invalid tenant_id"))?,
        };

        let resp = self.route_execution_use_case.execute(app_req).await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(RouteExecutionResponse {
            target_node_id: resp.target_node_id.0.to_string(),
            worker_grpc_address: resp.worker_grpc_address,
        }))
    }

    type ForwardExecutionStream = Pin<Box<dyn Stream<Item = Result<ExecutionEvent, Status>> + Send>>;

    async fn forward_execution(
        &self,
        request: Request<ForwardExecutionRequest>,
    ) -> Result<Response<Self::ForwardExecutionStream>, Status> {
        let (_node_id, inner): (NodeId, ForwardExecutionInner) = self.authenticate_node(request.into_inner().envelope).await?;
        
        let app_req = AppForwardExecutionRequest {
            execution_id: ExecutionId::from_string(&inner.execution_id).map_err(|_| Status::invalid_argument("Invalid execution_id"))?,
            agent_id: AgentId::from_string(&inner.agent_id).map_err(|_| Status::invalid_argument("Invalid agent_id"))?,
            input: inner.input,
            tenant_id: TenantId::from_string(&inner.tenant_id).map_err(|_| Status::invalid_argument("Invalid tenant_id"))?,
            originating_node_id: inner.originating_node_id,
            user_security_token: inner.user_security_token,
        };

        let stream = self.forward_execution_use_case.execute(app_req).await
            .map_err(|e| Status::internal(e.to_string()))?;

        let output_stream = tokio_stream::StreamExt::map(stream, |res| {
            res.map_err(|e| Status::internal(e.to_string()))
                .and_then(|_ev| {
                    Err(Status::unimplemented("ExecutionEvent mapping not implemented"))
                })
        });

        Ok(Response::new(Box::pin(output_stream)))
    }

    async fn sync_config(
        &self,
        _request: Request<SyncConfigRequest>,
    ) -> Result<Response<SyncConfigResponse>, Status> {
        Err(Status::unimplemented("Not yet implemented"))
    }

    async fn push_config(
        &self,
        _request: Request<PushConfigRequest>,
    ) -> Result<Response<PushConfigResponse>, Status> {
        Err(Status::unimplemented("Not yet implemented"))
    }

    async fn list_peers(
        &self,
        _request: Request<ListPeersRequest>,
    ) -> Result<Response<ListPeersResponse>, Status> {
        let peers = self.cluster_repo.list_peers_by_status(NodePeerStatus::Active).await
            .map_err(|e| Status::internal(e.to_string()))?;

        use crate::infrastructure::aegis_cluster_proto::NodeInfo;
        let nodes = peers.into_iter().map(|p| NodeInfo {
            node_id: p.node_id.0.to_string(),
            role: match p.role {
                DomainNodeRole::Controller => crate::infrastructure::aegis_cluster_proto::NodeRole::Controller.into(),
                DomainNodeRole::Worker => crate::infrastructure::aegis_cluster_proto::NodeRole::Worker.into(),
                DomainNodeRole::Hybrid => crate::infrastructure::aegis_cluster_proto::NodeRole::Hybrid.into(),
            },
            status: match p.status {
                NodePeerStatus::Active => crate::infrastructure::aegis_cluster_proto::NodeStatus::NodeStatusActive.into(),
                NodePeerStatus::Draining => crate::infrastructure::aegis_cluster_proto::NodeStatus::NodeStatusDraining.into(),
                NodePeerStatus::Unhealthy => crate::infrastructure::aegis_cluster_proto::NodeStatus::NodeStatusUnhealthy.into(),
            },
            grpc_address: p.grpc_address,
            tags: p.capabilities.tags,
            ..Default::default()
        }).collect();

        Ok(Response::new(ListPeersResponse {
            nodes,
            cluster_id: "aegis-cluster".to_string(),
        }))
    }
}
