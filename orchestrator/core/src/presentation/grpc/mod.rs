// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # gRPC Presentation Layer (ADR-026)
//!
//! Tonic-based gRPC service implementations.
//!
//! | Module | Service | Notes |
//! |--------|---------|-------|
//! | [`server`] | `OrchestratorService` | Agent/execution/workflow management + event streaming |
//! | [`auth_interceptor`] | `KeycloakAuthInterceptor` | gRPC JWT validation interceptor (ADR-041) |
//!
//! The Control Plane UI (`aegis-control-plane`) and Zaru product
//! (`aegis-zaru-deployment`) connect to this service for real-time
//! execution event streaming (ADR-026 gRPC server-stream).

pub mod auth_interceptor;
pub mod server;
