use crate::domain::{Pattern, Skill, PatternId, SkillId, ErrorSignature};
use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct CortexMetrics {
    pub total_patterns: u64,
    pub total_skills: u64,
    pub average_success_rate: f64,
}

#[async_trait]
pub trait CortexService: Send + Sync {
    async fn store_pattern(&self, pattern: Pattern) -> Result<PatternId>;
    async fn search_patterns(&self, error: &ErrorSignature) -> Result<Vec<Pattern>>;
    async fn get_skill(&self, skill_id: SkillId) -> Result<Skill>;
    async fn get_metrics(&self) -> Result<CortexMetrics>;
}
