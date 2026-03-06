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
//! See Also: ADR-043 (AEGIS Agent Runtimes), ADR-045 (Container Registry & Image Management)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

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
            .map_err(|e| RegistryError::ParseError(format!("YAML parse error: {}", e)))?;

        Ok(Self {
            spec: manifest.spec,
        })
    }

    /// Load the registry from a YAML string (useful for testing).
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::ParseError`] if the YAML is malformed.
    pub fn from_yaml_str(content: &str) -> Result<Self, RegistryError> {
        let manifest: RegistryManifest = serde_yaml::from_str(content)
            .map_err(|e| RegistryError::ParseError(format!("YAML parse error: {}", e)))?;

        Ok(Self {
            spec: manifest.spec,
        })
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
            other => panic!("Expected UnsupportedLanguage error, got: {:?}", other),
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
            other => panic!("Expected UnsupportedVersion error, got: {:?}", other),
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
}
