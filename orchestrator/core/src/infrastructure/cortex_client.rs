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
    /// Bearer key for the Cortex service. Prints redacted.
    api_key: Option<crate::domain::secrets::SensitiveString>,
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
        Ok(Self {
            client,
            api_key: api_key.map(crate::domain::secrets::SensitiveString::new),
        })
    }

    /// Wrap a request body in a `tonic::Request`, injecting the `Authorization`
    /// header when an API key is configured.
    fn authed_request<T>(&self, body: T) -> tonic::Request<T> {
        let mut req = tonic::Request::new(body);
        if let Some(ref key) = self.api_key {
            // Read to send as the bearer header.
            if let Ok(val) =
                tonic::metadata::MetadataValue::try_from(format!("Bearer {}", key.expose()))
            {
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

    #[tokio::test]
    async fn cortex_client_debug_does_not_print_the_api_key() {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        let client = CortexGrpcClient {
            client: CortexServiceClient::new(channel),
            api_key: Some("Mk7-cortex-api-key-marker".into()),
        };
        let printed = format!("{client:?}");
        assert!(
            !printed.contains("Mk7-cortex-api-key-marker"),
            "CortexGrpcClient's Debug printed its API key: {printed}"
        );
    }

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

#[cfg(test)]
mod cortex_bearer_tests {
    use super::*;
    use crate::infrastructure::aegis_cortex_proto::{
        ApplyCortisolRequest, ApplyCortisolResponse, ApplyDopamineRequest, ApplyDopamineResponse,
        CreateEdgeRequest, CreateEdgeResponse, CreateNodeRequest, CreateNodeResponse,
        DiscoverAgentsRequest, DiscoverAgentsResponse, DiscoverWorkflowsRequest,
        DiscoverWorkflowsResponse, GetMetricsRequest, GetMetricsResponse, GetPatternRequest,
        GetPatternResponse, GetSkillRequest, GetSkillResponse, HealthCheckRequest,
        HealthCheckResponse, IndexAgentRequest, IndexAgentResponse, IndexWorkflowRequest,
        IndexWorkflowResponse, ListSkillsRequest, ListSkillsResponse, QueryGraphRequest,
        QueryGraphResponse, QueryPatternsRequest, QueryPatternsResponse,
        RemoveDiscoveryAgentRequest, RemoveDiscoveryAgentResponse, RemoveDiscoveryWorkflowRequest,
        RemoveDiscoveryWorkflowResponse, StorePatternRequest, StorePatternResponse,
        StoreTrajectoryPatternRequest, StoreTrajectoryPatternResponse, TraverseGraphRequest,
        TraverseGraphResponse, TriggerConsolidationRequest, TriggerConsolidationResponse,
        TriggerPruningRequest, TriggerPruningResponse, UpdatePatternSuccessRequest,
        UpdatePatternSuccessResponse,
    };

    /// A stand-in Cortex service on a loopback port: records the
    /// `authorization` metadata of a QueryPatterns call.
    struct RecordingCortex {
        seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }

    #[tonic::async_trait]
    impl crate::infrastructure::aegis_cortex_proto::cortex_service_server::CortexService
        for RecordingCortex
    {
        async fn query_patterns(
            &self,
            request: tonic::Request<QueryPatternsRequest>,
        ) -> Result<tonic::Response<QueryPatternsResponse>, tonic::Status> {
            *self.seen.lock().unwrap() = request
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            Ok(tonic::Response::new(QueryPatternsResponse::default()))
        }
        async fn store_pattern(
            &self,
            _: tonic::Request<StorePatternRequest>,
        ) -> Result<tonic::Response<StorePatternResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn store_trajectory_pattern(
            &self,
            _: tonic::Request<StoreTrajectoryPatternRequest>,
        ) -> Result<tonic::Response<StoreTrajectoryPatternResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn get_pattern(
            &self,
            _: tonic::Request<GetPatternRequest>,
        ) -> Result<tonic::Response<GetPatternResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn apply_dopamine(
            &self,
            _: tonic::Request<ApplyDopamineRequest>,
        ) -> Result<tonic::Response<ApplyDopamineResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn apply_cortisol(
            &self,
            _: tonic::Request<ApplyCortisolRequest>,
        ) -> Result<tonic::Response<ApplyCortisolResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn update_pattern_success(
            &self,
            _: tonic::Request<UpdatePatternSuccessRequest>,
        ) -> Result<tonic::Response<UpdatePatternSuccessResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn create_node(
            &self,
            _: tonic::Request<CreateNodeRequest>,
        ) -> Result<tonic::Response<CreateNodeResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn create_edge(
            &self,
            _: tonic::Request<CreateEdgeRequest>,
        ) -> Result<tonic::Response<CreateEdgeResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn traverse_graph(
            &self,
            _: tonic::Request<TraverseGraphRequest>,
        ) -> Result<tonic::Response<TraverseGraphResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn query_graph(
            &self,
            _: tonic::Request<QueryGraphRequest>,
        ) -> Result<tonic::Response<QueryGraphResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn list_skills(
            &self,
            _: tonic::Request<ListSkillsRequest>,
        ) -> Result<tonic::Response<ListSkillsResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn get_skill(
            &self,
            _: tonic::Request<GetSkillRequest>,
        ) -> Result<tonic::Response<GetSkillResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn index_agent(
            &self,
            _: tonic::Request<IndexAgentRequest>,
        ) -> Result<tonic::Response<IndexAgentResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn index_workflow(
            &self,
            _: tonic::Request<IndexWorkflowRequest>,
        ) -> Result<tonic::Response<IndexWorkflowResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn remove_agent(
            &self,
            _: tonic::Request<RemoveDiscoveryAgentRequest>,
        ) -> Result<tonic::Response<RemoveDiscoveryAgentResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn remove_workflow(
            &self,
            _: tonic::Request<RemoveDiscoveryWorkflowRequest>,
        ) -> Result<tonic::Response<RemoveDiscoveryWorkflowResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn discover_agents(
            &self,
            _: tonic::Request<DiscoverAgentsRequest>,
        ) -> Result<tonic::Response<DiscoverAgentsResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn discover_workflows(
            &self,
            _: tonic::Request<DiscoverWorkflowsRequest>,
        ) -> Result<tonic::Response<DiscoverWorkflowsResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn trigger_consolidation(
            &self,
            _: tonic::Request<TriggerConsolidationRequest>,
        ) -> Result<tonic::Response<TriggerConsolidationResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn trigger_pruning(
            &self,
            _: tonic::Request<TriggerPruningRequest>,
        ) -> Result<tonic::Response<TriggerPruningResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn get_metrics(
            &self,
            _: tonic::Request<GetMetricsRequest>,
        ) -> Result<tonic::Response<GetMetricsResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
        async fn health_check(
            &self,
            _: tonic::Request<HealthCheckRequest>,
        ) -> Result<tonic::Response<HealthCheckResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used by this test"))
        }
    }

    /// A call from the Cortex client carries the API key, exactly as
    /// configured, as its bearer token.
    #[tokio::test]
    async fn cortex_client_sends_the_api_key_as_configured() {
        use crate::infrastructure::aegis_cortex_proto::cortex_service_server::CortexServiceServer;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let service = RecordingCortex { seen: seen.clone() };
        tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            let _ = tonic::transport::Server::builder()
                .add_service(CortexServiceServer::new(service))
                .serve_with_incoming(incoming)
                .await;
        });
        let client = CortexGrpcClient::new(
            format!("http://{addr}"),
            Some("Mk7-cortex-bearer-marker".to_string()),
        )
        .await
        .expect("connect to the stand-in service");

        client
            .query_patterns(QueryPatternsRequest::default())
            .await
            .expect("served");

        assert_eq!(
            seen.lock().unwrap().clone().as_deref(),
            Some("Bearer Mk7-cortex-bearer-marker"),
            "the Cortex client presented a different bearer token than the key configured"
        );
    }
}
