// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SMCP Infrastructure (BC-12, ADR-035)
//!
//! Infrastructure implementations for the Secure Model Context Protocol.
//! This module provides the concrete cryptographic machinery that backs the
//! domain-level `SmcpSession` aggregate.
//!
//! ## Module Map
//!
//! | Module | Description |
//! |---|---|
//! | [`attestation`] | `AttestationService` trait + request/response types for the initial handshake |
//! | [`envelope`] | `SmcpEnvelope` (outer signed wrapper) and `ContextClaims` (JWT payload) |
//! | [`middleware`] | `SmcpMiddleware` — session-level verify-and-unwrap per tool call |
//! | [`policy_engine`] | `PolicyEngine` — thin shim delegating to `SecurityContext::evaluate` |
//! | [`signature`] | Ed25519 keypair generation and signing utilities |
//! | [`audit`] | Emits `SmcpEvent` audit records to the event bus |
//! | [`session_repository`] | In-memory `SmcpSessionRepository` implementation |
//!
//! ## Security Model
//!
//! ```text
//! Agent container
//!   └─ AttestationRequest { public_key, container_id }
//!         └─ AttestationService::attest() → SecurityToken (JWT)
//! Agent container (per tool call)
//!   └─ SmcpEnvelope { security_token, Ed25519(signature), inner_mcp }
//!         └─ SmcpMiddleware::verify_and_unwrap()
//!               └─ SmcpSession::evaluate_call()
//!                     ├─ check session status
//!                     ├─ verify Ed25519 signature (envelope.rs)
//!                     └─ PolicyEngine::evaluate() → SecurityContext::evaluate()
//! ```
//!
//! See ADR-035 for the full threat model and protocol specification.
pub mod attestation;
pub mod envelope;
pub mod middleware;
pub mod policy_engine;
pub mod signature;
pub mod audit;
pub mod session_repository;

pub use attestation::{AttestationRequest, AttestationResponse, AttestationService};
pub use envelope::{SmcpEnvelope, ContextClaims, AudienceClaim};
pub use middleware::SmcpMiddleware;
pub use policy_engine::PolicyEngine;
