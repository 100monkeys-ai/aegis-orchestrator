use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub id: Uuid,
    pub description: String,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub frequency: u64,
    pub success_rate: f64,
    // Embedding vector would be handled by the store, but maybe we store dimension here?
    // For now, keep it simple.
}

impl Pattern {
    pub fn new(description: String, tags: Vec<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            description,
            tags,
            created_at: Utc::now(),
            frequency: 1,
            success_rate: 1.0, 
        }
    }
}
