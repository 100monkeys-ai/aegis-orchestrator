// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # StandardRuntime Registry
//!
//! Canonical, vetted mapping of language+version → Docker image tags per ADR-043 and ADR-045.
//!
//! ## Design
//!
//! The registry enforces deterministic, production-grade Docker image resolution:
//! - Each language+version pair maps to exactly one image tag (no "latest")
//! - All images use production variants (-slim for Debian, -alpine for Alpine)
//! - The registry is committed to the repository and loaded at daemon startup
//! - Invalid language/version combinations are rejected at execution validation time
//!
//! CustomRuntime (`spec.runtime.image`) bypasses the registry entirely and accepts
//! user-supplied image references at user risk.
//!
//! ## Model images
//!
//! `spec.models` maps a model class and name to a model image (ADR-143 M4): a
//! workflow's `ContainerRun` names one as `image: "model:<class>/<name>"` and
//! the registry resolves it. Every model image is named by a tag and an
//! `@sha256:` digest; an entry without one is refused when the registry loads,
//! because pulling by digest is what verifies the weights that run.
//!
//! See Also: ADR-043 (AEGIS Agent Runtimes), ADR-045 (Container Registry & Image Management),
//! ADR-143 (non-language models)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

use crate::domain::cluster::MergedConfig;
use crate::domain::runtime::ContainerResources;

/// The prefix of a `ContainerRun` image that names a model image of the
/// registry rather than a container image (ADR-143 M3).
pub const MODEL_IMAGE_PREFIX: &str = "model:";

/// Errors returned by the StandardRuntime registry.
#[derive(Debug, Error, Clone)]
pub enum RegistryError {
    /// The registry file could not be found or read.
    #[error("Registry file not found: {0}")]
    FileNotFound(String),
    /// The registry file is malformed (YAML parsing error).
    #[error("Invalid registry format: {0}")]
    ParseError(String),
    /// The requested language is not supported by the registry.
    #[error("Unsupported language '{language}': {available}", available = .available.join(", "))]
    UnsupportedLanguage {
        language: String,
        available: Vec<String>,
    },
    /// The requested language+version combination is not supported.
    #[error("Unsupported {language} version '{version}': {available}", available = .available.join(", "))]
    UnsupportedVersion {
        language: String,
        version: String,
        available: Vec<String>,
    },
    /// The registry was accessed before being initialized.
    #[error("Registry not initialized")]
    NotInitialized,
    /// A model image is not named by a tag and an `@sha256:` digest (ADR-143 M4).
    #[error("model '{model}' must name its image by digest")]
    ModelImageNotPinned { model: String },
    /// A `model:` reference names no model of the registry (ADR-143 M6).
    #[error("unknown model '{model}'; the models are {available}", available = list_or_none(.available))]
    UnknownModel {
        model: String,
        available: Vec<String>,
    },
    /// A `model:` image is not of the form `model:<class>/<name>`.
    #[error("a model image is named model:<class>/<name>, not '{0}'")]
    InvalidModelReference(String),
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

/// A `model:<class>/<name>` reference, as a `ContainerRun` image names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelReference {
    pub class: String,
    pub name: String,
}

impl ModelReference {
    /// Read an image string. `None` when the image is not a `model:` reference;
    /// an error when it is one and is not `model:<class>/<name>`.
    pub fn from_image(image: &str) -> Option<Result<Self, RegistryError>> {
        let rest = image.strip_prefix(MODEL_IMAGE_PREFIX)?;
        let parsed = match rest.split_once('/') {
            Some((class, name)) if is_model_segment(class) && is_model_segment(name) => Ok(Self {
                class: class.to_string(),
                name: name.to_string(),
            }),
            _ => Err(RegistryError::InvalidModelReference(image.to_string())),
        };
        Some(parsed)
    }

    /// `<class>/<name>`, as the refusals name a model.
    pub fn key(&self) -> String {
        format!("{}/{}", self.class, self.name)
    }
}

fn is_model_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '.'
        })
}

/// True when `image` is a full reference with a tag and an `@sha256:` digest
/// of 64 hexadecimal characters.
fn is_pinned_by_digest(image: &str) -> bool {
    let Some((name_and_tag, digest)) = image.split_once("@sha256:") else {
        return false;
    };
    let last_segment = name_and_tag.rsplit('/').next().unwrap_or_default();
    let has_tag = matches!(last_segment.split_once(':'), Some((repo, tag)) if !repo.is_empty() && !tag.is_empty());
    has_tag && digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit())
}

/// A model image of the registry (ADR-143 M4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// The image, by tag and digest
    /// (`ghcr.io/org/image:tag@sha256:<64 hex>`).
    pub image: String,
    /// Human-readable description of the model.
    #[serde(default)]
    pub description: String,
    /// The command a model step runs when its state sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// The CPU, memory and timeout a model step gets when its state sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ContainerResources>,
}

/// Runtime metadata from the registry (optional bootstrapping info, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeMetadata {
    /// Additional environment variables to inject at spawn time.
    /// Used for cases like TypeScript requiring global npm installs.
    #[serde(default)]
    pub bootstrap_env: HashMap<String, String>,
    /// Human-readable description of this runtime.
    #[serde(default)]
    pub description: String,
    /// Whether this runtime is deprecated and should not be used for new agents.
    /// Defaults to `false` when absent from the registry YAML.
    #[serde(default)]
    pub deprecated: bool,
}

/// A single language+version mapping in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEntry {
    /// Fully-qualified Docker image reference (e.g., "python:3.11-slim").
    pub image: String,
    /// Optional bootstrap and metadata.
    #[serde(flatten)]
    pub metadata: RuntimeMetadata,
}

/// Root structure for the StandardRuntime registry YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryManifest {
    /// Kubernetes-style API version.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Kubernetes-style kind.
    pub kind: String,
    /// Metadata section.
    pub metadata: serde_yaml::Mapping,
    /// Spec section containing the registry data.
    pub spec: RegistrySpec,
}

/// Spec section of the registry manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrySpec {
    /// Global registry URL (typically "docker.io" for Phase 1).
    /// Phase 2+ may support custom registry proxy.
    pub registry_url: String,
    /// Language → version → RuntimeEntry mappings.
    pub runtimes: HashMap<String, HashMap<String, RuntimeEntry>>,
    /// Model class → name → ModelEntry mappings (ADR-143 M4).
    #[serde(default)]
    pub models: HashMap<String, HashMap<String, ModelEntry>>,
    /// Optional metadata (version constraints, supported isolation modes, etc.).
    #[serde(default)]
    pub metadata: serde_yaml::Mapping,
}

/// The StandardRuntime registry, loaded at daemon startup.
///
/// This is a singleton that is shared across all concurrent execution requests.
/// Lookups are O(1) (HashMap access).
#[derive(Debug, Clone)]
pub struct StandardRuntimeRegistry {
    spec: RegistrySpec,
}

impl StandardRuntimeRegistry {
    /// Load the registry from a YAML file.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::FileNotFound`] if the file does not exist,
    /// or [`RegistryError::ParseError`] if the YAML is malformed.
    ///
    /// # Blocking I/O
    ///
    /// This function performs **synchronous** file I/O via [`std::fs::read_to_string`].
    /// It is intentionally synchronous because it is only called once at daemon startup,
    /// before the Tokio runtime processes any requests. Do **not** call this function
    /// from within an `async` context — use `tokio::task::spawn_blocking` if needed.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .map_err(|e| RegistryError::FileNotFound(format!("{}:{}", path.display(), e)))?;

        let manifest: RegistryManifest = serde_yaml::from_str(&content)
            .map_err(|e| RegistryError::ParseError(format!("YAML parse error: {e}")))?;

        Self::from_spec(manifest.spec)
    }

    /// Load the registry from a YAML string (useful for testing).
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::ParseError`] if the YAML is malformed.
    pub fn from_yaml_str(content: &str) -> Result<Self, RegistryError> {
        let manifest: RegistryManifest = serde_yaml::from_str(content)
            .map_err(|e| RegistryError::ParseError(format!("YAML parse error: {e}")))?;

        Self::from_spec(manifest.spec)
    }

    /// Load the registry from a merged database configuration (ADR-060).
    ///
    /// Looks for a `runtime-registry` or `runtime` key in the merged payload
    /// and deserialises the value as a [`RegistryManifest`].
    pub fn from_merged_config(merged: &MergedConfig) -> Result<Self, RegistryError> {
        let runtime_value = merged
            .payload
            .get("runtime-registry")
            .or_else(|| merged.payload.get("runtime"))
            .ok_or_else(|| {
                RegistryError::ParseError(
                    "No runtime-registry section in merged config".to_string(),
                )
            })?;
        let manifest: RegistryManifest = serde_json::from_value(runtime_value.clone())
            .map_err(|e| RegistryError::ParseError(format!("JSON parse error: {e}")))?;
        Self::from_spec(manifest.spec)
    }

    /// Every load ends here: a model image not named by digest is refused
    /// (ADR-143 M4), in sorted order so the refusal names the same model on
    /// every load.
    fn from_spec(spec: RegistrySpec) -> Result<Self, RegistryError> {
        let mut models: Vec<(String, &ModelEntry)> = spec
            .models
            .iter()
            .flat_map(|(class, names)| {
                names
                    .iter()
                    .map(move |(name, entry)| (format!("{class}/{name}"), entry))
            })
            .collect();
        models.sort_by(|a, b| a.0.cmp(&b.0));
        for (model, entry) in models {
            if !is_pinned_by_digest(&entry.image) {
                return Err(RegistryError::ModelImageNotPinned { model });
            }
        }
        Ok(Self { spec })
    }

    /// Resolve a `model:<class>/<name>` reference to its model entry (ADR-143 M3).
    ///
    /// # Errors
    ///
    /// [`RegistryError::InvalidModelReference`] for an image that is not of
    /// that form, [`RegistryError::UnknownModel`] for a model the registry
    /// does not hold.
    pub fn resolve_model(&self, reference: &str) -> Result<ModelEntry, RegistryError> {
        let parsed = ModelReference::from_image(reference)
            .unwrap_or_else(|| Err(RegistryError::InvalidModelReference(reference.to_string())))?;
        self.spec
            .models
            .get(&parsed.class)
            .and_then(|names| names.get(&parsed.name))
            .cloned()
            .ok_or_else(|| RegistryError::UnknownModel {
                model: parsed.key(),
                available: self.model_names(),
            })
    }

    /// Every model of the registry as `<class>/<name>`, sorted.
    pub fn model_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .spec
            .models
            .iter()
            .flat_map(|(class, names)| names.keys().map(move |name| format!("{class}/{name}")))
            .collect();
        names.sort();
        names
    }

    /// Resolve a language+version to a fully-qualified Docker image.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::UnsupportedLanguage`] if the language is not in the registry,
    /// or [`RegistryError::UnsupportedVersion`] if the version is not supported for that language.
    pub fn resolve(&self, language: &str, version: &str) -> Result<String, RegistryError> {
        let lang_map =
            self.spec
                .runtimes
                .get(language)
                .ok_or_else(|| RegistryError::UnsupportedLanguage {
                    language: language.to_string(),
                    available: self.spec.runtimes.keys().cloned().collect(),
                })?;

        let entry = lang_map
            .get(version)
            .ok_or_else(|| RegistryError::UnsupportedVersion {
                language: language.to_string(),
                version: version.to_string(),
                available: lang_map.keys().cloned().collect(),
            })?;

        Ok(entry.image.clone())
    }

    /// Resolve a language+version and return the full RuntimeEntry (with metadata).
    ///
    /// # Errors
    ///
    /// Same as [`Self::resolve`].
    pub fn resolve_entry(
        &self,
        language: &str,
        version: &str,
    ) -> Result<RuntimeEntry, RegistryError> {
        let lang_map =
            self.spec
                .runtimes
                .get(language)
                .ok_or_else(|| RegistryError::UnsupportedLanguage {
                    language: language.to_string(),
                    available: self.spec.runtimes.keys().cloned().collect(),
                })?;

        lang_map
            .get(version)
            .cloned()
            .ok_or_else(|| RegistryError::UnsupportedVersion {
                language: language.to_string(),
                version: version.to_string(),
                available: lang_map.keys().cloned().collect(),
            })
    }

    /// Get the global registry URL (typically "docker.io" for Phase 1).
    pub fn registry_url(&self) -> &str {
        &self.spec.registry_url
    }

    /// List all supported languages.
    pub fn supported_languages(&self) -> Vec<String> {
        let mut langs: Vec<_> = self.spec.runtimes.keys().cloned().collect();
        langs.sort();
        langs
    }

    /// List all supported versions for a given language.
    ///
    /// Returns an empty Vec if the language is not supported.
    pub fn supported_versions(&self, language: &str) -> Vec<String> {
        let mut versions: Vec<_> = self
            .spec
            .runtimes
            .get(language)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        // Sort by each numeric segment so "3.9" < "3.10" rather than lexicographically.
        versions.sort_by(|a, b| {
            let a_parts: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
            let b_parts: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
            a_parts.cmp(&b_parts)
        });
        versions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_python() {
        let registry_yaml = r#"
apiVersion: aegis.ai/v1
kind: RuntimeRegistry
metadata:
  name: test-registry
spec:
  registry_url: docker.io
  runtimes:
    python:
      "3.11":
        image: "python:3.11-slim"
        description: "Python 3.11 with slim Debian base"
"#;

        let registry = StandardRuntimeRegistry::from_yaml_str(registry_yaml).unwrap();
        assert_eq!(
            registry.resolve("python", "3.11").unwrap(),
            "python:3.11-slim"
        );
    }

    #[test]
    fn test_unsupported_language() {
        let registry_yaml = r#"
apiVersion: aegis.ai/v1
kind: RuntimeRegistry
metadata:
  name: test-registry
spec:
  registry_url: docker.io
  runtimes:
    python:
      "3.11":
        image: "python:3.11-slim"
        description: "Python 3.11 with slim Debian base"
"#;

        let registry = StandardRuntimeRegistry::from_yaml_str(registry_yaml).unwrap();
        let result = registry.resolve("ruby", "3.0");
        match result {
            Err(RegistryError::UnsupportedLanguage { language, .. }) => {
                assert_eq!(language, "ruby");
            }
            other => panic!("Expected UnsupportedLanguage error, got: {other:?}"),
        }
    }

    #[test]
    fn test_unsupported_version() {
        let registry_yaml = r#"
apiVersion: aegis.ai/v1
kind: RuntimeRegistry
metadata:
  name: test-registry
spec:
  registry_url: docker.io
  runtimes:
    python:
      "3.11":
        image: "python:3.11-slim"
        description: "Python 3.11"
"#;

        let registry = StandardRuntimeRegistry::from_yaml_str(registry_yaml).unwrap();
        let result = registry.resolve("python", "3.9");
        match result {
            Err(RegistryError::UnsupportedVersion {
                language, version, ..
            }) => {
                assert_eq!(language, "python");
                assert_eq!(version, "3.9");
            }
            other => panic!("Expected UnsupportedVersion error, got: {other:?}"),
        }
    }

    #[test]
    fn test_supported_languages() {
        let registry_yaml = r#"
apiVersion: aegis.ai/v1
kind: RuntimeRegistry
metadata:
  name: test-registry
spec:
  registry_url: docker.io
  runtimes:
    python:
      "3.11":
        image: "python:3.11-slim"
    javascript:
      "20":
        image: "node:20-alpine"
"#;

        let registry = StandardRuntimeRegistry::from_yaml_str(registry_yaml).unwrap();
        let langs = registry.supported_languages();
        assert_eq!(langs.len(), 2);
        assert!(langs.contains(&"python".to_string()));
        assert!(langs.contains(&"javascript".to_string()));
    }

    #[test]
    fn test_supported_versions() {
        let registry_yaml = r#"
apiVersion: aegis.ai/v1
kind: RuntimeRegistry
metadata:
  name: test-registry
spec:
  registry_url: docker.io
  runtimes:
    python:
      "3.9":
        image: "python:3.9-slim"
      "3.10":
        image: "python:3.10-slim"
      "3.11":
        image: "python:3.11-slim"
"#;

        let registry = StandardRuntimeRegistry::from_yaml_str(registry_yaml).unwrap();
        let versions = registry.supported_versions("python");
        assert_eq!(versions, vec!["3.9", "3.10", "3.11"]);
    }

    #[test]
    fn test_resolve_entry_with_metadata() {
        let registry_yaml = r#"
apiVersion: aegis.ai/v1
kind: RuntimeRegistry
metadata:
  name: test-registry
spec:
  registry_url: docker.io
  runtimes:
    typescript:
      "5.1":
        image: "node:20-alpine"
        description: "TypeScript 5.1 via Node.js 20"
        bootstrap_env:
          TYPESCRIPT_VERSION: "5.1"
"#;

        let registry = StandardRuntimeRegistry::from_yaml_str(registry_yaml).unwrap();
        let entry = registry.resolve_entry("typescript", "5.1").unwrap();
        assert_eq!(entry.image, "node:20-alpine");
        assert_eq!(entry.metadata.description, "TypeScript 5.1 via Node.js 20");
        assert_eq!(
            entry.metadata.bootstrap_env.get("TYPESCRIPT_VERSION"),
            Some(&"5.1".to_string())
        );
    }

    #[test]
    fn test_from_merged_config_runtime_registry_key() {
        let merged = MergedConfig {
            payload: serde_json::json!({
                "runtime-registry": {
                    "apiVersion": "aegis.ai/v1",
                    "kind": "RuntimeRegistry",
                    "metadata": { "name": "merged-registry" },
                    "spec": {
                        "registry_url": "docker.io",
                        "runtimes": {
                            "python": {
                                "3.12": {
                                    "image": "python:3.12-slim",
                                    "description": "Python 3.12"
                                }
                            }
                        }
                    }
                }
            }),
            version: "v1".to_string(),
        };

        let registry = StandardRuntimeRegistry::from_merged_config(&merged).unwrap();
        assert_eq!(
            registry.resolve("python", "3.12").unwrap(),
            "python:3.12-slim"
        );
    }

    #[test]
    fn test_from_merged_config_runtime_fallback_key() {
        let merged = MergedConfig {
            payload: serde_json::json!({
                "runtime": {
                    "apiVersion": "aegis.ai/v1",
                    "kind": "RuntimeRegistry",
                    "metadata": { "name": "fallback-registry" },
                    "spec": {
                        "registry_url": "ghcr.io",
                        "runtimes": {
                            "javascript": {
                                "20": {
                                    "image": "node:20-alpine",
                                    "description": "Node.js 20"
                                }
                            }
                        }
                    }
                }
            }),
            version: "v1".to_string(),
        };

        let registry = StandardRuntimeRegistry::from_merged_config(&merged).unwrap();
        assert_eq!(
            registry.resolve("javascript", "20").unwrap(),
            "node:20-alpine"
        );
        assert_eq!(registry.registry_url(), "ghcr.io");
    }

    #[test]
    fn test_from_merged_config_missing_section() {
        let merged = MergedConfig {
            payload: serde_json::json!({
                "other": "data"
            }),
            version: "v1".to_string(),
        };

        let result = StandardRuntimeRegistry::from_merged_config(&merged);
        assert!(result.is_err());
        match result.unwrap_err() {
            RegistryError::ParseError(msg) => {
                assert!(msg.contains("No runtime-registry section"));
            }
            other => panic!("Expected ParseError, got: {other:?}"),
        }
    }
}
