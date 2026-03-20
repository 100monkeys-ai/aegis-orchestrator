// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::application::execution::ExecutionService;
use crate::domain::agent::AgentId;
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{ExecutionId, ExecutionInput};
use crate::domain::volume::TenantId;
use anyhow::{anyhow, Result};
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;

pub type ExecutionStream = Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>;

pub struct ForwardExecutionRequest {
    pub execution_id: ExecutionId,
    pub agent_id: AgentId,
    pub input: String,
    pub tenant_id: TenantId,
    pub originating_node_id: String,
    pub user_security_token: String,
}

pub struct ForwardExecutionUseCase {
    execution_service: Arc<dyn ExecutionService>,
}

impl ForwardExecutionUseCase {
    pub fn new(execution_service: Arc<dyn ExecutionService>) -> Self {
        Self { execution_service }
    }

    pub async fn execute(&self, req: ForwardExecutionRequest) -> Result<ExecutionStream> {
        // 1. Parse input JSON
        let input: ExecutionInput = serde_json::from_str(&req.input)
            .map_err(|e| anyhow!("Invalid execution input: {}", e))?;

        // 2. Start execution locally
        // Note: We might need a way to reuse the execution_id if it was pre-allocated by the controller.
        // For now, we assume start_execution creates a new one, but we might want a 'resume' or 'import' call.
        // ADR-059 says the worker runs it.
        let local_id = self
            .execution_service
            .start_execution(req.agent_id, input)
            .await?;

        // If the execution_id from the request is different from local_id, we have a problem
        // with identity across nodes. But in Phase 1, the client will use the local_id
        // returned by the worker after the stream starts or similar.
        // Actually, the proto has execution_id in ForwardExecutionInner.
        // We should probably ensure the execution service can use a provided ID.

        // 3. Stream events back
        let stream = self.execution_service.stream_execution(local_id).await?;

        Ok(stream)
    }
}
