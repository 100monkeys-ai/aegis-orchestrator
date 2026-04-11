// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Agent Domain Types (BC-1, AGENTS.md §Agent Domain)
//!
//! Defines the `Agent` aggregate root and the `AgentManifest` value object that
//! declares all agent capabilities, constraints, and execution strategy.
//!
//! ## Key Types
//!
//! | Type | Description |
//! |------|-------------|
//! | [`AgentId`] | UUID newtype uniquely identifying an agent definition |
//! | [`Agent`] | Aggregate root tracking manifest + lifecycle status |
//! | [`AgentManifest`] | Kubernetes-style YAML config (`apiVersion/kind/metadata/spec`) |
//! | [`AgentSpec`] | Parsed `spec` stanza: runtime, security, validation, resources |
//! | [`SecurityConfig`] | Inline security policy from the manifest |
//! | [`VolumeSpec`] | Volume declaration in `spec.volumes[]` |
//!
//! ## Manifest Schema
//!
//! # Code Quality Principles
//!
//! - Validate agent manifests before they cross into runtime or persistence code.
//! - Keep the manifest model declarative and free of transport concerns.
//! - Fail closed on missing runtime, security, or resource requirements.
//!
//! ```yaml
//! apiVersion: aegis.ai/v1
//! kind: Agent
//! metadata:
//!   name: code-reviewer
//! spec:
//!   runtime:
//!     language: python
//!     version: "3.11"
//!     model: default   # alias resolved to a real model in aegis-config.yaml
//!   security:
//!     network:
//!       allowlist: ["github.com"]
//!   resources:
//!     max_iterations: 5
//!     timeout_seconds: 300
//! ```
//!
//! See AGENTS.md §Agent Domain ubiquitous language.

pub use crate::domain::shared_kernel::{AgentId, ImagePullPolicy};

use crate::domain::tenant::TenantId;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Visibility scope for an agent definition (ADR-076 / ADR-097).
///
/// Scope determines which principals can discover and execute an agent.
/// Scope is mutable via explicit promote/demote operations (not via save).
///
/// Two-level hierarchy (broadest to narrowest): Global > Tenant
///
/// Invariants:
/// - `Global` agents MUST have `tenant_id == TenantId::system()`.
/// - `Tenant` agents belong to a single tenant.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentScope {
    /// Platform-wide: visible to all tenants, managed by operators.
    /// Stored with `tenant_id = "aegis-system"`.
    Global,

    /// Tenant-wide: visible to all users within the owning tenant.
    #[default]
    Tenant,
}

impl AgentScope {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            AgentScope::Global => "global",
            AgentScope::Tenant => "tenant",
        }
    }
}

impl std::fmt::Display for AgentScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_db_str())
    }
}

impl std::str::FromStr for AgentScope {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "global" => Ok(AgentScope::Global),
            "tenant" => Ok(AgentScope::Tenant),
            _ => Err(anyhow::anyhow!("unknown scope: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    #[serde(default)]
    pub tenant_id: TenantId,
    #[serde(default)]
    pub scope: AgentScope,
    pub name: String,
    pub manifest: AgentManifest,
    pub status: AgentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Kubernetes-style Agent Manifest (v1.0)
/// Follows spec: MANIFEST_SPEC_V1.md
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentManifest {
    /// API version (e.g., "100monkeys.ai/v1")
    #[serde(rename = "apiVersion")]
    pub api_version: String,

    /// Resource kind (must be "Agent")
    pub kind: String,

    /// Kubernetes-style metadata
    pub metadata: ManifestMetadata,

    /// Agent specification
    pub spec: AgentSpec,
}

/// Kubernetes-style metadata
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct ManifestMetadata {
    /// Unique agent name (DNS label format)
    pub name: String,

    /// Manifest schema version (semantic versioning)
    pub version: String,

    /// Optional human-readable description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Optional labels for categorization
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub labels: std::collections::HashMap<String, String>,

    /// Optional annotations (non-identifying metadata)
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub annotations: std::collections::HashMap<String, String>,
}

/// Agent specification (the main configuration)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct AgentSpec {
    /// Runtime configuration
    pub runtime: RuntimeConfig,

    /// Optional task definition
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskConfig>,

    /// Optional context attachments
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ContextItem>,

    /// Optional execution strategy
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionStrategy>,

    /// Optional security permissions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<SecurityConfig>,

    /// Optional scheduling configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleConfig>,

    /// Optional tools/MCP servers
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,

    /// Optional environment variables
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,

    /// Optional volume mounts
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeSpec>,

    /// Optional advanced configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advanced: Option<AdvancedConfig>,

    /// Optional JSON Schema describing execution inputs.
    /// Used by the Zaru client context panel to render typed form fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,

    /// Optional named security context for this agent's executions (ADR-102).
    /// When set, overrides the caller's context at runtime.
    /// Only `aegis-system-*` names permitted; only Operators/ServiceAccounts may set this.
    #[serde(
        rename = "security_context",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub security_context: Option<String>,

    /// Optional egress handler fired after execution completes (ADR-103).
    ///
    /// When set, the [`crate::application::output_handler_service::OutputHandlerService`]
    /// invokes this handler immediately after `ExecutionCompleted` is published.
    /// If `required` is `true` and the handler fails, the execution is marked failed.
    #[serde(
        rename = "output_handler",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub output_handler: Option<crate::domain::output_handler::OutputHandlerConfig>,
}

/// Runtime configuration
///
/// Supports two mutually exclusive runtime modes:
/// - **StandardRuntime**: language + version (resolved to official Docker image)
/// - **CustomRuntime**: image (user-supplied fully-qualified container image)
///
/// Validation ensures exactly one mode is specified (not both).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct RuntimeConfig {
    /// Programming language (python, javascript, typescript, rust, go)
    /// Mutually exclusive with `image` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    /// Language version (e.g., "3.11", "20", "1.75")
    /// Mutually exclusive with `image` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Custom Docker image reference (fully-qualified: registry/repo:tag)
    /// Mutually exclusive with `language` + `version` fields.
    /// Example: "ghcr.io/myorg/agent:v1.0.0"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Image pull policy (Always, IfNotPresent, Never)
    /// Controls whether the runtime image is pulled from registry.
    #[serde(default = "default_image_pull_policy")]
    pub image_pull_policy: ImagePullPolicy,

    /// Optional isolation mode (inherit, firecracker, docker, process)
    #[serde(default = "default_isolation")]
    pub isolation: String,

    /// LLM model alias for this agent's primary LLM calls.
    /// Maps to a concrete provider + model in `aegis-config.yaml`.
    /// Standard aliases: `default`, `fast`, `smart`, `cheap`, `local`.
    #[serde(default = "default_model_alias")]
    pub model: String,

    /// Optional temperature override for this agent's LLM calls.
    /// Lower values (0.1-0.3) for deterministic agents (judges, validators).
    /// Higher values (0.5-0.7) for creative agents (generators).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

impl RuntimeConfig {
    /// Validate mutual exclusion: exactly one of (language+version) or (image) must be specified.
    pub fn validate(&self) -> Result<(), String> {
        let has_standard = self.language.is_some() && self.version.is_some();
        let has_language_without_version = self.language.is_some() && self.version.is_none();
        let has_version_without_language = self.version.is_some() && self.language.is_none();
        let has_custom = self.image.is_some();

        // Reject partial standard runtime specification
        if has_language_without_version {
            return Err("language requires version to be specified".to_string());
        }
        if has_version_without_language {
            return Err("version requires language to be specified".to_string());
        }

        // Enforce mutual exclusion
        if has_standard && has_custom {
            return Err(
                "cannot specify both image and language+version (mutually exclusive)".to_string(),
            );
        }

        // Require at least one
        if !has_standard && !has_custom {
            return Err(
                "must specify either standard runtime (language+version) or custom runtime (image)"
                    .to_string(),
            );
        }

        // Validate custom image format (must be fully-qualified)
        if let Some(img) = &self.image {
            if !img.contains('/') {
                return Err(
                    "image must be fully-qualified: registry/repo:tag (e.g., ghcr.io/org/image:v1.0)"
                        .to_string(),
                );
            }
        }

        Ok(())
    }

    /// Determine runtime type (standard or custom)
    pub fn runtime_type(&self) -> RuntimeType {
        if self.image.is_some() {
            RuntimeType::Custom
        } else {
            RuntimeType::Standard
        }
    }
}

/// Runtime type discriminator
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeType {
    /// AEGIS standard runtime (language + version pair)
    Standard,
    /// User-supplied custom runtime (Docker image)
    Custom,
}

/// Volume specification in agent manifest
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct VolumeSpec {
    /// Volume name (used as identifier)
    pub name: String,

    /// Storage class: "ephemeral" or "persistent"
    pub storage_class: String,

    /// New volume backend type (e.g., "seaweedfs", "opendal", "hostPath", "seal")
    /// Defaults to "seaweedfs" if omitted for backward compatibility.
    #[serde(default = "default_volume_type", rename = "type")]
    pub volume_type: String,

    /// External provider name (e.g., "s3", "gcs" for OpenDAL)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    /// Provider-specific configuration (bucket name, paths, etc)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,

    /// Mount path inside container
    pub mount_path: String,

    /// Access mode: "read-only" or "read-write"
    pub access_mode: String,

    /// Size limit (e.g., "1Gi", "500Mi")
    pub size_limit: String,

    /// TTL in hours (only for ephemeral volumes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_hours: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ScheduleConfig {
    Cron {
        cron: String,
        timezone: String,
        #[serde(default = "default_true")]
        enabled: bool,
    },
    Interval {
        seconds: u64,
        #[serde(default = "default_true")]
        enabled: bool,
    },
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct TaskConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContextItem {
    Text {
        content: String,
        description: Option<String>,
    },
    File {
        path: String,
        description: Option<String>,
    },
    Directory {
        path: String,
        description: Option<String>,
    },
    Url {
        url: String,
        description: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct ExecutionStrategy {
    #[serde(default)]
    pub mode: ExecutionMode,
    #[serde(default = "default_max_retries", alias = "max_iterations")]
    pub max_retries: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iteration_timeout: Option<String>,
    #[serde(default = "default_llm_timeout")]
    pub llm_timeout_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_validation: Option<ValidationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryConfig>,
}

impl Default for ExecutionStrategy {
    fn default() -> Self {
        Self {
            mode: ExecutionMode::OneShot,
            max_retries: default_max_retries(),
            iteration_timeout: None,
            llm_timeout_seconds: default_llm_timeout(),
            validation: None,
            tool_validation: None,
            delivery: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionMode {
    #[default]
    OneShot,
    Iterative,
}

/// Specification for a single validation step in the ordered pipeline.
///
/// Used in `spec.execution.validation` as an ordered list. Each entry is evaluated
/// in sequence; a step whose `min_score` is not met causes the iteration to be marked
/// as failed and triggers the refinement loop.
///
/// `semantic` and `multi_judge` variants spawn isolated child executions (ADR-016).
///
/// # YAML example
/// ```yaml
/// spec:
///   execution:
///     validation:
///       - type: exit_code
///         expected: 0
///       - type: json_schema
///         schema:
///           type: object
///           required: [result]
///       - type: semantic
///         judge_agent: output-judge
///         criteria: "Output must be idiomatic Rust with no unsafe blocks"
///         min_score: 0.8
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ValidatorSpec {
    /// Passes when the agent's last exit code matches `expected` (default 0).
    ExitCode {
        #[serde(default)]
        expected: i32,
        #[serde(default = "default_min_score_full")]
        min_score: f64,
    },
    /// Validates the agent's output against a JSON Schema document.
    JsonSchema {
        schema: serde_json::Value,
        #[serde(default = "default_min_score_full")]
        min_score: f64,
    },
    /// Validates the agent's output with a regular expression.
    Regex {
        pattern: String,
        /// Which output stream to match against: `"stdout"` (default) or `"stderr"`.
        #[serde(default = "default_stdout_target")]
        target: String,
        #[serde(default = "default_min_score_full")]
        min_score: f64,
    },
    /// Spawns a judge agent as a child execution to semantically evaluate output.
    ///
    /// The judge agent is looked up by name in the agent registry. Implements
    /// ADR-016 (Agent-as-Judge) and ADR-017 (Gradient Validation).
    Semantic {
        /// Name of the judge agent to spawn (must be a deployed agent).
        judge_agent: String,
        /// Human-readable description of the evaluation criteria passed to the judge.
        criteria: String,
        /// Minimum passing score (0.0–1.0). Default: 0.7.
        #[serde(default = "default_semantic_min_score")]
        min_score: f64,
        /// Minimum confidence required; scores below this threshold are treated as fails.
        #[serde(default)]
        min_confidence: f64,
        /// Timeout in seconds waiting for the judge execution to complete.
        #[serde(default = "default_validation_timeout")]
        timeout_seconds: u64,
    },
    /// Spawns multiple judge agents in parallel and aggregates their verdicts.
    ///
    /// Implements ADR-016 (Agent-as-Judge) and ADR-017 (Gradient Validation).
    MultiJudge {
        /// Names of judge agents to spawn (must be deployed agents).
        judges: Vec<String>,
        /// Consensus strategy used to combine individual judge scores.
        #[serde(default = "default_consensus_strategy")]
        #[schemars(skip)]
        consensus: crate::domain::workflow::ConsensusStrategy,
        /// Minimum number of judges that must complete for a valid result.
        #[serde(default = "default_one")]
        min_judges_required: usize,
        /// Human-readable description of the evaluation criteria passed to each judge.
        criteria: String,
        /// Minimum passing score (0.0–1.0). Default: 0.7.
        #[serde(default = "default_semantic_min_score")]
        min_score: f64,
        /// Minimum confidence required; scores below this threshold are treated as fails.
        #[serde(default)]
        min_confidence: f64,
        /// Timeout in seconds waiting for all judge executions to complete.
        #[serde(default = "default_validation_timeout")]
        timeout_seconds: u64,
    },
}

/// Ordered list of validation steps executed after each iteration.
///
/// Steps run in declaration order; the first step that fails prevents subsequent
/// steps from running and marks the iteration for refinement.
pub type ValidationConfig = Vec<ValidatorSpec>;

fn default_min_score_full() -> f64 {
    1.0
}
fn default_semantic_min_score() -> f64 {
    0.7
}
fn default_stdout_target() -> String {
    "stdout".to_string()
}
fn default_one() -> usize {
    1
}
fn default_consensus_strategy() -> crate::domain::workflow::ConsensusStrategy {
    crate::domain::workflow::ConsensusStrategy::WeightedAverage
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct DeliveryConfig {
    pub destinations: Vec<DeliveryDestination>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct DeliveryDestination {
    pub name: String,
    pub condition: DeliveryCondition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<TransformConfig>,
    #[serde(flatten)]
    pub config: DeliveryType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryCondition {
    OnSuccess,
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct TransformConfig {
    pub script: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_validation_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DeliveryType {
    Email { email: EmailConfig },
    Webhook { webhook: WebhookConfig },
    Rest { rest: RestConfig },
    Sms { sms: SmsConfig },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct EmailConfig {
    pub to: String,
    pub subject: String,
    pub body_template: Option<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(default = "default_post")]
    pub method: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct RestConfig {
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct SmsConfig {
    pub to: String,
    pub message: String,
}

/// Security configuration (renamed from PermissionsConfig to match spec)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct SecurityConfig {
    #[serde(default)]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub filesystem: FilesystemPolicy,
    #[serde(default)]
    pub resources: ResourceLimits,
}

/// Network access policy
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, JsonSchema)]
pub struct NetworkPolicy {
    /// Policy mode: "allow" (allowlist) | "deny" (denylist) | "none"
    #[serde(default = "default_network_mode")]
    pub mode: String,

    /// Allowed domains/IPs (for 'allow' mode)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowlist: Vec<String>,

    /// Denied domains/IPs (for 'deny' mode)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denylist: Vec<String>,
}

/// Filesystem access policy
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, JsonSchema)]
pub struct FilesystemPolicy {
    /// Readable paths
    #[serde(default)]
    pub read: Vec<String>,

    /// Writable paths
    #[serde(default)]
    pub write: Vec<String>,

    /// Read-only mode
    #[serde(default)]
    pub read_only: bool,
}

/// Resource limits
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct ResourceLimits {
    /// CPU quota in millicores (1000 = 1 CPU core)
    #[serde(default = "default_cpu")]
    pub cpu: u32,

    /// Memory limit (human-readable: "512Mi", "1Gi", "2G")
    #[serde(default = "default_memory")]
    pub memory: String,

    /// Disk space limit
    #[serde(default = "default_disk")]
    pub disk: String,

    /// Execution timeout (human-readable duration or seconds)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

impl ResourceLimits {
    /// Parse the `timeout` field (e.g., "60s", "5m", "1h") to seconds.
    ///
    /// Supported suffixes: `s` (seconds), `m` (minutes), `h` (hours).
    /// Bare integers are treated as seconds.
    /// Returns `None` if `timeout` is unset or cannot be parsed.
    pub fn parse_timeout_seconds(&self) -> Option<u64> {
        let raw = self.timeout.as_deref()?;
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        if raw.ends_with('h') {
            raw.trim_end_matches('h')
                .trim()
                .parse::<u64>()
                .ok()
                .map(|v| v * 3600)
        } else if raw.ends_with('m') {
            raw.trim_end_matches('m')
                .trim()
                .parse::<u64>()
                .ok()
                .map(|v| v * 60)
        } else if raw.ends_with('s') {
            raw.trim_end_matches('s').trim().parse::<u64>().ok()
        } else {
            raw.parse::<u64>().ok()
        }
    }

    /// Parse memory/disk string (e.g., "512Mi", "1Gi") to bytes
    /// Returns None if parsing fails
    pub fn parse_size_to_bytes(size_str: &str) -> Option<u64> {
        let size_str = size_str.trim();
        if size_str.ends_with("Gi") {
            size_str
                .trim_end_matches("Gi")
                .parse::<u64>()
                .ok()
                .map(|v| v * 1024 * 1024 * 1024)
        } else if size_str.ends_with("Mi") {
            size_str
                .trim_end_matches("Mi")
                .parse::<u64>()
                .ok()
                .map(|v| v * 1024 * 1024)
        } else if size_str.ends_with("Ki") {
            size_str
                .trim_end_matches("Ki")
                .parse::<u64>()
                .ok()
                .map(|v| v * 1024)
        } else {
            size_str.parse::<u64>().ok()
        }
    }

    /// Get memory limit in bytes
    pub fn memory_bytes(&self) -> Option<u64> {
        Self::parse_size_to_bytes(&self.memory)
    }

    /// Get disk limit in bytes
    pub fn disk_bytes(&self) -> Option<u64> {
        Self::parse_size_to_bytes(&self.disk)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct AdvancedConfig {
    #[serde(default)]
    pub warm_pool_size: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheckConfig>,
    /// Custom bootstrap script path (for CustomRuntime only)
    /// If specified, orchestrator will use this script instead of default bootstrap.py
    /// The script must be present inside the custom container image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct HealthCheckConfig {
    pub path: String,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
}

// MetadataConfig removed - now handled by ManifestMetadata in K8s format

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Active,
    Paused,
    Archived,
    Failed,
}

// Defaults
fn default_true() -> bool {
    true
}

fn default_volume_type() -> String {
    "seaweedfs".to_string()
}
fn default_max_retries() -> u32 {
    5
}
fn default_llm_timeout() -> u64 {
    300
}
fn default_validation_timeout() -> u64 {
    // 300 s matches default_llm_timeout so a judge's LLM call (typically 30–120 s)
    // completes well within the polling window. The previous 30 s default caused
    // SemanticAgentValidator to time out before the judge finished, resulting in
    // Err(e) in the supervisor and missing Feedback: sections in subsequent prompts.
    300
}
fn default_post() -> String {
    "POST".to_string()
}
fn default_cpu() -> u32 {
    1000
}
fn default_memory() -> String {
    "512Mi".to_string()
}
fn default_disk() -> String {
    "1Gi".to_string()
}

fn default_isolation() -> String {
    "inherit".to_string()
}

fn default_model_alias() -> String {
    "default".to_string()
}
fn default_network_mode() -> String {
    "allow".to_string()
}

fn default_image_pull_policy() -> ImagePullPolicy {
    ImagePullPolicy::IfNotPresent
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpu: default_cpu(),
            memory: default_memory(),
            disk: default_disk(),
            timeout: None,
        }
    }
}

impl Agent {
    pub fn new(manifest: AgentManifest) -> Self {
        let now = Utc::now();
        Self {
            id: AgentId::new(),
            tenant_id: TenantId::default(),
            scope: AgentScope::default(),
            name: manifest.metadata.name.clone(),
            manifest,
            status: AgentStatus::Active,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn update_manifest(&mut self, manifest: AgentManifest) {
        self.name = manifest.metadata.name.clone();
        self.manifest = manifest;
        self.updated_at = Utc::now();
    }

    pub fn pause(&mut self) {
        self.status = AgentStatus::Paused;
        self.updated_at = Utc::now();
    }

    pub fn resume(&mut self) {
        self.status = AgentStatus::Active;
        self.updated_at = Utc::now();
    }

    pub fn archive(&mut self) {
        self.status = AgentStatus::Archived;
        self.updated_at = Utc::now();
    }
}

impl AgentManifest {
    /// Validate the manifest structure and constraints
    pub fn validate(&self) -> Result<(), String> {
        // Validate API version
        if self.api_version != "100monkeys.ai/v1" {
            return Err(format!(
                "Invalid apiVersion: expected '100monkeys.ai/v1', got '{}'",
                self.api_version
            ));
        }

        // Validate kind
        if self.kind != "Agent" {
            return Err(format!(
                "Invalid kind: expected 'Agent', got '{}'",
                self.kind
            ));
        }

        // Validate name format (DNS label: lowercase alphanumeric with hyphens)
        if self.metadata.name.is_empty() {
            return Err("metadata.name cannot be empty".to_string());
        }
        for ch in self.metadata.name.chars() {
            if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '-' {
                return Err(format!(
                    "Invalid metadata.name: '{}' must be lowercase alphanumeric with hyphens",
                    self.metadata.name
                ));
            }
        }
        if self.metadata.name.starts_with('-') || self.metadata.name.ends_with('-') {
            return Err(format!(
                "Invalid metadata.name: '{}' cannot start or end with hyphen",
                self.metadata.name
            ));
        }

        // Execution strategy is optional and validated by its own invariants.

        // spec.task is required — an agent without a task block has no instruction and cannot run
        match &self.spec.task {
            None => {
                return Err(
                    "spec.task is required — agent must have a non-empty task.instruction"
                        .to_string(),
                );
            }
            Some(task) => match &task.instruction {
                None => {
                    return Err(
                        "spec.task.instruction is required — agent must have a non-empty task.instruction"
                            .to_string(),
                    );
                }
                Some(s) if s.trim().is_empty() => {
                    return Err(
                        "spec.task.instruction must not be empty — agent must have a non-empty task.instruction"
                            .to_string(),
                    );
                }
                _ => {}
            },
        }

        Ok(())
    }

    /// Get the runtime as a combined string (for standard runtimes)
    /// Returns None for custom runtimes (use image field instead)
    pub fn runtime_string(&self) -> Option<String> {
        match (&self.spec.runtime.language, &self.spec.runtime.version) {
            (Some(lang), Some(ver)) => Some(format!("{lang}:{ver}")),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manifest(name: &str) -> AgentManifest {
        AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: ManifestMetadata {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                description: None,
                labels: std::collections::HashMap::new(),
                annotations: std::collections::HashMap::new(),
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
                    instruction: Some("Do something useful".to_string()),
                    prompt_template: None,
                    input_data: None,
                }),
                context: vec![],
                execution: None,
                security: None,
                schedule: None,
                tools: vec![],
                env: std::collections::HashMap::new(),
                volumes: vec![],
                advanced: None,
                input_schema: None,
                security_context: None,
                output_handler: None,
            },
        }
    }

    // ── AgentId ──────────────────────────────────────────────────────────────

    #[test]
    fn test_agent_id_new_is_unique() {
        let a = AgentId::new();
        let b = AgentId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_agent_id_from_valid_string() {
        let id = AgentId::new();
        let s = id.0.to_string();
        let parsed = AgentId::from_string(&s).expect("should parse valid UUID");
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_agent_id_from_invalid_string() {
        assert!(AgentId::from_string("not-a-uuid").is_err());
    }

    #[test]
    fn test_agent_id_default() {
        let a = AgentId::default();
        let b = AgentId::default();
        assert_ne!(a, b, "default() should generate unique IDs");
    }

    // ── Agent lifecycle ───────────────────────────────────────────────────────

    #[test]
    fn test_agent_new_is_active() {
        let manifest = make_manifest("my-agent");
        let agent = Agent::new(manifest.clone());
        assert_eq!(agent.status, AgentStatus::Active);
        assert_eq!(agent.name, "my-agent");
        assert_eq!(agent.manifest, manifest);
    }

    #[test]
    fn test_agent_pause_and_resume() {
        let mut agent = Agent::new(make_manifest("test-agent"));
        agent.pause();
        assert_eq!(agent.status, AgentStatus::Paused);
        agent.resume();
        assert_eq!(agent.status, AgentStatus::Active);
    }

    #[test]
    fn test_agent_archive() {
        let mut agent = Agent::new(make_manifest("test-agent"));
        agent.archive();
        assert_eq!(agent.status, AgentStatus::Archived);
    }

    #[test]
    fn test_agent_update_manifest() {
        let mut agent = Agent::new(make_manifest("old-name"));
        let new_manifest = make_manifest("new-name");
        agent.update_manifest(new_manifest.clone());
        assert_eq!(agent.name, "new-name");
        assert_eq!(agent.manifest, new_manifest);
    }

    // ── AgentManifest validation ──────────────────────────────────────────────

    #[test]
    fn test_manifest_valid() {
        let manifest = make_manifest("my-agent");
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn test_manifest_invalid_api_version() {
        let mut manifest = make_manifest("my-agent");
        manifest.api_version = "wrong/v1".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(err.contains("apiVersion"));
    }

    #[test]
    fn test_manifest_invalid_kind() {
        let mut manifest = make_manifest("my-agent");
        manifest.kind = "WrongKind".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(err.contains("kind"));
    }

    #[test]
    fn test_manifest_empty_name() {
        let mut manifest = make_manifest("my-agent");
        manifest.metadata.name = "".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(err.contains("name"));
    }

    #[test]
    fn test_manifest_uppercase_name_rejected() {
        let mut manifest = make_manifest("my-agent");
        manifest.metadata.name = "MyAgent".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(err.contains("lowercase"));
    }

    #[test]
    fn test_manifest_leading_hyphen_rejected() {
        let mut manifest = make_manifest("my-agent");
        manifest.metadata.name = "-agent".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(err.contains("hyphen"));
    }

    #[test]
    fn test_manifest_trailing_hyphen_rejected() {
        let mut manifest = make_manifest("my-agent");
        manifest.metadata.name = "agent-".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(err.contains("hyphen"));
    }

    // ── ResourceLimits ────────────────────────────────────────────────────────

    #[test]
    fn test_resource_limits_parse_gibibytes() {
        assert_eq!(
            ResourceLimits::parse_size_to_bytes("1Gi"),
            Some(1024 * 1024 * 1024)
        );
        assert_eq!(
            ResourceLimits::parse_size_to_bytes("2Gi"),
            Some(2 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn test_resource_limits_parse_mebibytes() {
        assert_eq!(
            ResourceLimits::parse_size_to_bytes("512Mi"),
            Some(512 * 1024 * 1024)
        );
    }

    #[test]
    fn test_resource_limits_parse_kibibytes() {
        assert_eq!(ResourceLimits::parse_size_to_bytes("64Ki"), Some(64 * 1024));
    }

    #[test]
    fn test_resource_limits_parse_plain_bytes() {
        assert_eq!(ResourceLimits::parse_size_to_bytes("1024"), Some(1024));
    }

    #[test]
    fn test_resource_limits_parse_invalid() {
        assert_eq!(ResourceLimits::parse_size_to_bytes("invalid"), None);
    }

    #[test]
    fn test_resource_limits_memory_bytes() {
        let limits = ResourceLimits {
            cpu: 1000,
            memory: "256Mi".to_string(),
            disk: "1Gi".to_string(),
            timeout: None,
        };
        assert_eq!(limits.memory_bytes(), Some(256 * 1024 * 1024));
        assert_eq!(limits.disk_bytes(), Some(1024 * 1024 * 1024));
    }

    #[test]
    fn test_resource_limits_default() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.cpu, 1000);
        assert_eq!(limits.memory, "512Mi");
        assert_eq!(limits.disk, "1Gi");
        assert!(limits.timeout.is_none());
    }

    #[test]
    fn test_tool_validation_parsing() {
        let yaml = r#"
apiVersion: aegis.ai/v1
kind: Agent
metadata:
  name: test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    model: smart
  volumes: []
  execution:
    mode: iterative
    max_iterations: 5
    tool_validation:
      - type: semantic
        judge_agent: "code-quality-judge"
        criteria: "Test criteria"
        min_score: 0.85
        timeout_seconds: 60
  security:
    network:
      mode: block
      allowlist: []
    filesystem:
      read: []
      write: []
    resources: {}
  tools: []
"#;
        let manifest: AgentManifest = serde_yaml::from_str(yaml).unwrap();
        let exec = manifest.spec.execution.unwrap();
        let tool_val = exec.tool_validation.unwrap();
        assert_eq!(tool_val.len(), 1);
        assert!(
            matches!(&tool_val[0], ValidatorSpec::Semantic { .. }),
            "Expected semantic validator"
        );
        if let ValidatorSpec::Semantic { judge_agent, .. } = &tool_val[0] {
            assert_eq!(judge_agent, "code-quality-judge");
        }
    }
}
