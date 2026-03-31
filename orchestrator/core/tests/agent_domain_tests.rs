// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Agent Lifecycle Domain Tests (BC-1)
//!
//! Comprehensive unit tests for the Agent aggregate root and its value objects:
//! `Agent`, `AgentManifest`, `RuntimeConfig`, `SecurityConfig`, `ResourceLimits`,
//! `VolumeSpec`, `ExecutionStrategy`, `ScheduleConfig`, `ValidationConfig`, and
//! serialization round-trips.
//!
//! # Architecture
//!
//! - **Layer:** Domain
//! - **Bounded Context:** BC-1 Agent Lifecycle
//! - **Purpose:** Verify domain invariants, state transitions, and value-object semantics

use aegis_orchestrator_core::domain::agent::{
    Agent, AgentManifest, AgentSpec, AgentStatus, ContextItem, DeliveryCondition, DeliveryConfig,
    DeliveryDestination, DeliveryType, EmailConfig, ExecutionMode, ExecutionStrategy,
    FilesystemPolicy, ManifestMetadata, NetworkPolicy, ResourceLimits, RuntimeConfig, RuntimeType,
    ScheduleConfig, SecurityConfig, ValidatorSpec, VolumeSpec, WebhookConfig,
};
use aegis_orchestrator_core::domain::shared_kernel::{AgentId, ImagePullPolicy};
use aegis_orchestrator_core::domain::workflow::ConsensusStrategy;
use std::collections::HashMap;

// ============================================================================
// Test Fixtures
// ============================================================================

/// Build a minimal valid manifest with a standard runtime (language+version).
fn make_standard_manifest(name: &str) -> AgentManifest {
    AgentManifest {
        api_version: "100monkeys.ai/v1".to_string(),
        kind: "Agent".to_string(),
        metadata: ManifestMetadata {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            description: None,
            tags: vec![],
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
            },
            task: None,
            context: vec![],
            execution: None,
            security: None,
            schedule: None,
            tools: vec![],
            env: HashMap::new(),
            volumes: vec![],
            advanced: None,
        },
    }
}

/// Build a manifest with a custom Docker image runtime.
fn make_custom_runtime_manifest(name: &str, image: &str) -> AgentManifest {
    let mut m = make_standard_manifest(name);
    m.spec.runtime = RuntimeConfig {
        language: None,
        version: None,
        image: Some(image.to_string()),
        image_pull_policy: ImagePullPolicy::Always,
        isolation: "docker".to_string(),
        model: "default".to_string(),
    };
    m
}

/// Build a VolumeSpec for testing.
fn make_volume_spec(name: &str, storage_class: &str, mount_path: &str) -> VolumeSpec {
    VolumeSpec {
        name: name.to_string(),
        storage_class: storage_class.to_string(),
        volume_type: "seaweedfs".to_string(),
        provider: None,
        config: None,
        mount_path: mount_path.to_string(),
        access_mode: "read-write".to_string(),
        size_limit: "1Gi".to_string(),
        ttl_hours: None,
    }
}

// ============================================================================
// 1. Agent Creation
// ============================================================================

#[test]
fn agent_new_defaults_to_active() {
    let manifest = make_standard_manifest("my-agent");
    let agent = Agent::new(manifest);

    assert_eq!(agent.status, AgentStatus::Active);
    assert_eq!(agent.name, "my-agent");
}

#[test]
fn agent_new_copies_name_from_manifest_metadata() {
    let manifest = make_standard_manifest("code-reviewer");
    let agent = Agent::new(manifest);

    assert_eq!(agent.name, "code-reviewer");
    assert_eq!(agent.manifest.metadata.name, "code-reviewer");
}

#[test]
fn agent_new_generates_unique_ids() {
    let a = Agent::new(make_standard_manifest("agent-a"));
    let b = Agent::new(make_standard_manifest("agent-b"));

    assert_ne!(a.id, b.id);
}

#[test]
fn agent_new_sets_timestamps() {
    let agent = Agent::new(make_standard_manifest("ts-agent"));

    // created_at and updated_at should be very close to each other
    let diff = agent.updated_at - agent.created_at;
    assert!(diff.num_milliseconds().abs() < 100);
}

// ============================================================================
// 2. Agent State Transitions
// ============================================================================

#[test]
fn agent_pause_sets_paused_status() {
    let mut agent = Agent::new(make_standard_manifest("lifecycle-agent"));
    agent.pause();

    assert_eq!(agent.status, AgentStatus::Paused);
}

#[test]
fn agent_resume_returns_to_active() {
    let mut agent = Agent::new(make_standard_manifest("lifecycle-agent"));
    agent.pause();
    agent.resume();

    assert_eq!(agent.status, AgentStatus::Active);
}

#[test]
fn agent_archive_sets_archived_status() {
    let mut agent = Agent::new(make_standard_manifest("lifecycle-agent"));
    agent.archive();

    assert_eq!(agent.status, AgentStatus::Archived);
}

#[test]
fn agent_pause_updates_timestamp() {
    let mut agent = Agent::new(make_standard_manifest("ts-agent"));
    let before = agent.updated_at;
    // Small busy-wait to ensure time advances
    std::thread::sleep(std::time::Duration::from_millis(5));
    agent.pause();

    assert!(agent.updated_at >= before);
}

#[test]
fn agent_update_manifest_changes_name_and_manifest() {
    let mut agent = Agent::new(make_standard_manifest("old-name"));
    let new_manifest = make_standard_manifest("new-name");
    agent.update_manifest(new_manifest.clone());

    assert_eq!(agent.name, "new-name");
    assert_eq!(agent.manifest, new_manifest);
}

#[test]
fn agent_full_lifecycle_active_paused_active_archived() {
    let mut agent = Agent::new(make_standard_manifest("full-lifecycle"));

    assert_eq!(agent.status, AgentStatus::Active);
    agent.pause();
    assert_eq!(agent.status, AgentStatus::Paused);
    agent.resume();
    assert_eq!(agent.status, AgentStatus::Active);
    agent.archive();
    assert_eq!(agent.status, AgentStatus::Archived);
}

// ============================================================================
// 3. AgentManifest Validation
// ============================================================================

#[test]
fn manifest_valid_standard_runtime() {
    let manifest = make_standard_manifest("code-reviewer");
    assert!(manifest.validate().is_ok());
}

#[test]
fn manifest_invalid_api_version() {
    let mut m = make_standard_manifest("test");
    m.api_version = "aegis.ai/v2".to_string();
    let err = m.validate().unwrap_err();
    assert!(err.contains("apiVersion"));
}

#[test]
fn manifest_invalid_kind() {
    let mut m = make_standard_manifest("test");
    m.kind = "Workflow".to_string();
    let err = m.validate().unwrap_err();
    assert!(err.contains("kind"));
}

#[test]
fn manifest_empty_name_rejected() {
    let mut m = make_standard_manifest("test");
    m.metadata.name = String::new();
    let err = m.validate().unwrap_err();
    assert!(err.contains("name"));
}

#[test]
fn manifest_uppercase_name_rejected() {
    let mut m = make_standard_manifest("test");
    m.metadata.name = "MyAgent".to_string();
    let err = m.validate().unwrap_err();
    assert!(err.contains("lowercase"));
}

#[test]
fn manifest_leading_hyphen_rejected() {
    let mut m = make_standard_manifest("test");
    m.metadata.name = "-bad-name".to_string();
    let err = m.validate().unwrap_err();
    assert!(err.contains("hyphen"));
}

#[test]
fn manifest_trailing_hyphen_rejected() {
    let mut m = make_standard_manifest("test");
    m.metadata.name = "bad-name-".to_string();
    let err = m.validate().unwrap_err();
    assert!(err.contains("hyphen"));
}

#[test]
fn manifest_numeric_name_accepted() {
    let mut m = make_standard_manifest("test");
    m.metadata.name = "agent-42".to_string();
    assert!(m.validate().is_ok());
}

#[test]
fn manifest_runtime_string_for_standard_runtime() {
    let m = make_standard_manifest("test");
    assert_eq!(m.runtime_string(), Some("python:3.11".to_string()));
}

#[test]
fn manifest_runtime_string_none_for_custom_runtime() {
    let m = make_custom_runtime_manifest("test", "ghcr.io/myorg/agent:v1.0");
    assert_eq!(m.runtime_string(), None);
}

// ============================================================================
// 4. RuntimeConfig Validation
// ============================================================================

#[test]
fn runtime_config_valid_standard() {
    let rc = RuntimeConfig {
        language: Some("python".to_string()),
        version: Some("3.11".to_string()),
        image: None,
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        isolation: "inherit".to_string(),
        model: "default".to_string(),
    };
    assert!(rc.validate().is_ok());
    assert_eq!(rc.runtime_type(), RuntimeType::Standard);
}

#[test]
fn runtime_config_valid_custom() {
    let rc = RuntimeConfig {
        language: None,
        version: None,
        image: Some("ghcr.io/myorg/custom:v2".to_string()),
        image_pull_policy: ImagePullPolicy::Always,
        isolation: "docker".to_string(),
        model: "default".to_string(),
    };
    assert!(rc.validate().is_ok());
    assert_eq!(rc.runtime_type(), RuntimeType::Custom);
}

#[test]
fn runtime_config_both_specified_is_error() {
    let rc = RuntimeConfig {
        language: Some("python".to_string()),
        version: Some("3.11".to_string()),
        image: Some("ghcr.io/myorg/custom:v2".to_string()),
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        isolation: "inherit".to_string(),
        model: "default".to_string(),
    };
    let err = rc.validate().unwrap_err();
    assert!(err.contains("mutually exclusive"));
}

#[test]
fn runtime_config_neither_specified_is_error() {
    let rc = RuntimeConfig {
        language: None,
        version: None,
        image: None,
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        isolation: "inherit".to_string(),
        model: "default".to_string(),
    };
    let err = rc.validate().unwrap_err();
    assert!(err.contains("must specify"));
}

#[test]
fn runtime_config_language_without_version_is_error() {
    let rc = RuntimeConfig {
        language: Some("python".to_string()),
        version: None,
        image: None,
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        isolation: "inherit".to_string(),
        model: "default".to_string(),
    };
    let err = rc.validate().unwrap_err();
    assert!(err.contains("language requires version"));
}

#[test]
fn runtime_config_version_without_language_is_error() {
    let rc = RuntimeConfig {
        language: None,
        version: Some("3.11".to_string()),
        image: None,
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        isolation: "inherit".to_string(),
        model: "default".to_string(),
    };
    let err = rc.validate().unwrap_err();
    assert!(err.contains("version requires language"));
}

#[test]
fn runtime_config_custom_image_must_be_fully_qualified() {
    let rc = RuntimeConfig {
        language: None,
        version: None,
        image: Some("myimage:latest".to_string()), // no slash → not fully qualified
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        isolation: "inherit".to_string(),
        model: "default".to_string(),
    };
    let err = rc.validate().unwrap_err();
    assert!(err.contains("fully-qualified"));
}

// ============================================================================
// 5. SecurityConfig Defaults
// ============================================================================

#[test]
fn security_config_network_defaults() {
    let np = NetworkPolicy::default();
    assert!(np.allowlist.is_empty());
    assert!(np.denylist.is_empty());
}

#[test]
fn security_config_filesystem_defaults() {
    let fp = FilesystemPolicy::default();
    assert!(fp.read.is_empty());
    assert!(fp.write.is_empty());
    assert!(!fp.read_only);
}

#[test]
fn security_config_with_custom_network() {
    let sec = SecurityConfig {
        network: NetworkPolicy {
            mode: "allow".to_string(),
            allowlist: vec!["github.com".to_string(), "api.openai.com".to_string()],
            denylist: vec![],
        },
        filesystem: FilesystemPolicy::default(),
        resources: ResourceLimits::default(),
    };
    assert_eq!(sec.network.allowlist.len(), 2);
    assert_eq!(sec.network.mode, "allow");
}

#[test]
fn security_config_filesystem_read_only() {
    let fp = FilesystemPolicy {
        read: vec!["/workspace".to_string()],
        write: vec![],
        read_only: true,
    };
    assert!(fp.read_only);
    assert_eq!(fp.read.len(), 1);
}

// ============================================================================
// 6. ResourceLimits
// ============================================================================

#[test]
fn resource_limits_default_values() {
    let limits = ResourceLimits::default();
    assert_eq!(limits.cpu, 1000);
    assert_eq!(limits.memory, "512Mi");
    assert_eq!(limits.disk, "1Gi");
    assert!(limits.timeout.is_none());
}

#[test]
fn parse_size_to_bytes_mebibytes() {
    assert_eq!(
        ResourceLimits::parse_size_to_bytes("512Mi"),
        Some(512 * 1024 * 1024)
    );
}

#[test]
fn parse_size_to_bytes_gibibytes() {
    assert_eq!(
        ResourceLimits::parse_size_to_bytes("1Gi"),
        Some(1024 * 1024 * 1024)
    );
    assert_eq!(
        ResourceLimits::parse_size_to_bytes("4Gi"),
        Some(4 * 1024 * 1024 * 1024)
    );
}

#[test]
fn parse_size_to_bytes_kibibytes() {
    assert_eq!(
        ResourceLimits::parse_size_to_bytes("128Ki"),
        Some(128 * 1024)
    );
}

#[test]
fn parse_size_to_bytes_plain_number() {
    assert_eq!(ResourceLimits::parse_size_to_bytes("4096"), Some(4096));
}

#[test]
fn parse_size_to_bytes_invalid_returns_none() {
    assert_eq!(ResourceLimits::parse_size_to_bytes("not-a-size"), None);
    assert_eq!(ResourceLimits::parse_size_to_bytes(""), None);
}

#[test]
fn memory_bytes_and_disk_bytes() {
    let limits = ResourceLimits {
        cpu: 2000,
        memory: "256Mi".to_string(),
        disk: "2Gi".to_string(),
        timeout: None,
    };
    assert_eq!(limits.memory_bytes(), Some(256 * 1024 * 1024));
    assert_eq!(limits.disk_bytes(), Some(2 * 1024 * 1024 * 1024));
}

#[test]
fn parse_timeout_seconds_with_s_suffix() {
    let limits = ResourceLimits {
        timeout: Some("60s".to_string()),
        ..ResourceLimits::default()
    };
    assert_eq!(limits.parse_timeout_seconds(), Some(60));
}

#[test]
fn parse_timeout_seconds_with_m_suffix() {
    let limits = ResourceLimits {
        timeout: Some("5m".to_string()),
        ..ResourceLimits::default()
    };
    assert_eq!(limits.parse_timeout_seconds(), Some(300));
}

#[test]
fn parse_timeout_seconds_with_h_suffix() {
    let limits = ResourceLimits {
        timeout: Some("2h".to_string()),
        ..ResourceLimits::default()
    };
    assert_eq!(limits.parse_timeout_seconds(), Some(7200));
}

#[test]
fn parse_timeout_seconds_bare_integer() {
    let limits = ResourceLimits {
        timeout: Some("120".to_string()),
        ..ResourceLimits::default()
    };
    assert_eq!(limits.parse_timeout_seconds(), Some(120));
}

#[test]
fn parse_timeout_seconds_none_when_unset() {
    let limits = ResourceLimits::default();
    assert_eq!(limits.parse_timeout_seconds(), None);
}

#[test]
fn parse_timeout_seconds_empty_string_returns_none() {
    let limits = ResourceLimits {
        timeout: Some("".to_string()),
        ..ResourceLimits::default()
    };
    assert_eq!(limits.parse_timeout_seconds(), None);
}

// ============================================================================
// 7. VolumeSpec
// ============================================================================

#[test]
fn volume_spec_ephemeral_with_ttl() {
    let vol = VolumeSpec {
        name: "scratch".to_string(),
        storage_class: "ephemeral".to_string(),
        volume_type: "seaweedfs".to_string(),
        provider: None,
        config: None,
        mount_path: "/tmp/scratch".to_string(),
        access_mode: "read-write".to_string(),
        size_limit: "500Mi".to_string(),
        ttl_hours: Some(24),
    };
    assert_eq!(vol.storage_class, "ephemeral");
    assert_eq!(vol.ttl_hours, Some(24));
    assert_eq!(vol.volume_type, "seaweedfs");
}

#[test]
fn volume_spec_persistent() {
    let vol = make_volume_spec("data", "persistent", "/data");
    assert_eq!(vol.storage_class, "persistent");
    assert!(vol.ttl_hours.is_none());
}

#[test]
fn volume_spec_with_opendal_provider() {
    let vol = VolumeSpec {
        name: "s3-data".to_string(),
        storage_class: "persistent".to_string(),
        volume_type: "opendal".to_string(),
        provider: Some("s3".to_string()),
        config: Some(serde_json::json!({"bucket": "my-bucket"})),
        mount_path: "/mnt/s3".to_string(),
        access_mode: "read-only".to_string(),
        size_limit: "10Gi".to_string(),
        ttl_hours: None,
    };
    assert_eq!(vol.volume_type, "opendal");
    assert_eq!(vol.provider, Some("s3".to_string()));
    assert_eq!(vol.access_mode, "read-only");
}

#[test]
fn volume_spec_smcp_backend() {
    let vol = VolumeSpec {
        name: "remote-vol".to_string(),
        storage_class: "persistent".to_string(),
        volume_type: "smcp".to_string(),
        provider: None,
        config: None,
        mount_path: "/mnt/remote".to_string(),
        access_mode: "read-write".to_string(),
        size_limit: "2Gi".to_string(),
        ttl_hours: None,
    };
    assert_eq!(vol.volume_type, "smcp");
}

// ============================================================================
// 8. ExecutionStrategy
// ============================================================================

#[test]
fn execution_strategy_defaults() {
    let es = ExecutionStrategy::default();
    assert_eq!(es.max_retries, 5);
    assert_eq!(es.llm_timeout_seconds, 300);
    assert!(matches!(es.mode, ExecutionMode::OneShot));
    assert!(es.validation.is_none());
    assert!(es.tool_validation.is_none());
    assert!(es.delivery.is_none());
    assert!(es.iteration_timeout.is_none());
}

#[test]
fn execution_strategy_iterative_with_validation() {
    let es = ExecutionStrategy {
        mode: ExecutionMode::Iterative,
        max_retries: 10,
        iteration_timeout: Some("60s".to_string()),
        llm_timeout_seconds: 120,
        validation: Some(vec![ValidatorSpec::ExitCode {
            expected: 0,
            min_score: 1.0,
        }]),
        tool_validation: None,
        delivery: None,
    };
    assert!(matches!(es.mode, ExecutionMode::Iterative));
    assert_eq!(es.max_retries, 10);
    assert!(es.validation.is_some());
    assert_eq!(es.validation.as_ref().unwrap().len(), 1);
}

#[test]
fn execution_strategy_with_delivery_config() {
    let es = ExecutionStrategy {
        delivery: Some(DeliveryConfig {
            destinations: vec![DeliveryDestination {
                name: "notify-email".to_string(),
                condition: DeliveryCondition::OnSuccess,
                transform: None,
                config: DeliveryType::Email {
                    email: EmailConfig {
                        to: "admin@example.com".to_string(),
                        subject: "Agent completed".to_string(),
                        body_template: None,
                        attachments: vec![],
                    },
                },
            }],
        }),
        ..ExecutionStrategy::default()
    };
    let delivery = es.delivery.unwrap();
    assert_eq!(delivery.destinations.len(), 1);
    assert_eq!(delivery.destinations[0].name, "notify-email");
    assert!(matches!(
        delivery.destinations[0].condition,
        DeliveryCondition::OnSuccess
    ));
}

#[test]
fn execution_strategy_webhook_delivery() {
    let dest = DeliveryDestination {
        name: "webhook-notify".to_string(),
        condition: DeliveryCondition::Always,
        transform: None,
        config: DeliveryType::Webhook {
            webhook: WebhookConfig {
                url: "https://hooks.example.com/notify".to_string(),
                method: "POST".to_string(),
                headers: HashMap::new(),
                body: None,
            },
        },
    };
    assert!(matches!(dest.config, DeliveryType::Webhook { .. }));
}

// ============================================================================
// 9. ScheduleConfig
// ============================================================================

#[test]
fn schedule_config_cron_variant() {
    let sc = ScheduleConfig::Cron {
        cron: "0 */6 * * *".to_string(),
        timezone: "UTC".to_string(),
        enabled: true,
    };
    assert!(matches!(sc, ScheduleConfig::Cron { .. }));
    if let ScheduleConfig::Cron {
        cron,
        timezone,
        enabled,
    } = &sc
    {
        assert_eq!(cron, "0 */6 * * *");
        assert_eq!(timezone, "UTC");
        assert!(enabled);
    }
}

#[test]
fn schedule_config_interval_variant() {
    let sc = ScheduleConfig::Interval {
        seconds: 3600,
        enabled: true,
    };
    if let ScheduleConfig::Interval { seconds, enabled } = &sc {
        assert_eq!(*seconds, 3600);
        assert!(enabled);
    } else {
        panic!("expected Interval variant");
    }
}

#[test]
fn schedule_config_manual_variant() {
    let sc = ScheduleConfig::Manual;
    assert!(matches!(sc, ScheduleConfig::Manual));
}

// ============================================================================
// 10. ValidationConfig (Vec<ValidatorSpec>)
// ============================================================================

#[test]
fn validator_spec_exit_code() {
    let v = ValidatorSpec::ExitCode {
        expected: 0,
        min_score: 1.0,
    };
    assert!(matches!(v, ValidatorSpec::ExitCode { expected: 0, .. }));
}

#[test]
fn validator_spec_json_schema() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["result"]
    });
    let v = ValidatorSpec::JsonSchema {
        schema: schema.clone(),
        min_score: 1.0,
    };
    if let ValidatorSpec::JsonSchema {
        schema: s,
        min_score,
    } = &v
    {
        assert_eq!(s, &schema);
        assert_eq!(*min_score, 1.0);
    } else {
        panic!("expected JsonSchema variant");
    }
}

#[test]
fn validator_spec_regex() {
    let v = ValidatorSpec::Regex {
        pattern: r"^\{.*\}$".to_string(),
        target: "stdout".to_string(),
        min_score: 1.0,
    };
    if let ValidatorSpec::Regex {
        pattern, target, ..
    } = &v
    {
        assert_eq!(pattern, r"^\{.*\}$");
        assert_eq!(target, "stdout");
    } else {
        panic!("expected Regex variant");
    }
}

#[test]
fn validator_spec_semantic() {
    let v = ValidatorSpec::Semantic {
        judge_agent: "quality-judge".to_string(),
        criteria: "Output must be idiomatic Rust".to_string(),
        min_score: 0.8,
        min_confidence: 0.5,
        timeout_seconds: 120,
    };
    if let ValidatorSpec::Semantic {
        judge_agent,
        criteria,
        min_score,
        ..
    } = &v
    {
        assert_eq!(judge_agent, "quality-judge");
        assert_eq!(criteria, "Output must be idiomatic Rust");
        assert_eq!(*min_score, 0.8);
    } else {
        panic!("expected Semantic variant");
    }
}

#[test]
fn validator_spec_multi_judge() {
    let v = ValidatorSpec::MultiJudge {
        judges: vec![
            "security-judge".to_string(),
            "style-judge".to_string(),
            "correctness-judge".to_string(),
        ],
        consensus: ConsensusStrategy::Majority,
        min_judges_required: 2,
        criteria: "Evaluate code quality".to_string(),
        min_score: 0.7,
        min_confidence: 0.0,
        timeout_seconds: 300,
    };
    if let ValidatorSpec::MultiJudge {
        judges,
        consensus,
        min_judges_required,
        ..
    } = &v
    {
        assert_eq!(judges.len(), 3);
        assert!(matches!(consensus, ConsensusStrategy::Majority));
        assert_eq!(*min_judges_required, 2);
    } else {
        panic!("expected MultiJudge variant");
    }
}

#[test]
fn validation_config_ordered_pipeline() {
    let pipeline: Vec<ValidatorSpec> = vec![
        ValidatorSpec::ExitCode {
            expected: 0,
            min_score: 1.0,
        },
        ValidatorSpec::Regex {
            pattern: r"\{".to_string(),
            target: "stdout".to_string(),
            min_score: 1.0,
        },
        ValidatorSpec::Semantic {
            judge_agent: "output-judge".to_string(),
            criteria: "Must be valid JSON".to_string(),
            min_score: 0.7,
            min_confidence: 0.0,
            timeout_seconds: 300,
        },
    ];
    assert_eq!(pipeline.len(), 3);
    assert!(matches!(pipeline[0], ValidatorSpec::ExitCode { .. }));
    assert!(matches!(pipeline[1], ValidatorSpec::Regex { .. }));
    assert!(matches!(pipeline[2], ValidatorSpec::Semantic { .. }));
}

// ============================================================================
// 11. Serialization Round-Trips
// ============================================================================

#[test]
fn manifest_json_round_trip() {
    let original = make_standard_manifest("round-trip-agent");
    let json = serde_json::to_string_pretty(&original).expect("serialize to JSON");
    let deserialized: AgentManifest = serde_json::from_str(&json).expect("deserialize from JSON");
    assert_eq!(original, deserialized);
}

#[test]
fn manifest_yaml_round_trip() {
    let original = make_standard_manifest("yaml-agent");
    let yaml = serde_yaml::to_string(&original).expect("serialize to YAML");
    let deserialized: AgentManifest = serde_yaml::from_str(&yaml).expect("deserialize from YAML");
    assert_eq!(original, deserialized);
}

#[test]
fn custom_runtime_manifest_json_round_trip() {
    let original = make_custom_runtime_manifest("custom-rt", "ghcr.io/org/image:v1.0");
    let json = serde_json::to_string(&original).expect("serialize to JSON");
    let deserialized: AgentManifest = serde_json::from_str(&json).expect("deserialize from JSON");
    assert_eq!(original, deserialized);
}

#[test]
fn schedule_config_cron_json_round_trip() {
    let original = ScheduleConfig::Cron {
        cron: "0 0 * * *".to_string(),
        timezone: "America/New_York".to_string(),
        enabled: true,
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let deserialized: ScheduleConfig = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(original, deserialized);
}

#[test]
fn schedule_config_interval_json_round_trip() {
    let original = ScheduleConfig::Interval {
        seconds: 1800,
        enabled: false,
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let deserialized: ScheduleConfig = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(original, deserialized);
}

#[test]
fn schedule_config_manual_json_round_trip() {
    let original = ScheduleConfig::Manual;
    let json = serde_json::to_string(&original).expect("serialize");
    let deserialized: ScheduleConfig = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(original, deserialized);
}

#[test]
fn volume_spec_json_round_trip() {
    let original = make_volume_spec("workspace", "persistent", "/workspace");
    let json = serde_json::to_string(&original).expect("serialize");
    let deserialized: VolumeSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(original, deserialized);
}

#[test]
fn execution_strategy_json_round_trip() {
    let original = ExecutionStrategy {
        mode: ExecutionMode::Iterative,
        max_retries: 3,
        iteration_timeout: Some("120s".to_string()),
        llm_timeout_seconds: 60,
        validation: Some(vec![ValidatorSpec::ExitCode {
            expected: 0,
            min_score: 1.0,
        }]),
        tool_validation: None,
        delivery: None,
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let deserialized: ExecutionStrategy = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(original, deserialized);
}

#[test]
fn security_config_json_round_trip() {
    let original = SecurityConfig {
        network: NetworkPolicy {
            mode: "allow".to_string(),
            allowlist: vec!["github.com".to_string()],
            denylist: vec![],
        },
        filesystem: FilesystemPolicy {
            read: vec!["/workspace".to_string()],
            write: vec!["/output".to_string()],
            read_only: false,
        },
        resources: ResourceLimits {
            cpu: 2000,
            memory: "1Gi".to_string(),
            disk: "5Gi".to_string(),
            timeout: Some("10m".to_string()),
        },
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let deserialized: SecurityConfig = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(original, deserialized);
}

#[test]
fn resource_limits_yaml_round_trip() {
    let original = ResourceLimits {
        cpu: 4000,
        memory: "2Gi".to_string(),
        disk: "10Gi".to_string(),
        timeout: Some("1h".to_string()),
    };
    let yaml = serde_yaml::to_string(&original).expect("serialize to YAML");
    let deserialized: ResourceLimits = serde_yaml::from_str(&yaml).expect("deserialize from YAML");
    assert_eq!(original, deserialized);
}

#[test]
fn agent_status_json_round_trip() {
    let statuses = vec![
        AgentStatus::Active,
        AgentStatus::Paused,
        AgentStatus::Archived,
        AgentStatus::Failed,
    ];
    for status in statuses {
        let json = serde_json::to_string(&status).expect("serialize");
        let deserialized: AgentStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(status, deserialized);
    }
}

#[test]
fn image_pull_policy_json_round_trip() {
    let policies = vec![
        ImagePullPolicy::Always,
        ImagePullPolicy::IfNotPresent,
        ImagePullPolicy::Never,
    ];
    for policy in policies {
        let json = serde_json::to_string(&policy).expect("serialize");
        let deserialized: ImagePullPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(policy, deserialized);
    }
}

#[test]
fn context_item_variants_json_round_trip() {
    let items = vec![
        ContextItem::Text {
            content: "Some context".to_string(),
            description: Some("background info".to_string()),
        },
        ContextItem::File {
            path: "/workspace/input.txt".to_string(),
            description: None,
        },
        ContextItem::Directory {
            path: "/workspace/src".to_string(),
            description: Some("source code".to_string()),
        },
        ContextItem::Url {
            url: "https://example.com/spec.md".to_string(),
            description: None,
        },
    ];
    for item in items {
        let json = serde_json::to_string(&item).expect("serialize");
        let deserialized: ContextItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(item, deserialized);
    }
}

#[test]
fn validator_spec_exit_code_json_round_trip() {
    let original = ValidatorSpec::ExitCode {
        expected: 1,
        min_score: 0.5,
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let deserialized: ValidatorSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(original, deserialized);
}

#[test]
fn full_manifest_yaml_parsing() {
    let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: code-reviewer
  version: "1.0.0"
  description: "Reviews pull requests"
  labels:
    team: backend
spec:
  runtime:
    language: python
    version: "3.11"
    model: smart
  task:
    instruction: "Review the code for correctness"
  execution:
    mode: iterative
    max_retries: 3
    llm_timeout_seconds: 120
    validation:
      - type: exit_code
        expected: 0
      - type: regex
        pattern: "PASS"
        target: stdout
  security:
    network:
      mode: allow
      allowlist:
        - github.com
    filesystem:
      read:
        - /workspace
      write:
        - /output
      read_only: false
    resources:
      cpu: 2000
      memory: 1Gi
      disk: 5Gi
      timeout: 10m
  schedule:
    type: cron
    cron: "0 */6 * * *"
    timezone: UTC
  volumes:
    - name: workspace
      storage_class: persistent
      type: seaweedfs
      mount_path: /workspace
      access_mode: read-write
      size_limit: 2Gi
  tools:
    - github-mcp
  env:
    REVIEW_MODE: strict
"#;
    let manifest: AgentManifest = serde_yaml::from_str(yaml).expect("parse YAML manifest");
    assert!(manifest.validate().is_ok());
    assert_eq!(manifest.metadata.name, "code-reviewer");
    assert_eq!(
        manifest.metadata.description,
        Some("Reviews pull requests".to_string())
    );
    assert_eq!(manifest.metadata.labels.get("team").unwrap(), "backend");
    assert_eq!(manifest.spec.runtime.language, Some("python".to_string()));
    assert_eq!(manifest.spec.runtime.model, "smart");
    assert!(manifest.spec.task.is_some());
    assert!(manifest.spec.execution.is_some());

    let exec = manifest.spec.execution.as_ref().unwrap();
    assert!(matches!(exec.mode, ExecutionMode::Iterative));
    assert_eq!(exec.max_retries, 3);
    assert_eq!(exec.validation.as_ref().unwrap().len(), 2);

    assert!(matches!(
        manifest.spec.schedule,
        Some(ScheduleConfig::Cron { .. })
    ));
    assert_eq!(manifest.spec.volumes.len(), 1);
    assert_eq!(manifest.spec.tools, vec!["github-mcp"]);
    assert_eq!(manifest.spec.env.get("REVIEW_MODE").unwrap(), "strict");

    let sec = manifest.spec.security.as_ref().unwrap();
    assert_eq!(sec.resources.cpu, 2000);
    assert_eq!(sec.resources.memory, "1Gi");
    assert_eq!(sec.resources.parse_timeout_seconds(), Some(600));
}

// ============================================================================
// AgentId Shared Kernel
// ============================================================================

#[test]
fn agent_id_display_matches_uuid() {
    let id = AgentId::new();
    let display = format!("{id}");
    // Should be a valid UUID string
    assert!(uuid::Uuid::parse_str(&display).is_ok());
}

#[test]
fn agent_id_from_string_invalid() {
    assert!(AgentId::from_string("garbage").is_err());
}

#[test]
fn agent_id_equality() {
    let id = AgentId::new();
    let cloned = id;
    assert_eq!(id, cloned);
}
