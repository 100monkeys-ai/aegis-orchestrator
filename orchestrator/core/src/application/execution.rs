// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Execution Application Service (BC-2)
//!
//! Defines [`ExecutionService`] and provides [`StandardExecutionService`], the
//! production implementation of the 100monkeys iteration loop (ADR-005).
//!
//! ## Execution Lifecycle
//!
//! ```text
//! start_execution(agent_id, input)
//!   └─ resolve agent manifest (AgentLifecycleService)
//!   └─ ensure workspace volume exists (VolumeService)
//!   └─ register NFS volume context (NfsGatewayService, ADR-036)
//!   └─ Supervisor::run_loop
//!         └─ spawn container (AgentRuntime)
//!         └─ execute task (AgentRuntime)
//!         └─ evaluate output (ValidationService)
//!         └─ apply refinement or complete
//! ```
//!
//! All state transitions emit [`crate::domain::events::ExecutionEvent`]s onto the
//! [`crate::infrastructure::event_bus::EventBus`] via the `ExecutionMonitor` observer.
//!
//! ## NFS Gateway Integration
//!
//! # Code Quality Principles
//!
//! - Keep execution lifecycle logic in this service, not in transport or persistence adapters.
//! - Fail fast when agent, volume, or runtime prerequisites are missing.
//! - Preserve deterministic iteration transitions and explicit state changes.
//!
//! Attach an NFS gateway using [`StandardExecutionService::with_nfs_gateway`] so that
//! volume contexts are registered *before* the first container spawns. Without this,
//! NFS mounts in the container will fail with `ESTALE`.
//!
//! See Also: ADR-005 (Iterative Execution Strategy), ADR-036 (NFS Server Gateway)

use crate::application::agent::AgentLifecycleService;
use crate::application::nfs_gateway::{NfsGatewayService, VolumeRegistration};
use crate::application::ports::{
    CortexPatternPort, StoreTrajectoryPatternCommand, TrajectoryStepCommand,
};
use crate::application::validation_service::build_validation_pipeline;
use crate::application::volume_manager::VolumeService;
use crate::domain::agent::AgentId;
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{
    Execution, ExecutionError, ExecutionId, ExecutionInput, ExecutionStatus, Iteration,
};
use crate::domain::fsal::FsalAccessPolicy;
use crate::domain::iam::UserIdentity;
use crate::domain::node_config::resolve_env_value;
use crate::domain::repository::ExecutionRepository;
use crate::domain::runtime::RuntimeError;
use crate::domain::supervisor::{Supervisor, SupervisorObserver};
use crate::domain::volume::{
    AccessMode, FilerEndpoint, TenantId, VolumeId, VolumeMount, VolumeOwnership,
};
use crate::infrastructure::event_bus::{DomainEvent, EventBus, EventBusError};
use crate::infrastructure::prompt_template_engine::{PromptContext, PromptTemplateEngine};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use futures::Stream;
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Primary interface for running agents through the 100monkeys iteration loop (BC-2).
///
/// The concrete implementation is [`StandardExecutionService`]. Callers interact with
/// executions by ID; the service manages all state persistence and event emission.
///
/// # Invariants
///
/// - An execution runs up to `max_iterations` (default 10, per ADR-005) before failing.
/// - Only one iteration is active at a time per execution.
/// - Executions are persisted atomically; partial state is not visible to callers.
#[async_trait]
pub trait ExecutionService: Send + Sync {
    /// Start a new agent execution for `agent_id` with the given `input`.
    ///
    /// Spawns asynchronously; the returned [`ExecutionId`] can be polled via
    /// [`ExecutionService::get_execution`] or streamed via [`ExecutionService::stream_execution`].
    ///
    /// # Errors
    ///
    /// - Agent not found
    /// - Volume provisioning failure
    /// - Container spawn failure
    async fn start_execution(
        &self,
        agent_id: AgentId,
        input: ExecutionInput,
        security_context_name: String,
        identity: Option<&UserIdentity>,
    ) -> Result<ExecutionId>;

    /// Start an execution with a pre-assigned ID (used for cluster forwarding).
    /// The execution_id is imported from the originating node to preserve tracing correlation.
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        agent_id: AgentId,
        input: ExecutionInput,
        security_context_name: String,
        identity: Option<&UserIdentity>,
    ) -> Result<ExecutionId>;

    /// Start a new child execution spawned by a parent (e.g., a judge agent).
    ///
    /// Enforces `MAX_RECURSIVE_DEPTH` from the parent's [`crate::domain::execution::ExecutionHierarchy`].
    /// The child's execution ID is returned immediately; the supervisor loop runs
    /// asynchronously, same as [`ExecutionService::start_execution`].
    ///
    /// # Errors
    ///
    /// - Agent not found
    /// - Parent execution not found
    /// - Maximum recursive depth exceeded (ADR-039)
    async fn start_child_execution(
        &self,
        agent_id: AgentId,
        input: ExecutionInput,
        parent_execution_id: ExecutionId,
    ) -> Result<ExecutionId>;

    /// Retrieve the current state of an execution by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the execution does not exist.
    async fn get_execution(&self, id: ExecutionId) -> Result<Execution>;

    /// Retrieve the current state of an execution by tenant and ID.
    ///
    /// Default implementation falls back to [`ExecutionService::get_execution`]
    /// for callers that do not require tenant scoping.
    async fn get_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Execution> {
        self.get_execution(id).await
    }

    /// Retrieve an execution by ID without a tenant filter.
    ///
    /// Internal service-to-service use only. The caller holds a trusted orchestrator-
    /// provisioned ExecutionId and must use the returned execution's `tenant_id` field
    /// for all downstream tenant-scoped operations.
    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution>;

    /// Return all [`Iteration`]s for a given execution in order.
    ///
    /// # Errors
    ///
    /// Returns an error if the execution does not exist.
    async fn get_iterations(&self, exec_id: ExecutionId) -> Result<Vec<Iteration>>;

    /// Request cancellation of a running execution.
    ///
    /// The execution may not terminate immediately; monitor via
    /// [`ExecutionService::stream_execution`] for the `ExecutionCancelled` event.
    ///
    /// # Errors
    ///
    /// Returns an error if the execution does not exist or is already terminal.
    async fn cancel_execution(&self, id: ExecutionId) -> Result<()>;

    /// Open a live server-sent event stream of [`ExecutionEvent`]s for a single execution.
    ///
    /// The stream ends when the execution reaches a terminal state
    /// (`ExecutionCompleted`, `ExecutionFailed`, or `ExecutionCancelled`).
    async fn stream_execution(
        &self,
        id: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>>;

    /// Open a live stream of all [`DomainEvent`]s associated with a given agent.
    ///
    /// This includes events from all executions of the agent, not just a single one.
    /// Used by the Zaru client to power per-agent monitoring views.
    async fn stream_agent_events(
        &self,
        id: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>>;

    /// List executions, optionally filtered by `agent_id`, newest first, capped at `limit`.
    async fn list_executions(
        &self,
        agent_id: Option<AgentId>,
        limit: usize,
    ) -> Result<Vec<Execution>>;

    /// Permanently delete an execution record and all its iterations.
    ///
    /// # Errors
    ///
    /// Returns an error if the execution does not exist or is currently running.
    async fn delete_execution(&self, id: ExecutionId) -> Result<()>;

    /// Append an LLM interaction record to an existing iteration.
    ///
    /// Called by the Supervisor during execution to preserve the full prompt/response
    /// history for transparency and Cortex learning.
    async fn record_llm_interaction(
        &self,
        execution_id: ExecutionId,
        iteration: u8,
        interaction: crate::domain::execution::LlmInteraction,
    ) -> Result<()>;

    /// Store the recorded sequence of tool invocations for an iteration
    async fn store_iteration_trajectory(
        &self,
        execution_id: ExecutionId,
        iteration: u8,
        trajectory: Vec<crate::domain::execution::TrajectoryStep>,
    ) -> Result<()>;

    /// Record a policy-blocked tool invocation on the current iteration.
    ///
    /// Called from the tool invocation service whenever a `PolicyViolation` is
    /// raised so the validator can surface blocked tool names to the judge agent.
    ///
    /// The default implementation is a no-op; only `StandardExecutionService`
    /// persists the violation.
    async fn store_policy_violation(
        &self,
        _execution_id: ExecutionId,
        _tool_name: String,
    ) -> Result<()> {
        Ok(())
    }
}

pub struct StandardExecutionService {
    agent_service: Arc<dyn AgentLifecycleService>,
    volume_service: Arc<dyn VolumeService>,
    supervisor: Arc<Supervisor>,
    repository: Arc<dyn ExecutionRepository>,
    event_bus: Arc<EventBus>,
    config: Arc<crate::domain::node_config::NodeConfigManifest>,
    /// Optional NFS gateway for registering volume contexts before container spawn (ADR-036)
    nfs_gateway: Option<Arc<NfsGatewayService>>,
    /// StandardRuntime registry for validating language+version and resolving images (ADR-043).
    /// Required for StandardRuntime agents; executions will fail without it.
    runtime_registry: Option<Arc<crate::domain::runtime_registry::StandardRuntimeRegistry>>,
    /// Active cancellation tokens keyed by ExecutionId.
    /// Inserting a token on `start_execution` and calling `cancel()` on
    /// `cancel_execution` lets the Supervisor cooperatively terminate.
    cancellation_tokens: Arc<dashmap::DashMap<ExecutionId, CancellationToken>>,
    /// Self-reference used by judge validators to spawn child executions (ADR-016, ADR-039).
    /// Set once at composition root via `set_child_execution_service()`.
    child_executor: std::sync::OnceLock<Arc<dyn ExecutionService>>,
    /// Optional ToolRouter used to validate that an agent's requested tools actually exist
    /// before spawning the container (Safety & Polish).
    tool_router: Option<Arc<crate::infrastructure::tool_router::ToolRouter>>,
    /// Optional Cortex integration port to upload learned trajectories (ADR-049).
    cortex_client: Option<Arc<dyn CortexPatternPort>>,
    /// Optional rate limit enforcer for checking execution quotas (ADR-072).
    rate_limit_enforcer: Option<Arc<dyn crate::domain::rate_limit::RateLimitEnforcer>>,
    /// Optional rate limit policy resolver for resolving tier/tenant/user policies (ADR-072).
    rate_limit_resolver: Option<Arc<dyn crate::domain::rate_limit::RateLimitPolicyResolver>>,
    /// Optional swarm cancellation port for cascade-cancelling child swarms (BC-6).
    swarm_cancellation: Option<Arc<dyn crate::application::ports::SwarmCancellationPort>>,
    /// SEAL gateway client for pre-creating sessions before container spawn (ADR-088 §A8).
    seal_gateway_client: Option<Arc<dyn crate::application::ports::SealGatewayClient>>,
    /// Token issuer for minting JWTs during session pre-creation (ADR-088 §A8).
    token_issuer: Option<Arc<dyn crate::application::ports::SecurityTokenIssuerPort>>,
}

impl StandardExecutionService {
    const RESERVED_CONTEXT_OVERRIDE_KEYS: [&'static str; 3] =
        ["instruction", "iteration_number", "previous_error"];

    fn resolve_tenant_from_payload(payload: &serde_json::Value) -> Result<TenantId> {
        let tenant = payload
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("tenant").and_then(|v| v.as_str()))
            .unwrap_or(crate::domain::tenant::CONSUMER_SLUG);
        TenantId::from_string(tenant).map_err(|e| anyhow!("Invalid tenant_id '{tenant}': {e}"))
    }

    fn resolve_tenant_from_input(input: &ExecutionInput) -> Result<TenantId> {
        Self::resolve_tenant_from_payload(&input.input)
    }

    fn is_workspace_mount(path: &str) -> bool {
        path == "/workspace" || path.starts_with("/workspace/")
    }

    fn build_borrowed_mount(
        tenant_id: &TenantId,
        alias_volume_id: VolumeId,
        mount_point: PathBuf,
        filer_url: &str,
    ) -> VolumeMount {
        VolumeMount::new(
            alias_volume_id,
            mount_point,
            AccessMode::ReadOnly,
            FilerEndpoint::new(filer_url).expect("valid fallback filer endpoint"),
            format!("/aegis/volumes/{tenant_id}/{alias_volume_id}"),
        )
    }

    async fn build_judge_inherited_mounts(
        &self,
        tenant_id: &TenantId,
        parent_execution_id: ExecutionId,
        child_execution_id: ExecutionId,
    ) -> Result<Vec<VolumeMount>> {
        // Look up actually-provisioned volumes for the parent execution (not the
        // agent manifest declarations, which may be absent for dynamically created
        // volumes).
        let parent_volumes = self
            .volume_service
            .list_volumes_by_ownership(&VolumeOwnership::execution(parent_execution_id))
            .await?;
        if parent_volumes.is_empty() {
            return Ok(Vec::new());
        }

        let gateway = self.nfs_gateway.as_ref().ok_or_else(|| {
            anyhow!(
                "Judge execution {} requires inherited worker volumes, but the NFS gateway is not configured",
                child_execution_id
            )
        })?;

        let parent_mount_contexts = gateway
            .volume_registry()
            .find_all_by_execution(parent_execution_id);
        if parent_mount_contexts.is_empty() {
            return Err(anyhow!(
                "Judge execution {} requires inherited worker volumes, but parent execution {} has no registered NFS mount contexts",
                child_execution_id,
                parent_execution_id
            ));
        }

        let mount_contexts_by_volume_id = parent_mount_contexts
            .into_iter()
            .map(|ctx| (ctx.volume_id, ctx))
            .collect::<std::collections::HashMap<_, _>>();

        let mut borrowed = Vec::with_capacity(parent_volumes.len());
        for source_volume in &parent_volumes {
            let parent_ctx = match mount_contexts_by_volume_id.get(&source_volume.id) {
                Some(ctx) => ctx,
                None => {
                    tracing::warn!(
                        "Skipping inherited volume '{}' ({}): no NFS mount context registered for parent execution {}",
                        source_volume.name,
                        source_volume.id,
                        parent_execution_id
                    );
                    continue;
                }
            };

            let mount_path = parent_ctx.mount_point.to_string_lossy().to_string();
            if !Self::is_workspace_mount(&mount_path) {
                return Err(anyhow!(
                    "Invalid inherited mount path '{}': all mounts must be /workspace or /workspace/*",
                    mount_path
                ));
            }

            borrowed.push((
                VolumeId::new(),
                source_volume.clone(),
                parent_ctx.mount_point.clone(),
            ));
        }

        if borrowed.is_empty() {
            return Ok(Vec::new());
        }

        let read_policy = FsalAccessPolicy {
            read: vec!["/*".to_string()],
            write: vec![],
        };
        let mut mounts = Vec::with_capacity(borrowed.len());
        let filer_url = self
            .config
            .spec
            .storage
            .as_ref()
            .and_then(|s| s.seaweedfs.as_ref())
            .map(|sf| sf.filer_url.clone())
            .unwrap_or_else(|| "http://localhost:8888".to_string());
        for (alias_volume_id, source_volume, mount_point) in borrowed {
            gateway.register_borrowed_volume(alias_volume_id, child_execution_id, source_volume);
            let mount =
                Self::build_borrowed_mount(tenant_id, alias_volume_id, mount_point, &filer_url);
            gateway.register_volume(VolumeRegistration {
                volume_id: alias_volume_id,
                execution_id: child_execution_id,
                container_uid: 1000,
                container_gid: 1000,
                policy: read_policy.clone(),
                mount_point: mount.mount_point.clone(),
                remote_path: mount.remote_path.clone(),
            });
            mounts.push(mount);
        }

        tracing::info!(
            "ADR-049: injecting {} read-only inherited worker volume(s) into judge execution {}",
            mounts.len(),
            child_execution_id
        );

        Ok(mounts)
    }

    pub async fn get_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Execution> {
        self.repository
            .find_by_id_for_tenant(tenant_id, id)
            .await?
            .ok_or_else(|| anyhow!("Execution not found"))
    }

    pub async fn get_iterations_for_tenant(
        &self,
        tenant_id: &TenantId,
        exec_id: ExecutionId,
    ) -> Result<Vec<Iteration>> {
        let execution = self.get_execution_for_tenant(tenant_id, exec_id).await?;
        Ok(execution.iterations().to_vec())
    }

    pub async fn cancel_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<()> {
        if let Some(token) = self.cancellation_tokens.get(&id) {
            token.cancel();
        }

        let mut execution = self.get_execution_for_tenant(tenant_id, id).await?;
        execution.status = ExecutionStatus::Cancelled;
        execution.ended_at = Some(Utc::now());
        self.repository
            .save_for_tenant(tenant_id, &execution)
            .await?;

        self.event_bus
            .publish_execution_event(ExecutionEvent::ExecutionCancelled {
                execution_id: id,
                agent_id: execution.agent_id,
                reason: None,
                cancelled_at: Utc::now(),
            });

        // Cascade cancellation to any child swarm associated with this execution (BC-6).
        if let Some(ref port) = self.swarm_cancellation {
            if let Err(e) = port.cascade_cancel_for_execution(id).await {
                tracing::warn!(
                    "swarm cascade cancellation failed for execution {:?}: {}",
                    id,
                    e
                );
            }
        }

        self.cancellation_tokens.remove(&id);

        Ok(())
    }

    pub async fn list_executions_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: Option<AgentId>,
        workflow_id: Option<crate::domain::workflow::WorkflowId>,
        limit: usize,
    ) -> Result<Vec<Execution>> {
        if let Some(wid) = workflow_id {
            Ok(self
                .repository
                .find_by_workflow_for_tenant(tenant_id, wid, limit)
                .await?)
        } else if let Some(aid) = agent_id {
            Ok(self
                .repository
                .find_by_agent_for_tenant(tenant_id, aid, limit)
                .await?)
        } else {
            Ok(self
                .repository
                .find_recent_for_tenant(tenant_id, limit)
                .await?)
        }
    }

    pub async fn delete_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<()> {
        self.repository.delete_for_tenant(tenant_id, id).await?;
        Ok(())
    }

    pub fn new(
        agent_service: Arc<dyn AgentLifecycleService>,
        volume_service: Arc<dyn VolumeService>,
        supervisor: Arc<Supervisor>,
        repository: Arc<dyn ExecutionRepository>,
        event_bus: Arc<EventBus>,
        config: Arc<crate::domain::node_config::NodeConfigManifest>,
    ) -> Self {
        Self {
            agent_service,
            volume_service,
            supervisor,
            repository,
            event_bus,
            config,
            nfs_gateway: None,
            runtime_registry: None,
            cancellation_tokens: Arc::new(dashmap::DashMap::new()),
            child_executor: std::sync::OnceLock::new(),
            tool_router: None,
            cortex_client: None,
            rate_limit_enforcer: None,
            rate_limit_resolver: None,
            swarm_cancellation: None,
            seal_gateway_client: None,
            token_issuer: None,
        }
    }

    /// Attach an NFS gateway so volume contexts are registered before agent containers spawn
    pub fn with_nfs_gateway(mut self, gateway: Arc<NfsGatewayService>) -> Self {
        self.nfs_gateway = Some(gateway);
        self
    }

    /// Set the self-referential child execution service used by judge validators (ADR-016).
    ///
    /// Must be called from the composition root immediately after constructing the
    /// `Arc<StandardExecutionService>`, before any executions are started:
    ///
    /// ```rust,ignore
    /// let svc = Arc::new(StandardExecutionService::new(...));
    /// svc.set_child_execution_service(svc.clone());
    /// ```
    pub fn set_child_execution_service(&self, svc: Arc<dyn ExecutionService>) {
        // OnceLock silently ignores a second set; the service is wired once at startup.
        let _ = self.child_executor.set(svc);
    }

    /// Attach a StandardRuntime registry for validated language+version → image resolution (ADR-043).
    /// Required when running StandardRuntime agents. Executions that use `language`+`version`
    /// without a configured registry will immediately fail with a clear error.
    pub fn with_runtime_registry(
        mut self,
        registry: Arc<crate::domain::runtime_registry::StandardRuntimeRegistry>,
    ) -> Self {
        self.runtime_registry = Some(registry);
        self
    }

    /// Attach a ToolRouter to validate that an agent's requested tools actually exist
    /// before spawning the container.
    pub fn with_tool_router(
        mut self,
        tool_router: Arc<crate::infrastructure::tool_router::ToolRouter>,
    ) -> Self {
        self.tool_router = Some(tool_router);
        self
    }

    /// Attach a Cortex port for Trajectory Consolidation (ADR-049).
    pub fn with_cortex_client(mut self, cortex_client: Arc<dyn CortexPatternPort>) -> Self {
        self.cortex_client = Some(cortex_client);
        self
    }

    /// Attach rate limiting enforcement for agent execution quotas (ADR-072).
    pub fn with_rate_limiting(
        mut self,
        enforcer: Arc<dyn crate::domain::rate_limit::RateLimitEnforcer>,
        resolver: Arc<dyn crate::domain::rate_limit::RateLimitPolicyResolver>,
    ) -> Self {
        self.rate_limit_enforcer = Some(enforcer);
        self.rate_limit_resolver = Some(resolver);
        self
    }

    /// Attach swarm cascade cancellation port (BC-6).
    pub fn with_swarm_cancellation(
        mut self,
        port: Arc<dyn crate::application::ports::SwarmCancellationPort>,
    ) -> Self {
        self.swarm_cancellation = Some(port);
        self
    }

    /// Attach SEAL gateway client and token issuer for session pre-creation (ADR-088 §A8).
    ///
    /// When both are provided, `do_start_execution()` generates an ephemeral Ed25519
    /// keypair, mints a JWT, POSTs the session to the SEAL gateway, and injects
    /// `AEGIS_SEAL_PRIVATE_KEY` + `AEGIS_SEAL_TOKEN` into the container environment.
    pub fn with_seal_session_precreation(
        mut self,
        gateway_client: Arc<dyn crate::application::ports::SealGatewayClient>,
        token_issuer: Arc<dyn crate::application::ports::SecurityTokenIssuerPort>,
    ) -> Self {
        self.seal_gateway_client = Some(gateway_client);
        self.token_issuer = Some(token_issuer);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::nfs_gateway::EventBusPublisher;
    use crate::domain::agent::{
        Agent, AgentManifest, AgentSpec, ImagePullPolicy, ManifestMetadata, RuntimeConfig,
        TaskConfig, VolumeSpec,
    };
    use crate::domain::execution::ExecutionInput;
    use crate::domain::repository::{AgentRepository, ExecutionRepository, VolumeRepository};
    use crate::domain::runtime::{
        AgentRuntime, InstanceId, InstanceStatus, RuntimeConfig as WorkerRuntimeConfig, TaskInput,
        TaskOutput,
    };
    use crate::domain::storage::StorageProvider;
    use crate::domain::tenant::TenantId as CoreTenantId;
    use crate::domain::volume::{StorageClass, Volume, VolumeBackend};
    use crate::infrastructure::event_bus::EventBus;
    use crate::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
    };
    use crate::infrastructure::storage::LocalHostStorageProvider;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct TestRuntime {
        spawned: Mutex<Vec<WorkerRuntimeConfig>>,
        executed_inputs: Mutex<Vec<TaskInput>>,
    }

    #[async_trait]
    impl AgentRuntime for TestRuntime {
        async fn spawn(&self, config: WorkerRuntimeConfig) -> Result<InstanceId, RuntimeError> {
            self.spawned.lock().unwrap().push(config);
            Ok(InstanceId::new("test-instance"))
        }

        async fn execute(
            &self,
            _id: &InstanceId,
            input: TaskInput,
        ) -> Result<TaskOutput, RuntimeError> {
            self.executed_inputs.lock().unwrap().push(input);
            Ok(TaskOutput {
                result: serde_json::Value::String("ok".to_string()),
                logs: Vec::new(),
                tool_calls: Vec::new(),
                exit_code: 0,
                trajectory: vec![],
            })
        }

        async fn terminate(&self, _id: &InstanceId) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn status(&self, _id: &InstanceId) -> Result<InstanceStatus, RuntimeError> {
            Ok(InstanceStatus {
                id: InstanceId::new("test-instance"),
                state: "running".to_string(),
                uptime_seconds: 0,
                memory_usage_mb: 0,
                cpu_usage_percent: 0.0,
            })
        }
    }

    #[derive(Clone)]
    struct TestVolumeService {
        volumes: HashMap<VolumeId, Volume>,
    }

    #[async_trait]
    impl VolumeService for TestVolumeService {
        async fn create_volume(
            &self,
            _name: String,
            _tenant_id: TenantId,
            _storage_class: StorageClass,
            _size_limit_mb: u64,
            _ownership: VolumeOwnership,
        ) -> Result<VolumeId> {
            anyhow::bail!("not used in test")
        }

        async fn get_volume(&self, id: VolumeId) -> Result<Volume> {
            self.volumes
                .get(&id)
                .cloned()
                .ok_or_else(|| anyhow!("volume {id} not found"))
        }

        async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> Result<Vec<Volume>> {
            Ok(self
                .volumes
                .values()
                .filter(|volume| volume.tenant_id == tenant_id)
                .cloned()
                .collect())
        }

        async fn list_volumes_by_ownership(
            &self,
            ownership: &VolumeOwnership,
        ) -> Result<Vec<Volume>> {
            Ok(self
                .volumes
                .values()
                .filter(|volume| &volume.ownership == ownership)
                .cloned()
                .collect())
        }

        async fn attach_volume(
            &self,
            _volume_id: VolumeId,
            _instance_id: crate::domain::runtime::InstanceId,
            _mount_point: PathBuf,
            _access_mode: AccessMode,
        ) -> Result<VolumeMount> {
            anyhow::bail!("not used in test")
        }

        async fn detach_volume(
            &self,
            _volume_id: VolumeId,
            _instance_id: crate::domain::runtime::InstanceId,
        ) -> Result<()> {
            anyhow::bail!("not used in test")
        }

        async fn delete_volume(&self, _volume_id: VolumeId) -> Result<()> {
            anyhow::bail!("not used in test")
        }

        async fn get_volume_usage(&self, _volume_id: VolumeId) -> Result<u64> {
            anyhow::bail!("not used in test")
        }

        async fn cleanup_expired_volumes(&self) -> Result<usize> {
            anyhow::bail!("not used in test")
        }

        async fn create_volumes_for_execution(
            &self,
            _execution_id: ExecutionId,
            _tenant_id: TenantId,
            _volume_specs: &[VolumeSpec],
            _storage_mode: &str,
        ) -> Result<Vec<Volume>> {
            anyhow::bail!("not used in test")
        }
    }

    fn make_agent(name: &str, role: Option<&str>, mount_path: Option<&str>) -> Agent {
        let mut labels = HashMap::new();
        if let Some(role) = role {
            labels.insert("role".to_string(), role.to_string());
        }

        Agent::new(AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: ManifestMetadata {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                description: Some(format!("{name} description")),
                labels,
                annotations: HashMap::new(),
            },
            spec: AgentSpec {
                runtime: RuntimeConfig {
                    language: None,
                    version: None,
                    image: Some(format!("ghcr.io/example/{name}:latest")),
                    image_pull_policy: ImagePullPolicy::IfNotPresent,
                    isolation: "docker".to_string(),
                    model: "default".to_string(),
                },
                task: Some(TaskConfig {
                    instruction: Some(format!("Run the {name} task")),
                    prompt_template: None,
                    input_data: None,
                }),
                context: Vec::new(),
                execution: None,
                security: None,
                schedule: None,
                tools: Vec::new(),
                env: HashMap::new(),
                volumes: mount_path
                    .map(|path| {
                        vec![VolumeSpec {
                            name: "workspace".to_string(),
                            storage_class: "persistent".to_string(),
                            volume_type: "hostPath".to_string(),
                            provider: None,
                            config: None,
                            mount_path: path.to_string(),
                            access_mode: "read-write".to_string(),
                            size_limit: "1Gi".to_string(),
                            ttl_hours: None,
                        }]
                    })
                    .unwrap_or_default(),
                advanced: None,
                input_schema: None,
                security_context: None,
            },
        })
    }

    fn make_parent_execution(agent_id: AgentId) -> Execution {
        Execution::new(
            agent_id,
            ExecutionInput {
                intent: Some("parent".to_string()),
                input: serde_json::json!({ "tenant_id": "zaru-consumer" }),
            },
            1,
            "aegis-system-operator".to_string(),
        )
    }

    async fn wait_for_spawn(runtime: &TestRuntime) -> WorkerRuntimeConfig {
        for _ in 0..20 {
            if let Some(config) = runtime.spawned.lock().unwrap().last().cloned() {
                return config;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("runtime spawn was not observed");
    }

    #[tokio::test]
    async fn judge_inherits_parent_explicit_volumes_as_read_only_mounts() {
        let tenant_id = CoreTenantId::local_default();
        let parent_agent = make_agent("worker", None, Some("/workspace/project"));
        let judge_agent = make_agent("judge", Some("judge"), None);
        let parent_execution = make_parent_execution(parent_agent.id);

        let storage_root = tempfile::tempdir().unwrap();
        let storage_provider: Arc<dyn StorageProvider> =
            Arc::new(LocalHostStorageProvider::new(storage_root.path()).unwrap());
        let volume_repository: Arc<dyn VolumeRepository> =
            Arc::new(InMemoryVolumeRepository::new());
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let nfs_gateway = Arc::new(NfsGatewayService::new(
            storage_provider.clone(),
            volume_repository,
            Arc::new(EventBusPublisher::new(event_bus.clone())),
            Some(0),
        ));

        let mut parent_volume = Volume::new(
            "workspace".to_string(),
            tenant_id.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: PathBuf::from("/tmp/worker-volume"),
            },
            1024 * 1024,
            VolumeOwnership::execution(parent_execution.id),
        )
        .unwrap();
        parent_volume.mark_available().unwrap();

        nfs_gateway.register_volume(VolumeRegistration {
            volume_id: parent_volume.id,
            execution_id: parent_execution.id,
            container_uid: 1000,
            container_gid: 1000,
            policy: FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: vec!["/*".to_string()],
            },
            mount_point: PathBuf::from("/workspace/project"),
            remote_path: String::new(),
        });

        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        agent_repo
            .save_for_tenant(&tenant_id, &parent_agent)
            .await
            .unwrap();
        agent_repo
            .save_for_tenant(&tenant_id, &judge_agent)
            .await
            .unwrap();

        let execution_repo: Arc<dyn ExecutionRepository> =
            Arc::new(InMemoryExecutionRepository::new());
        execution_repo
            .save_for_tenant(&tenant_id, &parent_execution)
            .await
            .unwrap();

        let runtime = Arc::new(TestRuntime::default());
        let supervisor = Arc::new(Supervisor::new(runtime.clone()));
        let volume_service = Arc::new(TestVolumeService {
            volumes: HashMap::from([(parent_volume.id, parent_volume.clone())]),
        });

        let service = StandardExecutionService::new(
            agent_repo,
            volume_service,
            supervisor,
            execution_repo,
            event_bus,
            Arc::new(crate::domain::node_config::NodeConfigManifest::default()),
        )
        .with_nfs_gateway(nfs_gateway.clone());

        let child_execution_id = service
            .start_child_execution(
                judge_agent.id,
                ExecutionInput {
                    intent: Some("judge".to_string()),
                    input: serde_json::json!({ "tenant_id": "zaru-consumer" }),
                },
                parent_execution.id,
            )
            .await
            .unwrap();

        let spawned = wait_for_spawn(runtime.as_ref()).await;
        assert_eq!(spawned.volumes.len(), 1);
        assert_eq!(
            spawned.volumes[0].mount_point,
            PathBuf::from("/workspace/project")
        );
        assert_eq!(spawned.volumes[0].access_mode, AccessMode::ReadOnly);
        assert!(
            !spawned.volumes[0].remote_path.starts_with("/aegis/seal/"),
            "judge should no longer mount via the SEAL proxy path"
        );

        let child_mounts = nfs_gateway
            .volume_registry()
            .find_all_by_execution(child_execution_id);
        assert_eq!(child_mounts.len(), 1);
        assert_eq!(
            child_mounts[0].mount_point,
            PathBuf::from("/workspace/project")
        );
        assert_ne!(child_mounts[0].volume_id, parent_volume.id);
    }

    #[tokio::test]
    async fn judge_fails_fast_when_parent_has_provisioned_volumes_but_no_nfs_gateway() {
        let tenant_id = CoreTenantId::local_default();
        let parent_agent = make_agent("worker", None, Some("/workspace/project"));
        let judge_agent = make_agent("judge", Some("judge"), None);
        let parent_execution = make_parent_execution(parent_agent.id);

        // Create a provisioned volume owned by the parent execution so the
        // new provisioned-volume-driven path does NOT exit early.
        let mut parent_volume = Volume::new(
            "workspace".to_string(),
            tenant_id.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: PathBuf::from("/tmp/worker-volume"),
            },
            1024 * 1024,
            VolumeOwnership::execution(parent_execution.id),
        )
        .unwrap();
        parent_volume.mark_available().unwrap();

        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        agent_repo
            .save_for_tenant(&tenant_id, &parent_agent)
            .await
            .unwrap();
        agent_repo
            .save_for_tenant(&tenant_id, &judge_agent)
            .await
            .unwrap();

        let execution_repo = Arc::new(InMemoryExecutionRepository::new());
        execution_repo
            .save_for_tenant(&tenant_id, &parent_execution)
            .await
            .unwrap();

        let runtime = Arc::new(TestRuntime::default());
        let supervisor = Arc::new(Supervisor::new(runtime));
        // No NFS gateway configured, but provisioned volumes exist →
        // should fail with "NFS gateway is not configured".
        let service = StandardExecutionService::new(
            agent_repo,
            Arc::new(TestVolumeService {
                volumes: HashMap::from([(parent_volume.id, parent_volume)]),
            }),
            supervisor,
            execution_repo.clone(),
            Arc::new(EventBus::with_default_capacity()),
            Arc::new(crate::domain::node_config::NodeConfigManifest::default()),
        );

        let err = service
            .start_child_execution(
                judge_agent.id,
                ExecutionInput {
                    intent: Some("judge".to_string()),
                    input: serde_json::json!({ "tenant_id": "zaru-consumer" }),
                },
                parent_execution.id,
            )
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("NFS gateway is not configured"),
            "expected NFS gateway error, got: {err}"
        );
        let executions = execution_repo
            .find_recent_for_tenant(&tenant_id, 10)
            .await
            .unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].id, parent_execution.id);
    }

    #[test]
    fn extract_context_overrides_rejects_reserved_keys() {
        let err = StandardExecutionService::extract_context_overrides(&serde_json::json!({
            "context_overrides": {
                "instruction": "nope"
            }
        }))
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("Context override key 'instruction' is reserved"));
    }

    #[test]
    fn extract_context_overrides_accepts_object_values() {
        let overrides = StandardExecutionService::extract_context_overrides(&serde_json::json!({
            "context_overrides": {
                "repo": "aegis",
                "metadata": {
                    "owner": "100monkeys"
                }
            }
        }))
        .unwrap();

        assert_eq!(overrides.get("repo"), Some(&serde_json::json!("aegis")));
        assert_eq!(
            overrides.get("metadata"),
            Some(&serde_json::json!({ "owner": "100monkeys" }))
        );
    }

    #[test]
    fn resolve_tenant_from_payload_extracts_tenant_id() {
        let payload = serde_json::json!({ "tenant_id": "acme-corp" });
        let tenant = StandardExecutionService::resolve_tenant_from_payload(&payload).unwrap();
        assert_eq!(tenant.as_str(), "acme-corp");
    }

    #[test]
    fn resolve_tenant_from_payload_defaults_to_consumer_when_missing() {
        let payload = serde_json::json!({ "task": "run" });
        let tenant = StandardExecutionService::resolve_tenant_from_payload(&payload).unwrap();
        assert_eq!(tenant, CoreTenantId::consumer());
    }

    #[tokio::test]
    async fn cross_tenant_child_spawn_is_rejected() {
        let tenant_id = CoreTenantId::consumer();
        let parent_agent = make_agent("parent-worker", None, None);
        let child_agent = make_agent("child-worker", None, None);

        // Parent execution belongs to zaru-consumer
        let parent_execution = make_parent_execution(parent_agent.id);

        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        agent_repo
            .save_for_tenant(&tenant_id, &parent_agent)
            .await
            .unwrap();
        agent_repo
            .save_for_tenant(&tenant_id, &child_agent)
            .await
            .unwrap();

        let execution_repo: Arc<dyn ExecutionRepository> =
            Arc::new(InMemoryExecutionRepository::new());
        execution_repo
            .save_for_tenant(&tenant_id, &parent_execution)
            .await
            .unwrap();

        let runtime = Arc::new(TestRuntime::default());
        let supervisor = Arc::new(Supervisor::new(runtime.clone()));
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let volume_service = Arc::new(TestVolumeService {
            volumes: HashMap::new(),
        });

        let service = StandardExecutionService::new(
            agent_repo,
            volume_service,
            supervisor,
            execution_repo,
            event_bus,
            Arc::new(crate::domain::node_config::NodeConfigManifest::default()),
        );

        // Attempt child spawn with a different tenant
        let err = service
            .start_child_execution(
                child_agent.id,
                ExecutionInput {
                    intent: Some("child-task".to_string()),
                    input: serde_json::json!({ "tenant_id": "other-tenant" }),
                },
                parent_execution.id,
            )
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("Cross-tenant spawn forbidden"),
            "expected cross-tenant error, got: {err}"
        );
    }
}

struct ExecutionMonitor {
    execution_id: ExecutionId,
    agent_id: AgentId,
    repository: Arc<dyn ExecutionRepository>,
    event_bus: Arc<EventBus>,
}

#[async_trait]
impl SupervisorObserver for ExecutionMonitor {
    async fn on_iteration_start(&self, iteration: u8, action: &str) {
        let now = Utc::now();

        metrics::counter!("aegis_execution_iterations_total").increment(1);

        // Update DB
        if let Ok(Some(mut exec)) = self.repository.find_by_id(self.execution_id).await {
            {
                let _ = exec.start_iteration(action.to_string());
            }
            let _ = self.repository.save(&exec).await;
        }
        // Emit Event
        self.event_bus
            .publish_execution_event(ExecutionEvent::IterationStarted {
                execution_id: self.execution_id,
                agent_id: self.agent_id,
                iteration_number: iteration,
                action: action.to_string(),
                started_at: now,
            });
    }

    async fn on_console_output(&self, iteration: u8, stream: &str, content: &str) {
        let now = Utc::now();
        // Streams live to user but doesn't persist (stored in validation_results instead)
        self.event_bus
            .publish_execution_event(ExecutionEvent::ConsoleOutput {
                execution_id: self.execution_id,
                agent_id: self.agent_id,
                iteration_number: iteration,
                stream: stream.to_string(),
                content: content.to_string(),
                timestamp: now,
            });
    }

    async fn on_iteration_complete(&self, iteration: u8, output: &str, _exit_code: i64) {
        let now = Utc::now();
        if let Ok(Some(mut exec)) = self.repository.find_by_id(self.execution_id).await {
            exec.complete_iteration(output.to_string());
            let _ = self.repository.save(&exec).await;
        }
        self.event_bus
            .publish_execution_event(ExecutionEvent::IterationCompleted {
                execution_id: self.execution_id,
                agent_id: self.agent_id,
                iteration_number: iteration,
                output: output.to_string(),
                completed_at: now,
            });
    }

    async fn on_iteration_fail(&self, iteration: u8, error: &str) {
        let now = Utc::now();
        // Map string error to IterationError
        let iter_error = crate::domain::execution::IterationError {
            message: error.to_string(),
            details: None,
        };

        if let Ok(Some(mut exec)) = self.repository.find_by_id(self.execution_id).await {
            exec.fail_iteration(iter_error.clone());
            let _ = self.repository.save(&exec).await;
        }

        self.event_bus
            .publish_execution_event(ExecutionEvent::IterationFailed {
                execution_id: self.execution_id,
                agent_id: self.agent_id,
                iteration_number: iteration,
                error: iter_error,
                failed_at: now,
            });

        // Emit RefinementApplied to signal that a retry/refinement cycle is about to begin.
        // code_diff is empty until the orchestrator tracks per-iteration diffs; Cortex fields
        // will be populated once the Cortex gRPC interface returns pattern metadata.
        self.event_bus
            .publish_execution_event(ExecutionEvent::RefinementApplied {
                execution_id: self.execution_id,
                agent_id: self.agent_id,
                iteration_number: iteration,
                code_diff: crate::domain::execution::CodeDiff {
                    file_path: String::new(),
                    diff: String::new(),
                },
                applied_at: now,
                cortex_pattern_id: None,
                cortex_pattern_category: None,
                cortex_success_score: None,
                cortex_solution_approach: None,
            });
    }

    async fn on_instance_spawned(
        &self,
        iteration: u8,
        instance_id: &crate::domain::runtime::InstanceId,
    ) {
        let now = Utc::now();
        self.event_bus
            .publish_execution_event(ExecutionEvent::InstanceSpawned {
                execution_id: self.execution_id,
                agent_id: self.agent_id,
                iteration_number: iteration,
                instance_id: instance_id.clone(),
                spawned_at: now,
            });
    }

    async fn on_instance_terminated(
        &self,
        iteration: u8,
        instance_id: &crate::domain::runtime::InstanceId,
    ) {
        let now = Utc::now();
        self.event_bus
            .publish_execution_event(ExecutionEvent::InstanceTerminated {
                execution_id: self.execution_id,
                agent_id: self.agent_id,
                iteration_number: iteration,
                instance_id: instance_id.clone(),
                terminated_at: now,
            });
    }

    async fn on_validation_complete(
        &self,
        iteration: u8,
        results: &crate::domain::validation::ValidationResults,
        passed: bool,
    ) {
        // Derive score + confidence from the pipeline result for the event payload.
        // Prefer the gradient field (last validator's raw score) for richness;
        // fall back to a binary 1.0/0.0 based on `passed`.
        let (score, confidence) = match &results.gradient {
            Some(g) => (g.score, g.confidence),
            None => (if passed { 1.0 } else { 0.0 }, 1.0),
        };

        self.event_bus
            .publish_execution_event(ExecutionEvent::Validation(
                crate::domain::events::ValidationEvent::GradientValidationPerformed {
                    execution_id: self.execution_id,
                    agent_id: self.agent_id,
                    iteration_number: iteration,
                    score,
                    confidence,
                    validated_at: Utc::now(),
                },
            ));

        // Persist validation results so Iteration.validation_results is populated.
        if let Ok(Some(mut exec)) = self.repository.find_by_id(self.execution_id).await {
            if let Err(e) = exec.store_validation_results(iteration, results.clone()) {
                tracing::warn!(
                    "Failed to store validation results for execution {} iteration {}: {}",
                    self.execution_id,
                    iteration,
                    e
                );
            } else {
                let _ = self.repository.save(&exec).await;
            }
        }
    }
}

impl StandardExecutionService {
    /// Extract structured user input from `ExecutionInput.input` (ADR-092).
    ///
    /// Returns a `serde_json::Value` preserving the original structure so that
    /// Handlebars can resolve `{{input}}` (string) or `{{input.KEY}}`
    /// (dot-notation into objects) natively.
    ///
    /// Priority order:
    /// 1. Direct string → return as `Value::String`
    /// 2. Object with `workflow_input` key → return that value as-is
    /// 3. Object with `input` key → return that value as-is
    /// 4. Any other object → return the entire object as-is
    /// 5. Null → error
    /// 6. Other scalars → return as-is
    fn extract_user_input(input: &serde_json::Value) -> Result<serde_json::Value> {
        match input {
            // Direct string value — pass through
            serde_json::Value::String(_) => Ok(input.clone()),

            // Object — check for special keys in priority order
            serde_json::Value::Object(map) => {
                // Priority 1: workflow_input (from WorkflowEngine)
                if let Some(value) = map.get("workflow_input") {
                    if value.is_null() {
                        return Err(ExecutionError::InvalidExecutionInput(
                            "workflow_input value cannot be null".to_string(),
                        )
                        .into());
                    }
                    return Ok(value.clone());
                }

                // Priority 2: input (from CLI/direct calls)
                if let Some(value) = map.get("input") {
                    if value.is_null() {
                        return Err(ExecutionError::InvalidExecutionInput(
                            "Input value cannot be null".to_string(),
                        )
                        .into());
                    }
                    return Ok(value.clone());
                }

                // Fallback: return the entire object as-is (structured input)
                Ok(input.clone())
            }

            // Null or empty
            serde_json::Value::Null => Err(ExecutionError::InvalidExecutionInput(
                "Input is null or empty".to_string(),
            )
            .into()),

            // Other types (bool, number, array): return as-is
            _ => Ok(input.clone()),
        }
    }

    fn validate_context_override_keys(map: &JsonMap<String, JsonValue>) -> Result<()> {
        if let Some(key) = map.keys().find(|key| {
            Self::RESERVED_CONTEXT_OVERRIDE_KEYS
                .iter()
                .any(|reserved| reserved == key)
        }) {
            return Err(ExecutionError::InvalidExecutionInput(format!(
                "Context override key '{key}' is reserved and cannot be overridden"
            ))
            .into());
        }

        Ok(())
    }

    fn extract_context_overrides(
        payload: &serde_json::Value,
    ) -> Result<JsonMap<String, JsonValue>> {
        let serde_json::Value::Object(map) = payload else {
            return Ok(JsonMap::new());
        };

        let overrides = map.get("context_overrides").cloned();

        match overrides {
            None | Some(JsonValue::Null) => Ok(JsonMap::new()),
            Some(JsonValue::Object(map)) => {
                Self::validate_context_override_keys(&map)?;
                Ok(map)
            }
            Some(_) => Err(ExecutionError::InvalidExecutionInput(
                "Context overrides must be a JSON object".to_string(),
            )
            .into()),
        }
    }

    /// Prepare execution input by rendering the agent's prompt template (ADR-092).
    ///
    /// The prompt template receives three first-class variables:
    /// - `{{intent}}` — caller-supplied free-text (from `ExecutionInput.intent`)
    /// - `{{input}}` / `{{input.KEY}}` — structured user input
    /// - `{{instruction}}` — agent's task instruction from the manifest
    ///
    /// The template controls layout; the caller's `intent` is passed through
    /// unmodified (no hardcoded prepend logic).
    ///
    /// Three cases:
    /// - **Only `input`**: render template; store result in `intent`.
    /// - **Only `intent`**: use caller's free-text directly; skip rendering.
    /// - **Both**: render template with both `{{intent}}` and `{{input}}`
    ///   available; store result in `intent`.
    fn prepare_execution_input(
        &self,
        mut input: ExecutionInput,
        agent: &crate::domain::agent::Agent,
    ) -> Result<ExecutionInput> {
        let context_overrides = Self::extract_context_overrides(&input.input)?;

        // Attempt to extract structured user input.
        let user_input_result = Self::extract_user_input(&input.input);
        let has_input = user_input_result.is_ok();

        if has_input {
            const DEFAULT_PROMPT_TEMPLATE: &str = "{{instruction}}{{#if intent}}\n\nTask: {{intent}}{{/if}}\n\nUser: {{input}}\nAgent:";

            let task_spec = agent
                .manifest
                .spec
                .task
                .as_ref()
                .ok_or(ExecutionError::MissingPromptTemplate)?;

            let prompt_template = task_spec
                .prompt_template
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_PROMPT_TEMPLATE);

            let agent_instruction = task_spec
                .instruction
                .as_ref()
                .or(agent.manifest.metadata.description.as_ref())
                .map(|s| s.as_str())
                .unwrap_or("");

            let user_input = user_input_result?;

            // Build PromptContext with intent as a first-class template variable.
            // The template controls layout — no hardcoded prepend logic.
            let mut context = PromptContext::new()
                .instruction(agent_instruction)
                .input(user_input)
                .iteration_number(1);

            // Pass caller's intent into the template context so `{{intent}}` resolves.
            if let Some(ref caller_intent) = input.intent {
                context = context.intent(caller_intent.clone());
            }

            context.extras = context_overrides.clone().into_iter().collect();

            let template_engine = PromptTemplateEngine::new();
            let rendered_prompt = template_engine
                .render(prompt_template, &context)
                .map_err(|e| ExecutionError::PromptRenderFailed(e.to_string()))?;

            input.intent = Some(rendered_prompt);
        }
        // else: only intent was supplied — leave it unchanged; no template to render.

        if let serde_json::Value::Object(input_obj) = &mut input.input {
            input_obj.insert(
                "context_overrides".to_string(),
                JsonValue::Object(context_overrides),
            );
        }

        Ok(input)
    }
}

// Private execution lifecycle helpers for StandardExecutionService.
impl StandardExecutionService {
    /// Shared implementation for `start_execution` and `start_execution_with_id`.
    /// When `imported_id` is `Some`, the execution re-uses a pre-assigned identity
    /// (cluster forwarding). When `None`, a fresh ID is generated locally.
    async fn do_start_execution(
        &self,
        imported_id: Option<ExecutionId>,
        agent_id: AgentId,
        input: ExecutionInput,
        security_context_name: String,
        identity: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        let tenant_id = Self::resolve_tenant_from_input(&input)?;

        // 0. Rate limit check (ADR-072): enforce AgentExecution quota at tenant scope
        if let (Some(enforcer), Some(resolver)) =
            (&self.rate_limit_enforcer, &self.rate_limit_resolver)
        {
            use crate::domain::rate_limit::{RateLimitResourceType, RateLimitScope};

            let scope = RateLimitScope::Tenant {
                tenant_id: tenant_id.clone(),
            };

            // Build a default identity for tenant-scoped resolution (user identity not
            // available at this layer — tier defaults apply via the resolver).
            let default_identity = crate::domain::iam::UserIdentity {
                sub: "tenant-scope".to_string(),
                realm_slug: "aegis-system".to_string(),
                email: None,
                identity_kind: crate::domain::iam::IdentityKind::TenantUser {
                    tenant_slug: tenant_id.as_str().to_string(),
                },
            };

            let resource_type = RateLimitResourceType::AgentExecution;

            match resolver
                .resolve_policy(&default_identity, &tenant_id, &resource_type)
                .await
            {
                Ok(policy) => match enforcer.check_and_increment(&scope, &policy, 1).await {
                    Ok(decision) if !decision.allowed => {
                        let retry_hint = decision
                            .retry_after_seconds
                            .map(|s| format!(", retry after {s}s"))
                            .unwrap_or_default();
                        tracing::warn!(
                            tenant_id = %tenant_id.as_str(),
                            agent_id = %agent_id,
                            bucket = ?decision.exhausted_bucket,
                            "Rate limit exceeded for AgentExecution"
                        );
                        anyhow::bail!(
                            "Rate limit exceeded for agent execution (tenant: {}){retry_hint}",
                            tenant_id.as_str()
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            tenant_id = %tenant_id.as_str(),
                            error = %e,
                            "Rate limit enforcement error (allowing execution)"
                        );
                    }
                    Ok(_) => {} // allowed
                },
                Err(e) => {
                    tracing::warn!(
                        tenant_id = %tenant_id.as_str(),
                        error = %e,
                        "Rate limit policy resolution failed (allowing execution)"
                    );
                }
            }

            // ADR-072: dual-scope — enforce user-scoped rate limit when identity is available
            if let Some(id) = identity {
                let user_scope = RateLimitScope::User {
                    user_id: id.sub.clone(),
                };
                let resource_type = RateLimitResourceType::AgentExecution;

                match resolver
                    .resolve_policy(id, &tenant_id, &resource_type)
                    .await
                {
                    Ok(policy) => {
                        match enforcer.check_and_increment(&user_scope, &policy, 1).await {
                            Ok(decision) if !decision.allowed => {
                                let retry_hint = decision
                                    .retry_after_seconds
                                    .map(|s| format!(", retry after {s}s"))
                                    .unwrap_or_default();
                                tracing::warn!(
                                    user_id = %id.sub,
                                    tenant_id = %tenant_id.as_str(),
                                    agent_id = %agent_id,
                                    bucket = ?decision.exhausted_bucket,
                                    "User-scoped rate limit exceeded for AgentExecution"
                                );
                                anyhow::bail!(
                                    "Rate limit exceeded for agent execution (user: {}){retry_hint}",
                                    id.sub
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    user_id = %id.sub,
                                    error = %e,
                                    "User-scoped rate limit enforcement error (allowing execution)"
                                );
                            }
                            Ok(_) => {} // allowed
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            user_id = %id.sub,
                            error = %e,
                            "User-scoped rate limit policy resolution failed (allowing execution)"
                        );
                    }
                }
            }
        }

        // 1. Fetch Agent
        let agent = self
            .agent_service
            .get_agent_visible(&tenant_id, agent_id)
            .await?;

        // ADR-102: Use manifest-declared security_context if present; otherwise use caller's context.
        let security_context_name = agent
            .manifest
            .spec
            .security_context
            .as_deref()
            .map(|s| s.to_string())
            .unwrap_or(security_context_name);

        // 1.5 Validate that all tools requested by the agent exist in the ToolRouter index (Safety & Polish)
        if let Some(router) = &self.tool_router {
            let available_tools = router.list_tools().await.map_err(|e| {
                ExecutionError::InvalidExecutionInput(format!("Failed to query tool router: {e}"))
            })?;

            let requested_tools = agent.manifest.spec.tools.clone();
            for req_tool in requested_tools {
                if !available_tools.iter().any(|t| t.name == req_tool) {
                    return Err(ExecutionError::InvalidExecutionInput(format!(
                        "Agent requested tool '{req_tool}' but it is not available in the current node configuration."
                    )).into());
                }
            }
        }

        // 2. Prepare execution input (render prompt template if needed)
        let prepared_input = self.prepare_execution_input(input, &agent)?;

        // 3. Create Execution Record
        let max_retries = if let Some(exec) = &agent.manifest.spec.execution {
            exec.max_retries as u8
        } else {
            3 // Default
        };

        // Retain a copy before `security_context_name` is moved into the Execution (ADR-088 §A8).
        let seal_security_context = security_context_name.clone();

        let mut execution = match imported_id {
            Some(id) => Execution::new_with_id(
                id,
                agent_id,
                prepared_input.clone(),
                max_retries,
                security_context_name,
            ),
            None => Execution::new(
                agent_id,
                prepared_input.clone(),
                max_retries,
                security_context_name,
            ),
        };
        let execution_id = execution.id;

        // 3. Save initial state
        self.repository
            .save_for_tenant(&tenant_id, &execution)
            .await?;

        // Emit Started Event
        self.event_bus
            .publish_execution_event(ExecutionEvent::ExecutionStarted {
                execution_id,
                agent_id,
                started_at: Utc::now(),
            });

        metrics::counter!("aegis_executions_total", "kind" => "root", "status" => "started")
            .increment(1);
        metrics::gauge!("aegis_execution_active").increment(1.0);

        // --- SEAL session pre-creation (ADR-088 §A8) ---
        //
        // Generate an ephemeral Ed25519 keypair, mint a JWT with enriched claims,
        // POST the session to the SEAL gateway, and capture credentials for injection
        // into the container environment. If the gateway is unreachable or the token
        // issuer is not configured the execution proceeds — the agent can still attest
        // manually via POST /v1/seal/attest.
        let seal_credentials: Option<(String, String)> = if let (
            Some(gateway_client),
            Some(token_issuer),
        ) =
            (&self.seal_gateway_client, &self.token_issuer)
        {
            use base64::engine::general_purpose::STANDARD;
            use base64::Engine;
            use ed25519_dalek::SigningKey;

            // 1. Generate ephemeral Ed25519 keypair
            let signing_key = SigningKey::generate(&mut rand_core::OsRng);
            let public_key_bytes = signing_key.verifying_key().to_bytes();
            let public_key_b64 = STANDARD.encode(public_key_bytes);
            let private_key_b64 = STANDARD.encode(signing_key.to_bytes());

            // 2. Build and mint JWT
            let now = chrono::Utc::now();
            let exp = now + chrono::Duration::hours(1);

            let container_id_str = String::new(); // populated post-start if available
            let mut claims = crate::application::ports::AttestationTokenClaims {
                agent_id: agent_id.0.to_string(),
                execution_id: execution_id.0.to_string(),
                security_context: seal_security_context.clone(),
                iss: None,
                aud: Some(crate::application::ports::TokenAudience::Single(
                    "aegis-agents".to_string(),
                )),
                exp: Some(exp.timestamp()),
                iat: Some(now.timestamp()),
                nbf: None,
                jti: Some(uuid::Uuid::new_v4().to_string()),
                sub: agent_id.0.to_string(),
                scp: seal_security_context.clone(),
                wid: if container_id_str.is_empty() {
                    execution_id.0.to_string()
                } else {
                    container_id_str
                },
                tenant_id: Some(tenant_id.as_str().to_string()),
            };

            match token_issuer.issue(&mut claims) {
                Ok(token) => {
                    // 3. POST session to gateway
                    // Derive allowed_tool_patterns from the agent manifest's spec.tools.
                    // An empty tools list (system agents with no declared restrictions)
                    // falls back to ["*"]. Attestation will overwrite this session with
                    // manifest-derived patterns via the same logic; the two must agree.
                    let pre_create_tool_patterns = if agent.manifest.spec.tools.is_empty() {
                        vec!["*".to_string()]
                    } else {
                        agent.manifest.spec.tools.clone()
                    };
                    let session_request = crate::application::ports::SealSessionCreateRequest {
                        execution_id: execution_id.0.to_string(),
                        agent_id: agent_id.0.to_string(),
                        security_context: seal_security_context.clone(),
                        public_key_b64: public_key_b64.clone(),
                        security_token: token.clone(),
                        session_status: "Active".to_string(),
                        expires_at: exp.to_rfc3339(),
                        allowed_tool_patterns: pre_create_tool_patterns,
                    };

                    if let Err(e) = gateway_client.create_session(session_request).await {
                        tracing::warn!(
                            execution_id = %execution_id,
                            error = %e,
                            "Failed to pre-create SEAL session on gateway; agent can still attest manually"
                        );
                    }

                    Some((private_key_b64, token))
                }
                Err(e) => {
                    tracing::warn!(
                        execution_id = %execution_id,
                        error = %e,
                        "Failed to mint SEAL token for session pre-creation; agent can still attest manually"
                    );
                    None
                }
            }
        } else {
            None
        };

        // 4. Spawn Runtime
        let mut env = agent.manifest.spec.env.clone();

        // Inject instruction from default task if available
        if let Some(task_spec) = &agent.manifest.spec.task {
            if let Some(instr) = &task_spec.instruction {
                env.insert("AEGIS_AGENT_INSTRUCTION".to_string(), instr.clone());
            }
            // Inject prompt template if specified
            if let Some(prompt_tpl) = &task_spec.prompt_template {
                env.insert("AEGIS_PROMPT_TEMPLATE".to_string(), prompt_tpl.clone());
            }
        }
        // Fallback to description if not set above
        if !env.contains_key("AEGIS_AGENT_INSTRUCTION") {
            if let Some(desc) = &agent.manifest.metadata.description {
                env.insert("AEGIS_AGENT_INSTRUCTION".to_string(), desc.clone());
            }
        }

        // Inject other metadata
        env.insert("AEGIS_AGENT_ID".to_string(), agent_id.0.to_string());
        env.insert("AEGIS_EXECUTION_ID".to_string(), execution_id.0.to_string());

        // Inject Orchestrator URL
        // Resolve from config (supports env:VAR_NAME), fallback to host.docker.internal
        let orchestrator_url = resolve_env_value(&self.config.spec.runtime.orchestrator_url)
            .ok()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| {
                let port = self
                    .config
                    .spec
                    .network
                    .as_ref()
                    .map(|n| n.port)
                    .unwrap_or(8088);
                format!("http://host.docker.internal:{port}")
            });
        env.insert("AEGIS_ORCHESTRATOR_URL".to_string(), orchestrator_url);

        // Inject LLM timeout for bootstrap.py (default 300 seconds)
        let llm_timeout_seconds = if let Some(exec_strategy) = &agent.manifest.spec.execution {
            exec_strategy.llm_timeout_seconds
        } else {
            300
        };
        env.insert(
            "AEGIS_LLM_TIMEOUT_SECONDS".to_string(),
            llm_timeout_seconds.to_string(),
        );

        // Inject model alias so bootstrap.py routes this agent's LLM calls to the
        // correct provider (e.g. "judge" → anthropic/claude-haiku, "smart" → local).
        // Falls back to "default" when spec.runtime.model is not set in the manifest.
        env.insert(
            "AEGIS_MODEL_ALIAS".to_string(),
            agent.manifest.spec.runtime.model.clone(),
        );

        // Inject pre-created SEAL credentials into container environment (ADR-088 §A8)
        if let Some((private_key, token)) = seal_credentials {
            env.insert("AEGIS_SEAL_PRIVATE_KEY".to_string(), private_key);
            env.insert("AEGIS_SEAL_TOKEN".to_string(), token);
        }

        // Convert resource limits from domain format to runtime format
        let resources = if let Some(security) = &agent.manifest.spec.security {
            crate::domain::runtime::ResourceLimits {
                cpu_millis: Some(security.resources.cpu),
                memory_bytes: security.resources.memory_bytes(),
                disk_bytes: security.resources.disk_bytes(),
                timeout_seconds: security.resources.parse_timeout_seconds(),
            }
        } else {
            crate::domain::runtime::ResourceLimits {
                cpu_millis: None,
                memory_bytes: None,
                disk_bytes: None,
                timeout_seconds: None,
            }
        };

        // Create volumes from manifest if specified
        tracing::info!(
            "Checking for volumes in agent manifest: {} volume(s) specified",
            agent.manifest.spec.volumes.len()
        );
        let volume_mounts = if !agent.manifest.spec.volumes.is_empty() {
            tracing::info!("Creating volumes for execution {}", execution_id.0);

            // Get storage config from node config
            let storage_config = self
                .config
                .spec
                .storage
                .as_ref()
                .ok_or_else(|| anyhow!("Storage configuration not found in node config"))?;

            tracing::debug!("Storage config: backend={}", storage_config.backend);

            // Create volumes using volume service
            let volumes = self
                .volume_service
                .create_volumes_for_execution(
                    execution_id,
                    tenant_id.clone(),
                    &agent.manifest.spec.volumes,
                    &storage_config.backend,
                )
                .await?;

            tracing::info!("Successfully created {} volume(s)", volumes.len());

            // Build VolumeMount objects from created volumes
            volumes
                .iter()
                .map(|volume| {
                    // Find corresponding spec to get mount_path and access_mode
                    let spec = agent
                        .manifest
                        .spec
                        .volumes
                        .iter()
                        .find(|s| s.name == volume.name)
                        .expect("Volume spec not found for created volume");

                    let access_mode = match spec.access_mode.as_str() {
                        "read-only" => AccessMode::ReadOnly,
                        _ => AccessMode::ReadWrite,
                    };

                    volume.to_mount(PathBuf::from(&spec.mount_path), access_mode)
                })
                .collect::<Vec<VolumeMount>>()
        } else {
            Vec::new()
        };

        // Mount policy: all mount points must live under /workspace, and must not overlap.
        if !volume_mounts.is_empty() {
            let mut mount_paths = volume_mounts
                .iter()
                .map(|m| m.mount_point.to_string_lossy().to_string())
                .collect::<Vec<String>>();
            mount_paths.sort();

            for path in &mount_paths {
                if !(path == "/workspace" || path.starts_with("/workspace/")) {
                    return Err(anyhow!(
                        "Invalid mount path '{path}': all mounts must be /workspace or /workspace/*"
                    ));
                }
            }

            for i in 0..mount_paths.len() {
                for j in (i + 1)..mount_paths.len() {
                    let a = mount_paths[i].trim_end_matches('/');
                    let b = mount_paths[j].trim_end_matches('/');
                    if a == "/workspace" || b == "/workspace" {
                        continue;
                    }
                    if a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
                    {
                        return Err(anyhow!(
                            "Overlapping mount paths are not allowed: '{}' and '{}'",
                            mount_paths[i],
                            mount_paths[j]
                        ));
                    }
                }
            }
        }

        // Register each volume with the NFS gateway BEFORE the container mounts it (ADR-036)
        // This populates the in-memory volume_registry so the NFS server can authorize requests.
        if let Some(ref gw) = self.nfs_gateway {
            for mount in &volume_mounts {
                let policy = FsalAccessPolicy {
                    read: vec!["/*".to_string()],
                    write: vec!["/*".to_string()],
                };
                gw.register_volume(VolumeRegistration {
                    volume_id: mount.volume_id,
                    execution_id,
                    container_uid: 1000,
                    container_gid: 1000,
                    policy,
                    mount_point: mount.mount_point.clone(),
                    remote_path: mount.remote_path.clone(),
                });
                tracing::info!(
                    "Registered volume {} with NFS gateway for execution {}",
                    mount.volume_id,
                    execution_id
                );
            }
        }

        let runtime_config = crate::domain::runtime::RuntimeConfig {
            language: agent
                .manifest
                .spec
                .runtime
                .language
                .clone()
                .unwrap_or_default(),
            version: agent
                .manifest
                .spec
                .runtime
                .version
                .clone()
                .unwrap_or_default(),
            isolation: agent.manifest.spec.runtime.isolation.clone(),
            env,
            image_pull_policy: agent.manifest.spec.runtime.image_pull_policy,
            resources,
            execution: agent.manifest.spec.execution.clone().unwrap_or_default(),
            volumes: volume_mounts,
            container_uid: 1000,
            container_gid: 1000,
            keep_container_on_failure: std::env::var("AEGIS_KEEP_CONTAINER")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(false),
            // Resolve container image (ADR-043: StandardRuntime, ADR-044: CustomRuntime).
            // CustomRuntime: use spec.runtime.image verbatim.
            // StandardRuntime: registry maps language+version → slim tag (e.g. python:3.11-slim).
            image: if let Some(custom_image) = agent.manifest.spec.runtime.image.as_deref() {
                tracing::info!("CustomRuntime image: {}", custom_image);
                custom_image.to_string()
            } else {
                let language = agent
                    .manifest
                    .spec
                    .runtime
                    .language
                    .as_deref()
                    .unwrap_or("");
                let version = agent.manifest.spec.runtime.version.as_deref().unwrap_or("");
                if let Some(ref registry) = self.runtime_registry {
                    match registry.resolve(language, version) {
                        Ok(img) => {
                            tracing::info!(
                                "Resolved StandardRuntime image for {}/{}: {}",
                                language,
                                version,
                                img
                            );
                            img
                        }
                        Err(e) => {
                            tracing::error!("StandardRuntime validation failed: {}", e);
                            return Err(anyhow!(
                                "Unsupported Standard Runtime: {language} {version}. {e}"
                            ));
                        }
                    }
                } else {
                    return Err(anyhow!(
                        "StandardRuntime registry not configured; cannot resolve image for \
                         language='{language}' version='{version}'. Call `.with_runtime_registry()` when \
                         building StandardExecutionService (ADR-043)."
                    ));
                }
            },
            // Forward custom bootstrap path from spec.advanced (ADR-044).
            // None → orchestrator copies its own bootstrap into the container (StandardRuntime).
            // Some(path) → bootstrap already present in image; skip injection, exec at path.
            bootstrap_path: agent
                .manifest
                .spec
                .advanced
                .as_ref()
                .and_then(|a| a.bootstrap_path.clone()),
            // Attach execution_id so ContainerRuntime can correlate image events (ADR-045).
            execution_id,
        };

        execution.start();
        self.repository
            .save_for_tenant(&tenant_id, &execution)
            .await?;

        // 5. Run Supervisor Loop
        let supervisor = self.supervisor.clone();
        let repository = self.repository.clone();
        let event_bus = self.event_bus.clone();
        let exec_input = prepared_input.clone();
        let tenant_id_for_task = tenant_id.clone();

        let monitor = Arc::new(ExecutionMonitor {
            execution_id,
            agent_id,
            repository: repository.clone(),
            event_bus: event_bus.clone(),
        });

        // Build gradient validation pipeline from manifest config (ADR-017).
        let validation_pipeline = agent
            .manifest
            .spec
            .execution
            .as_ref()
            .and_then(|e| e.validation.as_ref())
            .filter(|v| !v.is_empty())
            .map(|v| {
                let child_svc = self.child_executor.get().cloned().expect(
                    "child_executor not set; call set_child_execution_service() at startup",
                );
                Arc::new(build_validation_pipeline(
                    v,
                    self.agent_service.clone(),
                    child_svc,
                    self.event_bus.clone(),
                    execution_id,
                    tenant_id.clone(),
                ))
            });

        // Create a cancellation token for this execution so cancel_execution() can
        // signal the Supervisor loop to stop cooperatively.
        let cancellation_token = CancellationToken::new();
        self.cancellation_tokens
            .insert(execution_id, cancellation_token.clone());
        let tokens_map = self.cancellation_tokens.clone();
        let cortex_client = self.cortex_client.clone();
        let start_time = Utc::now();

        tokio::spawn(async move {
            let result = supervisor
                .run_loop(
                    runtime_config,
                    exec_input,
                    max_retries as u32,
                    monitor,
                    cancellation_token,
                    validation_pipeline,
                )
                .await;

            let duration = Utc::now() - start_time;
            let duration_seconds = duration.num_milliseconds() as f64 / 1000.0;
            metrics::gauge!("aegis_execution_active").decrement(1.0);

            // Clean up the token from the map now that the execution is done
            tokens_map.remove(&execution_id);

            match result {
                Ok(final_output) => {
                    metrics::counter!(
                        "aegis_executions_total",
                        "kind" => "root",
                        "status" => "completed"
                    )
                    .increment(1);
                    metrics::histogram!(
                        "aegis_execution_duration_seconds",
                        "kind" => "root",
                        "status" => "completed"
                    )
                    .record(duration_seconds);
                    // Update to completed
                    if let Ok(Some(mut exec)) = repository
                        .find_by_id_for_tenant(&tenant_id_for_task, execution_id)
                        .await
                    {
                        exec.complete();
                        let total_iterations = exec.iterations().len() as u8;
                        let _ = repository.save_for_tenant(&tenant_id_for_task, &exec).await;

                        StandardExecutionService::store_trajectory_in_cortex(&cortex_client, &exec);

                        event_bus.publish_execution_event(ExecutionEvent::ExecutionCompleted {
                            execution_id,
                            agent_id,
                            final_output: final_output.clone(),
                            total_iterations,
                            completed_at: Utc::now(),
                        });
                    }
                }
                Err(RuntimeError::TimedOut(timeout_secs)) => {
                    metrics::counter!(
                        "aegis_executions_total",
                        "kind" => "root",
                        "status" => "timed_out"
                    )
                    .increment(1);
                    metrics::histogram!(
                        "aegis_execution_duration_seconds",
                        "kind" => "root",
                        "status" => "timed_out"
                    )
                    .record(duration_seconds);
                    // Execution timed out — emit specific timeout event
                    if let Ok(Some(mut exec)) = repository
                        .find_by_id_for_tenant(&tenant_id_for_task, execution_id)
                        .await
                    {
                        exec.fail(format!("Execution timed out after {timeout_secs} seconds"));
                        let total_iterations = exec.iterations().len() as u8;
                        let _ = repository.save_for_tenant(&tenant_id_for_task, &exec).await;

                        event_bus.publish_execution_event(ExecutionEvent::ExecutionTimedOut {
                            execution_id,
                            agent_id,
                            timeout_seconds: timeout_secs,
                            total_iterations,
                            timed_out_at: Utc::now(),
                        });
                    }
                }
                Err(RuntimeError::Cancelled) => {
                    metrics::counter!(
                        "aegis_executions_total",
                        "kind" => "root",
                        "status" => "cancelled"
                    )
                    .increment(1);
                    metrics::histogram!(
                        "aegis_execution_duration_seconds",
                        "kind" => "root",
                        "status" => "cancelled"
                    )
                    .record(duration_seconds);
                    // Execution was cancelled — status already set by cancel_execution(),
                    // but ensure we mark it if the token was triggered externally.
                    if let Ok(Some(mut exec)) = repository
                        .find_by_id_for_tenant(&tenant_id_for_task, execution_id)
                        .await
                    {
                        if exec.status != ExecutionStatus::Cancelled {
                            exec.status = ExecutionStatus::Cancelled;
                            exec.ended_at = Some(Utc::now());
                            let _ = repository.save_for_tenant(&tenant_id_for_task, &exec).await;

                            event_bus.publish_execution_event(ExecutionEvent::ExecutionCancelled {
                                execution_id,
                                agent_id,
                                reason: Some("Cancelled via cancellation token".to_string()),
                                cancelled_at: Utc::now(),
                            });
                        }
                    }
                }
                Err(e) => {
                    metrics::counter!(
                        "aegis_executions_total",
                        "kind" => "root",
                        "status" => "failed"
                    )
                    .increment(1);
                    metrics::histogram!(
                        "aegis_execution_duration_seconds",
                        "kind" => "root",
                        "status" => "failed"
                    )
                    .record(duration_seconds);
                    // Update to failed (generic failure)
                    if let Ok(Some(mut exec)) = repository
                        .find_by_id_for_tenant(&tenant_id_for_task, execution_id)
                        .await
                    {
                        exec.fail(e.to_string());
                        let total_iterations = exec.iterations().len() as u8;
                        let _ = repository.save_for_tenant(&tenant_id_for_task, &exec).await;

                        event_bus.publish_execution_event(ExecutionEvent::ExecutionFailed {
                            execution_id,
                            agent_id,
                            reason: e.to_string(),
                            total_iterations,
                            failed_at: Utc::now(),
                        });
                    }
                }
            }
        });

        Ok(execution_id)
    }
}

#[async_trait]
impl ExecutionService for StandardExecutionService {
    async fn start_execution(
        &self,
        agent_id: AgentId,
        input: ExecutionInput,
        security_context_name: String,
        identity: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        self.do_start_execution(None, agent_id, input, security_context_name, identity)
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
        self.do_start_execution(
            Some(execution_id),
            agent_id,
            input,
            security_context_name,
            identity,
        )
        .await
    }

    async fn get_execution(&self, id: ExecutionId) -> Result<Execution> {
        self.get_execution_for_tenant(&TenantId::local_default(), id)
            .await
    }

    async fn get_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Execution> {
        StandardExecutionService::get_execution_for_tenant(self, tenant_id, id).await
    }

    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        self.repository
            .find_by_id_unscoped(id)
            .await?
            .ok_or_else(|| anyhow!("Execution not found"))
    }

    async fn get_iterations(&self, exec_id: ExecutionId) -> Result<Vec<Iteration>> {
        self.get_iterations_for_tenant(&TenantId::local_default(), exec_id)
            .await
    }

    async fn cancel_execution(&self, id: ExecutionId) -> Result<()> {
        self.cancel_execution_for_tenant(&TenantId::local_default(), id)
            .await
    }

    async fn stream_execution(
        &self,
        id: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        let receiver = self.event_bus.subscribe_execution(id);

        let stream = futures::stream::unfold(receiver, |mut receiver| async move {
            match receiver.recv().await {
                Ok(event) => Some((Ok(event), receiver)),
                Err(EventBusError::Closed) => None,
                Err(e) => Some((Err(anyhow!("Event bus error: {e}")), receiver)),
            }
        });

        Ok(Box::pin(stream))
    }

    async fn stream_agent_events(
        &self,
        _id: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        // Agent event streaming is implemented in daemon/server.rs::stream_agent_events_handler
        // which subscribes to EventBus directly. This trait method returns empty for interface compliance.
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn list_executions(
        &self,
        agent_id: Option<AgentId>,
        limit: usize,
    ) -> Result<Vec<Execution>> {
        self.list_executions_for_tenant(&TenantId::local_default(), agent_id, None, limit)
            .await
    }

    async fn start_child_execution(
        &self,
        agent_id: AgentId,
        input: ExecutionInput,
        parent_execution_id: ExecutionId,
    ) -> Result<ExecutionId> {
        // 1. Fetch parent to validate hierarchy depth and build child hierarchy.
        let parent = self
            .repository
            .find_by_id(parent_execution_id)
            .await?
            .ok_or_else(|| anyhow!("Parent execution {parent_execution_id} not found"))?;
        let parent_tenant_id = Self::resolve_tenant_from_input(&parent.input)?;
        let child_tenant_id = Self::resolve_tenant_from_input(&input)?;

        // Cross-tenant spawn check (ADR-056 Phase 3): child executions must
        // belong to the same tenant as their parent.
        if parent_tenant_id != child_tenant_id {
            return Err(anyhow!(
                "Cross-tenant spawn forbidden: parent tenant '{}' != child tenant '{}'",
                parent_tenant_id.as_str(),
                child_tenant_id.as_str()
            ));
        }
        let tenant_id = parent_tenant_id;

        if !parent.can_spawn_child() {
            return Err(anyhow!(
                "Maximum recursive depth reached at depth {}; cannot spawn child from execution {}",
                parent.depth(),
                parent_execution_id
            ));
        }

        // 2. Fetch judge agent.
        let agent = self
            .agent_service
            .get_agent_visible(&tenant_id, agent_id)
            .await?;
        // 3. Prepare input (render judge's prompt template).
        let prepared_input = self.prepare_execution_input(input, &agent)?;

        // 4. Create child execution record with hierarchy.
        let max_retries = agent
            .manifest
            .spec
            .execution
            .as_ref()
            .map(|e| e.max_retries as u8)
            .unwrap_or(3);

        let parent_agent_id = parent.agent_id;

        let mut child_execution = crate::domain::execution::Execution::new_child(
            agent_id,
            prepared_input.clone(),
            max_retries,
            &parent,
        )
        .map_err(|e| anyhow!("Failed to create child execution: {e}"))?;
        let child_execution_id = child_execution.id;

        self.event_bus
            .publish_execution_event(ExecutionEvent::ChildExecutionSpawned {
                execution_id: parent_execution_id,
                agent_id: parent_agent_id,
                parent_execution_id,
                child_execution_id,
                child_agent_id: agent_id,
                spawned_at: Utc::now(),
            });

        // 5. Build runtime config.
        let mut env = agent.manifest.spec.env.clone();
        if let Some(task_spec) = &agent.manifest.spec.task {
            if let Some(instr) = &task_spec.instruction {
                env.insert("AEGIS_AGENT_INSTRUCTION".to_string(), instr.clone());
            }
            if let Some(prompt_tpl) = &task_spec.prompt_template {
                env.insert("AEGIS_PROMPT_TEMPLATE".to_string(), prompt_tpl.clone());
            }
        }
        if !env.contains_key("AEGIS_AGENT_INSTRUCTION") {
            if let Some(desc) = &agent.manifest.metadata.description {
                env.insert("AEGIS_AGENT_INSTRUCTION".to_string(), desc.clone());
            }
        }
        env.insert("AEGIS_AGENT_ID".to_string(), agent_id.0.to_string());
        env.insert(
            "AEGIS_EXECUTION_ID".to_string(),
            child_execution_id.0.to_string(),
        );
        let orchestrator_url = resolve_env_value(&self.config.spec.runtime.orchestrator_url)
            .ok()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| {
                let port = self
                    .config
                    .spec
                    .network
                    .as_ref()
                    .map(|n| n.port)
                    .unwrap_or(8088);
                format!("http://host.docker.internal:{port}")
            });
        env.insert("AEGIS_ORCHESTRATOR_URL".to_string(), orchestrator_url);
        let llm_timeout_seconds = agent
            .manifest
            .spec
            .execution
            .as_ref()
            .map(|e| e.llm_timeout_seconds)
            .unwrap_or(300);
        env.insert(
            "AEGIS_LLM_TIMEOUT_SECONDS".to_string(),
            llm_timeout_seconds.to_string(),
        );

        // Inject model alias so bootstrap.py routes this child agent's LLM calls to the
        // correct provider (e.g. "judge" → anthropic/claude-haiku, "smart" → local).
        // Falls back to "default" when spec.runtime.model is not set in the manifest.
        env.insert(
            "AEGIS_MODEL_ALIAS".to_string(),
            agent.manifest.spec.runtime.model.clone(),
        );

        let resources = if let Some(security) = &agent.manifest.spec.security {
            crate::domain::runtime::ResourceLimits {
                cpu_millis: Some(security.resources.cpu),
                memory_bytes: security.resources.memory_bytes(),
                disk_bytes: security.resources.disk_bytes(),
                timeout_seconds: security.resources.parse_timeout_seconds(),
            }
        } else {
            crate::domain::runtime::ResourceLimits {
                cpu_millis: None,
                memory_bytes: None,
                disk_bytes: None,
                timeout_seconds: None,
            }
        };

        let image = if let Some(custom_image) = agent.manifest.spec.runtime.image.as_deref() {
            custom_image.to_string()
        } else {
            let language = agent
                .manifest
                .spec
                .runtime
                .language
                .as_deref()
                .unwrap_or("");
            let version = agent.manifest.spec.runtime.version.as_deref().unwrap_or("");
            if let Some(ref reg) = self.runtime_registry {
                reg.resolve(language, version).map_err(|e| {
                    anyhow!("Unsupported Standard Runtime for judge: {language} {version}. {e}")
                })?
            } else {
                return Err(anyhow!(
                    "StandardRuntime registry not configured; cannot resolve judge agent image"
                ));
            }
        };

        let judge_inherited_mounts: Vec<VolumeMount> = {
            let is_judge = agent
                .manifest
                .metadata
                .labels
                .get("role")
                .map(|s| s.as_str())
                == Some("judge");
            if is_judge {
                self.build_judge_inherited_mounts(
                    &tenant_id,
                    parent_execution_id,
                    child_execution_id,
                )
                .await?
            } else {
                Vec::new()
            }
        };

        // Provision the judge's own declared volumes from its manifest (mirrors start_execution logic).
        let own_volume_mounts = if !agent.manifest.spec.volumes.is_empty() {
            tracing::info!(
                "Creating volumes for child execution {}",
                child_execution_id.0
            );

            let storage_config = self
                .config
                .spec
                .storage
                .as_ref()
                .ok_or_else(|| anyhow!("Storage configuration not found in node config"))?;

            let volumes = self
                .volume_service
                .create_volumes_for_execution(
                    child_execution_id,
                    tenant_id.clone(),
                    &agent.manifest.spec.volumes,
                    &storage_config.backend,
                )
                .await?;

            tracing::info!(
                "Successfully created {} own volume(s) for child execution",
                volumes.len()
            );

            volumes
                .iter()
                .map(|volume| {
                    let spec = agent
                        .manifest
                        .spec
                        .volumes
                        .iter()
                        .find(|s| s.name == volume.name)
                        .expect("Volume spec not found for created volume");

                    let access_mode = match spec.access_mode.as_str() {
                        "read-only" => AccessMode::ReadOnly,
                        _ => AccessMode::ReadWrite,
                    };

                    volume.to_mount(PathBuf::from(&spec.mount_path), access_mode)
                })
                .collect::<Vec<VolumeMount>>()
        } else {
            Vec::new()
        };

        // Validate own volume mount paths (must be under /workspace, no overlaps with each other
        // or with inherited mounts).
        if !own_volume_mounts.is_empty() {
            let inherited_paths: Vec<String> = judge_inherited_mounts
                .iter()
                .map(|m| m.mount_point.to_string_lossy().to_string())
                .collect();

            for mount in &own_volume_mounts {
                let path = mount.mount_point.to_string_lossy().to_string();
                if !(path == "/workspace" || path.starts_with("/workspace/")) {
                    return Err(anyhow!(
                        "Invalid mount path '{path}': all mounts must be /workspace or /workspace/*"
                    ));
                }
                // Own volumes must not overlap with inherited mount paths.
                for inh in &inherited_paths {
                    let a = path.trim_end_matches('/');
                    let b = inh.trim_end_matches('/');
                    if a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
                    {
                        return Err(anyhow!(
                            "Child execution own mount '{}' overlaps with inherited mount '{}'",
                            path,
                            inh
                        ));
                    }
                }
            }

            // Check own mounts for mutual overlaps (skip /workspace root).
            let mut own_paths: Vec<String> = own_volume_mounts
                .iter()
                .map(|m| m.mount_point.to_string_lossy().to_string())
                .collect();
            own_paths.sort();
            for i in 0..own_paths.len() {
                for j in (i + 1)..own_paths.len() {
                    let a = own_paths[i].trim_end_matches('/');
                    let b = own_paths[j].trim_end_matches('/');
                    if a == "/workspace" || b == "/workspace" {
                        continue;
                    }
                    if a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
                    {
                        return Err(anyhow!(
                            "Overlapping mount paths are not allowed: '{}' and '{}'",
                            own_paths[i],
                            own_paths[j]
                        ));
                    }
                }
            }
        }

        // Register own volumes with the NFS gateway (ADR-036).
        if let Some(ref gw) = self.nfs_gateway {
            for mount in &own_volume_mounts {
                let policy = FsalAccessPolicy {
                    read: vec!["/*".to_string()],
                    write: vec!["/*".to_string()],
                };
                gw.register_volume(VolumeRegistration {
                    volume_id: mount.volume_id,
                    execution_id: child_execution_id,
                    container_uid: 1000,
                    container_gid: 1000,
                    policy,
                    mount_point: mount.mount_point.clone(),
                    remote_path: mount.remote_path.clone(),
                });
                tracing::info!(
                    "Registered own volume {} with NFS gateway for child execution {}",
                    mount.volume_id,
                    child_execution_id
                );
            }
        }

        // Merge inherited mounts and own mounts. Inherited mounts take precedence for
        // overlapping paths (validated above to not overlap, so this is a simple concat).
        let all_volumes = {
            let mut merged = judge_inherited_mounts.clone();
            merged.extend(own_volume_mounts);
            merged
        };

        let runtime_config = crate::domain::runtime::RuntimeConfig {
            language: agent
                .manifest
                .spec
                .runtime
                .language
                .clone()
                .unwrap_or_default(),
            version: agent
                .manifest
                .spec
                .runtime
                .version
                .clone()
                .unwrap_or_default(),
            isolation: agent.manifest.spec.runtime.isolation.clone(),
            env,
            image_pull_policy: agent.manifest.spec.runtime.image_pull_policy,
            resources,
            execution: agent.manifest.spec.execution.clone().unwrap_or_default(),
            volumes: all_volumes,
            container_uid: 1000,
            container_gid: 1000,
            keep_container_on_failure: std::env::var("AEGIS_KEEP_CONTAINER")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(false),
            image,
            bootstrap_path: agent
                .manifest
                .spec
                .advanced
                .as_ref()
                .and_then(|a| a.bootstrap_path.clone()),
            execution_id: child_execution_id,
        };

        child_execution.start();
        self.repository
            .save_for_tenant(&tenant_id, &child_execution)
            .await?;

        self.event_bus
            .publish_execution_event(ExecutionEvent::ExecutionStarted {
                execution_id: child_execution_id,
                agent_id,
                started_at: Utc::now(),
            });

        metrics::counter!("aegis_executions_total", "kind" => "child", "status" => "started")
            .increment(1);
        metrics::gauge!("aegis_execution_active").increment(1.0);

        // 6. Run Supervisor loop in background task.
        let supervisor = self.supervisor.clone();
        let repository = self.repository.clone();
        let event_bus = self.event_bus.clone();
        let monitor = Arc::new(ExecutionMonitor {
            execution_id: child_execution_id,
            agent_id,
            repository: repository.clone(),
            event_bus: event_bus.clone(),
        });

        // Judge agents may declare their own (nested) validation steps.
        let validation_pipeline = agent
            .manifest
            .spec
            .execution
            .as_ref()
            .and_then(|e| e.validation.as_ref())
            .filter(|v| !v.is_empty())
            .map(|v| {
                let child_svc = self
                    .child_executor
                    .get()
                    .cloned()
                    .expect("child_executor not set");
                Arc::new(build_validation_pipeline(
                    v,
                    self.agent_service.clone(),
                    child_svc,
                    self.event_bus.clone(),
                    child_execution_id,
                    tenant_id.clone(),
                ))
            });

        let cancellation_token = CancellationToken::new();
        self.cancellation_tokens
            .insert(child_execution_id, cancellation_token.clone());
        let tokens_map = self.cancellation_tokens.clone();
        let cortex_client = self.cortex_client.clone();
        let tenant_id_for_task = tenant_id.clone();
        let parent_execution_id_for_task = parent_execution_id;
        let parent_agent_id_for_task = parent_agent_id;

        tokio::spawn(async move {
            let result = supervisor
                .run_loop(
                    runtime_config,
                    prepared_input,
                    max_retries as u32,
                    monitor,
                    cancellation_token,
                    validation_pipeline,
                )
                .await;

            tokens_map.remove(&child_execution_id);

            match result {
                Ok(final_output) => {
                    if let Ok(Some(mut exec)) = repository
                        .find_by_id_for_tenant(&tenant_id_for_task, child_execution_id)
                        .await
                    {
                        exec.complete();
                        let total_iterations = exec.iterations().len() as u8;
                        let _ = repository.save_for_tenant(&tenant_id_for_task, &exec).await;

                        StandardExecutionService::store_trajectory_in_cortex(&cortex_client, &exec);

                        let completed_at = Utc::now();
                        event_bus.publish_execution_event(ExecutionEvent::ExecutionCompleted {
                            execution_id: child_execution_id,
                            agent_id,
                            final_output: final_output.clone(),
                            total_iterations,
                            completed_at,
                        });

                        event_bus.publish_execution_event(
                            ExecutionEvent::ChildExecutionCompleted {
                                execution_id: parent_execution_id_for_task,
                                agent_id: parent_agent_id_for_task,
                                child_execution_id,
                                outcome: final_output,
                                completed_at,
                            },
                        );
                    }
                }
                Err(RuntimeError::TimedOut(timeout_secs)) => {
                    if let Ok(Some(mut exec)) = repository
                        .find_by_id_for_tenant(&tenant_id_for_task, child_execution_id)
                        .await
                    {
                        exec.fail(format!("Execution timed out after {timeout_secs} seconds"));
                        let total_iterations = exec.iterations().len() as u8;
                        let _ = repository.save_for_tenant(&tenant_id_for_task, &exec).await;
                        event_bus.publish_execution_event(ExecutionEvent::ExecutionTimedOut {
                            execution_id: child_execution_id,
                            agent_id,
                            timeout_seconds: timeout_secs,
                            total_iterations,
                            timed_out_at: Utc::now(),
                        });
                    }
                }
                Err(RuntimeError::Cancelled) => {
                    if let Ok(Some(mut exec)) = repository
                        .find_by_id_for_tenant(&tenant_id_for_task, child_execution_id)
                        .await
                    {
                        if exec.status != ExecutionStatus::Cancelled {
                            exec.status = ExecutionStatus::Cancelled;
                            exec.ended_at = Some(Utc::now());
                            let _ = repository.save_for_tenant(&tenant_id_for_task, &exec).await;
                            event_bus.publish_execution_event(ExecutionEvent::ExecutionCancelled {
                                execution_id: child_execution_id,
                                agent_id,
                                reason: Some("Cancelled".to_string()),
                                cancelled_at: Utc::now(),
                            });
                        }
                    }
                }
                Err(e) => {
                    if let Ok(Some(mut exec)) = repository
                        .find_by_id_for_tenant(&tenant_id_for_task, child_execution_id)
                        .await
                    {
                        exec.fail(e.to_string());
                        let total_iterations = exec.iterations().len() as u8;
                        let _ = repository.save_for_tenant(&tenant_id_for_task, &exec).await;
                        event_bus.publish_execution_event(ExecutionEvent::ExecutionFailed {
                            execution_id: child_execution_id,
                            agent_id,
                            reason: e.to_string(),
                            total_iterations,
                            failed_at: Utc::now(),
                        });
                    }
                }
            }
        });

        Ok(child_execution_id)
    }

    async fn delete_execution(&self, id: ExecutionId) -> Result<()> {
        self.delete_execution_for_tenant(&TenantId::local_default(), id)
            .await
    }

    async fn record_llm_interaction(
        &self,
        execution_id: ExecutionId,
        iteration: u8,
        interaction: crate::domain::execution::LlmInteraction,
    ) -> Result<()> {
        if let Some(mut exec) = self.repository.find_by_id(execution_id).await? {
            if let Err(e) = exec.add_llm_interaction(iteration, interaction) {
                tracing::warn!(
                    "Failed to record LLM interaction for execution {} iteration {}: {}",
                    execution_id.0,
                    iteration,
                    e
                );
            } else {
                self.repository.save(&exec).await?;
            }
        }
        Ok(())
    }

    async fn store_iteration_trajectory(
        &self,
        execution_id: ExecutionId,
        iteration: u8,
        trajectory: Vec<crate::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        if let Some(mut exec) = self.repository.find_by_id(execution_id).await? {
            if let Err(e) = exec.store_iteration_trajectory(iteration, trajectory) {
                tracing::warn!(
                    "Failed to record trajectory for execution {} iteration {}: {}",
                    execution_id.0,
                    iteration,
                    e
                );
            } else {
                self.repository.save(&exec).await?;
            }
        }
        Ok(())
    }

    async fn store_policy_violation(
        &self,
        execution_id: ExecutionId,
        tool_name: String,
    ) -> Result<()> {
        if let Some(mut exec) = self.repository.find_by_id(execution_id).await? {
            exec.add_policy_violation(tool_name);
            self.repository.save(&exec).await?;
        }
        Ok(())
    }
}

impl StandardExecutionService {
    fn store_trajectory_in_cortex(
        cortex_client_opt: &Option<Arc<dyn CortexPatternPort>>,
        exec: &Execution,
    ) {
        if let Some(cortex) = cortex_client_opt {
            if let Some(last_iter) = exec.iterations().last() {
                if let Some(trajectory) = last_iter.trajectory.clone() {
                    if !trajectory.is_empty() {
                        let score = last_iter
                            .validation_results
                            .as_ref()
                            .and_then(|vr| vr.gradient.as_ref())
                            .map(|g| g.score)
                            .unwrap_or(1.0);

                        let task_signature = exec.input.intent.clone().unwrap_or_default();
                        let steps = trajectory
                            .into_iter()
                            .map(|step| TrajectoryStepCommand {
                                tool_name: step.tool_name,
                                arguments_json: step.arguments_json,
                            })
                            .collect();

                        let req = StoreTrajectoryPatternCommand {
                            task_signature,
                            steps,
                            success_score: score,
                        };

                        let cortex_clone = cortex.clone();
                        tokio::spawn(async move {
                            if let Err(e) = cortex_clone.store_trajectory_pattern(req).await {
                                tracing::warn!(
                                    "Failed to store trajectory pattern in Cortex: {}",
                                    e
                                );
                            } else {
                                tracing::info!("Successfully stored TrajectoryPattern in Cortex");
                            }
                        });
                    }
                }
            }
        }
    }
}
