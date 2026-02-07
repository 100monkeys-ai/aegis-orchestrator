// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

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

impl Default for PatternId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub id: PatternId,
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
            id: PatternId::new(),
            description,
            tags,
            created_at: Utc::now(),
            frequency: 1,
            success_rate: 1.0, 
        }
    }
}
