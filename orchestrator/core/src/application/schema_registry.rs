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
// Flattened tagged enums
// =============================================================================

/// Copy a struct's own properties into each closed variant of the tagged enum
/// it flattens.
///
/// A workflow state (`WorkflowStateYaml`) flattens `StateKindYaml`, an
/// internally tagged enum with `deny_unknown_fields`, beside `transitions`,
/// `timeout` and `max_state_visits`. schemars 0.8 renders that as the
/// struct's `properties` next to a `oneOf` whose every branch carries
/// `additionalProperties: false` and only the variant's own properties, and
/// `additionalProperties` in a branch sees only that branch's `properties`, so
/// a state carrying `transitions` fails every branch although serde parses it.
/// Each closed branch here gains the struct's properties it does not already
/// name; it stays closed, so a field neither the struct nor the variant
/// declares is still refused. The struct's `required` stays where it is.
fn admit_flattened_fields(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            let common = match (map.get("properties"), map.get("oneOf")) {
                (Some(Value::Object(properties)), Some(Value::Array(_))) => {
                    Some(properties.clone())
                }
                _ => None,
            };
            if let (Some(common), Some(Value::Array(branches))) = (common, map.get_mut("oneOf")) {
                for branch in branches.iter_mut() {
                    let Value::Object(branch) = branch else {
                        continue;
                    };
                    if branch.get("additionalProperties") != Some(&Value::Bool(false)) {
                        continue;
                    }
                    if let Some(Value::Object(own)) = branch.get_mut("properties") {
                        for (name, property) in &common {
                            own.entry(name.clone()).or_insert_with(|| property.clone());
                        }
                    }
                }
            }
            for child in map.values_mut() {
                admit_flattened_fields(child);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                admit_flattened_fields(item);
            }
        }
        _ => {}
    }
}

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

        let mut workflow_schema = serde_json::to_value(schema_for!(WorkflowManifest))
            .expect("WorkflowManifest JSON Schema serialisation must not fail");
        admit_flattened_fields(&mut workflow_schema);

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
                };
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
                };
            }
        };

        // Compile the JSON Schema and collect validation errors
        let compiled = match jsonschema::validator_for(schema) {
            Ok(v) => v,
            Err(e) => {
                return ValidationResult {
                    valid: false,
                    errors: vec![format!("Schema compilation error: {}", e)],
                };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The Forge as it ships, read from the template the CLI installs.
    const THE_FORGE: &str = include_str!("../../../../cli/templates/workflows/the-forge.yaml");

    fn one_state_workflow(state_yaml: &str) -> String {
        format!(
            "apiVersion: 100monkeys.ai/v1\n\
             kind: Workflow\n\
             metadata:\n  name: one-state\n  version: \"1.0.0\"\n\
             spec:\n  initial_state: ONLY\n  states:\n    ONLY:\n{state_yaml}"
        )
    }

    #[test]
    fn the_forge_manifest_validates_as_a_workflow() {
        let registry = SchemaRegistry::build();
        let result = registry.validate("workflow", THE_FORGE);
        assert!(
            result.valid,
            "the-forge.yaml was refused: {:?}",
            result.errors
        );
    }

    #[test]
    fn an_action_state_with_transitions_validates() {
        let registry = SchemaRegistry::build();
        let manifest = one_state_workflow(
            "      kind: Action\n      action: image_generate\n      args:\n        prompt: \"a monkey\"\n        size: \"1024x1024\"\n      transitions: []\n",
        );
        let result = registry.validate("workflow", &manifest);
        assert!(result.valid, "refused: {:?}", result.errors);
    }

    #[test]
    fn a_system_state_with_transitions_timeout_and_visits_validates() {
        let registry = SchemaRegistry::build();
        let manifest = one_state_workflow(
            "      kind: System\n      command: \"true\"\n      timeout: 30s\n      max_state_visits: 3\n      transitions:\n        - condition: always\n          target: ONLY\n",
        );
        let result = registry.validate("workflow", &manifest);
        assert!(result.valid, "refused: {:?}", result.errors);
    }

    #[test]
    fn a_state_with_an_unknown_field_is_still_refused() {
        let registry = SchemaRegistry::build();
        let manifest = one_state_workflow(
            "      kind: Action\n      action: image_generate\n      colour: blue\n      transitions: []\n",
        );
        let result = registry.validate("workflow", &manifest);
        assert!(!result.valid, "a state with an unknown field validated");
    }

    #[test]
    fn a_state_without_transitions_is_refused() {
        let registry = SchemaRegistry::build();
        let manifest = one_state_workflow("      kind: Action\n      action: image_generate\n");
        let result = registry.validate("workflow", &manifest);
        assert!(!result.valid, "a state without transitions validated");
    }

    /// Count the closed `oneOf` branches that lack a property declared beside
    /// the `oneOf`: each is a field the parser accepts and the schema refuses.
    fn closed_branches_missing_siblings(schema: &Value) -> usize {
        match schema {
            Value::Object(map) => {
                let mut count = 0;
                if let (Some(Value::Object(common)), Some(Value::Array(branches))) =
                    (map.get("properties"), map.get("oneOf"))
                {
                    for branch in branches {
                        let closed =
                            branch.get("additionalProperties") == Some(&Value::Bool(false));
                        let own = branch.get("properties").and_then(Value::as_object);
                        if closed
                            && own.is_some_and(|own| common.keys().any(|k| !own.contains_key(k)))
                        {
                            count += 1;
                        }
                    }
                }
                count
                    + map
                        .values()
                        .map(closed_branches_missing_siblings)
                        .sum::<usize>()
            }
            Value::Array(items) => items.iter().map(closed_branches_missing_siblings).sum(),
            _ => 0,
        }
    }

    #[test]
    fn no_closed_branch_of_either_schema_lacks_its_struct_fields() {
        let registry = SchemaRegistry::build();
        for key in ["workflow/manifest/v1", "agent/manifest/v1"] {
            let schema = registry.get(key).expect("schema registered");
            assert_eq!(closed_branches_missing_siblings(schema), 0, "{key}");
        }
    }

    #[test]
    fn the_parser_accepts_what_the_schema_accepts() {
        let manifest = one_state_workflow(
            "      kind: Action\n      action: image_generate\n      args:\n        prompt: \"a monkey\"\n      transitions: []\n",
        );
        let parsed: Result<WorkflowManifest, _> = serde_yaml::from_str(&manifest);
        assert!(parsed.is_ok(), "parser refused: {:?}", parsed.err());
        let unknown = one_state_workflow(
            "      kind: Action\n      action: image_generate\n      colour: blue\n      transitions: []\n",
        );
        let parsed: Result<WorkflowManifest, _> = serde_yaml::from_str(&unknown);
        assert!(parsed.is_err(), "parser accepted an unknown field");
    }
}
