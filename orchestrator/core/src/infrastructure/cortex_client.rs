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

/// Reduce a tool-argument JSON blob to a content-free *shape* signature
/// before forwarding it to the Cortex learning service.
///
/// Audit 002 §4.37.3 — Cortex stores **process data**, not customer content.
/// The orchestrator was forwarding the full `arguments_json` payload, which
/// for tools like `web_fetch`, `mcp.send_email`, `db.query`, etc. carries
/// the raw user data in the call. Cortex only needs the structural
/// trajectory (which keys were present, which types) to learn tool-call
/// patterns; the leaf values are never required and must not leave the
/// orchestrator boundary.
///
/// Strategy: parse the JSON; for objects, retain keys but replace each
/// value with a JSON-type tag (`"<string>"`, `"<number>"`, `"<bool>"`,
/// `"<null>"`, `"<array:N>"`, `"<object>"`). Recursively redact nested
/// objects and arrays. On parse failure, drop the payload entirely with
/// a fixed `"<unparseable>"` marker so we never accidentally exfiltrate a
/// malformed-but-leaky string.
fn redact_arguments_for_cortex(arguments_json: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(v) => v,
        Err(_) => return "\"<unparseable>\"".to_string(),
    };
    serde_json::to_string(&redact_value(&value)).unwrap_or_else(|_| "\"<error>\"".to_string())
}

fn redact_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let redacted: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), redact_value(v)))
                .collect();
            serde_json::Value::Object(redacted)
        }
        serde_json::Value::Array(items) => {
            // Preserve length (useful structural signal for trajectory
            // matching) but drop every element value.
            serde_json::Value::String(format!("<array:{}>", items.len()))
        }
        serde_json::Value::String(_) => serde_json::Value::String("<string>".to_string()),
        serde_json::Value::Number(_) => serde_json::Value::String("<number>".to_string()),
        serde_json::Value::Bool(_) => serde_json::Value::String("<bool>".to_string()),
        serde_json::Value::Null => serde_json::Value::String("<null>".to_string()),
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
                        // Audit 002 §4.37.3: redact customer content from
                        // the payload before crossing the orchestrator
                        // boundary. Cortex receives the structural shape
                        // only.
                        arguments_json: redact_arguments_for_cortex(&s.arguments_json),
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

#[cfg(test)]
mod cortex_redaction_tests {
    use super::*;

    /// Audit 002 §4.37.3 regression — leaf values of every JSON type must
    /// be replaced with a type tag so customer content cannot egress to
    /// Cortex through `arguments_json`.
    #[test]
    fn redact_strips_string_number_bool_null_leaves() {
        let input = r#"{
            "url": "https://victim.example.com/secret?api_key=AKIA1234",
            "retries": 3,
            "force": true,
            "context": null
        }"#;
        let out = redact_arguments_for_cortex(input);
        assert!(!out.contains("victim.example.com"));
        assert!(!out.contains("AKIA1234"));
        assert!(!out.contains('3'));
        assert!(out.contains("<string>"));
        assert!(out.contains("<number>"));
        assert!(out.contains("<bool>"));
        assert!(out.contains("<null>"));
        // Keys themselves are preserved — they're tool-schema fields,
        // not customer content.
        assert!(out.contains("\"url\""));
        assert!(out.contains("\"retries\""));
    }

    #[test]
    fn redact_collapses_arrays_to_length_signal() {
        let input = r#"{"to": ["alice@example.com", "bob@example.com", "eve@example.com"]}"#;
        let out = redact_arguments_for_cortex(input);
        assert!(!out.contains("alice"));
        assert!(!out.contains("eve"));
        assert!(out.contains("<array:3>"));
    }

    #[test]
    fn redact_recurses_into_nested_objects() {
        let input = r#"{"outer": {"inner_secret": "DROP TABLE users"}}"#;
        let out = redact_arguments_for_cortex(input);
        assert!(!out.contains("DROP TABLE"));
        assert!(out.contains("\"inner_secret\""));
        assert!(out.contains("<string>"));
    }

    #[test]
    fn redact_unparseable_input_is_dropped_entirely() {
        let out = redact_arguments_for_cortex("not json at all { secret_value");
        assert_eq!(out, "\"<unparseable>\"");
    }
}
