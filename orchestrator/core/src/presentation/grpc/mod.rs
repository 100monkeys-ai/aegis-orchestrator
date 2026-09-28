// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # gRPC Presentation Layer (ADR-026)
//!
//! Tonic-based gRPC service implementations.
//!
//! | Module | Service | Notes |
//! |--------|---------|-------|
//! | [`server`] | `OrchestratorService` | Agent/execution/workflow management + event streaming |
//! | [`auth_interceptor`] | `GrpcIamAuthInterceptor` | gRPC JWT validation interceptor (ADR-041) |
//! | [`health`] | `grpc.health.v1.Health` | Readiness reported by every gRPC server the orchestrator starts |
//! | [`rate_limit_interceptor`] | `GrpcRateLimiter` | Per-user rate limiting guard (ADR-072) |
//!
//! The Zaru client (`zaru-client`) connects to this service for
//! real-time execution event streaming (ADR-026 gRPC server-stream).

pub mod auth_interceptor;
pub mod health;
pub mod rate_limit_interceptor;
pub mod server;
