// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use async_trait::async_trait;
use crate::domain::pattern::CortexPattern;
use anyhow::Result;

pub mod cortex_service;
pub mod cortex_pruner;

pub use cortex_service::{CortexService, StandardCortexService, EventBus};
pub use cortex_pruner::{CortexPruner, CortexPrunerConfig};

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn search(&self, query_vector: &[f32], limit: usize) -> Result<Vec<CortexPattern>>;
    async fn add(&self, pattern: CortexPattern, vector: &[f32]) -> Result<()>;
}
