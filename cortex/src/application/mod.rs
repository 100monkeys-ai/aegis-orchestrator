// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Cortex Application Layer (BC-5, ADR-018/024)
//!
//! Use-case interfaces and supporting traits for the learning and memory subsystem.
//!
//! | Symbol | Purpose |
//! |--------|---------|
//! | [`VectorStore`] | Abstraction over vector similarity search backends |
//! | [`CortexService`] | Main use-case trait: store, search, reinforce patterns |
//! | [`CortexPruner`] | Scheduled garbage collection of low-weight patterns |
//!
//! See ADR-018 (Weighted Memory), ADR-024 (Holographic Memory).

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
