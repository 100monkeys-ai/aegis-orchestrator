// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Presentation Layer (`aegis-core`)
//!
//! HTTP and gRPC surface that translates external requests into application
//! service calls. **No business logic lives here** — all real work is
//! delegated to application services in `crate::application`.
//!
//! | Module | Transport | Description |
//! |--------|-----------|-------------|
//! | [`api`] | HTTP/SSE (Axum) | REST endpoints + Server-Sent Events for execution streaming |
//! | [`grpc`] | gRPC (Tonic) | Orchestrator gRPC service, streamed by the Control Plane UI |

pub mod api;
pub mod grpc;
