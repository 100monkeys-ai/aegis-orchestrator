use uuid::Uuid;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentManifest {
    #[serde(default = "default_manifest_version")]
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution_targets: Vec<String>,
    pub agent: AgentIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ContextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionsConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advanced: Option<AdvancedConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MetadataConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentIdentity {
    pub name: String,
    pub runtime: String,
    #[serde(default = "default_true")]
    pub autopull: bool,
    #[serde(default)]
    pub memory: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
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
    #[serde(default = "default_max_retries")]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticValidation {
    pub model: String,
    pub prompt: String,
    pub threshold: f64,
    #[serde(default = "default_validation_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub fallback_on_unavailable: FallbackBehavior,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub network: NetworkPermissions,
    #[serde(default)]
    pub fs: FsPermissions,
    #[serde(default)]
    pub resources: ResourceLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NetworkPermissions {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FsPermissions {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResourceLimits {
    #[serde(default = "default_cpu")]
    pub cpu: u32,
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_disk")]
    pub disk: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetadataConfig {
    pub author: Option<String>,
    pub created: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub cost_center: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Active,
    Paused,
    Archived,
    Failed,
}

// Defaults
fn default_true() -> bool { true }
fn default_timeout() -> u64 { 300 }
fn default_max_retries() -> u32 { 5 }
fn default_system_timeout() -> u64 { 90 }
fn default_validation_timeout() -> u64 { 30 }
fn default_post() -> String { "POST".to_string() }
fn default_cpu() -> u32 { 1000 }
fn default_memory() -> String { "512Mi".to_string() }
fn default_disk() -> String { "1Gi".to_string() }
fn default_manifest_version() -> String { "0.1.0".to_string() }

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpu: default_cpu(),
            memory: default_memory(),
            disk: default_disk(),
        }
    }
}

impl Agent {
    pub fn new(manifest: AgentManifest) -> Self {
        let now = Utc::now();
        Self {
            id: AgentId::new(),
            name: manifest.agent.name.clone(),
            manifest,
            status: AgentStatus::Active,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn update_manifest(&mut self, manifest: AgentManifest) {
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
