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
//! | `DockerRuntime` | Phase 1 (current) | `crate::infrastructure::runtime` |
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

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use chrono::{DateTime, Utc};
use crate::domain::volume::VolumeMount;

/// Configuration for spawning an isolated agent runtime instance.
///
/// Parsed from the `spec.runtime` section of an agent manifest and passed directly
/// to [`AgentRuntime::spawn`]. The runtime implementation converts this into a Docker
/// container (Phase 1) or Firecracker MicroVM (Phase 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Target programming language (e.g. `"python"`, `"typescript"`, `"rust"`).
    /// Combined with `version` to derive a Docker image tag via [`RuntimeConfig::to_image`].
    pub language: String,
    /// Language runtime version (e.g. `"3.12"`, `"20"`, `"1.76"`). Used in image tag.
    pub version: String,
    /// Isolation mode. Must be `"docker"` or `"inherit"` in Phase 1.
    /// `"firecracker"` and `"process"` are rejected by [`RuntimeConfig::validate_isolation`]
    /// until their respective phases are implemented.
    pub isolation: String,
    /// Additional environment variables injected into the container at spawn time.
    pub env: HashMap<String, String>,
    /// If `true`, the runtime will `docker pull` the image before spawning if it is
    /// not already present locally. Useful in CI; disable for air-gapped deployments.
    pub autopull: bool,
    /// CPU, memory, and disk resource limits applied to the container.
    pub resources: ResourceLimits,
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
/// three to bound agent blast radius.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// CPU quota expressed as millicores (1000 = 1 full CPU core).
    /// Passed to Docker as `--cpus` (divided by 1000).
    pub cpu_millis: Option<u32>,
    /// Maximum RSS memory in bytes (`--memory` in Docker).
    pub memory_bytes: Option<u64>,
    /// Maximum ephemeral disk usage in bytes (enforced via volume quota).
    pub disk_bytes: Option<u64>,
}

impl RuntimeConfig {
    /// Derive the Docker image tag from `language` and `version`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use aegis_core::domain::runtime::RuntimeConfig;
    /// let cfg = RuntimeConfig { language: "python".to_string(), version: "3.12".to_string(), ..Default::default() };
    /// assert_eq!(cfg.to_image(), "python:3.12");
    /// ```
    pub fn to_image(&self) -> String {
        match self.language.as_str() {
            "python" => format!("python:{}", self.version),
            "javascript" | "typescript" => format!("node:{}", self.version),
            "rust" => format!("rust:{}", self.version),
            "go" => format!("golang:{}", self.version),
            _ => format!("{}:{}", self.language, self.version),
        }
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
                "Firecracker isolation is not yet implemented. Use 'docker' or 'inherit'.".to_string()
            )),
            "process" => Err(RuntimeError::SpawnFailed(
                "Process isolation is not yet implemented. Use 'docker' or 'inherit'.".to_string()
            )),
            other => Err(RuntimeError::SpawnFailed(
                format!("Unknown isolation mode '{}'. Supported: docker, inherit", other)
            )),
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
/// Implemented by `DockerRuntime` (Phase 1) in `crate::infrastructure::runtime` and
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
