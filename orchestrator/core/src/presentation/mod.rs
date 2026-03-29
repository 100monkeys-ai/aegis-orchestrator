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
//! | [`grpc`] | gRPC (Tonic) | Orchestrator gRPC service, streamed by the Zaru client |
//! | [`webhook_guard`] | HTTP | HMAC-SHA256 extractor for webhook requests (ADR-021) |
//! | [`stimulus_handlers`] | HTTP | `POST /v1/stimuli` and `POST /v1/webhooks/{source}` handlers (ADR-021) |
//! | [`keycloak_auth`] | HTTP | IAM/OIDC JWT auth middleware for HTTP endpoints (ADR-041) |
//! | [`tenant_middleware`] | HTTP | TenantContext extraction middleware (ADR-056) |
//! | [`metrics_middleware`] | HTTP | Prometheus metrics tracking (ADR-058) |

pub mod api;
pub mod grpc;
pub mod keycloak_auth;
pub mod metrics_middleware;
pub mod stimulus_handlers;
pub mod tenant_middleware;
pub mod webhook_guard;
