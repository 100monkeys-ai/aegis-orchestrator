// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// An AEGIS agent manifest (agent.yaml) - v1.1
/// Conforms to MANIFEST_SPEC_V1.md
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    /// Required: Schema version (must be "1.1")
    #[serde(default = "default_version")]
    pub version: String,
    
    /// Optional: Execution targets for node routing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_targets: Option<Vec<String>>,
    
    /// Required: Agent definition
    pub agent: AgentSpec,
    
    /// Optional: Scheduling configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Schedule>,
    
    /// Optional: Task definition (instructions, skills, input)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<Task>,
    
    /// Optional: Context attachments
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<ContextItem>>,
    
    /// Optional: 100monkeys iterative execution configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<Execution>,
    
    /// Optional: Security permissions (default: deny all)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
    
    /// Optional: MCP tools
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    
    /// Optional: Environment variables
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    
    /// Optional: Advanced configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advanced: Option<Advanced>,
    
    /// Optional: Metadata (not interpreted by system)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Required: Agent name (kebab-case)
    pub name: String,
    
    /// Required: Runtime (e.g., "python:3.11", "node:20")
    pub runtime: String,
    
    /// Optional: Auto-pull image (default: true)
    #[serde(default = "default_true")]
    pub autopull: bool,
    
    /// Optional: Enable Cortex memory
    #[serde(default)]
    pub memory: bool,
    
    /// Optional: Description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    
    /// Optional: Semantic version
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    
    /// Optional: Timeout in seconds (default: 300)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    /// Scheduling type
    #[serde(rename = "type")]
    pub schedule_type: ScheduleType,
    
    /// Cron expression (if type is "cron")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    
    /// Timezone (e.g., "America/New_York")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    
    /// Whether scheduling is active
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleType {
    Cron,
    Interval,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Pre-built skill packages from agentskills.io
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agentskills: Option<Vec<String>>,
    
    /// High-level instruction
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    
    /// Custom prompt template with placeholders: {agent_instruction}, {user_input}
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
    
    /// Structured input parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_data: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    /// Context type
    #[serde(rename = "type")]
    pub context_type: ContextType,
    
    /// Text content (for type "text")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    
    /// File/directory path (for types "file", "directory")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    
    /// URL (for type "url")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    
    /// Description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextType {
    Text,
    File,
    Directory,
    Url,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    /// Execution mode
    #[serde(default = "default_one_shot")]
    pub mode: ExecutionMode,
    
    /// Maximum refinement attempts (1-20, default: 5)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    
    /// Timeout per iteration in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_per_iteration: Option<u32>,
    
    /// Validation configuration (MVP: system validation only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationConfig>,
    
    /// Delivery configuration (Phase 2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionMode {
    OneShot,
    Iterative,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationConfig {
    /// System validation (exit code, stderr)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemValidation>,
    
    /// Output format validation (Phase 2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputValidation>,
    
    /// Custom script validation (Phase 2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptValidation>,
    
    /// Semantic validation (LLM-as-a-Judge, Phase 2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic: Option<SemanticValidation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemValidation {
    /// Must exit with code 0
    #[serde(default = "default_true")]
    pub must_succeed: bool,
    
    /// Allow stderr output
    #[serde(default)]
    pub allow_stderr: bool,
    
    /// Timeout in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputValidation {
    /// Expected format
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    
    /// JSON Schema for validation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    
    /// Regex pattern
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptValidation {
    /// Path to validation script
    pub path: String,
    
    /// Script description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    
    /// Timeout in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticValidation {
    /// Model alias (from node config)
    pub model: String,
    
    /// Evaluation prompt
    pub prompt: String,
    
    /// Acceptance threshold (0.0-1.0)
    #[serde(default = "default_threshold")]
    pub threshold: f32,
    
    /// Timeout in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
    
    /// Behavior when LLM unavailable
    #[serde(default = "default_skip")]
    pub fallback_on_unavailable: FallbackBehavior,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FallbackBehavior {
    Skip,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryConfig {
    /// Delivery destinations
    #[serde(default)]
    pub destinations: Vec<DeliveryDestination>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryDestination {
    /// Destination name
    pub name: String,
    
    /// When to deliver
    #[serde(default = "default_on_success")]
    pub condition: DeliveryCondition,
    
    /// ETL transformation (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<TransformConfig>,
    
    /// Destination type
    #[serde(rename = "type")]
    pub destination_type: DestinationType,
    
    /// Type-specific configuration
    #[serde(flatten)]
    pub config: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryCondition {
    OnSuccess,
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DestinationType {
    Email,
    Webhook,
    Rest,
    Sms,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformConfig {
    /// Transformation script
    pub script: String,
    
    /// Script arguments
    #[serde(default)]
    pub args: Vec<String>,
    
    /// Timeout in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permissions {
    /// Network access permissions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkPermissions>,
    
    /// Filesystem permissions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs: Option<FilesystemPermissions>,
    
    /// Resource limits
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPermissions {
    /// Allowed domains/IPs
    #[serde(default)]
    pub allow: Vec<String>,
    
    /// Denied domains/IPs
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemPermissions {
    /// Readable paths
    #[serde(default)]
    pub read: Vec<String>,
    
    /// Writable paths
    #[serde(default)]
    pub write: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// CPU in millicores
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    
    /// Memory (e.g., "512Mi", "1Gi")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    
    /// Disk space (e.g., "1Gi", "10Gi")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Advanced {
    /// Warm instance pool size
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm_pool_size: Option<u32>,
    
    /// Enable swarm coordination
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swarm_enabled: Option<bool>,
    
    /// Startup script
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_script: Option<String>,
    
    /// Health check configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// HTTP endpoint to check
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    
    /// Interval in seconds
    #[serde(default = "default_health_interval")]
    pub interval_seconds: u32,
    
    /// Timeout in seconds
    #[serde(default = "default_health_timeout")]
    pub timeout_seconds: u32,
}

// Default value functions
fn default_true() -> bool {
    true
}

fn default_one_shot() -> ExecutionMode {
    ExecutionMode::OneShot
}

fn default_threshold() -> f32 {
    0.8
}

fn default_skip() -> FallbackBehavior {
    FallbackBehavior::Skip
}

fn default_on_success() -> DeliveryCondition {
    DeliveryCondition::OnSuccess
}

fn default_health_interval() -> u32 {
    30
}

fn default_health_timeout() -> u32 {
    5
}

fn default_version() -> String {
    "1.1".to_string()
}



impl AgentManifest {
    /// Load a manifest from a YAML file.
    pub fn from_yaml_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let manifest = serde_yaml::from_str(&content)?;
        Ok(manifest)
    }

    /// Save a manifest to a YAML file.
    pub fn to_yaml_file(&self, path: impl AsRef<std::path::Path>) -> anyhow::Result<()> {
        let yaml = serde_yaml::to_string(self)?;
        std::fs::write(path, yaml)?;
        Ok(())
    }
    
    /// Parse manifest from YAML string
    pub fn from_yaml_str(yaml: &str) -> anyhow::Result<Self> {
        let manifest = serde_yaml::from_str(yaml)?;
        Ok(manifest)
    }
    
    /// Convert manifest to YAML string
    pub fn to_yaml_str(&self) -> anyhow::Result<String> {
        let yaml = serde_yaml::to_string(self)?;
        Ok(yaml)
    }
}

