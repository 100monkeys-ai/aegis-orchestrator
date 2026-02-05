use crate::domain::execution::{Execution, ExecutionId, Iteration};
use crate::domain::events::ExecutionEvent;
use crate::domain::agent::AgentId;
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

#[derive(Debug, Clone)]
pub struct ExecutionInput {
    pub input: String,
    // Add other execution parameters as needed
}

#[async_trait]
pub trait ExecutionService: Send + Sync {
    async fn start_execution(&self, agent_id: AgentId, input: ExecutionInput) -> Result<ExecutionId>;
    async fn get_execution(&self, id: ExecutionId) -> Result<Execution>;
    async fn get_iterations(&self, exec_id: ExecutionId) -> Result<Vec<Iteration>>;
    async fn cancel_execution(&self, id: ExecutionId) -> Result<()>;
    // Note: returning impl Stream in trait needs simpler signature or boxing for object safety.
    // For now, we will use a BoxStream type alias or similar if we were using it directly, 
    // but `async_trait` doesn't support `impl Stream` return types directly easily.
    // We'll define it to return a pinned box stream.
    async fn stream_execution(&self, id: ExecutionId) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>>;
}
