// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # `aegis-cortex` — Learning & Memory Crate (BC-5, ADR-018/024/029)
//!
//! The Cortex is AEGIS's system-wide learning layer. It indexes failed iterations
//! that required refinement, stores error→solution `Pattern` pairs, and surfaces
//! them semantically to the orchestrator during future executions — allowing the
//! system to improve over time without manual prompt engineering.
//!
//! ## Crate Layout
//!
//! | Module | Layer | Contents |
//! |--------|-------|----------|
//! | [`domain`] | Domain | `Pattern`, `Skill`, `CortexGraph` aggregates; domain events |
//! | [`application`] | Application | `CortexService` use-case trait |
//! | [`infrastructure`] | Infra | Vector store adapters, embedding clients |
//!
//! ## Key Concepts
//!
//! - **Pattern**: An error→solution pair. Discovered when an `Execution` iteration
//!   fails and a `RefinementApplied` event is emitted. Stored with a normalised
//!   `ErrorSignature` (type + hash) for deduplication.
//! - **Skill**: A higher-level capability composed of one or more `Pattern`s.
//! - **Weight**: Deduplication counter — incremented when a semantically identical
//!   pattern is stored again (ADR-018 Weighted Cortex Memory).
//! - **SuccessRate**: Float (0.0–1.0) updated by `PatternApplied` events.
//!
//! ## Phase Notes
//!
//! ⚠️ Phase 1 — In-memory storage only. Pattern data is not persisted across
//! orchestrator restarts. Phase 2 will add PostgreSQL persistence + pgvector
//! semantic search (ADR-025).
//!
//! See ADR-018 (Weighted Cortex Memory), ADR-024 (Holographic Cortex Memory),
//! ADR-028 (Embedding Model), ADR-029 (Time-Decay), AGENTS.md §BC-5.

pub mod domain;
pub mod application;
pub mod infrastructure;

pub use domain::*;
pub use infrastructure::*;
