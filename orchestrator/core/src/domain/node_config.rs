// Node Configuration Types - Implements NODE_CONFIGURATION_SPEC.md
//
// Defines the configuration schema for AEGIS Agent Host nodes, including:
// - Node identity and capabilities
// - LLM provider configuration (BYOLLM support)
// - Model alias mapping for provider independence
// - LLM selection strategies
// - Network and observability settings

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Complete node configuration matching NODE_CONFIGURATION_SPEC.md
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Node identity and capabilities
    pub node: NodeIdentity,
    
    /// LLM provider configurations
    #[serde(default)]
    pub llm_providers: Vec<LLMProviderConfig>,
    
    /// LLM selection strategy
    #[serde(default)]
    pub llm_selection: LLMSelection,
    
    /// Network configuration (for edge nodes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    
    /// Observability configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observability: Option<ObservabilityConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    /// Unique node identifier (e.g., "edge-node-001")
    pub id: String,
    
    /// Node type
    #[serde(rename = "type")]
    pub node_type: NodeType,
    
    /// Geographic region (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    
    /// Capability tags for execution_targets matching
    #[serde(default)]
    pub tags: Vec<String>,
    
    /// Available compute resources
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<NodeResources>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    Edge,
    Orchestrator,
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResources {
    /// CPU cores available
    pub cpu_cores: u32,
    
    /// Memory in GB
    pub memory_gb: u32,
    
    /// Disk space in GB
    pub disk_gb: u32,
    
    /// GPU available
    #[serde(default)]
    pub gpu: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMProviderConfig {
    /// Unique provider name (e.g., "ollama-local", "openai")
    pub name: String,
    
    /// Provider type
    #[serde(rename = "type")]
    pub provider_type: String, // "ollama", "openai", "anthropic", "openai-compatible"
    
    /// API endpoint URL
    pub endpoint: String,
    
    /// API key (supports "env:VAR_NAME" for environment variables)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    
    /// Whether this provider is active
    #[serde(default = "default_true")]
    pub enabled: bool,
    
    /// Available models on this provider
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model alias used in agent manifests (e.g., "default", "fast", "smart")
    pub alias: String,
    
    /// Actual model identifier for the provider API
    pub model: String,
    
    /// Model capabilities
    pub capabilities: Vec<String>, // ["chat", "embedding", "reasoning", "vision"]
    
    /// Maximum context window size in tokens
    pub context_window: u32,
    
    /// Cost per 1,000 tokens (0.0 for local models)
    #[serde(default)]
    pub cost_per_1k_tokens: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMSelection {
    /// Selection strategy when multiple providers match
    #[serde(default)]
    pub strategy: LLMSelectionStrategy,
    
    /// Default provider name when no preference specified
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    
    /// Fallback provider if primary fails
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_provider: Option<String>,
    
    /// Maximum retry attempts on failure
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    
    /// Delay between retries in milliseconds
    #[serde(default = "default_retry_delay")]
    pub retry_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LLMSelectionStrategy {
    PreferLocal,
    PreferCloud,
    CostOptimized,
    LatencyOptimized,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Orchestrator endpoint (WebSocket URL for edge nodes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchestrator_endpoint: Option<String>,
    
    /// Heartbeat interval in seconds
    #[serde(default = "default_heartbeat")]
    pub heartbeat_interval_seconds: u64,
    
    /// TLS certificate configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Path to TLS certificate
    pub cert_path: String,
    
    /// Path to TLS private key
    pub key_path: String,
    
    /// Path to CA certificate (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    /// Logging configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingConfig>,
    
    /// Metrics configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsConfig>,
    
    /// Tracing configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracing: Option<TracingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level (e.g., "info", "debug", "trace")
    #[serde(default = "default_log_level")]
    pub level: String,
    
    /// Output format ("json" or "text")
    #[serde(default = "default_log_format")]
    pub format: String,
    
    /// Log file path (optional, defaults to stdout)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// Enable metrics exposition
    #[serde(default = "default_true")]
    pub enabled: bool,
    
    /// Metrics endpoint port
    #[serde(default = "default_metrics_port")]
    pub port: u16,
    
    /// Metrics path (e.g., "/metrics")
    #[serde(default = "default_metrics_path")]
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingConfig {
    /// Enable distributed tracing
    #[serde(default)]
    pub enabled: bool,
    
    /// OpenTelemetry collector endpoint
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otlp_endpoint: Option<String>,
}

// Default value functions
fn default_true() -> bool {
    true
}

fn default_max_retries() -> u32 {
    3
}

fn default_retry_delay() -> u64 {
    1000
}

fn default_heartbeat() -> u64 {
    30
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_format() -> String {
    "json".to_string()
}

fn default_metrics_port() -> u16 {
    9090
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

impl Default for LLMSelectionStrategy {
    fn default() -> Self {
        Self::PreferLocal
    }
}

impl Default for LLMSelection {
    fn default() -> Self {
        Self {
            strategy: LLMSelectionStrategy::PreferLocal,
            default_provider: None,
            fallback_provider: None,
            max_retries: 3,
            retry_delay_ms: 1000,
        }
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            node: NodeIdentity {
                id: hostname::get()
                    .ok()
                    .and_then(|h| h.into_string().ok())
                    .unwrap_or_else(|| "aegis-node".to_string()),
                node_type: NodeType::Edge,
                region: None,
                tags: vec![],
                resources: None,
            },
            llm_providers: vec![],
            llm_selection: LLMSelection::default(),
            network: None,
            observability: None,
        }
    }
}

impl NodeConfig {
    /// Load configuration from YAML file
    pub fn from_yaml_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config = serde_yaml::from_str(&content)?;
        Ok(config)
    }
    
    /// Save configuration to YAML file
    pub fn to_yaml_file(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        let yaml = serde_yaml::to_string(self)?;
        std::fs::write(path, yaml)?;
        Ok(())
    }
    
    /// Parse configuration from YAML string
    pub fn from_yaml_str(yaml: &str) -> anyhow::Result<Self> {
        let config = serde_yaml::from_str(yaml)?;
        Ok(config)
    }
    
    /// Discover configuration file using precedence order
    /// 1. --config flag (passed as argument)
    /// 2. AEGIS_CONFIG_PATH environment variable
    /// 3. ./aegis-config.yaml (working directory)
    /// 4. ~/.aegis/config.yaml (user home)
    /// 5. /etc/aegis/config.yaml (system, Unix) or C:\ProgramData\Aegis\config.yaml (Windows)
    pub fn discover_config(cli_path: Option<PathBuf>) -> Option<PathBuf> {
        // 1. CLI flag (highest priority)
        if let Some(path) = cli_path {
            if path.exists() {
                return Some(path);
            }
        }
        
        // 2. Environment variable
        if let Ok(path) = std::env::var("AEGIS_CONFIG_PATH") {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }
        
        // 3. Working directory
        let cwd = PathBuf::from("./aegis-config.yaml");
        if cwd.exists() {
            return Some(cwd);
        }
        
        // 4. User home
        if let Some(home) = dirs::home_dir() {
            let user_config = home.join(".aegis").join("config.yaml");
            if user_config.exists() {
                return Some(user_config);
            }
        }
        
        // 5. System config
        #[cfg(unix)]
        let system_config = PathBuf::from("/etc/aegis/config.yaml");
        #[cfg(windows)]
        let system_config = PathBuf::from("C:\\ProgramData\\Aegis\\config.yaml");
        
        if system_config.exists() {
            return Some(system_config);
        }
        
        None
    }
    
    /// Load configuration with discovery, fallback to default
    pub fn load_or_default(cli_path: Option<PathBuf>) -> anyhow::Result<Self> {
        if let Some(config_path) = Self::discover_config(cli_path) {
            tracing::info!("Loading configuration from: {:?}", config_path);
            Self::from_yaml_file(config_path)
        } else {
            tracing::warn!("No configuration file found, using defaults");
            Ok(Self::default())
        }
    }
    
    /// Validate configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate node ID is not empty
        if self.node.id.is_empty() {
            anyhow::bail!("Node ID cannot be empty");
        }
        
        // Validate LLM providers
        for provider in &self.llm_providers {
            if provider.name.is_empty() {
                anyhow::bail!("LLM provider name cannot be empty");
            }
            
            if provider.endpoint.is_empty() {
                anyhow::bail!("LLM provider endpoint cannot be empty for: {}", provider.name);
            }
            
            if provider.models.is_empty() {
                anyhow::bail!("LLM provider must have at least one model: {}", provider.name);
            }
            
            for model in &provider.models {
                if model.alias.is_empty() {
                    anyhow::bail!("Model alias cannot be empty in provider: {}", provider.name);
                }
                
                if model.model.is_empty() {
                    anyhow::bail!("Model identifier cannot be empty for alias: {}", model.alias);
                }
            }
        }
        
        // Validate default/fallback providers exist
        if let Some(default_provider) = &self.llm_selection.default_provider {
            if !self.llm_providers.iter().any(|p| &p.name == default_provider) {
                anyhow::bail!("Default provider '{}' not found in llm_providers", default_provider);
            }
        }
        
        if let Some(fallback_provider) = &self.llm_selection.fallback_provider {
            if !self.llm_providers.iter().any(|p| &p.name == fallback_provider) {
                anyhow::bail!("Fallback provider '{}' not found in llm_providers", fallback_provider);
            }
        }
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_default_config() {
        let config = NodeConfig::default();
        assert_eq!(config.node.node_type, NodeType::Edge);
        assert!(config.llm_providers.is_empty());
    }
    
    #[test]
    fn test_yaml_roundtrip() {
        let config = NodeConfig {
            node: NodeIdentity {
                id: "test-node".to_string(),
                node_type: NodeType::Edge,
                region: Some("us-east-1".to_string()),
                tags: vec!["production".to_string()],
                resources: Some(NodeResources {
                    cpu_cores: 4,
                    memory_gb: 16,
                    disk_gb: 100,
                    gpu: false,
                }),
            },
            llm_providers: vec![
                LLMProviderConfig {
                    name: "ollama".to_string(),
                    provider_type: "ollama".to_string(),
                    endpoint: "http://localhost:11434".to_string(),
                    api_key: None,
                    enabled: true,
                    models: vec![
                        ModelConfig {
                            alias: "default".to_string(),
                            model: "llama3.2:latest".to_string(),
                            capabilities: vec!["chat".to_string(), "reasoning".to_string()],
                            context_window: 8192,
                            cost_per_1k_tokens: 0.0,
                        },
                    ],
                },
            ],
            llm_selection: LLMSelection::default(),
            network: None,
            observability: None,
        };
        
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: NodeConfig = serde_yaml::from_str(&yaml).unwrap();
        
        assert_eq!(parsed.node.id, "test-node");
        assert_eq!(parsed.llm_providers.len(), 1);
        assert_eq!(parsed.llm_providers[0].name, "ollama");
    }
    
    #[test]
    fn test_validation() {
        let mut config = NodeConfig::default();
        
        // Empty node ID should fail
        config.node.id = "".to_string();
        assert!(config.validate().is_err());
        
        // Fix node ID
        config.node.id = "test-node".to_string();
        assert!(config.validate().is_ok());
        
        // Add invalid provider (no models)
        config.llm_providers.push(LLMProviderConfig {
            name: "invalid".to_string(),
            provider_type: "openai".to_string(),
            endpoint: "https://api.openai.com".to_string(),
            api_key: None,
            enabled: true,
            models: vec![],
        });
        assert!(config.validate().is_err());
    }
}
