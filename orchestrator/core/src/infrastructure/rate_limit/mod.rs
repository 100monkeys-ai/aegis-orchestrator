// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Rate Limit Infrastructure (ADR-072)
//!
//! Three-layer enforcement architecture:
//!
//! | Layer | Enforcer | Buckets | Storage |
//! |-------|----------|---------|---------|
//! | Burst | [`GovernorBurstEnforcer`] | `PerMinute` | In-memory token bucket |
//! | Window | [`PostgresWindowEnforcer`] | `Hourly..Monthly` | PostgreSQL sliding counters |
//! | Composite | [`CompositeRateLimitEnforcer`] | All | Delegates to both |

pub mod burst_enforcer;
pub mod composite_enforcer;
pub mod policy_resolver;
pub mod postgres_enforcer;

pub use burst_enforcer::GovernorBurstEnforcer;
pub use composite_enforcer::CompositeRateLimitEnforcer;
pub use policy_resolver::HierarchicalPolicyResolver;
pub use postgres_enforcer::PostgresWindowEnforcer;
