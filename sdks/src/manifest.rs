use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// An AEGIS agent manifest (agent.yaml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub version: String,
    pub agent: AgentSpec,
    pub permissions: Permissions,
    pub tools: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub runtime: String,
    #[serde(default)]
    pub memory: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permissions {
    pub network: NetworkPermissions,
    pub fs: FilesystemPermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPermissions {
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemPermissions {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

impl AgentManifest {
    /// Load a manifest from a YAML file.
    pub fn from_yaml_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let manifest = serde_yaml::from_str(&content)?;
        Ok(manifest)
    }

    /// Save a manifest to a YAML file.
    pub fn to_yaml_file(&self, path: impl AsRef<std::path::Path>) -> anyhow::Result<()> {
        let yaml = serde_yaml::to_string(self)?;
        std::fs::write(path, yaml)?;
        Ok(())
    }
}
