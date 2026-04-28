// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Agent Runtime Domain
//!
//! Domain interface for isolated execution environments. Defines the `AgentRuntime`
//! trait and associated value objects that the Execution Context (BC-2) depends on.
//!
//! ## Design
//!
//! The runtime layer is a **dependency-inversion boundary**: the domain defines the
//! `AgentRuntime` trait; the infrastructure layer provides concrete implementations.
//!
//! | Implementation | Phase | Location |
//! |---|---|---|
//! | `ContainerRuntime` | Phase 1 (current) | `crate::infrastructure::runtime` |
//! | `FirecrackerRuntime` | Phase 2 (deferred) | `crate::infrastructure::runtime` |
//!
//! ## UID/GID Squashing (ADR-036)
//!
//! `RuntimeConfig::container_uid` and `container_gid` tell the NFS Server Gateway
//! which UID/GID to squash all file ownership to. This eliminates kernel-level
//! permission conflicts between the container user and the SeaweedFS storage backend.
//! Defaults to UID/GID 1000 (standard non-root container user).
//!
//! See Also: ADR-027 (Docker Runtime), ADR-036 (NFS Server Gateway)

use crate::domain::shared_kernel::{ExecutionId, ImagePullPolicy};
// Conformist: BC-2 (Execution) conforms to BC-7 (Storage Gateway) volume model.
// The infrastructure layer (ContainerRuntime) accesses VolumeMount fields including
// AccessMode enum variants, so a full ACL wrapper is not cost-effective here.
use crate::domain::volume::VolumeMount;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// Configuration for spawning an isolated agent runtime instance.
///
/// Parsed from the `spec.runtime` section of an agent manifest and passed directly
/// to [`AgentRuntime::spawn`]. The runtime implementation converts this into a Docker
/// container (Phase 1) or Firecracker MicroVM (Phase 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Target programming language (e.g. `"python"`, `"typescript"`, `"rust"`).
    /// Combined with `version` to derive a Docker image tag via [`RuntimeConfig::resolve_image_with_registry`].
    pub language: String,
    /// Language runtime version (e.g. `"3.12"`, `"20"`, `"1.76"`). Used in image tag.
    pub version: String,
    /// Isolation mode. Must be `"docker"` or `"inherit"` in Phase 1.
    /// `"firecracker"` and `"process"` are rejected by [`RuntimeConfig::validate_isolation`]
    /// until their respective phases are implemented.
    pub isolation: String,
    /// Additional environment variables injected into the container at spawn time.
    pub env: HashMap<String, String>,
    /// Image pull policy for runtime images.
    pub image_pull_policy: ImagePullPolicy,
    /// CPU, memory, and disk resource limits applied to the container.
    pub resources: ResourceLimits,
    /// Execution strategy from the agent manifest (iteration settings, validation, etc.)
    pub execution: crate::domain::agent::ExecutionStrategy,
    /// Volume mounts to attach to the runtime instance.
    /// Each entry is fulfilled by the NFS Server Gateway before the container starts (ADR-036).
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    /// UID squashed onto all file operations by the NFS Server Gateway (ADR-036).
    /// Defaults to 1000 (standard non-root Linux user). Must match the UID of the
    /// process running inside the container to avoid permission conflicts.
    #[serde(default = "default_container_uid")]
    pub container_uid: u32,
    /// GID squashed onto all file operations by the NFS Server Gateway (ADR-036).
    /// Defaults to 1000. See `container_uid` for rationale.
    #[serde(default = "default_container_gid")]
    pub container_gid: u32,
    /// If `true`, keep failed containers alive for post-mortem debugging.
    /// Successful containers are always cleaned up. Manual cleanup required:
    /// `docker rm -f <container_id>`
    #[serde(default)]
    pub keep_container_on_failure: bool,
    /// Fully-resolved container image reference used at spawn time.
    ///
    /// For **StandardRuntime** this is the registry-resolved tag (e.g. `"python:3.11-slim"`),
    /// obtained via [`RuntimeConfig::resolve_image_with_registry`] in
    /// `application/execution.rs` before constructing this config.
    /// For **CustomRuntime** this is the verbatim `spec.runtime.image` value from the manifest.
    ///
    /// Always non-empty by the time `ContainerRuntime::spawn()` is called (ADR-043, ADR-044).
    pub image: String,
    /// In-container path to the bootstrap script (ADR-044).
    ///
    /// `None` (default) means the orchestrator copies its own `bootstrap.py` into
    /// the container at `/usr/local/bin/aegis-bootstrap` (StandardRuntime behaviour).
    ///
    /// `Some(path)` signals CustomRuntime: the script is already present inside the
    /// image at `path`; the orchestrator skips injection and execs directly at that path.
    /// Sourced from `spec.advanced.bootstrap_path` in the agent manifest.
    pub bootstrap_path: Option<String>,
    /// The `ExecutionId` of the execution that spawned this runtime instance.
    ///
    /// Propagated into [`crate::domain::events::ImageManagementEvent`] variants so
    /// that image pull telemetry can be correlated back to the specific execution
    /// (ADR-045).
    pub execution_id: ExecutionId,
}

fn default_container_uid() -> u32 {
    1000
}

fn default_container_gid() -> u32 {
    1000
}

/// Resource consumption limits enforced by the container runtime.
///
/// All fields are optional; `None` means the runtime applies no limit for that
/// resource (useful for development). Production manifests should always set all
/// fields to bound agent blast radius.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// CPU quota expressed as millicores (1000 = 1 full CPU core).
    /// Passed to Docker as `--cpus` (divided by 1000).
    pub cpu_millis: Option<u32>,
    /// Maximum RSS memory in bytes (`--memory` in Docker).
    pub memory_bytes: Option<u64>,
    /// Maximum ephemeral disk usage in bytes (enforced via volume quota).
    pub disk_bytes: Option<u64>,
    /// Hard wall-clock timeout for the entire execution in **seconds**.
    /// The [`crate::domain::supervisor::Supervisor`] wraps its loop in
    /// `tokio::time::timeout` using this value. When elapsed, the running
    /// container is terminated and the execution is marked `Failed`.
    ///
    /// `None` falls back to the global default (1800s / 30 min).
    pub timeout_seconds: Option<u64>,
}

impl RuntimeConfig {
    /// Derive the Docker image tag from `language` and `version` using the StandardRuntime registry.
    ///
    /// This is the sole mechanism for resolving StandardRuntime images (ADR-043). The
    /// `application/execution.rs` layer calls this before constructing [`RuntimeConfig`];
    /// by the time `ContainerRuntime::spawn()` is invoked, `config.image` is always pre-set.
    ///
    /// # Errors
    ///
    /// Returns an error if the language or version is not recognised by the registry.
    /// [`crate::application::execution::StandardExecutionService`] propagates this as
    /// an [`ExecutionError`](crate::domain::execution::ExecutionError) and the
    /// execution is immediately failed — no container is started.
    pub fn resolve_image_with_registry(
        &self,
        registry: &crate::domain::runtime_registry::StandardRuntimeRegistry,
    ) -> Result<String, crate::domain::runtime_registry::RegistryError> {
        registry.resolve(&self.language, &self.version)
    }

    /// Validate that the requested isolation mode is supported in the current phase.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::SpawnFailed`] for `"firecracker"`, `"process"`, or any
    /// unknown isolation string. Only `"docker"` and `"inherit"` are accepted in Phase 1.
    pub fn validate_isolation(&self) -> Result<(), RuntimeError> {
        match self.isolation.as_str() {
            "docker" | "inherit" => Ok(()),
            "firecracker" => Err(RuntimeError::SpawnFailed(
                "Firecracker isolation is not yet implemented. Use 'docker' or 'inherit'."
                    .to_string(),
            )),
            "process" => Err(RuntimeError::SpawnFailed(
                "Process isolation is not yet implemented. Use 'docker' or 'inherit'.".to_string(),
            )),
            other => Err(RuntimeError::SpawnFailed(format!(
                "Unknown isolation mode '{other}'. Supported: docker, inherit"
            ))),
        }
    }
}

/// Opaque identifier for a running runtime instance (container / MicroVM).
///
/// In Phase 1 this wraps the Docker container ID returned by `bollard` on spawn.
/// The value is treated as an opaque string by the domain; infrastructure layers
/// are responsible for interpreting it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceId(pub String);

impl InstanceId {
    /// Wrap an existing container/instance ID string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the raw ID string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Input payload sent to an agent container via the Supervisor.
///
/// The `prompt` is compiled from the agent manifest template and Blackboard context
/// by [`crate::infrastructure::prompt_template_engine::PromptTemplateEngine`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInput {
    /// The fully-rendered LLM prompt text.
    pub prompt: String,
    /// Additional structured context values available to the agent (e.g. previous
    /// iteration output, Blackboard state).
    pub context: HashMap<String, serde_json::Value>,
}

/// Output produced by one agent execution iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutput {
    /// Final structured result (typically parsed from the agent's stdout or a
    /// well-known output file like `/workspace/output.json`).
    pub result: serde_json::Value,
    /// Raw log lines captured from the container (stdout + stderr).
    pub logs: Vec<String>,
    /// MCP tool calls made by the agent during this iteration, in chronological order.
    pub tool_calls: Vec<ToolCall>,
    /// Container exit code. 0 indicates success; non-zero triggers iteration failure.
    pub exit_code: i64,
    /// Live inner-loop tool trajectory for this iteration, populated by the supervisor
    /// from the execution repository after the container exits.  Runtimes that do not
    /// use the inner-loop gateway (e.g. direct shell execution) leave this empty; the
    /// supervisor fills it in for inner-loop executions.
    #[serde(default)]
    pub trajectory: Vec<crate::domain::execution::TrajectoryStep>,
}

/// A single MCP tool invocation recorded during an iteration.
///
/// All tool calls are proxied through the orchestrator (ADR-033), so this record
/// represents the orchestrator's view of the call, not raw agent output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// MCP tool name (e.g. `"filesystem.read"`, `"web_search.search"`).
    pub tool: String,
    /// Arguments passed to the tool.
    pub input: serde_json::Value,
    /// Result returned by the tool server.
    pub output: serde_json::Value,
    /// Wall-clock timestamp when the tool call completed.
    pub timestamp: DateTime<Utc>,
}

/// Errors returned by [`AgentRuntime`] implementations.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// The runtime could not create a new container/MicroVM instance.
    /// Includes Docker daemon errors, image pull failures, and unsupported isolation modes.
    #[error("Failed to spawn instance: {0}")]
    SpawnFailed(String),
    /// The task was dispatched to the instance but the execution itself failed
    /// (e.g. the agent process exited with a non-zero code or timed out).
    #[error("Failed to execute task: {0}")]
    ExecutionFailed(String),
    /// [`AgentRuntime::terminate`] could not stop or remove the instance.
    #[error("Failed to terminate instance: {0}")]
    TerminationFailed(String),
    /// The supplied [`InstanceId`] does not correspond to a known running instance.
    #[error("Instance not found: {0}")]
    InstanceNotFound(String),
    /// The orchestrator does not have permission to perform the requested operation
    /// on the runtime (e.g. Docker socket access denied).
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    /// The execution exceeded its wall-clock timeout and was forcibly terminated.
    /// Contains the timeout duration in seconds for diagnostics.
    #[error("Execution timed out after {0} seconds")]
    TimedOut(u64),
    /// `ensure_image` did not complete within `IMAGE_PULL_TIMEOUT_SECS`.
    /// Surfaces a slow/unreachable registry as a fast, typed failure rather
    /// than an unbounded silent hang in the spawn path.
    #[error("Image pull for '{image}' timed out after {timeout_secs} seconds")]
    ImagePullTimeout {
        /// Fully-qualified image reference whose pull stalled.
        image: String,
        /// Wall-clock budget in seconds that elapsed before timeout.
        timeout_secs: u64,
    },
    /// The host-side FUSE daemon's `Mount` RPC did not return within
    /// `FUSE_MOUNT_TIMEOUT_SECS`. Surfaces a wedged FUSE daemon as a fast,
    /// typed failure rather than an unbounded silent hang in spawn().
    #[error("FUSE Mount RPC for execution {execution_id} volume {volume_id} timed out after {timeout_secs} seconds")]
    FuseMountTimeout {
        /// Execution whose volume mount stalled.
        execution_id: String,
        /// Volume identifier the daemon was asked to mount.
        volume_id: String,
        /// Wall-clock budget in seconds that elapsed before timeout.
        timeout_secs: u64,
    },
    /// The pre-spawn FUSE daemon `Health` probe reported the daemon as
    /// unhealthy (or did not respond within the probe budget). Surfaces a
    /// degraded daemon to the iteration error path before any Mount RPC is
    /// issued, so the user run fails fast with a diagnostic error class.
    #[error("FUSE daemon reported unhealthy: {reason}")]
    FuseDaemonUnhealthy {
        /// Human-readable reason — typically degraded mount counts or a probe
        /// timeout reported by the orchestrator-side health check.
        reason: String,
    },
    /// The execution was cancelled via [`crate::application::execution::ExecutionService::cancel_execution_for_tenant`].
    /// The Supervisor observed the cancellation token and terminated the running instance.
    #[error("Execution was cancelled")]
    Cancelled,
    /// The container exists but the runtime cannot stop or remove it because the
    /// agent process is wedged in an uninterruptible kernel state (typically a
    /// hung FUSE mount or other D-state syscall). Manifests as Podman/Docker
    /// returning HTTP 500 with body "given PID did not die within timeout".
    /// The reaper consumes this variant to drive backoff and quarantine.
    #[error("Container {container_id} could not be killed (engine: {engine}); agent process likely in D-state")]
    Unkillable {
        /// Container ID that the runtime failed to terminate.
        container_id: String,
        /// Engine kind that returned the failure, used to select the operator playbook.
        engine: ContainerEngineKind,
    },
}

/// Identifies which container engine the orchestrator is talking to.
///
/// Detected at startup from the engine's `/version` endpoint and threaded
/// through label generation and error reporting so operators get the right
/// diagnostic playbook (Podman → check FUSE daemon; Docker → generic D-state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContainerEngineKind {
    /// Rootless or rootful Podman engine (production runtime per ADR-067).
    Podman,
    /// Docker engine (development/fallback).
    Docker,
    /// `/version` returned but the payload did not match either engine.
    Unknown,
}

impl ContainerEngineKind {
    /// Wire-label value emitted on the `aegis.runtime` container label.
    pub fn label_value(self) -> &'static str {
        match self {
            ContainerEngineKind::Podman => "podman",
            ContainerEngineKind::Docker => "docker",
            ContainerEngineKind::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ContainerEngineKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label_value())
    }
}

/// Current resource-usage snapshot for a running [`InstanceId`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    /// The identifier of the instance this status describes.
    pub id: InstanceId,
    /// Container/VM state string as reported by the runtime (e.g. `"running"`, `"exited"`).
    pub state: String,
    /// Seconds since the instance was spawned.
    pub uptime_seconds: u64,
    /// Current RSS memory usage in mebibytes.
    pub memory_usage_mb: u64,
    /// CPU utilisation percentage (0.0–100.0).
    pub cpu_usage_percent: f64,
}

/// Core abstraction over isolated execution environments (BC-2 Execution Context).
///
/// Implemented by `ContainerRuntime` (Phase 1) in `crate::infrastructure::runtime` and
/// by `FirecrackerRuntime` (Phase 2, not yet implemented).
///
/// # Invariants
///
/// - `spawn` must return an [`InstanceId`] that can be passed to all other methods.
/// - `execute` is called exactly once per iteration; the instance is not reused.
/// - `terminate` must always succeed even if the instance has already exited.
///
/// # See Also
///
/// ADR-027 (Docker Runtime Implementation), ADR-005 (Iterative Execution Strategy)
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Spawn a new isolated runtime instance from the given configuration.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::SpawnFailed`] if the container/VM cannot be created.
    async fn spawn(&self, config: RuntimeConfig) -> Result<InstanceId, RuntimeError>;

    /// Execute a single iteration task inside a running instance.
    ///
    /// The instance must have been previously created with [`AgentRuntime::spawn`].
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InstanceNotFound`] if `id` is not a live instance,
    /// or [`RuntimeError::ExecutionFailed`] if the agent process errors out.
    async fn execute(&self, id: &InstanceId, input: TaskInput) -> Result<TaskOutput, RuntimeError>;

    /// Terminate and remove the runtime instance.
    ///
    /// Safe to call even if the instance has already exited. After this call the
    /// [`InstanceId`] is no longer valid.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::TerminationFailed`] only if the runtime cannot clean up
    /// (e.g. Docker daemon unreachable).
    async fn terminate(&self, id: &InstanceId) -> Result<(), RuntimeError>;

    /// Query current resource usage for a running instance.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InstanceNotFound`] if `id` is unknown.
    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus, RuntimeError>;
}

// ============================================================================
// Container Step Runner (ADR-050)
// ============================================================================

use crate::domain::workflow::StateName;
use schemars::JsonSchema;
use std::time::Duration;

/// Volume mount configuration for container step execution.
///
/// Defines how a named volume is mounted inside a workflow container step.
/// Part of the Execution bounded context (BC-2) runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContainerVolumeMount {
    /// Volume name (references a workflow-level or execution-level volume)
    pub name: String,
    /// Absolute path inside the container where the volume appears
    pub mount_path: String,
    /// Whether the volume is mounted read-only (default: false)
    #[serde(default)]
    pub read_only: bool,
}

/// Resource constraints for container step execution.
///
/// Configures CPU, memory, and timeout limits for workflow container steps.
/// Part of the Execution bounded context (BC-2) runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct ContainerResources {
    /// CPU quota in millicores (e.g., 1000 = 1 vCPU, 500 = 0.5 vCPU)
    #[serde(default)]
    pub cpu: Option<u32>,
    /// Memory limit as a string (e.g., "512Mi", "2Gi")
    #[serde(default)]
    pub memory: Option<String>,
    /// Hard wall-clock timeout for the entire container execution
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub timeout: Option<Duration>,
}

/// Configuration for a single deterministic CI/CD container step (ADR-050).
///
/// Unlike [`RuntimeConfig`], this does NOT inject bootstrap.py or
/// `AEGIS_ORCHESTRATOR_URL`. The container runs the `command` directly and exits.
#[derive(Debug, Clone)]
pub struct ContainerStepConfig {
    /// Human-readable step label — used in events and Synapse UI
    pub name: String,

    /// Container image reference (already resolved; may be a RegistryImage)
    pub image: String,

    /// Image pull policy
    pub image_pull_policy: ImagePullPolicy,

    /// Argv to execute inside the container (`sh -c` wrapping applied externally
    /// when the caller sets `shell: true` before constructing this config)
    pub command: Vec<String>,

    /// Environment variables
    pub env: HashMap<String, String>,

    /// Working directory inside the container
    pub workdir: Option<String>,

    /// Volume mounts — resolved to NFS mount options by the infrastructure layer
    pub volumes: Vec<ContainerVolumeMount>,

    /// CPU, memory, and timeout limits
    pub resources: Option<ContainerResources>,

    /// Secret-backend path (`secret:path/to/secret`) for private registry credentials
    pub registry_credentials: Option<String>,

    /// Correlation — links this step to the parent workflow execution
    pub execution_id: ExecutionId,

    /// Logical name of the workflow state that triggered this step
    pub state_name: StateName,

    /// If true, the container's root filesystem is mounted read-only (ADR-087 D5).
    pub read_only_root_filesystem: bool,

    /// Override the user the container process runs as (e.g. "65534:65534") (ADR-087 D5).
    pub run_as_user: Option<String>,

    /// Docker network mode for this container step (ADR-087 D5).
    /// When set, overrides the runner-level default network mode.
    pub network_mode: Option<String>,

    /// Workflow execution UUID that owns the workspace volume. Used by the
    /// container step runner to register volumes with the correct
    /// `workflow_execution_id` in the NFS volume registry so FSAL can
    /// authorize `VolumeOwnership::WorkflowExecution` without DB mutations.
    pub workflow_execution_id: Option<uuid::Uuid>,
}

/// Result of a successfully completed container step (ADR-050).
#[derive(Debug, Clone)]
pub struct ContainerStepResult {
    /// Process exit code (0 = success by convention)
    pub exit_code: i32,

    /// Captured stdout (tail-truncated at 1 MiB)
    pub stdout: String,

    /// Captured stderr (tail-truncated at 1 MiB)
    pub stderr: String,

    /// Wall-clock duration of the container execution in milliseconds
    pub duration_ms: u64,
}

/// Errors that can arise during a container step execution (ADR-050).
#[derive(Debug, Error)]
pub enum ContainerStepError {
    /// The container image could not be pulled from the registry
    #[error("image pull failed for '{image}': {error}")]
    ImagePullFailed { image: String, error: String },

    /// The step exceeded its configured timeout
    #[error("container step timed out after {timeout_secs}s")]
    TimeoutExpired { timeout_secs: u64 },

    /// A required volume could not be mounted
    #[error("volume mount failed for '{volume}': {error}")]
    VolumeMountFailed { volume: String, error: String },

    /// The container was killed because it exceeded a resource limit
    #[error("resource exhausted: {detail}")]
    ResourceExhausted { detail: String },

    /// An unexpected Docker API error
    #[error("docker error: {0}")]
    DockerError(String),
}

/// Domain trait for executing deterministic CI/CD container steps (ADR-050).
///
/// # Invariants
/// - Implementors MUST NOT inject bootstrap.py or set `AEGIS_ORCHESTRATOR_URL`
/// - Implementors MUST clean up the container (stop + remove) on both success and error
/// - Volumes MUST be mounted via FUSE transport (ADR-107); no NFS fallback is permitted
/// - stdout + stderr MUST be tail-truncated at 1 MiB to prevent memory exhaustion
#[async_trait]
pub trait ContainerStepRunner: Send + Sync {
    /// Execute a single CI/CD container step and return its result.
    ///
    /// The implementation is responsible for the full container lifecycle:
    /// pull image → create → start → capture output → inspect exit code → remove.
    async fn run_step(
        &self,
        config: ContainerStepConfig,
    ) -> Result<ContainerStepResult, ContainerStepError>;
}
