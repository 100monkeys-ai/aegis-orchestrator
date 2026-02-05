use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PatternId(pub Uuid);

impl PatternId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SkillId(pub Uuid);

impl SkillId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorSignature(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionApproach(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub id: PatternId,
    pub error_signature: ErrorSignature,
    pub solution: SolutionApproach,
    pub frequency: u64,
    pub success_rate: f64,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub category: String,
    pub patterns: Vec<PatternId>,
    pub usage_count: u64,
    pub success_rate: f64,
    pub first_learned: DateTime<Utc>,
    pub last_used: DateTime<Utc>,
}
