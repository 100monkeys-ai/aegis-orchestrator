// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Security Context Domain Module (BC-4/BC-12, ADR-035)
//!
//! Defines the **protocol-level** permission boundary used by the SMCP layer.
//! Three sub-modules form the aggregate:
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`capability`] | `Capability`, `RateLimit` value objects |
//! | [`security_context`] | `SecurityContext` aggregate root, `SecurityContextMetadata` |
//! | [`repository`] | `SecurityContextRepository` persistence trait |
//!
//! # Relation to `domain::policy`
//!
//! [`crate::domain::policy::SecurityPolicy`] controls **infrastructure-level** isolation
//! (container networking, filesystem mounts, OS resource limits).
//! `SecurityContext` controls **protocol-level** authorization (which MCP tools an agent
//! may invoke). Both are enforced by the orchestrator; agents see neither directly.
//!
//! See ADR-035 (SMCP Implementation), AGENTS.md §Bounded Contexts §4.

pub mod capability;
pub mod repository;
#[allow(clippy::module_inception)]
pub mod security_context;

pub use capability::{Capability, RateLimit};
pub use repository::SecurityContextRepository;
pub use security_context::{
    validate_context_ownership, PolicyViolation, SecurityContext, SecurityContextMetadata,
};

pub use crate::domain::rate_limit::{
    RateLimitBucket, RateLimitDecision, RateLimitEnforcer, RateLimitError, RateLimitPolicy,
    RateLimitPolicyResolver, RateLimitPolicySource, RateLimitResourceType, RateLimitScope,
    RateLimitWindow,
};
