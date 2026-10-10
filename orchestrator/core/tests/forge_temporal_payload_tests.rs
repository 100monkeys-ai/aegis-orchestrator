// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A workflow run's prepared repositories reach the worker
//!
//! AEGIS ADR-141 F3: the worker writes the run's first repository onto the
//! blackboard, so the payload Temporal hands it carries the run's prepared
//! entries (each with `label`, `ref` and `started_from`) under its
//! top-level `repositories`. The start runs through the start use case and
//! the real `TemporalClient`, against a loopback stand-in for Temporal's
//! `StartWorkflowExecution` that keeps every request it was sent.
//!
//! | Scenario | Test |
//! |---|---|
//! | The prepared entries in the payload | `a_start_with_prepared_repositories_puts_the_filled_entries_in_the_temporal_payload` |

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tonic::codegen::{http, BoxFuture, Context, Poll, Service};

use aegis_orchestrator_core::application::git_repo_service::{
    PreparedRepository, RunMount, RunRepositories, RunRepositoryError,
};
use aegis_orchestrator_core::application::ports::WorkflowEnginePort;
use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::domain::git_repo::{parse_run_repositories, RunRepository};
use aegis_orchestrator_core::domain::repository::WorkflowRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use aegis_orchestrator_core::infrastructure::temporal_client::TemporalClient;
use aegis_orchestrator_core::infrastructure::temporal_proto::temporal::api::workflowservice::v1::{
    StartWorkflowExecutionRequest as TemporalStart, StartWorkflowExecutionResponse,
};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

const BINDING: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
const STARTED_FROM: &str = "0123456789abcdef0123456789abcdef01234567";

// ===========================================================================
// A stand-in for Temporal's StartWorkflowExecution
// ===========================================================================

/// Answers `StartWorkflowExecution` with a run id and keeps each request.
#[derive(Clone, Default)]
struct Temporal {
    starts: Arc<Mutex<Vec<TemporalStart>>>,
}

impl tonic::server::NamedService for Temporal {
    const NAME: &'static str = "temporal.api.workflowservice.v1.WorkflowService";
}

struct Starts(Arc<Mutex<Vec<TemporalStart>>>);

impl tonic::server::UnaryService<TemporalStart> for Starts {
    type Response = StartWorkflowExecutionResponse;
    type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;

    fn call(&mut self, request: tonic::Request<TemporalStart>) -> Self::Future {
        self.0.lock().unwrap().push(request.into_inner());
        Box::pin(async {
            Ok(tonic::Response::new(StartWorkflowExecutionResponse {
                run_id: "the-run".to_string(),
                started: true,
                ..Default::default()
            }))
        })
    }
}

impl Service<http::Request<tonic::body::Body>> for Temporal {
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let starts = self.starts.clone();
        Box::pin(async move {
            if request.uri().path()
                != "/temporal.api.workflowservice.v1.WorkflowService/StartWorkflowExecution"
            {
                return Ok(
                    tonic::Status::unimplemented(request.uri().path().to_string()).into_http(),
                );
            }
            let mut grpc = tonic::server::Grpc::new(tonic_prost::ProstCodec::default());
            Ok(grpc.unary(Starts(starts), request).await)
        })
    }
}

/// The stand-in, served on a loopback port; answers its address.
async fn serve(temporal: Temporal) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(temporal)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    format!("http://{address}")
}

// ===========================================================================
// The run's repositories, prepared as the git repository service fills them
// ===========================================================================

struct Filling;

#[async_trait]
impl RunRepositories for Filling {
    async fn prepare_for_run(
        &self,
        _tenant_id: &TenantId,
        _person: Option<&str>,
        _run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunRepository>, RunRepositoryError> {
        let filled = entries
            .iter()
            .map(|entry| {
                let mut value = serde_json::to_value(entry).unwrap();
                value["branch"] = json!("aegis/1234abcd");
                value["label"] = json!("app");
                value["ref"] = json!("main");
                value["started_from"] = json!(STARTED_FROM);
                value
            })
            .collect::<Vec<_>>();
        Ok(parse_run_repositories(&Value::Array(filled)).unwrap())
    }

    async fn mounts_for_run(
        &self,
        _tenant_id: &TenantId,
        _person: Option<&str>,
        _run: uuid::Uuid,
        _entries: &[RunRepository],
    ) -> Result<Vec<RunMount>, RunRepositoryError> {
        Ok(Vec::new())
    }

    fn release_run(&self, _run: uuid::Uuid) {}

    fn take_prepared(&self, _run: uuid::Uuid) -> Vec<PreparedRepository> {
        Vec::new()
    }
}

const MANIFEST: &str = r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: the-forge
  version: "1.0.0"
spec:
  initial_state: START
  repositories: 1
  states:
    START:
      kind: System
      command: "echo ok"
      transitions: []
"#;

/// AEGIS ADR-141 F3: the run's prepared entries are the Temporal payload's
/// top-level `repositories`; its `input` still holds none.
#[tokio::test]
async fn a_start_with_prepared_repositories_puts_the_filled_entries_in_the_temporal_payload() {
    let temporal = Temporal::default();
    let address = serve(temporal.clone()).await;
    let client = TemporalClient::new(&address, "default", "aegis-queue", "http://127.0.0.1:9")
        .await
        .unwrap_or_else(|e| panic!("the client did not reach the stand-in: {e:#}"));

    let tenant = TenantId::consumer();
    let workflows = Arc::new(InMemoryWorkflowRepository::new());
    workflows
        .save_for_tenant(&tenant, &WorkflowParser::parse_yaml(MANIFEST).unwrap())
        .await
        .unwrap();
    let use_case = StandardStartWorkflowExecutionUseCase::new(
        workflows,
        Arc::new(InMemoryWorkflowExecutionRepository::new()),
        Arc::new(tokio::sync::RwLock::new(Some(
            Arc::new(client) as Arc<dyn WorkflowEnginePort>
        ))),
        Arc::new(EventBus::new(8)),
    );
    use_case.set_repositories(Arc::new(Filling));
    use_case
        .start_execution(StartWorkflowExecutionRequest {
            workflow_id: "the-forge".to_string(),
            input: json!({ "task": "x", "repositories": [{ "binding_id": BINDING }] }),
            blackboard: None,
            version: None,
            tenant_id: Some(tenant.clone()),
            security_context_name: None,
            intent: None,
        })
        .await
        .unwrap_or_else(|e| panic!("the start failed: {e:#}"));

    let starts = temporal.starts.lock().unwrap();
    assert_eq!(starts.len(), 1, "one start reached Temporal");
    let payload: Value = serde_json::from_slice(
        &starts[0]
            .input
            .as_ref()
            .expect("the start carries an input")
            .payloads[0]
            .data,
    )
    .unwrap();
    assert_eq!(
        payload.get("repositories"),
        Some(&json!([{
            "binding_id": BINDING,
            "branch": "aegis/1234abcd",
            "label": "app",
            "ref": "main",
            "started_from": STARTED_FROM
        }])),
        "the payload carries no prepared repositories: {payload}"
    );
    assert_eq!(
        payload["input"].get("repositories"),
        None,
        "the workflow's input holds the repositories: {payload}"
    );
}
