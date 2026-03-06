// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # IAM Infrastructure Layer (BC-13, ADR-041)
//!
//! Implements [`crate::domain::iam::IdentityProvider`] using JWKS endpoint
//! fetching, JWT signature verification, and claim extraction.

pub mod keycloak_iam_service;

pub use keycloak_iam_service::StandardIamService;
