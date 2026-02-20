// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! AgentSkills Fetcher Service
//!
//! This module provides infrastructure for fetching pre-built instruction packages
//! from agentskills.io (or compatible registries).
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Download and cache skill definitions
//! - **Integration:** External HTTP API → Local cache → Agent runtime
//!
//! # Usage
//!
//! ```ignore
//! use agent_skills_client::AgentSkillsClient;
//!
//! let client = AgentSkillsClient::new("https://agentskills.io/api");
//! let skills = client.fetch_skills(vec!["email:imap-reader", "email:summarization"]).await?;
//! ```

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::fs;
use reqwest::Client;

// ============================================================================
// Domain Models
// ============================================================================

/// A fetched skill definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    /// Skill identifier (e.g., "email:imap-reader")
    pub id: String,
    
    /// Skill name
    pub name: String,
    
    /// Instruction content (SKILL.md format)
    pub content: String,
    
    /// Version
    pub version: String,
    
    /// Optional metadata
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

// ============================================================================
// Client Implementation
// ============================================================================

pub struct AgentSkillsClient {
    /// Base URL for skills registry
    base_url: String,
    
    /// HTTP client
    client: Client,
    
    /// Local cache directory
    cache_dir: PathBuf,
}

impl AgentSkillsClient {
    /// Create a new AgentSkills client
    pub fn new(base_url: impl Into<String>) -> Self {
        let cache_dir = Self::default_cache_dir();
        Self::new_with_cache(base_url, cache_dir)
    }
    
    /// Create client with custom cache directory
    pub fn new_with_cache(base_url: impl Into<String>, cache_dir: PathBuf) -> Self {
        Self {
            base_url: base_url.into(),
            client: Client::new(),
            cache_dir,
        }
    }
    
    /// Get default cache directory (~/.aegis/skills/)
    fn default_cache_dir() -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(".aegis").join("skills")
    }
    
    /// Fetch multiple skills by their IDs
    ///
    /// # Example
    /// ```ignore
    /// let skills = client.fetch_skills(vec![
    ///     "email:imap-reader",
    ///     "email:summarization",
    ///     "filesystem:write"
    /// ]).await?;
    /// ```
    pub async fn fetch_skills(&self, skill_ids: Vec<String>) -> Result<Vec<SkillDefinition>> {
        let mut skills = Vec::new();
        
        for skill_id in skill_ids {
            let skill = self.fetch_skill(&skill_id).await
                .with_context(|| format!("Failed to fetch skill: {}", skill_id))?;
            skills.push(skill);
        }
        
        Ok(skills)
    }
    
    /// Fetch a single skill by ID
    pub async fn fetch_skill(&self, skill_id: &str) -> Result<SkillDefinition> {
        // 1. Check cache first
        if let Some(cached) = self.load_from_cache(skill_id)? {
            return Ok(cached);
        }
        
        // 2. Fetch from remote
        let skill = self.fetch_from_remote(skill_id).await?;
        
        // 3. Save to cache
        self.save_to_cache(&skill)?;
        
        Ok(skill)
    }
    
    /// Fetch skill from remote registry
    async fn fetch_from_remote(&self, skill_id: &str) -> Result<SkillDefinition> {
        // Parse skill ID (format: "namespace:skill-name")
        let parts: Vec<&str> = skill_id.split(':').collect();
        if parts.len() != 2 {
            return Err(anyhow!("Invalid skill ID format: {}. Expected 'namespace:skill-name'", skill_id));
        }
        let (namespace, name) = (parts[0], parts[1]);
        
        // Build URL: https://agentskills.io/api/skills/{namespace}/{name}
        let url = format!("{}/skills/{}/{}", self.base_url, namespace, name);
        
        let response = self.client
            .get(&url)
            .header("User-Agent", "AEGIS/1.0")
            .send()
            .await
            .context("HTTP request failed")?;
        
        if !response.status().is_success() {
            return Err(anyhow!("Skill not found: {} (HTTP {})", skill_id, response.status()));
        }
        
        let skill: SkillDefinition = response
            .json()
            .await
            .context("Failed to parse skill JSON")?;
        
        Ok(skill)
    }
    
    /// Load skill from local cache
    fn load_from_cache(&self, skill_id: &str) -> Result<Option<SkillDefinition>> {
        let cache_path = self.cache_path(skill_id);
        
        if !cache_path.exists() {
            return Ok(None);
        }
        
        let content = fs::read_to_string(&cache_path)
            .with_context(|| format!("Failed to read cached skill: {:?}", cache_path))?;
        
        let skill: SkillDefinition = serde_json::from_str(&content)
            .context("Failed to parse cached skill")?;
        
        Ok(Some(skill))
    }
    
    /// Save skill to local cache
    fn save_to_cache(&self, skill: &SkillDefinition) -> Result<()> {
        // Ensure cache directory exists
        fs::create_dir_all(&self.cache_dir)
            .context("Failed to create cache directory")?;
        
        let cache_path = self.cache_path(&skill.id);
        
        let content = serde_json::to_string_pretty(skill)
            .context("Failed to serialize skill")?;
        
        fs::write(&cache_path, content)
            .with_context(|| format!("Failed to write cache file: {:?}", cache_path))?;
        
        Ok(())
    }
    
    /// Get cache file path for a skill ID
    fn cache_path(&self, skill_id: &str) -> PathBuf {
        // Replace ':' with '_' for filesystem safety
        let safe_id = skill_id.replace(':', "_");
        self.cache_dir.join(format!("{}.json", safe_id))
    }
    
    /// Clear entire cache
    pub fn clear_cache(&self) -> Result<()> {
        if self.cache_dir.exists() {
            fs::remove_dir_all(&self.cache_dir)
                .context("Failed to clear cache")?;
        }
        Ok(())
    }
    
    /// Clear cache for specific skill
    pub fn clear_skill_cache(&self, skill_id: &str) -> Result<()> {
        let cache_path = self.cache_path(skill_id);
        if cache_path.exists() {
            fs::remove_file(&cache_path)
                .with_context(|| format!("Failed to remove cache file: {:?}", cache_path))?;
        }
        Ok(())
    }
}

// ============================================================================
// Builder Pattern
// ============================================================================

pub struct AgentSkillsClientBuilder {
    base_url: Option<String>,
    cache_dir: Option<PathBuf>,
}

impl AgentSkillsClientBuilder {
    pub fn new() -> Self {
        Self {
            base_url: None,
            cache_dir: None,
        }
    }
    
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }
    
    pub fn cache_dir(mut self, dir: PathBuf) -> Self {
        self.cache_dir = Some(dir);
        self
    }
    
    pub fn build(self) -> AgentSkillsClient {
        let base_url = self.base_url.unwrap_or_else(|| "https://agentskills.io/api".to_string());
        let cache_dir = self.cache_dir.unwrap_or_else(AgentSkillsClient::default_cache_dir);
        
        AgentSkillsClient::new_with_cache(base_url, cache_dir)
    }
}

impl Default for AgentSkillsClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Load skills and concatenate their content into a single string
///
/// This is useful for injecting skills into agent context before execution.
pub fn skills_to_context_string(skills: &[SkillDefinition]) -> String {
    skills.iter()
        .map(|skill| {
            format!(
                "# Skill: {} ({})\n\n{}\n\n---\n\n",
                skill.name,
                skill.id,
                skill.content
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    
    #[test]
    fn test_cache_path() {
        let client = AgentSkillsClient::new("https://example.com");
        let path = client.cache_path("email:imap-reader");
        
        assert!(path.to_str().unwrap().contains("email_imap-reader.json"));
    }
    
    #[test]
    fn test_cache_roundtrip() {
        let temp_dir = env::temp_dir().join("aegis_test_cache");
        let client = AgentSkillsClient::new_with_cache("https://example.com", temp_dir.clone());
        
        let skill = SkillDefinition {
            id: "test:skill".to_string(),
            name: "Test Skill".to_string(),
            content: "This is a test skill".to_string(),
            version: "1.0.0".to_string(),
            metadata: HashMap::new(),
        };
        
        // Save
        client.save_to_cache(&skill).unwrap();
        
        // Load
        let loaded = client.load_from_cache("test:skill").unwrap().unwrap();
        
        assert_eq!(loaded.id, skill.id);
        assert_eq!(loaded.content, skill.content);
        
        // Cleanup
        client.clear_cache().unwrap();
    }
    
    #[test]
    fn test_skills_to_context_string() {
        let skills = vec![
            SkillDefinition {
                id: "email:reader".to_string(),
                name: "Email Reader".to_string(),
                content: "Step 1: Connect\nStep 2: Read".to_string(),
                version: "1.0.0".to_string(),
                metadata: HashMap::new(),
            },
            SkillDefinition {
                id: "email:writer".to_string(),
                name: "Email Writer".to_string(),
                content: "Step 1: Compose\nStep 2: Send".to_string(),
                version: "1.0.0".to_string(),
                metadata: HashMap::new(),
            },
        ];
        
        let context = skills_to_context_string(&skills);
        
        assert!(context.contains("# Skill: Email Reader"));
        assert!(context.contains("Step 1: Connect"));
        assert!(context.contains("# Skill: Email Writer"));
        assert!(context.contains("Step 1: Compose"));
        assert!(context.contains("---"));
    }
}
