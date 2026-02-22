// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Swarm Domain Layer (BC-6)
//!
//! Pure domain types for multi-agent coordination. No I/O dependencies.
//!
//! | Module | Key Types |
//! |--------|-----------|
//! | [`swarm`] | `Swarm`, `SwarmId`, `ResourceLock` |
//!
//! See AGENTS.md §BC-6 Swarm Coordination Context.

pub mod swarm;

pub use swarm::*;
