// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::execution::ExecutionId;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;
use tonic::transport::Channel;
use uuid::Uuid;

// Import generated protos
use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::workflow_service_client::WorkflowServiceClient;
use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::StartWorkflowExecutionRequest;
use crate::infrastructure::temporal_proto::temporal::api::common::v1::{WorkflowType, Payloads, Payload};

#[derive(Clone)]
pub struct TemporalClient {
    // We store the channel or client? Client is Clone but Channel is easier to manage?
    // Tonic clients are cheap to clone.
    client: WorkflowServiceClient<Channel>,
    namespace: String,
    task_queue: String,
}

impl TemporalClient {
    pub async fn new(address: &str, namespace: &str, task_queue: &str) -> Result<Self> {
        // Ensure address has scheme
         let addr = if address.contains("://") {
            address.to_string()
        } else {
            format!("http://{}", address)
        };

        let endpoint = Channel::from_shared(addr)
            .context("Invalid Temporal address")?
            .timeout(Duration::from_secs(10));
            
        let channel = endpoint.connect().await
            .context("Failed to connect to Temporal server")?;

        let client = WorkflowServiceClient::new(channel);

        Ok(Self {
            client,
            namespace: namespace.to_string(),
            task_queue: task_queue.to_string(),
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
}
