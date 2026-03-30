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
            tenant_id: TenantId::local_default().to_string(),
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
            .find_by_id_for_tenant(&TenantId::local_default(), *agent_id)
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
            tenant_id: TenantId::local_default().to_string(),
        };

        if let Err(e) = self.cortex_client.remove_discovery_agent(req).await {
            tracing::warn!(agent_id = %agent_id, error = %e, "Failed to remove agent from Cortex index");
        } else {
            tracing::debug!(agent_id = %agent_id, "Removed agent from Cortex discovery index");
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Workflow handlers
    // ──────────────────────────────────────────────────────────────────────

    async fn handle_workflow_upsert(&self, workflow_id: &WorkflowId, name: &str, version: &str) {
        // Look up full workflow from repo to get description, states, agents, labels
        let workflow = match self
            .workflow_repo
            .find_by_name_and_version_for_tenant(&TenantId::local_default(), name, version)
            .await
        {
            Ok(Some(w)) => w,
            Ok(None) => {
                // Fallback: try by ID
                match self
                    .workflow_repo
                    .find_by_id_for_tenant(&TenantId::local_default(), *workflow_id)
                    .await
                {
                    Ok(Some(w)) => w,
                    Ok(None) => {
                        tracing::warn!(
                            workflow_id = %workflow_id,
                            name = name,
                            "Workflow not found for index upsert, skipping"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(
                            workflow_id = %workflow_id,
                            error = %e,
                            "Failed to look up workflow by ID for index upsert"
                        );
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    error = %e,
                    "Failed to look up workflow for index upsert"
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
    // Backfill
    // ──────────────────────────────────────────────────────────────────────

    /// Backfill the Cortex discovery index from all existing agents and workflows.
    /// Called once at startup.
    pub async fn backfill(&self) -> anyhow::Result<(usize, usize)> {
        tracing::info!("Starting Cortex discovery index backfill");

        // ── Agents ──────────────────────────────────────────────────────
        let agents = self
            .agent_repo
            .list_all_for_tenant(&TenantId::local_default())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agents for backfill: {e}"))?;

        let mut agent_count = 0usize;
        for agent in &agents {
            self.handle_agent_upsert(&agent.id, &agent.manifest).await;
            agent_count += 1;
            if agent_count.is_multiple_of(50) {
                tracing::info!(agent_count, "Backfill progress: agents indexed");
            }
        }

        // ── Workflows ───────────────────────────────────────────────────
        let workflows = self
            .workflow_repo
            .list_all_for_tenant(&TenantId::local_default())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list workflows for backfill: {e}"))?;

        let mut workflow_count = 0usize;
        for workflow in &workflows {
            let version = workflow.metadata.version.as_deref().unwrap_or("0.1.0");
            self.handle_workflow_upsert(&workflow.id, &workflow.metadata.name, version)
                .await;
            workflow_count += 1;
            if workflow_count.is_multiple_of(50) {
                tracing::info!(workflow_count, "Backfill progress: workflows indexed");
            }
        }

        tracing::info!(
            agent_count,
            workflow_count,
            "Cortex discovery index backfill complete"
        );

        Ok((agent_count, workflow_count))
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
