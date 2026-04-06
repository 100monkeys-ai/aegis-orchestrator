// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Docker infrastructure adapters
//!
//! Provides `BollardContainerVerifier` for container liveness checks
//! used during SEAL attestation (ADR-035 §4.1).

mod container_verifier;
pub use container_verifier::BollardContainerVerifier;
