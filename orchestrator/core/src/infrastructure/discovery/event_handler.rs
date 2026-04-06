// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Discovery Index Event Handler (ADR-075)
//!
//! Subscribes to the domain event bus and maintains the Cortex discovery
//! indexes in response to agent and workflow lifecycle events. Runs as a
//! background tokio task. Failed indexing is logged but never blocks the
//! registration flow.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::domain::agent::AgentManifest;
use crate::domain::events::{AgentLifecycleEvent, WorkflowEvent};
use crate::domain::repository::{AgentRepository, WorkflowRepository};
use crate::domain::shared_kernel::AgentId;
use crate::domain::tenant::TenantId;
use crate::domain::workflow::WorkflowId;
use crate::infrastructure::aegis_cortex_proto::{
    IndexAgentRequest, IndexWorkflowRequest, RemoveDiscoveryAgentRequest,
    RemoveDiscoveryWorkflowRequest,
};
use crate::infrastructure::cortex_client::CortexGrpcClient;
use crate::infrastructure::event_bus::{DomainEvent, EventBus, EventBusError};

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryIndexEventHandler
// ──────────────────────────────────────────────────────────────────────────────

/// Background event handler that keeps the Cortex discovery indexes in sync
/// with agent and workflow lifecycle events from the domain event bus.
///
/// Spawn via [`DiscoveryIndexEventHandler::spawn`] — the returned
/// `JoinHandle` runs until the event bus is closed.
pub struct DiscoveryIndexEventHandler {
    cortex_client: Arc<CortexGrpcClient>,
    agent_repo: Arc<dyn AgentRepository>,
    workflow_repo: Arc<dyn WorkflowRepository>,
    event_bus: Arc<EventBus>,
}

impl DiscoveryIndexEventHandler {
    /// Create a new handler with all required dependencies.
    pub fn new(
        cortex_client: Arc<CortexGrpcClient>,
        agent_repo: Arc<dyn AgentRepository>,
        workflow_repo: Arc<dyn WorkflowRepository>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            cortex_client,
            agent_repo,
            workflow_repo,
            event_bus,
        }
    }

    /// Spawn the event handler as a background task. Returns a `JoinHandle`.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mut receiver = self.event_bus.subscribe();
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(event) => self.handle_event(event).await,
                    Err(EventBusError::Lagged(n)) => {
                        tracing::warn!(
                            lagged = n,
                            "Discovery event handler lagged, continuing — backfill will catch up"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Discovery event handler receiver error");
                    }
                }
            }
        })
    }

    // ──────────────────────────────────────────────────────────────────────
    // Event dispatch
    // ──────────────────────────────────────────────────────────────────────

    async fn handle_event(&self, event: DomainEvent) {
        match event {
            DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentDeployed {
                agent_id,
                manifest,
                ..
            }) => {
                self.handle_agent_upsert(&agent_id, &manifest).await;
            }
            DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentUpdated { agent_id, .. }) => {
                self.handle_agent_update(&agent_id).await;
            }
            DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentRemoved { agent_id, .. }) => {
                self.handle_agent_remove(&agent_id).await;
            }
            DomainEvent::Workflow(WorkflowEvent::WorkflowRegistered {
                workflow_id,
                name,
                version,
                ..
            }) => {
                self.handle_workflow_upsert(&workflow_id, &name, &version)
                    .await;
            }
            DomainEvent::Workflow(WorkflowEvent::WorkflowRemoved { workflow_id, .. }) => {
                self.handle_workflow_remove(&workflow_id).await;
            }
            _ => {} // Ignore other events
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Agent handlers
    // ──────────────────────────────────────────────────────────────────────

    async fn handle_agent_upsert(&self, agent_id: &AgentId, manifest: &AgentManifest) {
        let name = &manifest.metadata.name;
        let version = &manifest.metadata.version;
        let description = manifest
            .metadata
            .description
            .as_deref()
            .unwrap_or_default()
            .to_string();
        let labels = manifest.metadata.labels.clone();
        let tools = manifest.spec.tools.clone();
        let task_description = manifest
            .spec
            .task
            .as_ref()
            .and_then(|t| t.instruction.as_deref())
            .unwrap_or_default()
            .to_string();
        let runtime_language = manifest
            .spec
            .runtime
            .language
            .as_deref()
            .unwrap_or("unknown")
            .to_string();

        let req = IndexAgentRequest {
            agent_id: agent_id.to_string(),
            tenant_id: TenantId::consumer().to_string(),
            name: name.clone(),
            version: version.clone(),
            description,
            labels,
            tools,
            task_description,
            runtime_language,
            status: "Active".to_string(),
            is_platform_template: false,
            updated_at: Utc::now().to_rfc3339(),
            input_schema: manifest
                .spec
                .input_schema
                .as_ref()
                .and_then(|v| serde_json::to_string(v).ok()),
        };

        if let Err(e) = self.index_agent_with_retry(req).await {
            tracing::warn!(agent_id = %agent_id, error = %e, "Failed to index deployed agent in Cortex");
        } else {
            tracing::debug!(agent_id = %agent_id, "Indexed deployed agent in Cortex");
        }
    }

    async fn handle_agent_update(&self, agent_id: &AgentId) {
        let agent = match self
            .agent_repo
            .find_by_id_for_tenant(&TenantId::consumer(), *agent_id)
            .await
        {
            Ok(Some(a)) => a,
            Ok(None) => {
                tracing::warn!(agent_id = %agent_id, "Agent not found for index update, skipping");
                return;
            }
            Err(e) => {
                tracing::warn!(agent_id = %agent_id, error = %e, "Failed to look up agent for index update");
                return;
            }
        };

        self.handle_agent_upsert(agent_id, &agent.manifest).await;
    }

    async fn handle_agent_remove(&self, agent_id: &AgentId) {
        let req = RemoveDiscoveryAgentRequest {
            agent_id: agent_id.to_string(),
            tenant_id: TenantId::consumer().to_string(),
        };

        if let Err(e) = self.cortex_client.remove_discovery_agent(req).await {
            tracing::warn!(agent_id = %agent_id, error = %e, "Failed to remove agent from Cortex index");
        } else {
            tracing::debug!(agent_id = %agent_id, "Removed agent from Cortex discovery index");
        }
    }

    async fn handle_workflow_remove(&self, workflow_id: &WorkflowId) {
        let req = RemoveDiscoveryWorkflowRequest {
            workflow_id: workflow_id.to_string(),
            tenant_id: TenantId::consumer().to_string(),
        };

        if let Err(e) = self.cortex_client.remove_discovery_workflow(req).await {
            tracing::warn!(workflow_id = %workflow_id, error = %e, "Failed to remove workflow from Cortex index");
        } else {
            tracing::info!(workflow_id = %workflow_id, "Removed workflow from Cortex discovery index");
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Workflow handlers
    // ──────────────────────────────────────────────────────────────────────

    /// Search for a workflow by name+version and then by ID across both the
    /// consumer tenant and the system tenant. Built-in workflows are registered
    /// under `TenantId::system()`, so a single-tenant lookup always misses them.
    /// Errors at each step are logged and treated as not-found so that subsequent
    /// fallbacks are always attempted.
    async fn find_workflow_across_tenants(
        &self,
        workflow_id: &WorkflowId,
        name: &str,
        version: &str,
    ) -> Option<crate::domain::workflow::Workflow> {
        // 1. name+version under consumer
        match self
            .workflow_repo
            .find_by_name_and_version_for_tenant(&TenantId::consumer(), name, version)
            .await
        {
            Ok(Some(w)) => return Some(w),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    error = %e,
                    "Failed to look up workflow by name/version for consumer tenant"
                );
            }
        }

        // 2. name+version under system
        match self
            .workflow_repo
            .find_by_name_and_version_for_tenant(&TenantId::system(), name, version)
            .await
        {
            Ok(Some(w)) => return Some(w),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    error = %e,
                    "Failed to look up workflow by name/version for system tenant"
                );
            }
        }

        // 3. ID under consumer
        match self
            .workflow_repo
            .find_by_id_for_tenant(&TenantId::consumer(), *workflow_id)
            .await
        {
            Ok(Some(w)) => return Some(w),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    error = %e,
                    "Failed to look up workflow by ID for consumer tenant"
                );
            }
        }

        // 4. ID under system
        match self
            .workflow_repo
            .find_by_id_for_tenant(&TenantId::system(), *workflow_id)
            .await
        {
            Ok(Some(w)) => return Some(w),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    error = %e,
                    "Failed to look up workflow by ID for system tenant"
                );
            }
        }

        None
    }

    async fn handle_workflow_upsert(&self, workflow_id: &WorkflowId, name: &str, version: &str) {
        // Look up full workflow from repo to get description, states, agents, labels.
        // Searches across both the consumer and system tenants so that built-in
        // workflows (registered under TenantId::system()) are not silently skipped.
        let workflow = match self
            .find_workflow_across_tenants(workflow_id, name, version)
            .await
        {
            Some(w) => w,
            None => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    name = name,
                    "Workflow not found for index upsert, skipping"
                );
                return;
            }
        };

        let description = workflow
            .metadata
            .description
            .as_deref()
            .unwrap_or_default()
            .to_string();
        let labels = workflow.metadata.labels.clone();
        let state_names: Vec<String> = workflow.spec.states.keys().map(|s| s.to_string()).collect();
        let agent_names: Vec<String> = extract_agent_names_from_workflow(&workflow);

        let req = IndexWorkflowRequest {
            workflow_id: workflow_id.to_string(),
            tenant_id: workflow.tenant_id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            description,
            labels,
            state_names,
            agent_names,
            is_platform_template: false,
            updated_at: Utc::now().to_rfc3339(),
            input_schema: workflow
                .metadata
                .input_schema
                .as_ref()
                .and_then(|v| serde_json::to_string(v).ok()),
        };

        if let Err(e) = self.index_workflow_with_retry(req).await {
            tracing::warn!(workflow_id = %workflow_id, error = %e, "Failed to index workflow in Cortex");
        } else {
            tracing::debug!(workflow_id = %workflow_id, name = name, "Indexed workflow in Cortex");
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Retry helpers
    // ──────────────────────────────────────────────────────────────────────

    async fn index_agent_with_retry(&self, req: IndexAgentRequest) -> anyhow::Result<()> {
        let mut delay = Duration::from_millis(100);
        let mut last_err = None;
        for attempt in 0..3u32 {
            match self.cortex_client.index_agent(req.clone()).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        error = %e,
                        "Cortex IndexAgent failed, retrying"
                    );
                    last_err = Some(e);
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
        Err(anyhow::anyhow!(
            "Cortex IndexAgent failed after 3 attempts: {}",
            last_err.unwrap()
        ))
    }

    async fn index_workflow_with_retry(&self, req: IndexWorkflowRequest) -> anyhow::Result<()> {
        let mut delay = Duration::from_millis(100);
        let mut last_err = None;
        for attempt in 0..3u32 {
            match self.cortex_client.index_workflow(req.clone()).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        error = %e,
                        "Cortex IndexWorkflow failed, retrying"
                    );
                    last_err = Some(e);
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
        Err(anyhow::anyhow!(
            "Cortex IndexWorkflow failed after 3 attempts: {}",
            last_err.unwrap()
        ))
    }

    // ──────────────────────────────────────────────────────────────────────
    // Backfill / Reconcile
    // ──────────────────────────────────────────────────────────────────────

    /// Index all known agents and workflows into Cortex. Shared by [`Self::backfill`]
    /// and [`Self::reconcile`].
    async fn index_all(&self) -> anyhow::Result<(usize, usize)> {
        // ── Agents ──────────────────────────────────────────────────────
        let agents = self
            .agent_repo
            .list_all_for_tenant(&TenantId::consumer())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agents for indexing: {e}"))?;

        let mut agent_count = 0usize;
        for agent in &agents {
            self.handle_agent_upsert(&agent.id, &agent.manifest).await;
            agent_count += 1;
            if agent_count.is_multiple_of(50) {
                tracing::info!(agent_count, "Index progress: agents indexed");
            }
        }

        // ── Workflows ───────────────────────────────────────────────────
        let workflows = self
            .workflow_repo
            .list_all_for_tenant(&TenantId::consumer())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list workflows for indexing: {e}"))?;

        let mut workflow_count = 0usize;
        for workflow in &workflows {
            let version = workflow.metadata.version.as_deref().unwrap_or("0.1.0");
            self.handle_workflow_upsert(&workflow.id, &workflow.metadata.name, version)
                .await;
            workflow_count += 1;
            if workflow_count.is_multiple_of(50) {
                tracing::info!(workflow_count, "Index progress: workflows indexed");
            }
        }

        Ok((agent_count, workflow_count))
    }

    /// Backfill the Cortex discovery index from all existing agents and workflows.
    /// Called once at startup.
    pub async fn backfill(&self) -> anyhow::Result<(usize, usize)> {
        tracing::info!("Starting Cortex discovery index backfill");
        let result = self.index_all().await?;
        tracing::info!(
            agent_count = result.0,
            workflow_count = result.1,
            "Cortex discovery index backfill complete"
        );
        Ok(result)
    }

    /// Reconcile the Cortex discovery index against all known agents and workflows.
    /// Called periodically to correct any drift caused by event lag or transient failures.
    pub async fn reconcile(&self) -> anyhow::Result<(usize, usize)> {
        self.index_all().await
    }

    /// Spawn a background tokio task that calls [`Self::reconcile`] on the given interval.
    /// Errors are logged as warnings and never cause the loop to exit.
    pub fn spawn_reconciler(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match self.reconcile().await {
                    Ok((agents, workflows)) => {
                        tracing::debug!(
                            agents_indexed = agents,
                            workflows_indexed = workflows,
                            "Discovery reconciliation complete"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Discovery reconciliation failed");
                    }
                }
            }
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Extract agent names from all `StateKind::Agent` states in a workflow.
fn extract_agent_names_from_workflow(workflow: &crate::domain::workflow::Workflow) -> Vec<String> {
    use crate::domain::workflow::StateKind;

    let mut names = Vec::new();
    for state in workflow.spec.states.values() {
        if let StateKind::Agent { ref agent, .. } = state.kind {
            if !names.contains(agent) {
                names.push(agent.clone());
            }
        }
    }
    names
}
