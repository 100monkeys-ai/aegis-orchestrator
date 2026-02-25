// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Agent Manifest YAML Parser
//!
//! This module provides infrastructure for parsing agent YAML manifests
//! into domain objects, following the K8s-style format defined in MANIFEST_SPEC_V1.md.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Parse external YAML → Domain objects
//! - **Anti-Corruption:** Translates YAML schema to domain model
//!
//! # Manifest Format
//!
//! ```yaml
//! apiVersion: 100monkeys.ai/v1
//! kind: Agent
//! metadata:
//!   name: email-summarizer
//!   version: "1.0.0"
//!   description: "Summarizes emails using AI"
//! spec:
//!   runtime:
//!     language: "python"
//!     version: "3.11"
//!   task:
//!     instruction: "Summarize emails from the last 24 hours"
//!   execution:
//!     mode: "iterative"
//!     max_iterations: 10
//! ```

use crate::domain::agent::*;
use anyhow::{anyhow, Context, Result};
use std::path::Path;

// ============================================================================
// Parser API
// ============================================================================

pub struct AgentManifestParser;

impl AgentManifestParser {
    /// Parse agent manifest from YAML string
    pub fn parse_yaml(yaml: &str) -> Result<AgentManifest> {
        let mut manifest: AgentManifest =
            serde_yaml::from_str(yaml).context("Failed to parse YAML manifest")?;

        manifest.apply_defaults();

        // Validate the parsed manifest
        manifest
            .validate()
            .map_err(|e| anyhow!("Manifest validation failed: {}", e))?;

        Ok(manifest)
    }

    /// Parse agent manifest from YAML file
    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<AgentManifest> {
        let yaml = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read manifest file: {:?}", path.as_ref()))?;

        Self::parse_yaml(&yaml)
    }

    /// Serialize agent manifest to YAML string
    pub fn to_yaml(manifest: &AgentManifest) -> Result<String> {
        serde_yaml::to_string(manifest).context("Failed to serialize manifest to YAML")
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_manifest() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
"#;

        let manifest = AgentManifestParser::parse_yaml(yaml).unwrap();
        assert_eq!(manifest.api_version, "100monkeys.ai/v1");
        assert_eq!(manifest.kind, "Agent");
        assert_eq!(manifest.metadata.name, "test-agent");
        assert_eq!(manifest.spec.runtime.language, "python");
        assert_eq!(manifest.spec.runtime.version, "3.11");
    }

    #[test]
    fn test_parse_full_manifest() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: email-summarizer
  version: "1.0.0"
  description: "Summarizes emails using AI"
  labels:
    role: worker
    category: productivity
spec:
  runtime:
    language: python
    version: "3.11"
    entrypoint: main.py
    isolation: docker
  task:
    agentskills:
      - email:imap-reader
    instruction: |
      Summarize emails from the last 24 hours
    prompt_template: |
      {instruction}
      
      User: {input}
  execution:
    mode: iterative
    max_iterations: 10
    validation:
      system:
        must_succeed: true
        timeout_seconds: 90
      semantic:
        enabled: true
        model: "default"
        prompt: "Evaluate the output"
        threshold: 0.8
  security:
    network:
      mode: allow
      allowlist:
        - "imap.gmail.com"
    filesystem:
      read:
        - /data
      write:
        - /data/output
    resources:
      cpu: 1000
      memory: "512Mi"
      disk: "1Gi"
  tools:
    - "mcp:gmail"
  env:
    DEBUG: "true"
"#;

        let manifest = AgentManifestParser::parse_yaml(yaml).unwrap();
        assert_eq!(manifest.metadata.name, "email-summarizer");
        assert_eq!(manifest.spec.runtime.language, "python");

        // Check task
        let task = manifest.spec.task.as_ref().unwrap();
        assert_eq!(task.agentskills.len(), 1);
        assert!(task.instruction.is_some());

        // Check execution
        let execution = manifest.spec.execution.as_ref().unwrap();
        assert_eq!(execution.mode, ExecutionMode::Iterative);
        assert_eq!(execution.max_retries, 10);

        // Check security
        let security = manifest.spec.security.as_ref().unwrap();
        assert_eq!(security.network.mode, "allow");
        assert_eq!(security.network.allowlist.len(), 1);
        assert_eq!(security.resources.cpu, 1000);
    }

    #[test]
    fn test_parse_semantic_defaults_when_missing_model() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: semantic-defaults
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: "Summarize the input"
  execution:
    validation:
      semantic:
        prompt: "Judge this output"
"#;

        let manifest = AgentManifestParser::parse_yaml(yaml).unwrap();
        let semantic = manifest
            .spec
            .execution
            .as_ref()
            .and_then(|exec| exec.validation.as_ref())
            .and_then(|validation| validation.semantic.as_ref())
            .expect("semantic validation should be defaulted");

        assert_eq!(semantic.model, "default");
        assert!(!semantic.prompt.trim().is_empty());
    }

    #[test]
    fn test_auto_enable_semantic_when_task_instruction_present() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: semantic-auto
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: "Review this code"
"#;

        let manifest = AgentManifestParser::parse_yaml(yaml).unwrap();
        let semantic = manifest
            .spec
            .execution
            .as_ref()
            .and_then(|exec| exec.validation.as_ref())
            .and_then(|validation| validation.semantic.as_ref())
            .expect("semantic validation should be auto-created");

        assert_eq!(semantic.model, "default");
        assert_eq!(semantic.prompt, DEFAULT_SEMANTIC_PROMPT_TEMPLATE);
        assert!(semantic.enabled);
    }

    #[test]
    fn test_validate_api_version() {
        let yaml = r#"
apiVersion: invalid/v1
kind: Agent
metadata:
  name: test
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
"#;

        let result = AgentManifestParser::parse_yaml(yaml);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid apiVersion"));
    }

    #[test]
    fn test_validate_kind() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: WrongKind
metadata:
  name: test
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
"#;

        let result = AgentManifestParser::parse_yaml(yaml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid kind"));
    }

    #[test]
    fn test_validate_name_format() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: INVALID_NAME
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
"#;

        let result = AgentManifestParser::parse_yaml(yaml);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid metadata.name"));
    }

    #[test]
    fn test_roundtrip_serialization() {
        use std::collections::HashMap;

        let manifest = AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: ManifestMetadata {
                name: "test-agent".to_string(),
                version: "1.0.0".to_string(),
                description: Some("Test agent".to_string()),
                labels: HashMap::new(),
                annotations: HashMap::new(),
            },
            spec: AgentSpec {
                runtime: RuntimeConfig {
                    language: "python".to_string(),
                    version: "3.11".to_string(),
                    isolation: "inherit".to_string(),
                    autopull: true,
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
        };

        // Serialize to YAML
        let yaml = AgentManifestParser::to_yaml(&manifest).unwrap();

        // Parse back
        let parsed = AgentManifestParser::parse_yaml(&yaml).unwrap();

        // Compare
        assert_eq!(parsed.metadata.name, manifest.metadata.name);
        assert_eq!(parsed.spec.runtime.language, manifest.spec.runtime.language);
    }
}
