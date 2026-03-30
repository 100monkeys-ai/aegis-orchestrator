// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Discovery Infrastructure (ADR-075)
//!
//! Qdrant-backed implementation of the `DiscoveryIndex` port for semantic
//! search over agents and workflows.

pub mod event_handler;
pub mod qdrant_discovery_index;

pub use event_handler::DiscoveryIndexEventHandler;
pub use qdrant_discovery_index::QdrantDiscoveryIndex;
