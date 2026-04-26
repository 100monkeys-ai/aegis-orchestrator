// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Cluster-Aware Execution Service (BC-2 + BC-16)
//!
//! Wraps a local [`ExecutionService`] with routing logic for multi-node
//! clusters. When this node is a controller (or hybrid) and the cluster is
//! enabled, `start_execution` attempts to route the execution to a worker
//! node via [`RouteExecutionUseCase`] and forward it over gRPC. If routing
//! fails (no eligible workers, network error, etc.), the execution falls
//! back to the local service transparently.
//!
//! All non-execution-start methods delegate directly to the inner local
//! service — they do not involve cluster routing.

use crate::application::cluster::route_execution::{
    RouteExecutionRequest as RouteRequest, RouteExecutionUseCase,
};
use crate::application::execution::ExecutionService;
use crate::domain::agent::AgentId;
use crate::domain::cluster::NodeId;
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration, LlmInteraction, TrajectoryStep,
};
use crate::domain::iam::UserIdentity;
use crate::domain::node_config::NodeRole;
use crate::domain::volume::TenantId;
use crate::infrastructure::cluster::NodeClusterClient;
use crate::infrastructure::event_bus::DomainEvent;
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;

/// Configuration required for the controller to forward executions to workers.
#[derive(Clone)]
pub struct ClusterForwardingConfig {
    pub signing_key: Arc<ed25519_dalek::SigningKey>,
    pub controller_node_id: NodeId,
    /// The controller's own security token for authenticating with workers.
    pub security_token: String,
}

/// Execution service decorator that adds cluster-aware routing and forwarding.
///
/// On `start_execution`, if the node role is `Controller` or `Hybrid` and
/// cluster mode is enabled, the service:
///
/// 1. Calls `RouteExecutionUseCase` to select a worker
/// 2. Opens a gRPC connection to the selected worker
/// 3. Calls `ForwardExecution` and streams events back to the caller
///
/// If routing fails or no workers are available, it falls back to the inner
/// local execution service. All other trait methods delegate to the inner
/// service directly.
pub struct ClusterAwareExecutionService {
    inner: Arc<dyn ExecutionService>,
    route_use_case: Arc<RouteExecutionUseCase>,
    forwarding_config: ClusterForwardingConfig,
    node_role: NodeRole,
    cluster_enabled: bool,
}

impl ClusterAwareExecutionService {
    pub fn new(
        inner: Arc<dyn ExecutionService>,
        route_use_case: Arc<RouteExecutionUseCase>,
        forwarding_config: ClusterForwardingConfig,
        node_role: NodeRole,
        cluster_enabled: bool,
    ) -> Self {
        Self {
            inner,
            route_use_case,
            forwarding_config,
            node_role,
            cluster_enabled,
        }
    }

    /// Whether this node should attempt to route executions to workers.
    fn should_route(&self) -> bool {
        self.cluster_enabled && matches!(self.node_role, NodeRole::Controller | NodeRole::Hybrid)
    }

    /// Attempt to route and forward an execution to a remote worker.
    ///
    /// Returns `Ok(execution_id)` if the execution was successfully forwarded,
    /// or `Err` if routing/forwarding failed (caller should fall back to local).
    async fn try_forward(
        &self,
        execution_id: ExecutionId,
        agent_id: AgentId,
        input: &ExecutionInput,
        tenant_id: &TenantId,
        security_context_name: &str,
    ) -> Result<ExecutionId> {
        let route_req = RouteRequest {
            execution_id,
            agent_id,
            required_capabilities: Default::default(),
            preferred_tags: Vec::new(),
            tenant_id: tenant_id.clone(),
        };

        let route = self.route_use_case.execute(route_req).await?;

        tracing::info!(
            execution_id = %execution_id,
            target_node = %route.target_node_id,
            worker_address = %route.worker_grpc_address,
            "Routing execution to worker node"
        );

        let input_json = serde_json::to_string(input)?;

        let mut client = NodeClusterClient::connect_to_worker(
            &route.worker_grpc_address,
            self.forwarding_config.signing_key.clone(),
            self.forwarding_config.controller_node_id,
            self.forwarding_config.security_token.clone(),
        )
        .await?;

        // Fire-and-forget: the worker will stream events back, but we relay
        // them via the event bus so local subscribers (SSE, Cortex) still see
        // them. The actual streaming is kicked off here; the caller monitors
        // the execution via `stream_execution` on the event bus.
        let _stream = client
            .forward_execution(
                execution_id,
                agent_id,
                &input_json,
                &tenant_id.to_string(),
                &self.forwarding_config.controller_node_id.to_string(),
                &self.forwarding_config.security_token,
                security_context_name,
            )
            .await?;

        // TODO(B1-followup): Spawn a background task to consume the worker
        // stream and re-emit events onto the local EventBus so SSE subscribers
        // on the controller see real-time updates. For now the execution_id is
        // returned and the caller can poll the worker directly.

        tracing::info!(
            execution_id = %execution_id,
            target_node = %route.target_node_id,
            "Execution forwarded to worker"
        );

        Ok(execution_id)
    }
}

#[async_trait]
impl ExecutionService for ClusterAwareExecutionService {
    async fn start_execution(
        &self,
        agent_id: AgentId,
        input: ExecutionInput,
        security_context_name: String,
        identity: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        if self.should_route() {
            // Pre-generate an execution ID so it's consistent across routing
            // and potential local fallback.
            let execution_id = ExecutionId::new();
            let tenant_id = input
                .input
                .get("tenant_id")
                .and_then(|v| v.as_str())
                .or_else(|| input.input.get("tenant").and_then(|v| v.as_str()))
                .and_then(|s| TenantId::from_string(s).ok())
                .unwrap_or_else(|| {
                    TenantId::from_string(crate::domain::tenant::CONSUMER_SLUG)
                        .expect("CONSUMER_SLUG is a valid tenant identifier")
                });

            match self
                .try_forward(
                    execution_id,
                    agent_id,
                    &input,
                    &tenant_id,
                    &security_context_name,
                )
                .await
            {
                Ok(id) => return Ok(id),
                Err(e) => {
                    tracing::warn!(
                        execution_id = %execution_id,
                        error = %e,
                        "Cluster routing/forwarding failed, falling back to local execution"
                    );
                    // Fall through to local execution with the same ID.
                    return self
                        .inner
                        .start_execution_with_id(
                            execution_id,
                            agent_id,
                            input,
                            security_context_name,
                            identity,
                        )
                        .await;
                }
            }
        }

        self.inner
            .start_execution(agent_id, input, security_context_name, identity)
            .await
    }

    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        agent_id: AgentId,
        input: ExecutionInput,
        security_context_name: String,
        identity: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        if self.should_route() {
            let tenant_id = input
                .input
                .get("tenant_id")
                .and_then(|v| v.as_str())
                .or_else(|| input.input.get("tenant").and_then(|v| v.as_str()))
                .and_then(|s| TenantId::from_string(s).ok())
                .unwrap_or_else(|| {
                    TenantId::from_string(crate::domain::tenant::CONSUMER_SLUG)
                        .expect("CONSUMER_SLUG is a valid tenant identifier")
                });

            match self
                .try_forward(
                    execution_id,
                    agent_id,
                    &input,
                    &tenant_id,
                    &security_context_name,
                )
                .await
            {
                Ok(id) => return Ok(id),
                Err(e) => {
                    tracing::warn!(
                        execution_id = %execution_id,
                        error = %e,
                        "Cluster routing/forwarding failed, falling back to local execution"
                    );
                }
            }
        }

        self.inner
            .start_execution_with_id(
                execution_id,
                agent_id,
                input,
                security_context_name,
                identity,
            )
            .await
    }

    async fn start_child_execution(
        &self,
        agent_id: AgentId,
        input: ExecutionInput,
        parent_execution_id: ExecutionId,
    ) -> Result<ExecutionId> {
        // Child executions always run local to the parent's node to preserve
        // shared volume mounts and NFS contexts.
        self.inner
            .start_child_execution(agent_id, input, parent_execution_id)
            .await
    }

    async fn get_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Execution> {
        self.inner.get_execution_for_tenant(tenant_id, id).await
    }

    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        self.inner.get_execution_unscoped(id).await
    }

    async fn get_iterations_for_tenant(
        &self,
        tenant_id: &TenantId,
        exec_id: ExecutionId,
    ) -> Result<Vec<Iteration>> {
        self.inner
            .get_iterations_for_tenant(tenant_id, exec_id)
            .await
    }

    async fn cancel_execution(&self, id: ExecutionId) -> Result<()> {
        self.inner.cancel_execution(id).await
    }

    async fn stream_execution(
        &self,
        id: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        self.inner.stream_execution(id).await
    }

    async fn stream_agent_events(
        &self,
        id: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        self.inner.stream_agent_events(id).await
    }

    async fn list_executions(
        &self,
        agent_id: Option<AgentId>,
        limit: usize,
    ) -> Result<Vec<Execution>> {
        self.inner.list_executions(agent_id, limit).await
    }

    async fn delete_execution(&self, id: ExecutionId) -> Result<()> {
        self.inner.delete_execution(id).await
    }

    async fn record_llm_interaction(
        &self,
        execution_id: ExecutionId,
        iteration: u8,
        interaction: LlmInteraction,
    ) -> Result<()> {
        self.inner
            .record_llm_interaction(execution_id, iteration, interaction)
            .await
    }

    async fn store_iteration_trajectory(
        &self,
        execution_id: ExecutionId,
        iteration: u8,
        trajectory: Vec<TrajectoryStep>,
    ) -> Result<()> {
        self.inner
            .store_iteration_trajectory(execution_id, iteration, trajectory)
            .await
    }
}
