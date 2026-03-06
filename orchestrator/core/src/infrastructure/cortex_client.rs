// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Cortex gRPC Client (ADR-042)
//!
//! Infrastructure-layer client that forwards `QueryCortexPatterns` and
//! `StoreCortexPattern` gRPC calls from the Orchestrator to the standalone
//! `aegis-cortex` microservice.
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
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** gRPC proxy to standalone Cortex service
//! - **Related ADRs:** ADR-042 (Separate Cortex Repository)

use crate::application::ports::{CortexPatternPort, StoreTrajectoryPatternCommand};
use async_trait::async_trait;
use tonic::transport::Channel;
use tonic::Status;

use crate::infrastructure::aegis_runtime_proto::{
    aegis_runtime_client::AegisRuntimeClient, QueryCortexRequest, QueryCortexResponse,
    StoreCortexPatternRequest, StoreCortexPatternResponse, StoreTrajectoryPatternRequest,
    StoreTrajectoryPatternResponse,
};

/// Thin wrapper around `AegisRuntimeClient` that exposes only the Cortex RPCs.
///
/// `AegisRuntimeClient<Channel>` is `Clone` — cloning is cheap and re-uses the
/// same underlying HTTP/2 connection pool managed by the `Channel`.
#[derive(Debug, Clone)]
pub struct CortexGrpcClient {
    client: AegisRuntimeClient<Channel>,
}

impl CortexGrpcClient {
    /// Connect to the standalone Cortex service at `url` (e.g. `http://cortex:50052`).
    ///
    /// Returns an error if the endpoint URL is malformed or the initial
    /// connection setup fails.
    pub async fn new(url: String) -> Result<Self, tonic::transport::Error> {
        let client = AegisRuntimeClient::connect(url).await?;
        Ok(Self { client })
    }

    /// Forward a `QueryCortexPatterns` RPC to the Cortex service.
    pub async fn query_patterns(
        &self,
        request: QueryCortexRequest,
    ) -> Result<QueryCortexResponse, Status> {
        let mut client = self.client.clone();
        let response = client.query_cortex_patterns(request).await?;
        Ok(response.into_inner())
    }

    /// Forward a `StoreCortexPattern` RPC to the Cortex service.
    pub async fn store_pattern(
        &self,
        request: StoreCortexPatternRequest,
    ) -> Result<StoreCortexPatternResponse, Status> {
        let mut client = self.client.clone();
        let response = client.store_cortex_pattern(request).await?;
        Ok(response.into_inner())
    }

    /// Forward a `StoreTrajectoryPattern` RPC to the Cortex service (ADR-049).
    pub async fn store_trajectory_pattern(
        &self,
        request: StoreTrajectoryPatternRequest,
    ) -> Result<StoreTrajectoryPatternResponse, Status> {
        let mut client = self.client.clone();
        let response = client.store_trajectory_pattern(request).await?;
        Ok(response.into_inner())
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
                    |s| crate::infrastructure::aegis_runtime_proto::TrajectoryStep {
                        tool_name: s.tool_name,
                        arguments_json: s.arguments_json,
                    },
                )
                .collect(),
            success_score: request.success_score,
        };

        CortexGrpcClient::store_trajectory_pattern(self, proto_request)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}
