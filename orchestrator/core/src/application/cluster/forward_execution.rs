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
    /// ADR-083: security context name propagated from the originating node
    pub security_context_name: String,
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

        // 2. Start execution locally with the imported execution identity from the
        // originating node, preserving tracing correlation across the cluster.
        let local_id = self
            .execution_service
            .start_execution_with_id(
                req.execution_id,
                req.agent_id,
                input,
                req.security_context_name,
                None,
            )
            .await?;

        // 3. Stream events back
        let stream = self.execution_service.stream_execution(local_id).await?;

        Ok(stream)
    }
}
