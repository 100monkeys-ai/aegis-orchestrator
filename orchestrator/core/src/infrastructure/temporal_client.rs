// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Temporal.io gRPC Client
//!
//! Provides low-level gRPC client for interacting with Temporal.io workflow engine.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** gRPC communication with Temporal workflow service
//! - **Integration:** AEGIS Workflow Engine → Temporal.io gRPC API
//!
//! # Client Features
//!
//! - **Workflow Execution**: Start workflows with the Generic Interpreter pattern
//! - **Connection Management**: Persistent gRPC channel with timeout handling
//! - **JSON Payload Encoding**: Standard encoding for workflow inputs
//! - **Namespace Isolation**: Multi-tenant workflow execution support
//!
//! # Generic Interpreter Pattern
//!
//! This client uses a generic workflow pattern where:
//! 1. All AEGIS workflows execute via a single TypeScript workflow function (`aegis_workflow`)
//! 2. The workflow name and input are passed as payload parameters
//! 3. The TypeScript worker interprets the workflow definition at runtime
//!
//! Input structure:
//! ```json
//! {
//!   "workflow_name": "user-query-pipeline",
//!   "input": { "query": "..." }
//! }
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use temporal_client::TemporalClient;
//!
//! let client = TemporalClient::new(
//!     "localhost:7233",
//!     "default",
//!     "aegis-task-queue",
//!     "http://temporal-worker:3000"
//! ).await?;
//!
//! let run_id = client.start_workflow(
//!     "my-workflow",
//!     execution_id,
//!     input_params
//! ).await?;
//! ```
//!
//! # Configuration
//!
//! - **Address**: Temporal server endpoint (e.g., `localhost:7233`)
//! - **Namespace**: Logical isolation boundary for workflows
//! - **Task Queue**: Worker registration and task routing identifier

use crate::domain::execution::ExecutionId;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;
use tonic::transport::Channel;
use uuid::Uuid;
use reqwest::Client as HttpClient;

// Import generated protos
use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::workflow_service_client::WorkflowServiceClient;
use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::StartWorkflowExecutionRequest;
use crate::infrastructure::temporal_proto::temporal::api::common::v1::{WorkflowType, Payloads, Payload};

#[derive(Clone)]
pub struct TemporalClient {
    client: WorkflowServiceClient<Channel>,
    http_client: HttpClient,
    namespace: String,
    task_queue: String,
    #[allow(dead_code)]
    temporal_endpoint: String,
    worker_http_endpoint: String,
}

impl TemporalClient {
    pub async fn new(
        address: &str,
        namespace: &str,
        task_queue: &str,
        worker_http_endpoint: &str,
    ) -> Result<Self> {
        // Ensure address has scheme
        let addr = if address.contains("://") {
            address.to_string()
        } else {
            format!("http://{}", address)
        };

        let endpoint = Channel::from_shared(addr.clone())
            .context("Invalid Temporal address")?
            .timeout(Duration::from_secs(10));

        let channel = endpoint
            .connect()
            .await
            .context("Failed to connect to Temporal server")?;

        let client = WorkflowServiceClient::new(channel);
        let http_client = HttpClient::new();

        Ok(Self {
            client,
            http_client,
            namespace: namespace.to_string(),
            task_queue: task_queue.to_string(),
            temporal_endpoint: address.to_string(),
            worker_http_endpoint: worker_http_endpoint.to_string(),
        })
    }

    /// Start a workflow execution using the Generic Interpreter pattern
    pub async fn start_workflow(
        &self,
        workflow_name: &str,
        execution_id: ExecutionId,
        input: HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let workflow_id = execution_id.0.to_string();
        
        // Generic workflow type that the worker registers
        let workflow_type_name = "aegis_workflow";

        // Construct input payload matching GenericWorkflowInput interface in TS
        // interface GenericWorkflowInput { workflow_name: string; input: Record<string, any>; }
        let input_obj = serde_json::json!({
            "workflow_name": workflow_name,
            "input": input
        });

        // Serialize to JSON payload
        let json_bytes = serde_json::to_vec(&input_obj)?;
        
        // Metadata for JSON encoding
        let mut metadata = HashMap::new();
        metadata.insert(
            "encoding".to_string(), 
            "json/plain".as_bytes().to_vec()
        );

        let payload = Payload {
            metadata,
            data: json_bytes,
            // external_payloads was added in recent temporal api versions?
            // If prost generated it, we must provide it.
            // Check if it exists in the downloaded proto. 
            // Assuming strict error means it exists.
            // It is likely repeated?
            // Let's assume Vec::new().
            ..Default::default() 
        };

        let payloads = Payloads {
            payloads: vec![payload],
        };

        let request_id = Uuid::new_v4().to_string();

        let request = StartWorkflowExecutionRequest {
            namespace: self.namespace.clone(),
            workflow_id: workflow_id.clone(),
            workflow_type: Some(WorkflowType { name: workflow_type_name.to_string() }),
            task_queue: Some(crate::infrastructure::temporal_proto::temporal::api::taskqueue::v1::TaskQueue {
                name: self.task_queue.clone(),
                kind: 0, // Normal
                ..Default::default()
            }),
            input: Some(payloads),
            request_id,
            ..Default::default()
        };

        let mut client = self.client.clone();
        let response = client.start_workflow_execution(request).await
            .context("Failed to start workflow execution via gRPC")?;
            
        Ok(response.into_inner().run_id)
    }
    /// Get workflow execution history
    pub async fn get_workflow_history(
        &self,
        execution_id: String,
        run_id: Option<String>,
    ) -> Result<Vec<crate::infrastructure::temporal_proto::temporal::api::history::v1::HistoryEvent>> {
        use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::GetWorkflowExecutionHistoryRequest;
        use crate::infrastructure::temporal_proto::temporal::api::common::v1::WorkflowExecution;

        let request = GetWorkflowExecutionHistoryRequest {
            namespace: self.namespace.clone(),
            execution: Some(WorkflowExecution {
                workflow_id: execution_id,
                run_id: run_id.unwrap_or_default(),
            }),
            maximum_page_size: 1000,
            next_page_token: Vec::new(),
            wait_new_event: false,
            history_event_filter_type: 0, // All events
            ..Default::default()
        };

        let mut client = self.client.clone();
        let response = client.get_workflow_execution_history(request).await
            .context("Failed to get workflow history")?;
            
        Ok(response.into_inner().history.map(|h| h.events).unwrap_or_default())
    }

    /// Register a workflow definition with the Temporal worker
    ///
    /// This calls the TypeScript worker HTTP API to register a new workflow definition.
    /// The worker stores the definition in PostgreSQL for dynamic runtime interpretation.
    ///
    /// # HTTP Endpoint
    ///
    /// POST /{worker_http_endpoint}/register-workflow
    /// Body: JSON serialized TemporalWorkflowDefinition
    /// Response: 200 OK {status: "registered"} or error
    pub async fn register_temporal_workflow(
        &self,
        definition: &crate::application::temporal_mapper::TemporalWorkflowDefinition,
    ) -> Result<()> {
        let url = format!("{}/register-workflow", self.worker_http_endpoint);

        let response = self
            .http_client
            .post(&url)
            .json(definition)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("Failed to send workflow registration request to Temporal worker")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "(no body)".to_string());
            anyhow::bail!(
                "Failed to register workflow with Temporal worker: {} - {}",
                status,
                body
            );
        }

        Ok(())
    }
}
