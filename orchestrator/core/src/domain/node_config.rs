// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Node Configuration Domain Types
//!
//! Represents the runtime configuration of an **Aegis Node** — the machine
//! running the orchestrator host service. Loaded from `aegis-config.yaml`
//! at startup and validated before any other subsystem initialises.
//!
//! ## Top-Level Sections
//! | Section | Purpose |
//! |---------|---------|
//! | `runtime` | Docker socket path, Firecracker binary (Phase 2) |
//! | `storage` | SeaweedFS filer endpoints, local volume root |
//! | `nfs_gateway` | Bind address / port for NFS Server Gateway |
//! | `llm` | Provider credentials and model aliases |
//! | `secrets.backend` | Secrets backend connection (ADR-034) |
//! | `telemetry` | OTLP exporter endpoints |
//!
//! See AGENTS.md §Aegis Node, §Aegis Host.

// Node Configuration Types - Implements NODE_CONFIGURATION_SPEC_V1.md
//
// Defines the configuration schema for AEGIS Agent Host nodes, including:
// - Kubernetes-style manifest format (apiVersion/kind/metadata/spec)
// - Node identity and capabilities
// - LLM provider configuration (BYOLLM support)
// - Model alias mapping for provider independence
// - LLM selection strategies
// - Network and observability settings

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Top-level Kubernetes-style node configuration manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfigManifest {
    /// API version (must be "100monkeys.ai/v1")
    #[serde(rename = "apiVersion")]
    pub api_version: String,

    /// Resource kind (must be "NodeConfig")
    pub kind: String,

    /// Node metadata (name, labels, version)
    pub metadata: ManifestMetadata,

    /// Node configuration specification
    pub spec: NodeConfigSpec,
}

/// Manifest metadata (Kubernetes-style)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestMetadata {
    /// Human-readable node name (unique identifier)
    pub name: String,

    /// Optional: Configuration version for tracking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Optional: Labels for categorization and discovery
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,
}

/// Credentials for pulling container images from a private container registry (ADR-045).
///
/// Sourced from `spec.registry_credentials` in `aegis-config.yaml`.
/// The `registry` field is matched as a **prefix** of the resolved image reference
/// (e.g. `"ghcr.io"` matches `"ghcr.io/myorg/agent:v1.0"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryCredentials {
    /// Registry hostname prefix to match against image references
    /// (e.g. `"ghcr.io"`, `"registry.example.com:5000"`).
    pub registry: String,
    /// Username for HTTP Basic authentication with the Docker registry API.
    pub username: String,
    /// Password or personal access token.
    /// Supports `env:VAR_NAME` syntax for environment variable substitution.
    pub password: String,
}

/// Node configuration specification (content under spec:)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfigSpec {
    /// Node identity and capabilities
    pub node: NodeIdentity,

    /// Image tag for AEGIS-owned Docker images (e.g. `"0.1.0-pre-alpha"`).
    /// Written by `aegis init --tag <TAG>` and updated by `aegis update`.
    /// Defaults to `env!("CARGO_PKG_VERSION")` when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,

    /// LLM provider configurations
    #[serde(default)]
    pub llm_providers: Vec<LLMProviderConfig>,

    /// LLM selection strategy
    #[serde(default)]
    pub llm_selection: LLMSelection,

    /// Runtime configuration
    #[serde(default)]
    pub runtime: RuntimeConfig,

    /// Network configuration (for edge nodes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,

    /// Observability configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observability: Option<ObservabilityConfig>,

    /// Distributed storage configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageConfig>,

    /// MCP tool servers configurations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<McpServerConfig>>,

    /// Built-in dispatchers configurations (e.g. cmd.run) (ADR-040)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin_dispatchers: Option<Vec<BuiltinDispatcherConfig>>,

    /// Private container registry credentials for pulling images (ADR-045).
    /// Each entry covers one registry host prefix.
    #[serde(default)]
    pub registry_credentials: Vec<RegistryCredentials>,

    /// Database configuration (PostgreSQL)
    /// If omitted, the orchestrator uses InMemory repositories (development mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseConfig>,

    /// Temporal workflow engine configuration (ADR-022)
    /// If omitted, Temporal connection uses defaults (address: "temporal:7233").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal: Option<TemporalConfig>,

    /// Cortex (Learning & Memory) gRPC service configuration (ADR-042)
    /// If omitted, the orchestrator runs in memoryless mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cortex: Option<CortexConfig>,

    /// Workflow & Agent Discovery Service configuration (ADR-075).
    /// If omitted, the orchestrator runs without semantic search — enterprise feature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery: Option<DiscoveryConfig>,

    /// Secrets management configuration (ADR-034).
    /// Placed at `spec.secrets` in `aegis-config.yaml`.
    /// If omitted, the orchestrator uses `MockSecretStore` (dev/test only, logs a warning).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SecretsConfig>,

    /// SMCP protocol configuration (ADR-035)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smcp: Option<SmcpConfig>,

    /// Cluster configuration (ADR-059)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster: Option<ClusterConfig>,

    /// Named security contexts for SMCP (ADR-035)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_contexts: Option<Vec<SecurityContextDefinition>>,

    /// OIDC IAM configuration (ADR-041)
    /// If omitted, all auth middleware is disabled (pass-through for local development).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iam: Option<IamConfig>,

    /// gRPC authentication configuration (ADR-041)
    /// If omitted, gRPC endpoints are unauthenticated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grpc_auth: Option<GrpcAuthConfig>,

    /// External SMCP tooling gateway configuration (ADR-053).
    /// If omitted, orchestrator does not forward unknown tools to the gateway.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smcp_gateway: Option<SmcpGatewayConfig>,

    /// Whether to deploy vendored built-in agent and workflow templates on startup.
    /// Includes agent-creator-agent, workflow-generator-planner-agent, judge agents, etc.
    /// Default: false (disabled). Enable in deployment configs that need agent/workflow generation.
    #[serde(default)]
    pub deploy_builtins: bool,

    /// Maximum number of executions returned by a single `list_executions` request.
    /// Protects against excessive memory usage. Defaults to 1000 if not configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_execution_list_limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    /// Unique stable node identifier (UUID recommended)
    /// Note: Human-readable name is in metadata.name
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Number of GPUs available
    #[serde(default)]
    pub gpu_count: u32,

    /// Total VRAM in GB
    #[serde(default)]
    pub vram_gb: u32,

    /// GPU available (legacy flag, kept for compatibility but preferred to use count)
    #[serde(default)]
    pub gpu: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMProviderConfig {
    /// Unique provider name (e.g., "ollama-local", "openai")
    pub name: String,

    /// Provider type
    #[serde(rename = "type")]
    pub provider_type: String, // "ollama", "openai", "anthropic", "gemini", "openai-compatible"

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

impl LLMProviderConfig {
    /// Returns `true` when this provider runs inference locally (no external API call).
    ///
    /// Local provider types: `"ollama"`, `"openai-compatible"` (e.g. LM Studio, vLLM).
    /// Cloud provider types: `"openai"`, `"anthropic"`, `"gemini"`.
    /// Used by `ProviderRegistry::build_alias_map` to implement `LLMSelectionStrategy`.
    pub fn is_local(&self) -> bool {
        matches!(self.provider_type.as_str(), "ollama" | "openai-compatible")
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LLMSelectionStrategy {
    #[default]
    PreferLocal,
    PreferCloud,
    CostOptimized,
    LatencyOptimized,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Path to bootstrap script for agent containers
    /// Default: "assets/bootstrap.py" (relative to orchestrator binary)
    #[serde(default = "default_bootstrap_script")]
    pub bootstrap_script: String,

    /// Default isolation mode for agent execution
    /// Options: "docker", "firecracker", "inherit", "process"
    /// Default: "inherit" (uses whatever the parent process provides)
    #[serde(default = "default_isolation_mode")]
    pub default_isolation: String,

    /// Path to container runtime socket (for Docker/Podman-based isolation)
    /// Default: platform-specific container socket path (Unix) or named pipe (Windows)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_socket_path: Option<String>,

    /// Container network to attach agent containers to (e.g., "aegis-network", "bridge")
    /// If None, uses the runtime's default network behavior
    /// Supports env:VAR_NAME syntax for environment variable substitution
    /// Default: None (no explicit network)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_network_mode: Option<String>,

    /// Orchestrator URL for agent containers to call back to
    /// Used by agent bootstrap scripts to reach the LLM proxy endpoint
    /// Supports env:VAR_NAME syntax for environment variable substitution
    /// Default: "http://localhost:8088" (local development)
    /// Docker deployments should override to "http://aegis-runtime:8088"
    #[serde(default = "default_orchestrator_url")]
    pub orchestrator_url: String,

    /// NFS server hostname/IP for volume mounts (ADR-036)
    /// Used by the Docker daemon on the host operating system to mount NFS volumes.
    /// CRITICAL: Must be resolvable from the Host Environment where the Docker daemon runs, NOT the container network.
    /// Supports env:VAR_NAME syntax for environment variable substitution.
    /// Examples for local development:
    /// - "127.0.0.1" (WSL2/Linux Native with exposed port 2049)
    /// - "host.docker.internal" (Docker Desktop on Mac/Windows)
    /// - "172.17.0.1" (Docker bridge gateway IP)
    ///   Examples for production:
    /// - "192.168.1.10" (example physical host IP address for Firecracker VMs or remote hosts)
    ///   Default: "127.0.0.1" (covers local native daemon and WSL2 deployments)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nfs_server_host: Option<String>,

    /// NFS server port (ADR-036)
    /// Default: 2049
    #[serde(default = "default_nfs_port")]
    pub nfs_port: u16,

    /// NFS server mountport (ADR-036)
    /// Default: 2049
    #[serde(default = "default_nfs_port")]
    pub nfs_mountport: u16,

    /// Path to the StandardRuntime registry YAML file (ADR-043).
    /// Resolves language+version pairs to deterministic Docker images.
    /// Default: "runtime-registry.yaml" (relative to daemon working directory)
    #[serde(default = "default_runtime_registry_path")]
    pub runtime_registry_path: String,
}

fn default_runtime_registry_path() -> String {
    "runtime-registry.yaml".to_string()
}

fn default_nfs_port() -> u16 {
    2049
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            bootstrap_script: default_bootstrap_script(),
            default_isolation: default_isolation_mode(),
            container_socket_path: None,
            container_network_mode: None,
            orchestrator_url: default_orchestrator_url(),
            nfs_server_host: None,
            nfs_port: default_nfs_port(),
            nfs_mountport: default_nfs_port(),
            runtime_registry_path: default_runtime_registry_path(),
        }
    }
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

    /// Network bind address (e.g. "0.0.0.0" or "127.0.0.1")
    #[serde(default = "default_bind_address")]
    pub bind_address: String,

    /// HTTP API port
    #[serde(default = "default_api_port")]
    pub port: u16,

    /// gRPC API port
    #[serde(default = "default_grpc_port")]
    pub grpc_port: u16,
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

    /// OTLP collector endpoint.
    /// E.g. "http://localhost:4317" or "<https://otlp.datadoghq.com>"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otlp_endpoint: Option<String>,

    /// OTLP protocol. Defaults to `grpc`.
    #[serde(default)]
    pub otlp_protocol: OtlpProtocol,

    /// Static headers to inject into OTLP requests (e.g. for API keys).
    #[serde(default)]
    pub otlp_headers: std::collections::BTreeMap<String, String>,

    /// Minimum log level to export to OTLP. Defaults to "info".
    /// Local file/stdout logging retains its own level control.
    #[serde(default = "default_log_level")]
    pub min_level: String,

    /// Override the default service name ("aegis-orchestrator").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,

    /// Batch processing configuration.
    #[serde(default)]
    pub batch: OtlpBatchConfig,

    /// TLS configuration for the OTLP exporter.
    #[serde(default)]
    pub tls: OtlpTlsConfig,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
            file: None,
            otlp_endpoint: None,
            otlp_protocol: OtlpProtocol::default(),
            otlp_headers: std::collections::BTreeMap::new(),
            min_level: default_log_level(),
            service_name: None,
            batch: OtlpBatchConfig::default(),
            tls: OtlpTlsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OtlpProtocol {
    #[default]
    Grpc,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtlpBatchConfig {
    /// Maximum number of log records held in memory before the oldest are dropped.
    #[serde(default = "default_otlp_queue_size")]
    pub max_queue_size: usize,
    /// How often the background exporter flushes the batch (milliseconds).
    #[serde(default = "default_otlp_scheduled_delay_ms")]
    pub scheduled_delay_ms: u64,
    /// Max batch size in log records. Default: 512.
    #[serde(default = "default_otlp_max_export_batch_size")]
    pub max_export_batch_size: usize,
    /// Batch export timeout in milliseconds. Default: 10000.
    #[serde(default = "default_otlp_export_timeout_ms")]
    pub export_timeout_ms: u64,
}

impl Default for OtlpBatchConfig {
    fn default() -> Self {
        Self {
            max_queue_size: default_otlp_queue_size(),
            scheduled_delay_ms: default_otlp_scheduled_delay_ms(),
            max_export_batch_size: default_otlp_max_export_batch_size(),
            export_timeout_ms: default_otlp_export_timeout_ms(),
        }
    }
}

fn default_otlp_queue_size() -> usize {
    2048
}

fn default_otlp_scheduled_delay_ms() -> u64 {
    5000
}

fn default_otlp_max_export_batch_size() -> usize {
    512
}

fn default_otlp_export_timeout_ms() -> u64 {
    10000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtlpTlsConfig {
    /// Verify the collector's TLS certificate. Default: true.
    #[serde(default = "default_true")]
    pub verify: bool,
    /// Path to custom CA certificate PEM file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<PathBuf>,
}

impl Default for OtlpTlsConfig {
    fn default() -> Self {
        Self {
            verify: true,
            ca_cert_path: None,
        }
    }
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

/// Storage configuration for distributed agent file systems
/// Related: ADR-032 Unified Storage via SeaweedFS, ADR-036 NFS Server Gateway
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Storage backend: "seaweedfs", "local_host", or "opendal_memory"
    /// Default: "seaweedfs"
    #[serde(default = "default_storage_backend")]
    pub backend: String,

    /// NFS Server Gateway port (ADR-036)
    /// Default: 2049 (standard NFS port)
    #[serde(default = "default_storage_nfs_port")]
    pub nfs_port: Option<u16>,

    /// SeaweedFS configuration (required if backend: "seaweedfs")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seaweedfs: Option<SeaweedFSConfig>,

    /// Local host filesystem configuration (used if backend: "local_host")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_host: Option<LocalHostStorageConfig>,

    /// OpenDAL configuration (used if backend: "opendal")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opendal: Option<OpenDalConfig>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: default_storage_backend(),
            nfs_port: default_storage_nfs_port(),
            seaweedfs: None,
            local_host: Some(LocalHostStorageConfig::default()),
            opendal: None,
        }
    }
}

/// SeaweedFS distributed storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeaweedFSConfig {
    /// Filer endpoint URL (e.g., "http://seaweedfs-filer:8888")
    pub filer_url: String,

    /// Host mount location for volumes
    /// Default: platform-specific aegis storage directory
    #[serde(default = "default_seaweedfs_mount_point")]
    pub mount_point: String,

    /// Default TTL for ephemeral volumes (hours)
    /// Default: 24
    #[serde(default = "default_ttl_hours")]
    pub default_ttl_hours: u32,

    /// Default size limit for volumes (MB)
    /// Default: 1000
    #[serde(default = "default_size_limit_mb")]
    pub default_size_limit_mb: u64,

    /// Maximum allowed size limit (MB)
    /// Default: 10000
    #[serde(default = "default_max_size_limit_mb")]
    pub max_size_limit_mb: u64,

    /// Garbage collection interval (minutes)
    /// Default: 60
    #[serde(default = "default_gc_interval_minutes")]
    pub gc_interval_minutes: u32,

    /// Optional S3 gateway endpoint
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s3_endpoint: Option<String>,

    /// S3 region for gateway
    /// Default: "us-east-1"
    #[serde(default = "default_s3_region")]
    pub s3_region: String,
}

impl Default for SeaweedFSConfig {
    fn default() -> Self {
        Self {
            filer_url: "http://localhost:8888".to_string(),
            mount_point: default_seaweedfs_mount_point(),
            default_ttl_hours: default_ttl_hours(),
            default_size_limit_mb: default_size_limit_mb(),
            max_size_limit_mb: default_max_size_limit_mb(),
            gc_interval_minutes: default_gc_interval_minutes(),
            s3_endpoint: None,
            s3_region: default_s3_region(),
        }
    }
}

/// Local host filesystem storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalHostStorageConfig {
    /// Host filesystem mount point
    /// Default: platform-specific local-host volume directory
    #[serde(default = "default_local_host_mount_point")]
    pub mount_point: String,
}

impl Default for LocalHostStorageConfig {
    fn default() -> Self {
        Self {
            mount_point: default_local_host_mount_point(),
        }
    }
}

/// OpenDAL unified storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenDalConfig {
    /// Scheme provider (e.g. "s3", "gcs", "memory", "fs")
    pub provider: String,

    /// Configuration options for the provider. Values can use "env:VAR_NAME"
    #[serde(default)]
    pub options: std::collections::HashMap<String, String>,
}

impl Default for OpenDalConfig {
    fn default() -> Self {
        Self {
            provider: "memory".to_string(),
            options: std::collections::HashMap::new(),
        }
    }
}

/// Per-tool capability configuration, including operator-level judge optimization flags.
///
/// Replaces the previous flat `Vec<String>` capabilities list on both
/// `BuiltinDispatcherConfig` and `McpServerConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityConfig {
    /// Tool name exposed to agents (e.g. `"fs.read"`, `"cmd.run"`, `"gmail.send"`)
    pub name: String,

    /// When `true`, the inner-loop semantic judge (if enabled on the agent manifest via
    /// `spec.execution.tool_validation`) is skipped for this specific tool call.
    ///
    /// Intended for read-only or low-risk tools (e.g. `fs.read`, `fs.list`) where the
    /// latency cost of spawning a judge child execution is undesirable and the operation
    /// carries no write-side risk. Defaults to `false` (judge always runs when configured).
    ///
    /// **Security note**: This is an operator-level infrastructure opt-out set in the
    /// node configuration, not an agent-level privilege. Agents cannot influence this flag.
    #[serde(default)]
    pub skip_judge: bool,
}

/// Built-in tools configured directly inside the Orchestrator via Dispatch Protocol (ADR-040)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltinDispatcherConfig {
    /// Dispatcher name (e.g. "cmd.run")
    pub name: String,

    /// Description for the LLM
    pub description: String,

    /// Is this dispatcher active?
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Capabilities provided by this dispatcher
    #[serde(default)]
    pub capabilities: Vec<CapabilityConfig>,
}

/// MCP Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Server identifier (unique on this node)
    pub name: String,

    /// Whether to start this server
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Path to executable (absolute or relative to /usr/local/bin)
    pub executable: String,

    /// Command-line arguments
    #[serde(default)]
    pub args: Vec<String>,

    /// Tool capabilities provided by this server.
    /// Use object form to control per-tool `skip_judge` behaviour.
    #[serde(default)]
    pub capabilities: Vec<CapabilityConfig>,

    /// API keys/tokens for external services
    #[serde(default)]
    pub credentials: HashMap<String, String>,

    /// Health monitoring configuration
    #[serde(default)]
    pub health_check: McpHealthCheckConfig,

    /// Process resource constraints
    #[serde(default)]
    pub resource_limits: McpResourceLimitsConfig,

    /// Additional environment variables
    #[serde(default)]
    pub environment: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpHealthCheckConfig {
    /// Check interval in seconds
    #[serde(default = "default_health_check_interval_seconds")]
    pub interval_seconds: u64,

    /// Health check timeout in seconds
    #[serde(default = "default_health_check_timeout_seconds")]
    pub timeout_seconds: u64,

    /// Health check method
    #[serde(default = "default_health_check_method")]
    pub method: String,
}

impl Default for McpHealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_seconds: default_health_check_interval_seconds(),
            timeout_seconds: default_health_check_timeout_seconds(),
            method: default_health_check_method(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceLimitsConfig {
    /// CPU limit (1000 = 1 core)
    #[serde(default = "default_cpu_millicores")]
    pub cpu_millicores: u32,

    /// Memory limit in MB
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u32,
}

impl Default for McpResourceLimitsConfig {
    fn default() -> Self {
        Self {
            cpu_millicores: default_cpu_millicores(),
            memory_mb: default_memory_mb(),
        }
    }
}

/// Database configuration.
///
/// Defines the PostgreSQL connection used for persistent repositories.
/// If this section is omitted from the node config, the orchestrator uses
/// InMemory repositories (suitable for development / testing only).
///
/// The `url` field supports the `env:VAR_NAME` credential resolution pattern
/// (see §Credential Resolution Patterns in NODE_CONFIGURATION_SPEC_V1.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// PostgreSQL connection URL.
    /// Supports `env:VAR_NAME` for environment variable resolution
    /// and `secret:namespace/mount/path` for secret-backend references (Phase 2).
    /// Example: `"env:AEGIS_DATABASE_URL"` or `"postgresql://user:pass@host:5432/db"`
    pub url: String,

    /// Maximum connection pool size.
    #[serde(default = "default_db_max_connections")]
    pub max_connections: u32,

    /// Connection timeout in seconds.
    #[serde(default = "default_db_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
}

/// Temporal workflow engine configuration (ADR-022).
///
/// Configures the connection to the Temporal server used for durable workflow
/// execution. If this section is omitted, the orchestrator uses default values.
///
/// All string fields support the `env:VAR_NAME` credential resolution pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalConfig {
    /// Temporal server address (host:port).
    /// Example: `"temporal:7233"` (Docker), `"localhost:7233"` (local dev)
    #[serde(default = "default_temporal_address")]
    pub address: String,

    /// HTTP endpoint of the Temporal worker service.
    /// Used for workflow activity callbacks.
    /// Example: `"http://temporal-worker:3000"`
    #[serde(default = "default_temporal_worker_http_endpoint")]
    pub worker_http_endpoint: String,

    /// Shared HMAC secret for authenticating Temporal event callbacks.
    /// Supports `env:VAR_NAME` and `secret:` credential resolution patterns.
    /// If omitted, the `/v1/temporal-events` endpoint is unauthenticated (warns at startup).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_secret: Option<String>,

    /// Temporal namespace.
    #[serde(default = "default_temporal_namespace")]
    pub namespace: String,

    /// Temporal task queue.
    #[serde(default = "default_temporal_task_queue")]
    pub task_queue: String,

    /// Maximum number of connection retries when establishing the Temporal client.
    /// If omitted, a default of 30 retries is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connection_retries: Option<i32>,
}

impl Default for TemporalConfig {
    fn default() -> Self {
        Self {
            address: default_temporal_address(),
            worker_http_endpoint: default_temporal_worker_http_endpoint(),
            worker_secret: None,
            namespace: default_temporal_namespace(),
            task_queue: default_temporal_task_queue(),
            max_connection_retries: None,
        }
    }
}

/// Cortex (Learning & Memory) gRPC service configuration (ADR-042).
///
/// Configures the connection to the standalone Cortex service for pattern
/// storage and semantic search. If this section is omitted (or `grpc_url`
/// is `None`), the orchestrator runs in **memoryless mode** — no error,
/// no retry, patterns are simply not stored.
///
/// The `grpc_url` field supports the `env:VAR_NAME` credential resolution pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CortexConfig {
    /// gRPC URL of the Cortex service.
    /// Example: `"http://cortex:50052"`, `"env:CORTEX_GRPC_URL"`
    /// If `None`, the orchestrator runs in memoryless mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grpc_url: Option<String>,
}

/// Workflow & Agent Discovery Service configuration (ADR-075).
///
/// Configures semantic search over agents and workflows via Qdrant vector
/// store and the embedding service. Enterprise-only feature.
///
/// If this section is omitted (or both URLs are `None`), the orchestrator
/// runs without discovery — search tools return a clear error, and generator
/// agents skip the deduplication step. No error, no retry.
///
/// Both URL fields support the `env:VAR_NAME` credential resolution pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    /// Qdrant gRPC URL for vector storage.
    /// Example: `"http://aegis-cortex:6334"`, `"env:QDRANT_URL"`
    /// If `None`, discovery is disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qdrant_url: Option<String>,

    /// Embedding service gRPC URL for vector generation.
    /// Example: `"http://aegis-cortex:50054"`, `"env:EMBEDDING_SERVICE_URL"`
    /// If `None`, discovery is disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_url: Option<String>,

    /// Similarity score threshold for deduplication during generation.
    /// Agents/workflows with similarity above this threshold are flagged as
    /// potential duplicates. Default: 0.85.
    #[serde(default = "default_dedup_threshold")]
    pub deduplication_threshold: f64,

    /// Whether to backfill the discovery index on startup by iterating all
    /// existing agents and workflows from PostgreSQL. Default: true.
    #[serde(default = "default_true")]
    pub backfill_on_startup: bool,
}

fn default_dedup_threshold() -> f64 {
    0.85
}

/// Top-level secrets configuration wrapper (ADR-034).
///
/// Placed at `spec.secrets` in `aegis-config.yaml` and deserialized into
/// [`NodeConfigSpec::secrets`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsConfig {
    /// Secret backend configuration.
    /// If `None`, the orchestrator uses `MockSecretStore` (dev/test only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<SecretBackendConfig>,
}

/// Secret backend configuration (ADR-034).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretBackendConfig {
    /// Backend API address (e.g. "<https://secrets.internal:8200>")
    pub address: String,

    /// Authentication method (must be "approle" for orchestrators)
    #[serde(default = "default_secret_backend_auth_method")]
    pub auth_method: String,

    /// AppRole authentication credentials
    pub approle: SecretBackendAppRoleConfig,

    /// Default namespace for this node (e.g. "tenant-acme", "aegis-system")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    /// TLS configuration for communicating with the secret backend
    #[serde(default)]
    pub tls: SecretBackendTlsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretBackendAppRoleConfig {
    /// The public Role ID assigned to this orchestrator node
    pub role_id: String,

    /// The environment variable name containing the Secret ID.
    #[serde(default = "default_secret_backend_secret_id_env_var")]
    pub secret_id_env_var: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecretBackendTlsConfig {
    /// Path to a custom CA certificate PEM file to trust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<String>,

    /// Path to the client certificate PEM file (for mTLS)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<String>,

    /// Path to the client private key PEM file (for mTLS)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
}

fn default_secret_backend_auth_method() -> String {
    "approle".to_string()
}

fn default_secret_backend_secret_id_env_var() -> String {
    "OPENBAO_SECRET_ID".to_string()
}

/// SMCP protocol configuration (ADR-035 §6)
///
/// Defines the RSA key material used by the orchestrator to sign and verify
/// SecurityTokens (JWTs) during the SMCP attestation handshake.
///
/// Signing keys are loaded from PEM files on disk (paths specified by
/// `private_key_path` and `public_key_path`). The private key material
/// is read once at startup into process memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmcpConfig {
    /// Path to RSA private key PEM file (for signing SecurityTokens).
    pub private_key_path: String,
    /// Path to RSA public key PEM file (for verifying SecurityTokens).
    pub public_key_path: String,
    /// JWT issuer claim (e.g. `"aegis-orchestrator"`).
    #[serde(default = "default_smcp_issuer")]
    pub issuer: String,
    /// JWT audience claim(s).
    #[serde(default = "default_smcp_audiences")]
    pub audiences: Vec<String>,
    /// SecurityToken TTL in seconds. Default: 3600 (1 hour).
    #[serde(default = "default_smcp_token_ttl")]
    pub token_ttl_seconds: u64,
}

impl Default for SmcpConfig {
    fn default() -> Self {
        Self {
            private_key_path: String::new(),
            public_key_path: String::new(),
            issuer: default_smcp_issuer(),
            audiences: default_smcp_audiences(),
            token_ttl_seconds: default_smcp_token_ttl(),
        }
    }
}

fn default_node_role() -> NodeRole {
    NodeRole::default()
}

/// Cluster configuration (ADR-059).
///
/// Defines the node's role in a multi-node cluster (Controller, Worker, Hybrid).
/// Controllers manage routing and config sync; Workers execute agents and report
/// heartbeats. Hybrid nodes act as both (single-node default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// Master switch. false (default) = standalone mode; all other fields ignored.
    #[serde(default)]
    pub enabled: bool,
    /// Node role in the cluster. Default: "hybrid" (single-node backward compat).
    #[serde(default = "default_node_role")]
    pub role: NodeRole,
    /// Controller settings (required for workers)
    pub controller: Option<ClusterControllerConfig>,
    /// Controllers and hybrids only: port to bind NodeClusterService on.
    #[serde(default = "default_cluster_grpc_port")]
    pub cluster_grpc_port: u16,
    /// Phase 1: static peer list (empty on workers; listing known controllers for fallback)
    #[serde(default)]
    pub peers: Vec<String>,
    /// Path to persistent Ed25519 keypair. Generated by `aegis node init`.
    #[serde(default = "default_keypair_path")]
    pub node_keypair_path: PathBuf,
    /// Heartbeat interval in seconds. Default: 30.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    /// Re-attest this many seconds before NodeSecurityToken expiry. Default: 120.
    #[serde(default = "default_token_refresh_margin")]
    pub token_refresh_margin_secs: u64,
    /// TLS configuration for NodeClusterService.
    pub tls: Option<ClusterTlsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterControllerConfig {
    /// gRPC endpoint of the cluster controller (required for workers)
    pub endpoint: String,
    /// Secret token for initial node-to-controller authentication (Step 0)
    pub token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    Controller,
    Worker,
    #[default]
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterTlsConfig {
    /// TLS for NodeClusterService. Strongly recommended in production.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Path to TLS certificate.
    pub cert_path: String,
    /// Path to TLS private key.
    pub key_path: String,
    /// Path to CA certificate used to verify peer certificates (mTLS).
    pub ca_cert: String,
}

/// Declarative security context definition for YAML configuration (ADR-035 §2).
///
/// Allows security contexts to be defined in `aegis-config.yaml` and loaded
/// into the `InMemorySecurityContextRepository` at startup. Each definition
/// specifies a named permission boundary with capabilities and deny lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityContextDefinition {
    /// Unique context name (e.g. `"research-safe"`, `"coder-unrestricted"`).
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Tool capabilities granted by this context.
    #[serde(default)]
    pub capabilities: Vec<CapabilityDefinition>,
    /// Explicit tool deny list (overrides any matching capability).
    #[serde(default)]
    pub deny_list: Vec<String>,
}

/// YAML-serializable capability definition within a `SecurityContextDefinition`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDefinition {
    /// Tool name pattern (e.g. `"fs.*"`, `"web-search.search"`, `"*"`).
    pub tool_pattern: String,
    /// Allowed filesystem path prefixes for `fs.*` tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_allowlist: Option<Vec<String>>,
    /// Allowed shell commands for `cmd.run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_allowlist: Option<Vec<String>>,
    /// Allowed network domain suffixes for `web.*` tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_allowlist: Option<Vec<String>>,
    /// Per-capability rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDefinition>,
    /// Max response size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_response_size: Option<u64>,
}

/// YAML-serializable rate limit for a capability definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitDefinition {
    pub calls: u32,
    pub per_seconds: u32,
}

/// OIDC IAM configuration (ADR-041 §Node Configuration).
///
/// Defines the trusted identity realms, JWKS cache TTL, and custom claim names.
/// When this section is present in `aegis-config.yaml`, all HTTP and gRPC
/// auth middleware is enabled. When absent, auth is disabled (local dev mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IamConfig {
    /// All known realms — determines which JWKS endpoints to trust and cache.
    /// The platform validates JWTs against the realm matching the JWT's "iss" claim.
    pub realms: Vec<IamRealmConfig>,

    /// JWKS cache TTL in seconds — keys refreshed this often to support key rotation.
    /// Default: 300 (5 minutes).
    #[serde(default = "default_jwks_cache_ttl")]
    pub jwks_cache_ttl_seconds: u64,

    /// Custom claim names for OIDC attribute mappers.
    #[serde(default)]
    pub claims: IamClaimsConfig,
}

/// Individual realm configuration entry within `spec.iam.realms`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IamRealmConfig {
    /// Realm identifier: "aegis-system", "zaru-consumer", or "tenant-{slug}"
    pub slug: String,
    /// Full issuer URL: <https://auth.myzaru.com/realms/{slug}>
    pub issuer_url: String,
    /// JWKS endpoint: {issuer_url}/protocol/openid-connect/certs
    pub jwks_uri: String,
    /// Expected "aud" claim value for tokens from this realm
    pub audience: String,
    /// Realm classification: "system", "consumer", or "tenant"
    pub kind: String,
}

/// Custom claim names for OIDC attribute mappers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IamClaimsConfig {
    /// Custom claim mapper name for ZaruTier. Default: "zaru_tier"
    #[serde(default = "default_zaru_tier_claim")]
    pub zaru_tier: String,
    /// Role attribute name in aegis-system realm. Default: "aegis_role"
    #[serde(default = "default_aegis_role_claim")]
    pub aegis_role: String,
}

impl Default for IamClaimsConfig {
    fn default() -> Self {
        Self {
            zaru_tier: default_zaru_tier_claim(),
            aegis_role: default_aegis_role_claim(),
        }
    }
}

fn default_jwks_cache_ttl() -> u64 {
    300
}

fn default_zaru_tier_claim() -> String {
    "zaru_tier".to_string()
}

fn default_aegis_role_claim() -> String {
    "aegis_role".to_string()
}

/// gRPC authentication configuration (ADR-041 §gRPC Authentication Amendment).
///
/// When enabled, a `OIDCAuthInterceptor` is installed on the gRPC server
/// that validates Bearer JWTs on every call except exempted methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcAuthConfig {
    /// Whether gRPC JWT auth is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Methods exempt from token auth (e.g. inner loop bootstrap channel).
    #[serde(default)]
    pub exempt_methods: Vec<String>,
}

/// Standalone SMCP tooling gateway endpoint configuration (ADR-053).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmcpGatewayConfig {
    /// gRPC endpoint URL of the gateway invocation service.
    /// Example: "http://aegis-smcp-gateway:50055"
    pub url: String,
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

fn default_orchestrator_url() -> String {
    "http://localhost:8088".to_string()
}

fn default_log_format() -> String {
    "json".to_string()
}

fn default_metrics_port() -> u16 {
    9091
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

fn default_bind_address() -> String {
    "0.0.0.0".to_string()
}

fn default_api_port() -> u16 {
    8088
}

fn default_grpc_port() -> u16 {
    50051
}

fn default_storage_backend() -> String {
    "local_host".to_string()
}

fn default_storage_nfs_port() -> Option<u16> {
    Some(2049) // Standard NFS port (ADR-036)
}

fn default_seaweedfs_mount_point() -> String {
    PathBuf::from("/")
        .join("var")
        .join("lib")
        .join("aegis")
        .join("storage")
        .to_string_lossy()
        .into_owned()
}

fn default_local_host_mount_point() -> String {
    PathBuf::from("/")
        .join("var")
        .join("lib")
        .join("aegis")
        .join("local-host-volumes")
        .to_string_lossy()
        .into_owned()
}

fn default_ttl_hours() -> u32 {
    24
}

fn default_size_limit_mb() -> u64 {
    1000
}

fn default_max_size_limit_mb() -> u64 {
    10000
}

fn default_gc_interval_minutes() -> u32 {
    60
}

fn default_s3_region() -> String {
    "us-east-1".to_string()
}

fn default_health_check_interval_seconds() -> u64 {
    60
}
fn default_health_check_timeout_seconds() -> u64 {
    5
}
fn default_health_check_method() -> String {
    "tools/list".to_string()
}
fn default_cpu_millicores() -> u32 {
    1000
}
fn default_memory_mb() -> u32 {
    512
}
fn default_smcp_issuer() -> String {
    "aegis-orchestrator".to_string()
}
fn default_smcp_audiences() -> Vec<String> {
    vec!["aegis-agents".to_string()]
}
fn default_smcp_token_ttl() -> u64 {
    3600
}
fn default_db_max_connections() -> u32 {
    5
}
fn default_db_connect_timeout_seconds() -> u64 {
    5
}
fn default_temporal_address() -> String {
    "temporal:7233".to_string()
}
fn default_temporal_worker_http_endpoint() -> String {
    "http://localhost:3000".to_string()
}
fn default_temporal_namespace() -> String {
    "default".to_string()
}
fn default_temporal_task_queue() -> String {
    "aegis-agents".to_string()
}

fn default_cluster_grpc_port() -> u16 {
    50056
}
fn default_heartbeat_interval() -> u64 {
    30
}
fn default_token_refresh_margin() -> u64 {
    120
}
fn default_keypair_path() -> PathBuf {
    PathBuf::from("~/.aegis/node_keypair")
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

impl Default for NodeConfigSpec {
    fn default() -> Self {
        Self {
            node: NodeIdentity {
                id: uuid::Uuid::new_v4().to_string(),
                node_type: NodeType::Edge,
                region: None,
                tags: vec![],
                resources: None,
            },
            llm_providers: vec![],
            llm_selection: LLMSelection::default(),
            runtime: RuntimeConfig::default(),
            network: None,
            observability: None,
            storage: None,
            mcp_servers: None,
            builtin_dispatchers: None,
            registry_credentials: vec![],
            database: None,
            temporal: None,
            cortex: None,
            discovery: None,
            secrets: None,
            smcp: None,
            cluster: None,
            security_contexts: None,
            iam: None,
            grpc_auth: None,
            smcp_gateway: None,
            image_tag: None,
            deploy_builtins: false,
            max_execution_list_limit: None,
        }
    }
}

impl Default for NodeConfigManifest {
    fn default() -> Self {
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "aegis-node".to_string());

        Self {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "NodeConfig".to_string(),
            metadata: ManifestMetadata {
                name: hostname,
                version: Some("1.0.0".to_string()),
                labels: None,
            },
            spec: NodeConfigSpec::default(),
        }
    }
}

impl NodeConfigManifest {
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
    /// 1. AEGIS_CONFIG_PATH environment variable
    /// 2. ./aegis-config.yaml (working directory)
    /// 3. ~/.aegis/config.yaml (user home)
    /// 4. /etc/aegis/config.yaml (system, Unix) or C:\ProgramData\Aegis\config.yaml (Windows)
    pub fn discover_config() -> Option<PathBuf> {
        // 1. Environment variable
        if let Ok(path) = std::env::var("AEGIS_CONFIG_PATH") {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }

        // 2. Working directory
        let cwd = PathBuf::from("./aegis-config.yaml");
        if cwd.exists() {
            return Some(cwd);
        }

        // 3. User home (~/.aegis/aegis-config.yaml — matches what `aegis init` writes)
        if let Some(home) = dirs::home_dir() {
            let user_config = home.join(".aegis").join("aegis-config.yaml");
            if user_config.exists() {
                return Some(user_config);
            }
        }

        // 4. System config
        #[cfg(unix)]
        let system_config = PathBuf::from("/")
            .join("etc")
            .join("aegis")
            .join("aegis-config.yaml");
        #[cfg(windows)]
        let system_config = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir())
            .join("Aegis")
            .join("aegis-config.yaml");

        if system_config.exists() {
            return Some(system_config);
        }

        None
    }

    /// Load configuration with discovery, fallback to default
    pub fn load_or_default(cli_path: Option<PathBuf>) -> anyhow::Result<Self> {
        // 1. Explicit CLI path (Fail if missing/invalid)
        if let Some(path) = cli_path {
            tracing::info!("Loading configuration from explicit path: {:?}", path);
            let mut config = Self::from_yaml_file(&path)
                .map_err(|e| anyhow::anyhow!("Failed to search/load config at {path:?}: {e}"))?;
            config.apply_env_overrides();
            return Ok(config);
        }

        // 2. Discovery (Env -> Cwd -> Home -> System)
        if let Some(config_path) = Self::discover_config() {
            tracing::info!(
                "Loading configuration from discovered path: {:?}",
                config_path
            );
            let mut config = Self::from_yaml_file(config_path)?;
            config.apply_env_overrides();
            Ok(config)
        } else {
            tracing::warn!(
                "No configuration file found in standard locations. Using empty defaults."
            );
            let mut config = Self::default();
            config.apply_env_overrides();
            Ok(config)
        }
    }

    /// Returns true when the node is explicitly configured for production use.
    pub fn is_production(&self) -> bool {
        self.metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get("environment"))
            .map(|value| value.eq_ignore_ascii_case("production"))
            .unwrap_or(false)
            || self
                .spec
                .node
                .tags
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case("production"))
    }

    /// Apply environment variable overrides to configuration.
    ///
    /// Standard precedence: explicit YAML value > env var override > default.
    /// Only overrides fields that are still at their default values.
    pub fn apply_env_overrides(&mut self) {
        // AEGIS_PORT → spec.network.port (only if network config uses default port)
        if let Ok(port_str) = std::env::var("AEGIS_PORT") {
            if let Ok(port) = port_str.parse::<u16>() {
                let network = self.spec.network.get_or_insert_with(|| NetworkConfig {
                    bind_address: default_bind_address(),
                    port: default_api_port(),
                    grpc_port: default_grpc_port(),
                    orchestrator_endpoint: None,
                    heartbeat_interval_seconds: default_heartbeat(),
                    tls: None,
                });
                if network.port == default_api_port() {
                    network.port = port;
                }
            }
        }

        // AEGIS_HOST → spec.network.bind_address (only if at default)
        if let Ok(host) = std::env::var("AEGIS_HOST") {
            if !host.is_empty() {
                let network = self.spec.network.get_or_insert_with(|| NetworkConfig {
                    bind_address: default_bind_address(),
                    port: default_api_port(),
                    grpc_port: default_grpc_port(),
                    orchestrator_endpoint: None,
                    heartbeat_interval_seconds: default_heartbeat(),
                    tls: None,
                });
                if network.bind_address == default_bind_address() {
                    network.bind_address = host;
                }
            }
        }

        // AEGIS_LOG_LEVEL → spec.observability.logging.level (only if at default)
        if let Ok(level) = std::env::var("AEGIS_LOG_LEVEL") {
            if !level.is_empty() {
                let obs = self.spec.observability.get_or_insert(ObservabilityConfig {
                    logging: None,
                    metrics: None,
                    tracing: None,
                });
                let logging = obs.logging.get_or_insert_with(LoggingConfig::default);
                if logging.level == default_log_level() {
                    logging.level = level;
                }
            }
        }

        // AEGIS_OTLP_ENDPOINT
        if let Ok(endpoint) = std::env::var("AEGIS_OTLP_ENDPOINT") {
            if !endpoint.is_empty() {
                let obs = self.spec.observability.get_or_insert(ObservabilityConfig {
                    logging: None,
                    metrics: None,
                    tracing: None,
                });
                let logging = obs.logging.get_or_insert_with(LoggingConfig::default);
                logging.otlp_endpoint = Some(endpoint);
            }
        }

        // AEGIS_OTLP_PROTOCOL
        if let Ok(protocol) = std::env::var("AEGIS_OTLP_PROTOCOL") {
            if !protocol.is_empty() {
                let obs = self.spec.observability.get_or_insert(ObservabilityConfig {
                    logging: None,
                    metrics: None,
                    tracing: None,
                });
                let logging = obs.logging.get_or_insert_with(LoggingConfig::default);
                logging.otlp_protocol = match protocol.to_lowercase().as_str() {
                    "http" => OtlpProtocol::Http,
                    _ => OtlpProtocol::Grpc,
                };
            }
        }

        // AEGIS_OTLP_HEADERS (format: key=value,key=value)
        if let Ok(headers) = std::env::var("AEGIS_OTLP_HEADERS") {
            if !headers.is_empty() {
                let obs = self.spec.observability.get_or_insert(ObservabilityConfig {
                    logging: None,
                    metrics: None,
                    tracing: None,
                });
                let logging = obs.logging.get_or_insert_with(LoggingConfig::default);
                for pair in headers.split(',') {
                    if let Some((k, v)) = pair.split_once('=') {
                        logging
                            .otlp_headers
                            .insert(k.trim().to_string(), v.trim().to_string());
                    }
                }
            }
        }

        // AEGIS_OTLP_LOG_LEVEL
        if let Ok(min_level) = std::env::var("AEGIS_OTLP_LOG_LEVEL") {
            if !min_level.is_empty() {
                let obs = self.spec.observability.get_or_insert(ObservabilityConfig {
                    logging: None,
                    metrics: None,
                    tracing: None,
                });
                let logging = obs.logging.get_or_insert_with(LoggingConfig::default);
                logging.min_level = min_level;
            }
        }

        // AEGIS_OTLP_SERVICE_NAME
        if let Ok(service_name) = std::env::var("AEGIS_OTLP_SERVICE_NAME") {
            if !service_name.is_empty() {
                let obs = self.spec.observability.get_or_insert(ObservabilityConfig {
                    logging: None,
                    metrics: None,
                    tracing: None,
                });
                let logging = obs.logging.get_or_insert_with(LoggingConfig::default);
                logging.service_name = Some(service_name);
            }
        }

        // CONTAINER_HOST (Podman) / DOCKER_HOST (Docker) → container_socket_path.
        // Both use URI format "unix:///path/to/sock"; strip the "unix://" prefix.
        // Precedence: explicit YAML > CONTAINER_HOST > DOCKER_HOST > auto-detect.
        if self.spec.runtime.container_socket_path.is_none() {
            let host_uri = std::env::var("CONTAINER_HOST")
                .or_else(|_| std::env::var("DOCKER_HOST"))
                .ok();
            if let Some(uri) = host_uri {
                let path = uri.strip_prefix("unix://").unwrap_or(&uri);
                if !path.is_empty() {
                    self.spec.runtime.container_socket_path = Some(path.to_string());
                }
            }
        }
    }

    /// Validate configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate apiVersion
        if self.api_version != "100monkeys.ai/v1" {
            anyhow::bail!(
                "Invalid apiVersion: '{}'. Must be '100monkeys.ai/v1'",
                self.api_version
            );
        }

        // Validate kind
        if self.kind != "NodeConfig" {
            anyhow::bail!("Invalid kind: '{}'. Must be 'NodeConfig'", self.kind);
        }

        // Validate metadata.name
        if self.metadata.name.is_empty() {
            anyhow::bail!("metadata.name cannot be empty");
        }

        // Validate node ID is not empty
        if self.spec.node.id.is_empty() {
            anyhow::bail!("spec.node.id cannot be empty");
        }

        // Validate LLM providers
        for provider in &self.spec.llm_providers {
            if provider.name.is_empty() {
                anyhow::bail!("LLM provider name cannot be empty");
            }

            if provider.endpoint.is_empty() {
                anyhow::bail!(
                    "LLM provider endpoint cannot be empty for: {}",
                    provider.name
                );
            }

            if provider.models.is_empty() {
                anyhow::bail!(
                    "LLM provider must have at least one model: {}",
                    provider.name
                );
            }

            for model in &provider.models {
                if model.alias.is_empty() {
                    anyhow::bail!("Model alias cannot be empty in provider: {}", provider.name);
                }

                if model.model.is_empty() {
                    anyhow::bail!(
                        "Model identifier cannot be empty for alias: {}",
                        model.alias
                    );
                }
            }
        }

        // Validate default/fallback providers exist
        if let Some(default_provider) = &self.spec.llm_selection.default_provider {
            if !self
                .spec
                .llm_providers
                .iter()
                .any(|p| &p.name == default_provider)
            {
                anyhow::bail!("Default provider '{default_provider}' not found in llm_providers");
            }
        }

        if let Some(fallback_provider) = &self.spec.llm_selection.fallback_provider {
            if !self
                .spec
                .llm_providers
                .iter()
                .any(|p| &p.name == fallback_provider)
            {
                anyhow::bail!("Fallback provider '{fallback_provider}' not found in llm_providers");
            }
        }

        if self.is_production() {
            if self.spec.database.is_none() {
                anyhow::bail!("Production nodes must configure spec.database");
            }

            if self.spec.smcp.is_none() {
                anyhow::bail!("Production nodes must configure spec.smcp");
            }

            if self.spec.iam.is_none() {
                anyhow::bail!("Production nodes must configure spec.iam");
            }

            if self
                .spec
                .network
                .as_ref()
                .and_then(|network| network.tls.as_ref())
                .is_none()
            {
                anyhow::bail!("Production nodes must enable spec.network.tls");
            }
        }

        Ok(())
    }
}

/// Resolve a configuration value that may use the `env:VAR_NAME` pattern.
///
/// The `env:` prefix is the canonical way to inject secrets and deployment-specific
/// values into YAML configuration without hardcoding them. See
/// NODE_CONFIGURATION_SPEC_V1.md §Credential Resolution Patterns.
///
/// # Resolution patterns
///
/// | Pattern | Example | Behaviour |
/// |---------|---------|----------|
/// | `env:VAR` | `env:OPENAI_API_KEY` | Read `$OPENAI_API_KEY` from process env |
/// | literal | `sk-abcdef...` | Return as-is |
///
/// Phase 2 will add `secret:namespace/mount/path` for secret-backend references.
pub fn resolve_env_value(raw: &str) -> anyhow::Result<String> {
    if let Some(var_name) = raw.strip_prefix("env:") {
        std::env::var(var_name).map_err(|_| {
            anyhow::anyhow!(
                "Environment variable '{var_name}' not set (referenced via 'env:{var_name}' in config)",
            )
        })
    } else {
        Ok(raw.to_string())
    }
}

/// Resolve an optional configuration value that may use the `env:VAR_NAME` pattern.
///
/// Returns `None` if the input is `None` or if the env var is not set.
pub fn resolve_env_value_optional(raw: &Option<String>) -> Option<String> {
    raw.as_ref().and_then(|v| resolve_env_value(v).ok())
}

fn default_bootstrap_script() -> String {
    "assets/bootstrap.py".to_string()
}

fn default_isolation_mode() -> String {
    "inherit".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_manifest() {
        let manifest = NodeConfigManifest::default();
        assert_eq!(manifest.api_version, "100monkeys.ai/v1");
        assert_eq!(manifest.kind, "NodeConfig");
        assert!(!manifest.metadata.name.is_empty());
        assert_eq!(manifest.spec.node.node_type, NodeType::Edge);
        assert!(manifest.spec.llm_providers.is_empty());
    }

    #[test]
    fn test_yaml_roundtrip() {
        let manifest = NodeConfigManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "NodeConfig".to_string(),
            metadata: ManifestMetadata {
                name: "test-node".to_string(),
                version: Some("1.0.0".to_string()),
                labels: Some(HashMap::from([(
                    "environment".to_string(),
                    "test".to_string(),
                )])),
            },
            spec: NodeConfigSpec {
                node: NodeIdentity {
                    id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
                    node_type: NodeType::Edge,
                    region: Some("us-east-1".to_string()),
                    tags: vec!["production".to_string()],
                    resources: Some(NodeResources {
                        cpu_cores: 4,
                        memory_gb: 16,
                        disk_gb: 100,
                        gpu_count: 0,
                        vram_gb: 0,
                        gpu: false,
                    }),
                },
                llm_providers: vec![LLMProviderConfig {
                    name: "ollama".to_string(),
                    provider_type: "ollama".to_string(),
                    endpoint: "http://localhost:11434".to_string(),
                    api_key: None,
                    enabled: true,
                    models: vec![ModelConfig {
                        alias: "default".to_string(),
                        model: "llama3.2:latest".to_string(),
                        capabilities: vec!["chat".to_string(), "reasoning".to_string()],
                        context_window: 8192,
                        cost_per_1k_tokens: 0.0,
                    }],
                }],
                llm_selection: LLMSelection::default(),
                runtime: RuntimeConfig::default(),
                network: None,
                observability: None,
                storage: None, // Optional storage configuration (ADR-032)
                database: None,
                temporal: None,
                cortex: None,
                secrets: None,
                mcp_servers: None,
                builtin_dispatchers: None,
                smcp: None,
                cluster: None,
                security_contexts: None,
                registry_credentials: vec![],
                iam: None,
                grpc_auth: None,
                smcp_gateway: None,
                image_tag: None,
                deploy_builtins: false,
                max_execution_list_limit: None,
                discovery: None,
            },
        };

        let yaml = serde_yaml::to_string(&manifest).unwrap();
        let parsed: NodeConfigManifest = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(parsed.api_version, "100monkeys.ai/v1");
        assert_eq!(parsed.kind, "NodeConfig");
        assert_eq!(parsed.metadata.name, "test-node");
        assert_eq!(parsed.spec.node.id, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(parsed.spec.llm_providers.len(), 1);
        assert_eq!(parsed.spec.llm_providers[0].name, "ollama");
    }

    #[test]
    fn test_discover_config_home_dir() {
        // Regression test: discover_config() must find `aegis-config.yaml` (with
        // the `aegis-` prefix) in ~/.aegis/, matching what `aegis init` writes.
        // Before this fix the home-dir lookup used `config.yaml` (no prefix),
        // causing `aegis update` to silently fall back to empty defaults and then
        // fail with "spec.database not configured".
        use std::fs;

        let tmp = tempfile::tempdir().expect("tempdir");
        let aegis_dir = tmp.path().join(".aegis");
        fs::create_dir_all(&aegis_dir).expect("create .aegis dir");
        let config_file = aegis_dir.join("aegis-config.yaml");
        fs::write(
            &config_file,
            "apiVersion: 100monkeys.ai/v1\nkind: NodeConfig\n",
        )
        .expect("write config");

        // Override HOME so discover_config() searches our temp dir.
        std::env::remove_var("AEGIS_CONFIG_PATH");
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", tmp.path());

        let discovered = NodeConfigManifest::discover_config();

        // Restore HOME before any assertions that might panic.
        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(
            discovered.as_deref(),
            Some(config_file.as_path()),
            "discover_config() should find ~/.aegis/aegis-config.yaml (the file aegis init writes)",
        );
    }

    #[test]
    fn test_validation() {
        let mut manifest = NodeConfigManifest::default();

        // Valid default should pass
        assert!(manifest.validate().is_ok());

        // Invalid apiVersion should fail
        manifest.api_version = "wrong/v1".to_string();
        assert!(manifest.validate().is_err());
        manifest.api_version = "100monkeys.ai/v1".to_string();

        // Invalid kind should fail
        manifest.kind = "WrongKind".to_string();
        assert!(manifest.validate().is_err());
        manifest.kind = "NodeConfig".to_string();

        // Empty metadata.name should fail
        manifest.metadata.name = "".to_string();
        assert!(manifest.validate().is_err());
        manifest.metadata.name = "test-node".to_string();

        // Empty node ID should fail
        manifest.spec.node.id = "".to_string();
        assert!(manifest.validate().is_err());
        manifest.spec.node.id = "test-node-id".to_string();

        // Add invalid provider (no models)
        manifest.spec.llm_providers.push(LLMProviderConfig {
            name: "invalid".to_string(),
            provider_type: "openai".to_string(),
            endpoint: "https://api.openai.com".to_string(),
            api_key: None,
            enabled: true,
            models: vec![],
        });
        assert!(manifest.validate().is_err());
    }
}
