// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL Infrastructure (BC-12, ADR-035)
//!
//! Infrastructure implementations for the Signed Envelope Attestation Layer.
//! This module provides the concrete cryptographic machinery that backs the
//! domain-level `SealSession` aggregate.
//!
//! ## Module Map
//!
//! | Module | Description |
//! |---|---|
//! | [`attestation`] | `AttestationService` trait + request/response types for the initial handshake |
//! | [`envelope`] | `SealEnvelope` (outer signed wrapper) and `ContextClaims` (JWT payload) |
//! | [`middleware`] | `SealMiddleware` — session-level verify-and-unwrap per tool call |
//! | [`policy_engine`] | `PolicyEngine` — thin shim delegating to `SecurityContext::evaluate` |
//! | [`signature`] | Ed25519 keypair generation and signing utilities |
//! | [`audit`] | Emits `SealEvent` audit records to the event bus |
//! | [`session_repository`] | In-memory `SealSessionRepository` implementation |
//!
//! ## Security Model
//!
//! ```text
//! Agent container
//!   └─ AttestationRequest { public_key, container_id }
//!         └─ AttestationService::attest() → SecurityToken (JWT)
//! Agent container (per tool call)
//!   └─ SealEnvelope { security_token, Ed25519(signature), payload }
//!         └─ SealMiddleware::verify_and_unwrap()
//!               └─ SealSession::evaluate_call()
//!                     ├─ check session status
//!                     ├─ verify Ed25519 signature (envelope.rs)
//!                     └─ PolicyEngine::evaluate() → SecurityContext::evaluate()
//! ```
//!
//! See ADR-035 for the full threat model and protocol specification.
pub mod attestation;
pub mod audit;
pub mod envelope;
pub mod gateway_client;
pub mod middleware;
pub mod nonce_store;
pub mod policy_engine;
pub mod session_repository;
pub mod signature;

pub use attestation::{AttestationRequest, AttestationResponse, AttestationService};
pub use envelope::{AudienceClaim, ContextClaims, SealEnvelope};
pub use middleware::SealMiddleware;
pub use nonce_store::{InMemoryNonceStore, NonceOutcome, NonceStore, SEAL_REPLAY_TTL};
pub use policy_engine::PolicyEngine;
