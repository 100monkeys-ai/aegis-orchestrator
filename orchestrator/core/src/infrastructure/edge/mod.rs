// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Edge Mode Infrastructure (ADR-117)
//!
//! Postgres-backed repositories and gRPC handlers for the AEGIS Edge Daemon
//! aggregate. See `crate::domain::edge` for the trait contracts implemented
//! here.

pub mod connection_registry;
pub mod enrollment_token_repo;
pub mod group_repo;
pub mod postgres_repo;

pub use connection_registry::{
    EdgeCommandTx, EdgeConnectionGuard, EdgeConnectionRegistry, PendingEdgeCalls,
};
pub use enrollment_token_repo::PgEnrollmentTokenRepository;
pub use group_repo::PgEdgeGroupRepository;
pub use postgres_repo::PgEdgeDaemonRepository;
