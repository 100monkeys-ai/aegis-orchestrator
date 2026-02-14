// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Infrastructure layer for Cortex bounded context

pub mod repository;
pub mod lancedb_store;
// TODO: Re-enable once arrow-arith conflicts resolved (upstream issue)
// pub mod lancedb_repo;
pub mod graph_store;
pub mod embedding_client;

pub use repository::{PatternRepository, GraphRepository};
pub use lancedb_store::InMemoryPatternRepository;
// pub use lancedb_repo::LanceDBPatternRepository;
pub use graph_store::InMemoryGraphRepository;
pub use embedding_client::EmbeddingClient;
