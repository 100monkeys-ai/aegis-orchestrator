// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # `aegis-swarm` — Multi-Agent Coordination Crate (BC-6, AGENTS.md §BC-6)
//!
//! Manages parent–child agent hierarchies, inter-agent messaging, and atomic
//! resource locks within a coordinated group of agents (a **Swarm**).
//!
//! ## Crate Layout
//!
//! | Module | Layer | Contents |
//! |--------|-------|----------|
//! | [`domain`] | Domain | `Swarm`, `SwarmId`, `ResourceLock` aggregates |
//! | [`application`] | Application | `SwarmService` use-case trait |
//!
//! ## Key Concepts
//!
//! - **Swarm**: A directed graph of `AgentId`s with optional parent–child relationships.
//!   Created when an agent spawns child agents via `spec.spawn_agents` in its manifest.
//! - **ResourceLock**: An optimistic mutex token preventing concurrent writes to a
//!   shared resource across agents in the same swarm.
//! - **Cascade Cancellation**: When a parent execution is cancelled, the orchestrator
//!   propagates the cancellation to all child executions in the swarm.
//!
//! ## Phase Notes
//!
//! ⚠️ Phase 1 — Swarms are tracked in memory only. Persistent swarm state and
//! cross-node swarm federation are deferred to Phase 3
//! (see AGENTS.md §Swarm Coordination Context).
//!
//! See AGENTS.md §Swarm, AGENTS.md §BC-6.

pub mod domain;
pub mod application;

pub use domain::*;
