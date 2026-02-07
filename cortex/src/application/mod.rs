// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use async_trait::async_trait;
use crate::domain::pattern::Pattern;
use anyhow::Result;

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn search(&self, query_vector: &[f32], limit: usize) -> Result<Vec<Pattern>>;
    async fn add(&self, pattern: Pattern, vector: &[f32]) -> Result<()>;
}
