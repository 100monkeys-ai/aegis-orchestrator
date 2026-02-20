// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use uuid::Uuid;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub name: String,
    pub manifest: AgentManifest,
    pub status: AgentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Kubernetes-style Agent Manifest (v1.0)
/// Follows spec: MANIFEST_SPEC_V1.md
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentManifest {
    /// API version (e.g., "100monkeys.ai/v1")
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    
    /// Resource kind (must be "AgentManifest")
    pub kind: String,
    
    /// Kubernetes-style metadata
    pub metadata: ManifestMetadata,
    
    /// Agent specification
    pub spec: AgentSpec,
}

/// Kubernetes-style metadata
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
}

/// Runtime configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeConfig {
    /// Programming language (python, javascript, typescript, rust, go)
    pub language: String,
    
    /// Language version (e.g., "3.11", "20", "1.75")
    pub version: String,
    
    /// Optional isolation mode (inherit, firecracker, docker, process)
    #[serde(default = "default_isolation")]
    pub isolation: String,
    
    /// Optional autopull
    #[serde(default = "default_true")]
    pub autopull: bool,
}

/// Volume specification in agent manifest
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VolumeSpec {
    /// Volume name (used as identifier)
    pub name: String,
    
    /// Storage class: "ephemeral" or "persistent"
    pub storage_class: String,
    
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ScheduleConfig {
    Cron { 
        cron: String, 
        timezone: String, 
        #[serde(default = "default_true")] 
        enabled: bool 
    },
    Interval { 
        seconds: u64, 
        #[serde(default = "default_true")] 
        enabled: bool 
    },
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskConfig {
    #[serde(default)]
    pub agentskills: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContextItem {
    Text { content: String, description: Option<String> },
    File { path: String, description: Option<String> },
    Directory { path: String, description: Option<String> },
    Url { url: String, description: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionStrategy {
    #[serde(default)]
    pub mode: ExecutionMode,
    #[serde(default = "default_max_retries", alias = "max_iterations")]
    pub max_retries: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_per_iteration: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionMode {
    #[default]
    OneShot,
    Iterative,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemValidation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputValidation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptValidation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic: Option<SemanticValidation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemValidation {
    #[serde(default = "default_true")]
    pub must_succeed: bool,
    #[serde(default)]
    pub allow_stderr: bool,
    #[serde(default = "default_system_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputValidation {
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScriptValidation {
    pub path: String,
    pub description: Option<String>,
    #[serde(default = "default_validation_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SemanticValidation {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub model: String,
    pub prompt: String,
    #[serde(default = "default_semantic_threshold")]
    pub threshold: f64,
    #[serde(default = "default_validation_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub fallback_on_unavailable: FallbackBehavior,
}

impl<'de> Deserialize<'de> for SemanticValidation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SemanticValidationHelper {
            #[serde(default = "default_true")]
            enabled: bool,
            model: String,
            prompt: String,
            #[serde(default = "default_semantic_threshold")]
            threshold: f64,
            #[serde(default = "default_validation_timeout")]
            timeout_seconds: u64,
            #[serde(default)]
            fallback_on_unavailable: FallbackBehavior,
        }
        
        let helper = SemanticValidationHelper::deserialize(deserializer)?;
        
        // Validate threshold is in valid range
        if helper.threshold < 0.0 || helper.threshold > 1.0 {
            return Err(serde::de::Error::custom(
                format!("threshold must be between 0.0 and 1.0, got {}", helper.threshold)
            ));
        }
        
        Ok(SemanticValidation {
            enabled: helper.enabled,
            model: helper.model,
            prompt: helper.prompt,
            threshold: helper.threshold,
            timeout_seconds: helper.timeout_seconds,
            fallback_on_unavailable: helper.fallback_on_unavailable,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FallbackBehavior {
    #[default]
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveryConfig {
    pub destinations: Vec<DeliveryDestination>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveryDestination {
    pub name: String,
    pub condition: DeliveryCondition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<TransformConfig>,
    #[serde(flatten)]
    pub config: DeliveryType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryCondition {
    OnSuccess,
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransformConfig {
    pub script: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_validation_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DeliveryType {
    Email { 
        email: EmailConfig 
    },
    Webhook { 
        webhook: WebhookConfig 
    },
    Rest { 
        rest: RestConfig 
    },
    Sms { 
        sms: SmsConfig 
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmailConfig {
    pub to: String,
    pub subject: String,
    pub body_template: Option<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(default = "default_post")]
    pub method: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RestConfig {
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SmsConfig {
    pub to: String,
    pub message: String,
}

/// Security configuration (renamed from PermissionsConfig to match spec)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecurityConfig {
    #[serde(default)]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub filesystem: FilesystemPolicy,
    #[serde(default)]
    pub resources: ResourceLimits,
}

/// Network access policy
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Parse memory/disk string (e.g., "512Mi", "1Gi") to bytes
    /// Returns None if parsing fails
    pub fn parse_size_to_bytes(size_str: &str) -> Option<u64> {
        let size_str = size_str.trim();
        if size_str.ends_with("Gi") {
            size_str.trim_end_matches("Gi").parse::<u64>().ok().map(|v| v * 1024 * 1024 * 1024)
        } else if size_str.ends_with("Mi") {
            size_str.trim_end_matches("Mi").parse::<u64>().ok().map(|v| v * 1024 * 1024)
        } else if size_str.ends_with("Ki") {
            size_str.trim_end_matches("Ki").parse::<u64>().ok().map(|v| v * 1024)
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdvancedConfig {
    #[serde(default)]
    pub warm_pool_size: u32,
    #[serde(default)]
    pub swarm_enabled: bool,
    pub startup_script: Option<String>,
    pub health_check: Option<HealthCheckConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
fn default_true() -> bool { true }
fn default_max_retries() -> u32 { 5 }
fn default_system_timeout() -> u64 { 90 }
fn default_validation_timeout() -> u64 { 30 }
fn default_semantic_threshold() -> f64 { 0.8 }
fn default_post() -> String { "POST".to_string() }
fn default_cpu() -> u32 { 1000 }
fn default_memory() -> String { "512Mi".to_string() }
fn default_disk() -> String { "1Gi".to_string() }
fn default_isolation() -> String { "inherit".to_string() }
fn default_network_mode() -> String { "allow".to_string() }

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
            return Err(format!("Invalid apiVersion: expected '100monkeys.ai/v1', got '{}'", self.api_version));
        }
        
        // Validate kind
        if self.kind != "AgentManifest" {
            return Err(format!("Invalid kind: expected 'AgentManifest', got '{}'", self.kind));
        }
        
        // Validate name format (DNS label: lowercase alphanumeric with hyphens)
        if self.metadata.name.is_empty() {
            return Err("metadata.name cannot be empty".to_string());
        }
        for ch in self.metadata.name.chars() {
            if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '-' {
                return Err(format!("Invalid metadata.name: '{}' must be lowercase alphanumeric with hyphens", self.metadata.name));
            }
        }
        if self.metadata.name.starts_with('-') || self.metadata.name.ends_with('-') {
            return Err(format!("Invalid metadata.name: '{}' cannot start or end with hyphen", self.metadata.name));
        }
        
        // Validate timeout hierarchy if all are present
        if let Some(exec) = &self.spec.execution {
            if let Some(validation) = &exec.validation {
                if let Some(system) = &validation.system {
                    if let Some(security) = &self.spec.security {
                        if let Some(_timeout_str) = &security.resources.timeout {
                            // Parse timeouts and enforce hierarchy
                            // semantic timeout <= system timeout <= resource timeout
                            let system_timeout = system.timeout_seconds;
                            
                            if let Some(semantic) = &validation.semantic {
                                if semantic.timeout_seconds > system_timeout {
                                    return Err(format!(
                                        "Timeout hierarchy violation: semantic.timeout ({}) > system.timeout ({})",
                                        semantic.timeout_seconds, system_timeout
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Get the runtime as a combined string
    pub fn runtime_string(&self) -> String {
        format!("{}:{}", self.spec.runtime.language, self.spec.runtime.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manifest(name: &str) -> AgentManifest {
        AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "AgentManifest".to_string(),
            metadata: ManifestMetadata {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                description: None,
                labels: std::collections::HashMap::new(),
                annotations: std::collections::HashMap::new(),
            },
            spec: AgentSpec {
                runtime: RuntimeConfig {
                    language: "python".to_string(),
                    version: "3.11".to_string(),
                    isolation: "inherit".to_string(),
                    autopull: true,
                },
                task: None,
                context: vec![],
                execution: None,
                security: None,
                schedule: None,
                tools: vec![],
                env: std::collections::HashMap::new(),
                volumes: vec![],
                advanced: None,
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

    #[test]
    fn test_manifest_runtime_string() {
        let manifest = make_manifest("my-agent");
        assert_eq!(manifest.runtime_string(), "python:3.11");
    }

    // ── ResourceLimits ────────────────────────────────────────────────────────

    #[test]
    fn test_resource_limits_parse_gibibytes() {
        assert_eq!(ResourceLimits::parse_size_to_bytes("1Gi"), Some(1024 * 1024 * 1024));
        assert_eq!(ResourceLimits::parse_size_to_bytes("2Gi"), Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn test_resource_limits_parse_mebibytes() {
        assert_eq!(ResourceLimits::parse_size_to_bytes("512Mi"), Some(512 * 1024 * 1024));
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
}
