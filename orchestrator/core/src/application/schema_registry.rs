// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Schema Registry
//!
//! Application-layer service that vends JSON Schema documents derived directly
//! from the Rust manifest struct definitions via `schemars`.
//!
//! ## Design
//!
//! The registry is built **once at startup** from `schemars::schema_for!(AgentManifest)`.
//! When the Rust structs change, the schema agents see is automatically updated —
//! there are no manually maintained JSON Schema files to keep in sync.
//!
//! ## Agent Workflow
//!
//! ```text
//! schema.get {"key": "agent/manifest/v1"}
//!     → compose manifest YAML
//!     → schema.validate {"kind": "agent", "manifest_yaml": "..."}
//!     → aegis.agent.create   (only when valid: true)
//! ```
//!
//! ## Supported Keys
//!
//! | Key | Struct |
//! |-----|--------|
//! | `"agent/manifest/v1"` | [`AgentManifest`] (full top-level schema) |

use crate::domain::agent::AgentManifest;
use crate::infrastructure::workflow_parser::WorkflowManifest;
use schemars::schema_for;
use serde_json::Value;
use std::collections::HashMap;

// =============================================================================
// ValidationResult
// =============================================================================

/// Result of validating a YAML manifest against its canonical JSON Schema.
#[derive(Debug)]
pub struct ValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
}

// =============================================================================
// SchemaRegistry
// =============================================================================

/// Immutable registry of JSON Schema documents derived from Rust manifest types.
///
/// Constructed once at startup via [`SchemaRegistry::build`]; never mutated.
pub struct SchemaRegistry {
    schemas: HashMap<&'static str, Value>,
}

impl SchemaRegistry {
    /// Build the registry by deriving schemas from Rust manifest structs.
    ///
    /// Panics at startup if any schema cannot be serialised — this is a
    /// programming error, not a runtime error, so panic is appropriate.
    pub fn build() -> Self {
        let mut schemas = HashMap::new();

        let agent_schema = serde_json::to_value(schema_for!(AgentManifest))
            .expect("AgentManifest JSON Schema serialisation must not fail");

        schemas.insert("agent/manifest/v1", agent_schema);

        let workflow_schema = serde_json::to_value(schema_for!(WorkflowManifest))
            .expect("WorkflowManifest JSON Schema serialisation must not fail");

        schemas.insert("workflow/manifest/v1", workflow_schema);

        Self { schemas }
    }

    /// Return the JSON Schema `Value` for `key`, or `None` if the key is unknown.
    ///
    /// Supported keys: `"agent/manifest/v1"`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.schemas.get(key)
    }

    /// Validate `manifest_yaml` against the schema for `kind`.
    ///
    /// `kind` mapping:
    /// - `"agent"` → `"agent/manifest/v1"`
    ///
    /// Returns [`ValidationResult`] with `valid: true` and an empty `errors`
    /// vector on success, or `valid: false` with human-readable error messages
    /// on failure (YAML parse errors, schema violations, or unknown kind).
    pub fn validate(&self, kind: &str, manifest_yaml: &str) -> ValidationResult {
        let schema_key = match kind {
            "agent" => "agent/manifest/v1",
            "workflow" => "workflow/manifest/v1",
            other => {
                return ValidationResult {
                    valid: false,
                    errors: vec![format!(
                        "Unknown kind '{}'. Supported kinds: agent, workflow",
                        other
                    )],
                }
            }
        };

        let Some(schema) = self.schemas.get(schema_key) else {
            return ValidationResult {
                valid: false,
                errors: vec![format!("Schema not found for key '{}'", schema_key)],
            };
        };

        // Parse YAML → serde_json::Value
        let instance: Value = match serde_yaml::from_str(manifest_yaml) {
            Ok(v) => v,
            Err(e) => {
                return ValidationResult {
                    valid: false,
                    errors: vec![format!("YAML parse error: {}", e)],
                }
            }
        };

        // Compile the JSON Schema and collect validation errors
        let compiled = match jsonschema::validator_for(schema) {
            Ok(v) => v,
            Err(e) => {
                return ValidationResult {
                    valid: false,
                    errors: vec![format!("Schema compilation error: {}", e)],
                }
            }
        };

        let errors: Vec<String> = compiled
            .iter_errors(&instance)
            .map(|e| e.to_string())
            .collect();

        ValidationResult {
            valid: errors.is_empty(),
            errors,
        }
    }
}
