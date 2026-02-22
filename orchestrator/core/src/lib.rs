// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # aegis-core
//!
//! The central orchestration crate for AEGIS. This crate is the runtime heart of the
//! system — it owns the domain model, application use-cases, infrastructure wiring,
//! and gRPC/HTTP presentation surfaces for all major bounded contexts.
//!
//! ## Bounded Contexts Implemented
//!
//! | Bounded Context | Domain files | Key ADR |
//! |---|---|---|
//! | **BC-1 Agent Lifecycle** | [`domain::agent`] | – |
//! | **BC-2 Execution** | [`domain::execution`], [`domain::supervisor`] | ADR-005 |
//! | **BC-3 Workflow Orchestration** | [`domain::workflow`] | ADR-015 |
//! | **BC-4 Security Policy** | [`domain::policy`], [`domain::security_context`] | ADR-035 |
//! | **BC-7 Storage Gateway** | [`domain::volume`], [`domain::fsal`] | ADR-032, ADR-036 |
//! | **BC-11 Secrets & Identity** | [`infrastructure::secrets_manager`] | ADR-034 (Phase 4) |
//! | **BC-12 SMCP Protocol** | [`domain::smcp_session`], [`infrastructure::smcp`] | ADR-035 |
//!
//! ## Layer Structure
//!
//! ```text
//! presentation/   ← gRPC server (tonic), HTTP API (axum)
//!     ↓
//! application/    ← Use-cases, service traits, orchestration
//!     ↓
//! domain/         ← Aggregates, value objects, domain events, repository traits
//!     ↓
//! infrastructure/ ← Postgres repos, Docker runtime, NFS server, SMCP, LLM adapters
//! ```
//!
//! ## Integration Tests
//!
//! See `orchestrator/core/tests/` for integration tests covering the NFS gateway,
//! validation events, and Temporal workflow mapping.

pub mod domain;
pub mod application;
pub mod infrastructure;
pub mod presentation;

pub use domain::*;
