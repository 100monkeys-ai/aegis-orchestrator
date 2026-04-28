// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Discovery Index Event Handler (ADR-075)
//!
//! Subscribes to the domain event bus and maintains the Cortex discovery
//! indexes in response to agent and workflow lifecycle events. Runs as a
//! background tokio task. Failed indexing is logged but never blocks the
//! registration flow.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Mutex;
use tonic::Status;

use crate::domain::agent::AgentManifest;
use crate::domain::events::{AgentLifecycleEvent, WorkflowEvent};
use crate::domain::repository::{AgentRepository, WorkflowRepository};
use crate::domain::shared_kernel::AgentId;
use crate::domain::tenant::TenantId;
use crate::domain::workflow::WorkflowId;
use crate::infrastructure::aegis_cortex_proto::{
    IndexAgentRequest, IndexAgentResponse, IndexWorkflowRequest, IndexWorkflowResponse,
    RemoveDiscoveryAgentRequest, RemoveDiscoveryAgentResponse, RemoveDiscoveryWorkflowRequest,
    RemoveDiscoveryWorkflowResponse,
};
use crate::infrastructure::cortex_client::CortexGrpcClient;
use crate::infrastructure::event_bus::{DomainEvent, EventBus, EventBusError};

// ──────────────────────────────────────────────────────────────────────────────
// CortexDiscoveryClient trait
// ──────────────────────────────────────────────────────────────────────────────

/// Narrow trait over the subset of Cortex RPCs used by the discovery event
/// handler. Enables substituting a fake in tests without standing up a real
/// gRPC channel. Implemented for `CortexGrpcClient` below.
#[async_trait]
pub trait CortexDiscoveryClient: Send + Sync + 'static {
    async fn index_agent(&self, request: IndexAgentRequest) -> Result<IndexAgentResponse, Status>;

    async fn index_workflow(
        &self,
        request: IndexWorkflowRequest,
    ) -> Result<IndexWorkflowResponse, Status>;

    async fn remove_discovery_agent(
        &self,
        request: RemoveDiscoveryAgentRequest,
    ) -> Result<RemoveDiscoveryAgentResponse, Status>;

    async fn remove_discovery_workflow(
        &self,
        request: RemoveDiscoveryWorkflowRequest,
    ) -> Result<RemoveDiscoveryWorkflowResponse, Status>;
}

#[async_trait]
impl CortexDiscoveryClient for CortexGrpcClient {
    async fn index_agent(&self, request: IndexAgentRequest) -> Result<IndexAgentResponse, Status> {
        CortexGrpcClient::index_agent(self, request).await
    }

    async fn index_workflow(
        &self,
        request: IndexWorkflowRequest,
    ) -> Result<IndexWorkflowResponse, Status> {
        CortexGrpcClient::index_workflow(self, request).await
    }

    async fn remove_discovery_agent(
        &self,
        request: RemoveDiscoveryAgentRequest,
    ) -> Result<RemoveDiscoveryAgentResponse, Status> {
        CortexGrpcClient::remove_discovery_agent(self, request).await
    }

    async fn remove_discovery_workflow(
        &self,
        request: RemoveDiscoveryWorkflowRequest,
    ) -> Result<RemoveDiscoveryWorkflowResponse, Status> {
        CortexGrpcClient::remove_discovery_workflow(self, request).await
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryIndexEventHandler
// ──────────────────────────────────────────────────────────────────────────────

/// Background event handler that keeps the Cortex discovery indexes in sync
/// with agent and workflow lifecycle events from the domain event bus.
///
/// Spawn via [`DiscoveryIndexEventHandler::spawn`] — the returned
/// `JoinHandle` runs until the event bus is closed.
pub struct DiscoveryIndexEventHandler {
    cortex_client: Arc<dyn CortexDiscoveryClient>,
    agent_repo: Arc<dyn AgentRepository>,
    workflow_repo: Arc<dyn WorkflowRepository>,
    event_bus: Arc<EventBus>,
    /// Fingerprint of the last successful index for each agent. Used by
    /// [`Self::reconcile_drift`] to skip unchanged records. Populated by every
    /// successful `handle_agent_upsert`.
    agent_index_cache: Mutex<HashMap<AgentId, u64>>,
    /// Fingerprint of the last successful index for each workflow.
    workflow_index_cache: Mutex<HashMap<WorkflowId, u64>>,
}

impl DiscoveryIndexEventHandler {
    /// Create a new handler with all required dependencies.
    pub fn new(
        cortex_client: Arc<dyn CortexDiscoveryClient>,
        agent_repo: Arc<dyn AgentRepository>,
        workflow_repo: Arc<dyn WorkflowRepository>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            cortex_client,
            agent_repo,
            workflow_repo,
            event_bus,
            agent_index_cache: Mutex::new(HashMap::new()),
            workflow_index_cache: Mutex::new(HashMap::new()),
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
                tenant_id,
                manifest,
                ..
            }) => {
                self.handle_agent_upsert(&agent_id, &tenant_id, &manifest)
                    .await;
            }
            DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentUpdated {
                agent_id,
                tenant_id,
                ..
            }) => {
                self.handle_agent_update(&agent_id, &tenant_id).await;
            }
            DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentRemoved {
                agent_id,
                tenant_id,
                ..
            }) => {
                self.handle_agent_remove(&agent_id, &tenant_id).await;
            }
            DomainEvent::Workflow(WorkflowEvent::WorkflowRegistered {
                workflow_id,
                tenant_id,
                name,
                version,
                ..
            }) => {
                self.handle_workflow_upsert(&workflow_id, &tenant_id, &name, &version)
                    .await;
            }
            DomainEvent::Workflow(WorkflowEvent::WorkflowRemoved {
                workflow_id,
                tenant_id,
                ..
            }) => {
                self.handle_workflow_remove(&workflow_id, &tenant_id).await;
            }
            _ => {} // Ignore other events
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Fingerprints
    // ──────────────────────────────────────────────────────────────────────

    /// Compute a stable u64 fingerprint of the fields that describe an agent
    /// in the Cortex discovery index. `updated_at` is deliberately excluded —
    /// it changes every call and would defeat the cache.
    fn fingerprint_agent(req: &IndexAgentRequest) -> u64 {
        let mut hasher = DefaultHasher::new();
        req.agent_id.hash(&mut hasher);
        req.tenant_id.hash(&mut hasher);
        req.name.hash(&mut hasher);
        req.version.hash(&mut hasher);
        req.description.hash(&mut hasher);
        // `labels` is a HashMap — hash via a sorted key list for determinism.
        let mut label_pairs: Vec<(&String, &String)> = req.labels.iter().collect();
        label_pairs.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in label_pairs {
            k.hash(&mut hasher);
            v.hash(&mut hasher);
        }
        for tool in &req.tools {
            tool.hash(&mut hasher);
        }
        req.task_description.hash(&mut hasher);
        req.runtime_language.hash(&mut hasher);
        req.status.hash(&mut hasher);
        req.is_platform_template.hash(&mut hasher);
        req.input_schema.hash(&mut hasher);
        hasher.finish()
    }

    /// Compute a stable u64 fingerprint of the fields that describe a
    /// workflow in the Cortex discovery index. `updated_at` is excluded.
    fn fingerprint_workflow(req: &IndexWorkflowRequest) -> u64 {
        let mut hasher = DefaultHasher::new();
        req.workflow_id.hash(&mut hasher);
        req.tenant_id.hash(&mut hasher);
        req.name.hash(&mut hasher);
        req.version.hash(&mut hasher);
        req.description.hash(&mut hasher);
        let mut label_pairs: Vec<(&String, &String)> = req.labels.iter().collect();
        label_pairs.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in label_pairs {
            k.hash(&mut hasher);
            v.hash(&mut hasher);
        }
        for s in &req.state_names {
            s.hash(&mut hasher);
        }
        for a in &req.agent_names {
            a.hash(&mut hasher);
        }
        req.is_platform_template.hash(&mut hasher);
        req.input_schema.hash(&mut hasher);
        hasher.finish()
    }

    // ──────────────────────────────────────────────────────────────────────
    // Agent handlers
    // ──────────────────────────────────────────────────────────────────────

    fn build_agent_index_request(
        agent_id: &AgentId,
        tenant_id: &TenantId,
        manifest: &AgentManifest,
    ) -> IndexAgentRequest {
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

        IndexAgentRequest {
            agent_id: agent_id.to_string(),
            tenant_id: tenant_id.to_string(),
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
        }
    }

    async fn handle_agent_upsert(
        &self,
        agent_id: &AgentId,
        tenant_id: &TenantId,
        manifest: &AgentManifest,
    ) {
        let req = Self::build_agent_index_request(agent_id, tenant_id, manifest);
        let fp = Self::fingerprint_agent(&req);

        if let Err(e) = self.index_agent_with_retry(req).await {
            tracing::warn!(agent_id = %agent_id, error = %e, "Failed to index deployed agent in Cortex");
        } else {
            tracing::debug!(agent_id = %agent_id, "Indexed deployed agent in Cortex");
            self.agent_index_cache.lock().await.insert(*agent_id, fp);
        }
    }

    async fn handle_agent_update(&self, agent_id: &AgentId, tenant_id: &TenantId) {
        let agent = match self
            .agent_repo
            .find_by_id_for_tenant(tenant_id, *agent_id)
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

        self.handle_agent_upsert(agent_id, tenant_id, &agent.manifest)
            .await;
    }

    async fn handle_agent_remove(&self, agent_id: &AgentId, tenant_id: &TenantId) {
        let req = RemoveDiscoveryAgentRequest {
            agent_id: agent_id.to_string(),
            tenant_id: tenant_id.to_string(),
        };

        if let Err(e) = self.cortex_client.remove_discovery_agent(req).await {
            tracing::warn!(agent_id = %agent_id, error = %e, "Failed to remove agent from Cortex index");
        } else {
            tracing::debug!(agent_id = %agent_id, "Removed agent from Cortex discovery index");
            self.agent_index_cache.lock().await.remove(agent_id);
        }
    }

    async fn handle_workflow_remove(&self, workflow_id: &WorkflowId, tenant_id: &TenantId) {
        let req = RemoveDiscoveryWorkflowRequest {
            workflow_id: workflow_id.to_string(),
            tenant_id: tenant_id.to_string(),
        };

        if let Err(e) = self.cortex_client.remove_discovery_workflow(req).await {
            tracing::warn!(workflow_id = %workflow_id, error = %e, "Failed to remove workflow from Cortex index");
        } else {
            tracing::info!(workflow_id = %workflow_id, "Removed workflow from Cortex discovery index");
            self.workflow_index_cache.lock().await.remove(workflow_id);
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Workflow handlers
    // ──────────────────────────────────────────────────────────────────────

    /// Search for a workflow by name+version and then by ID under the given
    /// owning tenant. The tenant is supplied by the publishing event so the
    /// lookup is precise — no cross-tenant fallback is needed (built-in
    /// workflows under `TenantId::system()` are published with that tenant
    /// in the event).
    async fn find_workflow_for_tenant(
        &self,
        workflow_id: &WorkflowId,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Option<crate::domain::workflow::Workflow> {
        match self
            .workflow_repo
            .find_by_name_and_version_for_tenant(tenant_id, name, version)
            .await
        {
            Ok(Some(w)) => return Some(w),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    tenant_id = %tenant_id,
                    error = %e,
                    "Failed to look up workflow by name/version for tenant"
                );
            }
        }

        match self
            .workflow_repo
            .find_by_id_for_tenant(tenant_id, *workflow_id)
            .await
        {
            Ok(Some(w)) => return Some(w),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    tenant_id = %tenant_id,
                    error = %e,
                    "Failed to look up workflow by ID for tenant"
                );
            }
        }

        None
    }

    fn build_workflow_index_request(
        workflow: &crate::domain::workflow::Workflow,
        name: &str,
        version: &str,
    ) -> IndexWorkflowRequest {
        let description = workflow
            .metadata
            .description
            .as_deref()
            .unwrap_or_default()
            .to_string();
        let labels = workflow.metadata.labels.clone();
        let state_names: Vec<String> = workflow.spec.states.keys().map(|s| s.to_string()).collect();
        let agent_names: Vec<String> = extract_agent_names_from_workflow(workflow);

        IndexWorkflowRequest {
            workflow_id: workflow.id.to_string(),
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
        }
    }

    async fn handle_workflow_upsert(
        &self,
        workflow_id: &WorkflowId,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) {
        // Look up full workflow from repo to get description, states, agents, labels.
        // The owning tenant comes from the published event, so the lookup is
        // precise to that tenant.
        let workflow = match self
            .find_workflow_for_tenant(workflow_id, tenant_id, name, version)
            .await
        {
            Some(w) => w,
            None => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    tenant_id = %tenant_id,
                    name = name,
                    "Workflow not found for index upsert, skipping"
                );
                return;
            }
        };

        let req = Self::build_workflow_index_request(&workflow, name, version);
        let fp = Self::fingerprint_workflow(&req);

        if let Err(e) = self.index_workflow_with_retry(req).await {
            tracing::warn!(workflow_id = %workflow_id, error = %e, "Failed to index workflow in Cortex");
        } else {
            tracing::debug!(workflow_id = %workflow_id, name = name, "Indexed workflow in Cortex");
            self.workflow_index_cache
                .lock()
                .await
                .insert(*workflow_id, fp);
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

    /// Backfill the Cortex discovery index from all existing agents and workflows.
    /// Called once at startup. Unconditionally re-indexes every record — use
    /// [`Self::reconcile`] for the periodic drift-detection pass.
    ///
    /// Each record is indexed under its own `tenant_id` (sourced from the
    /// loaded entity), not a hardcoded singleton.
    pub async fn backfill(&self) -> anyhow::Result<(usize, usize)> {
        tracing::info!("Starting Cortex discovery index backfill");

        let agents = self
            .agent_repo
            .list_all()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agents for indexing: {e}"))?;

        let mut agent_count = 0usize;
        for agent in &agents {
            self.handle_agent_upsert(&agent.id, &agent.tenant_id, &agent.manifest)
                .await;
            agent_count += 1;
            if agent_count.is_multiple_of(50) {
                tracing::info!(agent_count, "Index progress: agents indexed");
            }
        }

        let workflows = self
            .workflow_repo
            .list_all()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list workflows for indexing: {e}"))?;

        let mut workflow_count = 0usize;
        for workflow in &workflows {
            let version = workflow.metadata.version.as_deref().unwrap_or("0.1.0");
            self.handle_workflow_upsert(
                &workflow.id,
                &workflow.tenant_id,
                &workflow.metadata.name,
                version,
            )
            .await;
            workflow_count += 1;
            if workflow_count.is_multiple_of(50) {
                tracing::info!(workflow_count, "Index progress: workflows indexed");
            }
        }

        tracing::info!(
            agent_count,
            workflow_count,
            "Cortex discovery index backfill complete"
        );
        Ok((agent_count, workflow_count))
    }

    /// Reconcile the Cortex discovery index against all known agents and
    /// workflows. Called periodically to correct drift caused by event lag or
    /// transient Cortex failures. Skips records whose fingerprint matches the
    /// cached last-indexed fingerprint — unchanged records are silent no-ops.
    pub async fn reconcile(&self) -> anyhow::Result<ReconcileSummary> {
        self.reconcile_drift().await
    }

    /// Drift-only reconciliation: iterate all agents and workflows, skip any
    /// whose current fingerprint matches the cached one. Returns counts of
    /// checked vs actually re-indexed records.
    async fn reconcile_drift(&self) -> anyhow::Result<ReconcileSummary> {
        let mut summary = ReconcileSummary::default();

        // ── Agents ──────────────────────────────────────────────────────
        // Iterate every agent across every tenant. Each agent is re-indexed
        // under its own owning `tenant_id`, never a hardcoded singleton.
        let agents = self
            .agent_repo
            .list_all()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list agents for reconciliation: {e}"))?;

        for agent in &agents {
            summary.agents_checked += 1;
            let req = Self::build_agent_index_request(&agent.id, &agent.tenant_id, &agent.manifest);
            let fp = Self::fingerprint_agent(&req);
            let cached = self.agent_index_cache.lock().await.get(&agent.id).copied();
            if cached == Some(fp) {
                continue;
            }

            if let Err(e) = self.index_agent_with_retry(req).await {
                tracing::warn!(agent_id = %agent.id, error = %e, "Failed to re-index drifted agent");
            } else {
                tracing::debug!(agent_id = %agent.id, "Re-indexed drifted agent");
                self.agent_index_cache.lock().await.insert(agent.id, fp);
                summary.agents_indexed += 1;
            }
        }

        // ── Workflows ───────────────────────────────────────────────────
        let workflows = self
            .workflow_repo
            .list_all()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list workflows for reconciliation: {e}"))?;

        for workflow in &workflows {
            summary.workflows_checked += 1;
            let version = workflow.metadata.version.as_deref().unwrap_or("0.1.0");
            let req =
                Self::build_workflow_index_request(workflow, &workflow.metadata.name, version);
            let fp = Self::fingerprint_workflow(&req);
            let cached = self
                .workflow_index_cache
                .lock()
                .await
                .get(&workflow.id)
                .copied();
            if cached == Some(fp) {
                continue;
            }

            if let Err(e) = self.index_workflow_with_retry(req).await {
                tracing::warn!(workflow_id = %workflow.id, error = %e, "Failed to re-index drifted workflow");
            } else {
                tracing::debug!(workflow_id = %workflow.id, "Re-indexed drifted workflow");
                self.workflow_index_cache
                    .lock()
                    .await
                    .insert(workflow.id, fp);
                summary.workflows_indexed += 1;
            }
        }

        tracing::debug!(
            agents_checked = summary.agents_checked,
            agents_indexed = summary.agents_indexed,
            workflows_checked = summary.workflows_checked,
            workflows_indexed = summary.workflows_indexed,
            "Discovery drift reconciliation complete"
        );

        Ok(summary)
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
                    Ok(summary) => {
                        tracing::debug!(
                            agents_checked = summary.agents_checked,
                            agents_indexed = summary.agents_indexed,
                            workflows_checked = summary.workflows_checked,
                            workflows_indexed = summary.workflows_indexed,
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

/// Counters returned by [`DiscoveryIndexEventHandler::reconcile`] describing
/// how many records were examined vs. actually re-indexed on a drift pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileSummary {
    pub agents_checked: usize,
    pub agents_indexed: usize,
    pub workflows_checked: usize,
    pub workflows_indexed: usize,
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

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{
        Agent, AgentManifest, AgentSpec, ImagePullPolicy, ManifestMetadata, RuntimeConfig,
        TaskConfig,
    };
    use crate::domain::workflow::{
        StateKind, StateName, Workflow, WorkflowMetadata, WorkflowSpec, WorkflowState,
    };
    use crate::infrastructure::aegis_cortex_proto::{
        IndexAgentResponse, IndexWorkflowResponse, RemoveDiscoveryAgentResponse,
        RemoveDiscoveryWorkflowResponse,
    };
    use crate::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryWorkflowRepository,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ─── Fake Cortex client ───────────────────────────────────────────────

    #[derive(Default)]
    struct FakeCortexClient {
        index_agent_calls: AtomicUsize,
        index_workflow_calls: AtomicUsize,
        index_agent_requests: Mutex<Vec<IndexAgentRequest>>,
        index_workflow_requests: Mutex<Vec<IndexWorkflowRequest>>,
        remove_agent_requests: Mutex<Vec<RemoveDiscoveryAgentRequest>>,
        remove_workflow_requests: Mutex<Vec<RemoveDiscoveryWorkflowRequest>>,
    }

    #[async_trait]
    impl CortexDiscoveryClient for FakeCortexClient {
        async fn index_agent(
            &self,
            request: IndexAgentRequest,
        ) -> Result<IndexAgentResponse, Status> {
            self.index_agent_calls.fetch_add(1, Ordering::SeqCst);
            self.index_agent_requests.lock().await.push(request);
            Ok(IndexAgentResponse::default())
        }

        async fn index_workflow(
            &self,
            request: IndexWorkflowRequest,
        ) -> Result<IndexWorkflowResponse, Status> {
            self.index_workflow_calls.fetch_add(1, Ordering::SeqCst);
            self.index_workflow_requests.lock().await.push(request);
            Ok(IndexWorkflowResponse::default())
        }

        async fn remove_discovery_agent(
            &self,
            request: RemoveDiscoveryAgentRequest,
        ) -> Result<RemoveDiscoveryAgentResponse, Status> {
            self.remove_agent_requests.lock().await.push(request);
            Ok(RemoveDiscoveryAgentResponse::default())
        }

        async fn remove_discovery_workflow(
            &self,
            request: RemoveDiscoveryWorkflowRequest,
        ) -> Result<RemoveDiscoveryWorkflowResponse, Status> {
            self.remove_workflow_requests.lock().await.push(request);
            Ok(RemoveDiscoveryWorkflowResponse::default())
        }
    }

    // ─── Fixtures ─────────────────────────────────────────────────────────

    fn make_manifest(name: &str) -> AgentManifest {
        AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: ManifestMetadata {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                description: Some(format!("desc for {name}")),
                labels: HashMap::new(),
                annotations: HashMap::new(),
            },
            spec: AgentSpec {
                runtime: RuntimeConfig {
                    language: Some("python".to_string()),
                    version: Some("3.11".to_string()),
                    image: None,
                    image_pull_policy: ImagePullPolicy::IfNotPresent,
                    isolation: "inherit".to_string(),
                    model: "default".to_string(),
                    temperature: None,
                },
                task: Some(TaskConfig {
                    instruction: Some(format!("task for {name}")),
                    prompt_template: None,
                    input_data: None,
                }),
                context: vec![],
                execution: None,
                security: None,
                schedule: None,
                tools: vec![],
                env: HashMap::new(),
                volumes: vec![],
                advanced: None,
                input_schema: None,
                security_context: None,
                output_handler: None,
            },
        }
    }

    fn make_agent(name: &str) -> Agent {
        let manifest = make_manifest(name);
        let mut agent = Agent::new(manifest);
        agent.tenant_id = TenantId::consumer();
        agent
    }

    fn make_workflow(name: &str, description: &str) -> Workflow {
        let metadata = WorkflowMetadata {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: Some(description.to_string()),
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
            output_schema: None,
            output_template: None,
        };

        let mut states = HashMap::new();
        states.insert(
            StateName::new("START").unwrap(),
            WorkflowState {
                kind: StateKind::System {
                    command: "echo".to_string(),
                    env: HashMap::new(),
                    workdir: None,
                },
                transitions: vec![],
                timeout: None,
                max_state_visits: None,
            },
        );

        let spec = WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
            max_total_transitions: None,
        };

        let mut wf = Workflow::new(metadata, spec).expect("valid workflow");
        wf.tenant_id = TenantId::consumer();
        wf
    }

    async fn seed_agents(
        repo: &InMemoryAgentRepository,
        names: &[&str],
    ) -> Vec<crate::domain::shared_kernel::AgentId> {
        let mut ids = Vec::new();
        for name in names {
            let agent = make_agent(name);
            repo.save_for_tenant(&TenantId::consumer(), &agent)
                .await
                .unwrap();
            ids.push(agent.id);
        }
        ids
    }

    async fn seed_workflows(repo: &InMemoryWorkflowRepository, names: &[&str]) -> Vec<WorkflowId> {
        let mut ids = Vec::new();
        for name in names {
            let wf = make_workflow(name, "initial");
            repo.save_for_tenant(&TenantId::consumer(), &wf)
                .await
                .unwrap();
            ids.push(wf.id);
        }
        ids
    }

    fn make_handler(
        cortex: Arc<FakeCortexClient>,
        agent_repo: Arc<InMemoryAgentRepository>,
        workflow_repo: Arc<InMemoryWorkflowRepository>,
    ) -> DiscoveryIndexEventHandler {
        let event_bus = Arc::new(EventBus::new(1024));
        DiscoveryIndexEventHandler::new(cortex, agent_repo, workflow_repo, event_bus)
    }

    // ─── Regression tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn reconcile_skips_unchanged_agents() {
        let cortex = Arc::new(FakeCortexClient::default());
        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());

        let ids = seed_agents(&agent_repo, &["alpha", "bravo", "charlie"]).await;
        assert_eq!(ids.len(), 3);

        let handler = make_handler(cortex.clone(), agent_repo.clone(), workflow_repo);

        // Backfill: expect 3 index_agent calls (one per agent).
        handler.backfill().await.expect("backfill ok");
        assert_eq!(
            cortex.index_agent_calls.load(Ordering::SeqCst),
            3,
            "backfill should index every agent"
        );

        // Reconcile with no changes: expect 0 additional index_agent calls.
        let summary = handler.reconcile().await.expect("reconcile ok");
        assert_eq!(
            cortex.index_agent_calls.load(Ordering::SeqCst),
            3,
            "reconcile should skip agents whose fingerprint is unchanged"
        );
        assert_eq!(summary.agents_checked, 3);
        assert_eq!(summary.agents_indexed, 0);

        // Mutate one agent's manifest (change description).
        let mut mutated = agent_repo
            .find_by_id_for_tenant(&TenantId::consumer(), ids[1])
            .await
            .unwrap()
            .expect("agent exists");
        let mut new_manifest = mutated.manifest.clone();
        new_manifest.metadata.description = Some("mutated description".to_string());
        mutated.update_manifest(new_manifest);
        agent_repo
            .save_for_tenant(&TenantId::consumer(), &mutated)
            .await
            .unwrap();

        // Reconcile: expect exactly 1 additional index_agent call for the mutated agent.
        let summary = handler.reconcile().await.expect("reconcile ok");
        assert_eq!(
            cortex.index_agent_calls.load(Ordering::SeqCst),
            4,
            "reconcile should re-index only the mutated agent"
        );
        assert_eq!(summary.agents_checked, 3);
        assert_eq!(summary.agents_indexed, 1);
    }

    #[tokio::test]
    async fn reconcile_skips_unchanged_workflows() {
        let cortex = Arc::new(FakeCortexClient::default());
        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());

        let ids = seed_workflows(&workflow_repo, &["wf-a", "wf-b", "wf-c"]).await;
        assert_eq!(ids.len(), 3);

        let handler = make_handler(cortex.clone(), agent_repo, workflow_repo.clone());

        // Backfill: expect 3 index_workflow calls.
        handler.backfill().await.expect("backfill ok");
        assert_eq!(
            cortex.index_workflow_calls.load(Ordering::SeqCst),
            3,
            "backfill should index every workflow"
        );

        // Reconcile with no changes: expect 0 additional index_workflow calls.
        let summary = handler.reconcile().await.expect("reconcile ok");
        assert_eq!(
            cortex.index_workflow_calls.load(Ordering::SeqCst),
            3,
            "reconcile should skip workflows whose fingerprint is unchanged"
        );
        assert_eq!(summary.workflows_checked, 3);
        assert_eq!(summary.workflows_indexed, 0);

        // Mutate one workflow (description change).
        let mut mutated = workflow_repo
            .find_by_id_for_tenant(&TenantId::consumer(), ids[2])
            .await
            .unwrap()
            .expect("workflow exists");
        mutated.metadata.description = Some("mutated wf desc".to_string());
        workflow_repo
            .save_for_tenant(&TenantId::consumer(), &mutated)
            .await
            .unwrap();

        // Reconcile: expect exactly 1 additional index_workflow call.
        let summary = handler.reconcile().await.expect("reconcile ok");
        assert_eq!(
            cortex.index_workflow_calls.load(Ordering::SeqCst),
            4,
            "reconcile should re-index only the mutated workflow"
        );
        assert_eq!(summary.workflows_checked, 3);
        assert_eq!(summary.workflows_indexed, 1);
    }

    // ─── Phase 3 regression: tenant leakage ────────────────────────────────

    /// Build an [`IndexAgentRequest`] for an agent under an explicit tenant.
    /// Asserts the request carries that tenant — i.e. the handler is *not*
    /// hardcoding `TenantId::consumer()` regardless of the actual owning
    /// tenant.
    #[tokio::test]
    async fn build_agent_index_request_uses_supplied_tenant() {
        let manifest = make_manifest("alpha");
        let agent_id = AgentId::new();
        let alice =
            TenantId::for_consumer_user("alice").expect("valid per-user tenant id for alice");

        let req =
            DiscoveryIndexEventHandler::build_agent_index_request(&agent_id, &alice, &manifest);

        assert_eq!(req.tenant_id, alice.to_string());
        assert_ne!(
            req.tenant_id,
            TenantId::consumer().to_string(),
            "handler must not fall back to the consumer singleton"
        );
    }

    /// Drive the handler with an `AgentDeployed` event for tenant Alice and
    /// assert the resulting Cortex `IndexAgentRequest` is tenant-scoped to
    /// Alice — never to the global consumer tenant.
    #[tokio::test]
    async fn handle_event_indexes_agent_under_event_tenant_id() {
        let cortex = Arc::new(FakeCortexClient::default());
        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        let handler = make_handler(cortex.clone(), agent_repo, workflow_repo);

        let alice =
            TenantId::for_consumer_user("alice").expect("valid per-user tenant id for alice");
        let manifest = make_manifest("alpha");
        let agent_id = AgentId::new();

        let event = DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentDeployed {
            agent_id,
            tenant_id: alice.clone(),
            manifest,
            deployed_at: Utc::now(),
        });
        handler.handle_event(event).await;

        let calls = cortex.index_agent_requests.lock().await;
        assert_eq!(calls.len(), 1, "exactly one IndexAgent call expected");
        assert_eq!(
            calls[0].tenant_id,
            alice.to_string(),
            "deployed agent must be indexed under the event's tenant_id, not consumer()"
        );
        assert_ne!(calls[0].tenant_id, TenantId::consumer().to_string());
    }

    /// Drive the handler with `WorkflowRegistered` for tenant Bob and assert
    /// the resulting Cortex `IndexWorkflowRequest` is tenant-scoped to Bob.
    #[tokio::test]
    async fn handle_event_indexes_workflow_under_event_tenant_id() {
        let cortex = Arc::new(FakeCortexClient::default());
        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());

        let bob = TenantId::for_consumer_user("bob").expect("valid per-user tenant id for bob");

        // Seed the workflow under Bob (handler must look it up under the
        // event's tenant, not the consumer singleton).
        let mut wf = make_workflow("wf-bob", "bob's workflow");
        wf.tenant_id = bob.clone();
        workflow_repo
            .save_for_tenant(&bob, &wf)
            .await
            .expect("save bob workflow");

        let handler = make_handler(cortex.clone(), agent_repo, workflow_repo);

        let event = DomainEvent::Workflow(WorkflowEvent::WorkflowRegistered {
            workflow_id: wf.id,
            tenant_id: bob.clone(),
            name: "wf-bob".to_string(),
            version: "1.0.0".to_string(),
            scope: crate::domain::workflow::WorkflowScope::default(),
            registered_at: Utc::now(),
        });
        handler.handle_event(event).await;

        let calls = cortex.index_workflow_requests.lock().await;
        assert_eq!(calls.len(), 1, "exactly one IndexWorkflow call expected");
        assert_eq!(
            calls[0].tenant_id,
            bob.to_string(),
            "workflow must be indexed under event's tenant"
        );
    }

    /// Drive the handler with `AgentRemoved` for an explicit tenant and
    /// assert the `RemoveDiscoveryAgentRequest` is tenant-scoped — not
    /// hardcoded to consumer.
    #[tokio::test]
    async fn handle_event_removes_agent_under_event_tenant_id() {
        let cortex = Arc::new(FakeCortexClient::default());
        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        let handler = make_handler(cortex.clone(), agent_repo, workflow_repo);

        let carol =
            TenantId::for_consumer_user("carol").expect("valid per-user tenant id for carol");
        let agent_id = AgentId::new();

        let event = DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentRemoved {
            agent_id,
            tenant_id: carol.clone(),
            removed_at: Utc::now(),
        });
        handler.handle_event(event).await;

        let calls = cortex.remove_agent_requests.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tenant_id, carol.to_string());
    }

    /// Drive `WorkflowRemoved` and assert the remove request carries the
    /// event's tenant.
    #[tokio::test]
    async fn handle_event_removes_workflow_under_event_tenant_id() {
        let cortex = Arc::new(FakeCortexClient::default());
        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        let handler = make_handler(cortex.clone(), agent_repo, workflow_repo);

        let dave = TenantId::for_consumer_user("dave").expect("valid per-user tenant id for dave");
        let workflow_id = WorkflowId::new();

        let event = DomainEvent::Workflow(WorkflowEvent::WorkflowRemoved {
            workflow_id,
            tenant_id: dave.clone(),
            workflow_name: "wf-dave".to_string(),
            removed_at: Utc::now(),
        });
        handler.handle_event(event).await;

        let calls = cortex.remove_workflow_requests.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tenant_id, dave.to_string());
    }

    /// Backfill must index every agent under its OWN owning tenant — agents
    /// from different per-user tenants must NOT collapse into the consumer
    /// singleton.
    #[tokio::test]
    async fn backfill_indexes_each_agent_under_its_own_tenant() {
        let cortex = Arc::new(FakeCortexClient::default());
        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());

        let alice =
            TenantId::for_consumer_user("alice").expect("valid per-user tenant id for alice");
        let bob = TenantId::for_consumer_user("bob").expect("valid per-user tenant id for bob");

        let mut alice_agent = make_agent("alpha");
        alice_agent.tenant_id = alice.clone();
        agent_repo
            .save_for_tenant(&alice, &alice_agent)
            .await
            .unwrap();

        let mut bob_agent = make_agent("beta");
        bob_agent.tenant_id = bob.clone();
        agent_repo.save_for_tenant(&bob, &bob_agent).await.unwrap();

        let handler = make_handler(cortex.clone(), agent_repo, workflow_repo);
        handler.backfill().await.expect("backfill ok");

        let calls = cortex.index_agent_requests.lock().await;
        assert_eq!(calls.len(), 2, "expected one IndexAgent per tenant");
        let tenants: std::collections::HashSet<&str> =
            calls.iter().map(|r| r.tenant_id.as_str()).collect();
        assert!(tenants.contains(alice.as_str()));
        assert!(tenants.contains(bob.as_str()));
        assert!(
            !tenants.contains(TenantId::consumer().as_str())
                || alice == TenantId::consumer()
                || bob == TenantId::consumer(),
            "no per-user agent should collapse onto the consumer singleton"
        );
    }
}
