// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Cortex gRPC Client (ADR-042)
//!
//! Infrastructure-layer client that forwards Cortex gRPC calls from the
//! Orchestrator to the standalone `aegis-cortex` microservice.
//!
//! ## Memoryless Mode
//!
//! If `CORTEX_GRPC_URL` is absent at startup, the Orchestrator never creates
//! a `CortexGrpcClient` and passes `None` throughout. The callers in
//! `AegisRuntimeService` detect `None` and return empty / no-op responses
//! without logging a warning on every call (one `INFO` at startup is enough).
//!
//! ## Connection
//!
//! Uses `tonic::transport::Channel` which maintains an internal connection pool.
//! Cloning the client is cheap and is the idiomatic way to obtain a `&mut self`
//! handle for each call while keeping the outer struct `Send + Sync`.
//!
//! ## Authentication
//!
//! When `api_key` is set, every outbound RPC includes an `Authorization: Bearer <key>`
//! metadata header. When absent (self-hosted or dev mode), requests are unauthenticated.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** gRPC proxy to standalone Cortex service
//! - **Related ADRs:** ADR-042 (Separate Cortex Repository)

use crate::application::ports::{CortexPatternPort, StoreTrajectoryPatternCommand};
use async_trait::async_trait;
use tonic::transport::Channel;
use tonic::Status;

use crate::infrastructure::aegis_cortex_proto::{
    cortex_service_client::CortexServiceClient, DiscoverAgentsRequest, DiscoverAgentsResponse,
    DiscoverWorkflowsRequest, DiscoverWorkflowsResponse, IndexAgentRequest, IndexAgentResponse,
    IndexWorkflowRequest, IndexWorkflowResponse, QueryPatternsRequest, QueryPatternsResponse,
    RemoveDiscoveryAgentRequest, RemoveDiscoveryAgentResponse, RemoveDiscoveryWorkflowRequest,
    RemoveDiscoveryWorkflowResponse, StorePatternRequest, StorePatternResponse,
    StoreTrajectoryPatternRequest, StoreTrajectoryPatternResponse,
};

/// Thin wrapper around `CortexServiceClient` that exposes Cortex RPCs.
///
/// `CortexServiceClient<Channel>` is `Clone` — cloning is cheap and re-uses the
/// same underlying HTTP/2 connection pool managed by the `Channel`.
#[derive(Debug, Clone)]
pub struct CortexGrpcClient {
    client: CortexServiceClient<Channel>,
    api_key: Option<String>,
}

impl CortexGrpcClient {
    /// Connect to the standalone Cortex service at `url` (e.g. `http://cortex:50052`).
    ///
    /// `api_key` — when `Some`, every RPC will include `Authorization: Bearer <key>`.
    /// Pass `None` for self-hosted or unauthenticated deployments.
    ///
    /// Returns an error if the endpoint URL is malformed or the initial
    /// connection setup fails.
    pub async fn new(
        url: String,
        api_key: Option<String>,
    ) -> Result<Self, tonic::transport::Error> {
        let client = CortexServiceClient::connect(url).await?;
        Ok(Self { client, api_key })
    }

    /// Wrap a request body in a `tonic::Request`, injecting the `Authorization`
    /// header when an API key is configured.
    fn authed_request<T>(&self, body: T) -> tonic::Request<T> {
        let mut req = tonic::Request::new(body);
        if let Some(ref key) = self.api_key {
            if let Ok(val) = tonic::metadata::MetadataValue::try_from(format!("Bearer {key}")) {
                req.metadata_mut().insert("authorization", val);
            }
        }
        req
    }

    /// Forward a `QueryPatterns` RPC to the Cortex service.
    pub async fn query_patterns(
        &self,
        request: QueryPatternsRequest,
    ) -> Result<QueryPatternsResponse, Status> {
        let mut client = self.client.clone();
        let response = client.query_patterns(self.authed_request(request)).await?;
        Ok(response.into_inner())
    }

    /// Forward a `StorePattern` RPC to the Cortex service.
    pub async fn store_pattern(
        &self,
        request: StorePatternRequest,
    ) -> Result<StorePatternResponse, Status> {
        let mut client = self.client.clone();
        let response = client.store_pattern(self.authed_request(request)).await?;
        Ok(response.into_inner())
    }

    /// Forward a `StoreTrajectoryPattern` RPC to the Cortex service (ADR-049).
    pub async fn store_trajectory_pattern(
        &self,
        request: StoreTrajectoryPatternRequest,
    ) -> Result<StoreTrajectoryPatternResponse, Status> {
        let mut client = self.client.clone();
        let response = client
            .store_trajectory_pattern(self.authed_request(request))
            .await?;
        Ok(response.into_inner())
    }

    /// Index (upsert) an agent in the Cortex discovery index.
    pub async fn index_agent(
        &self,
        request: IndexAgentRequest,
    ) -> Result<IndexAgentResponse, Status> {
        let mut client = self.client.clone();
        client
            .index_agent(self.authed_request(request))
            .await
            .map(|r| r.into_inner())
    }

    /// Index (upsert) a workflow in the Cortex discovery index.
    pub async fn index_workflow(
        &self,
        request: IndexWorkflowRequest,
    ) -> Result<IndexWorkflowResponse, Status> {
        let mut client = self.client.clone();
        client
            .index_workflow(self.authed_request(request))
            .await
            .map(|r| r.into_inner())
    }

    /// Remove an agent from the Cortex discovery index.
    pub async fn remove_discovery_agent(
        &self,
        request: RemoveDiscoveryAgentRequest,
    ) -> Result<RemoveDiscoveryAgentResponse, Status> {
        let mut client = self.client.clone();
        client
            .remove_agent(self.authed_request(request))
            .await
            .map(|r| r.into_inner())
    }

    /// Remove a workflow from the Cortex discovery index.
    pub async fn remove_discovery_workflow(
        &self,
        request: RemoveDiscoveryWorkflowRequest,
    ) -> Result<RemoveDiscoveryWorkflowResponse, Status> {
        let mut client = self.client.clone();
        client
            .remove_workflow(self.authed_request(request))
            .await
            .map(|r| r.into_inner())
    }

    /// Search for agents in the Cortex discovery index.
    pub async fn discover_agents(
        &self,
        request: DiscoverAgentsRequest,
    ) -> Result<DiscoverAgentsResponse, Status> {
        let mut client = self.client.clone();
        client
            .discover_agents(self.authed_request(request))
            .await
            .map(|r| r.into_inner())
    }

    /// Search for workflows in the Cortex discovery index.
    pub async fn discover_workflows(
        &self,
        request: DiscoverWorkflowsRequest,
    ) -> Result<DiscoverWorkflowsResponse, Status> {
        let mut client = self.client.clone();
        client
            .discover_workflows(self.authed_request(request))
            .await
            .map(|r| r.into_inner())
    }
}

#[async_trait]
impl CortexPatternPort for CortexGrpcClient {
    async fn store_trajectory_pattern(
        &self,
        request: StoreTrajectoryPatternCommand,
    ) -> anyhow::Result<()> {
        let proto_request = StoreTrajectoryPatternRequest {
            task_signature: request.task_signature,
            steps: request
                .steps
                .into_iter()
                .map(
                    |s| crate::infrastructure::aegis_cortex_proto::TrajectoryStep {
                        tool_name: s.tool_name,
                        arguments_json: s.arguments_json,
                        order_index: s.order_index,
                    },
                )
                .collect(),
            success_score: request.success_score,
            tenant_id: String::new(),
        };

        CortexGrpcClient::store_trajectory_pattern(self, proto_request)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}
