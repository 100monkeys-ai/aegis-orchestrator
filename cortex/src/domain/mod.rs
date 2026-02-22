// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Cortex Domain Layer (BC-5, ADR-018)
//!
//! Pure business rules for the learning and memory subsystem.
//! No I/O dependencies — infrastructure concerns are behind traits in
//! `crate::application`.
//!
//! | Module | Key Types |
//! |--------|-----------|
//! | [`pattern`] | `Pattern`, `PatternId`, `ErrorSignature`, `SolutionApproach` |
//! | [`skill`] | `Skill`, `SkillId` |
//! | [`events`] | `LearningEvent` (mirrors `aegis_core::domain::events::LearningEvent`) |
//! | [`graph`] | `CortexGraph` — weighted Pattern graph for holographic memory |
//!
//! See ADR-018, ADR-024, AGENTS.md §Cortex.

pub mod pattern;
pub mod skill;
pub mod events;
pub mod graph;

pub use pattern::*;
pub use skill::*;
pub use events::*;
pub use graph::*;
