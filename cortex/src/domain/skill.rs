// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Skill
//!
//! Provides skill functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** Implements skill

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};


#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SkillId(pub Uuid);

impl SkillId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SkillId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub description: Option<String>,
    pub code: Option<String>, // Inline code if applicable
    pub input_schema: Option<serde_json::Value>,
    pub output_schema: Option<serde_json::Value>,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Skill {
    pub fn new(name: String, description: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            id: SkillId::new(),
            name,
            description,
            code: None,
            input_schema: None,
            output_schema: None,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_id_new_is_unique() {
        let a = SkillId::new();
        let b = SkillId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_skill_id_default() {
        let a = SkillId::default();
        let b = SkillId::default();
        assert_ne!(a, b);
    }

    #[test]
    fn test_skill_new() {
        let skill = Skill::new("my-skill".to_string(), Some("A test skill".to_string()));
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description.as_deref(), Some("A test skill"));
        assert!(skill.code.is_none());
        assert!(skill.input_schema.is_none());
        assert!(skill.output_schema.is_none());
        assert!(skill.tags.is_empty());
    }

    #[test]
    fn test_skill_new_without_description() {
        let skill = Skill::new("no-desc".to_string(), None);
        assert!(skill.description.is_none());
    }

    #[test]
    fn test_skill_with_code() {
        let mut skill = Skill::new("code-skill".to_string(), None);
        skill.code = Some("def run(): pass".to_string());
        assert_eq!(skill.code.as_deref(), Some("def run(): pass"));
    }

    #[test]
    fn test_skill_with_tags() {
        let mut skill = Skill::new("tagged-skill".to_string(), None);
        skill.tags = vec!["python".to_string(), "utility".to_string()];
        assert_eq!(skill.tags.len(), 2);
        assert!(skill.tags.contains(&"python".to_string()));
    }

    #[test]
    fn test_skill_with_schemas() {
        let mut skill = Skill::new("schema-skill".to_string(), None);
        skill.input_schema = Some(serde_json::json!({"type": "string"}));
        skill.output_schema = Some(serde_json::json!({"type": "integer"}));
        assert!(skill.input_schema.is_some());
        assert!(skill.output_schema.is_some());
    }

    #[test]
    fn test_skill_serialization() {
        let skill = Skill::new("serializable".to_string(), Some("desc".to_string()));
        let json = serde_json::to_string(&skill).unwrap();
        let deserialized: Skill = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, skill.name);
        assert_eq!(deserialized.id, skill.id);
    }

    #[test]
    fn test_skill_id_serialization() {
        let id = SkillId::new();
        let json = serde_json::to_string(&id).unwrap();
        let deserialized: SkillId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deserialized);
    }
}
