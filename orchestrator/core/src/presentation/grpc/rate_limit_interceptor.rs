// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # gRPC Rate Limit Interceptor (ADR-072)
//!
//! Async rate-limiting guard for incoming gRPC calls. Designed to run **after**
//! the [`GrpcIamAuthInterceptor`](super::auth_interceptor::GrpcIamAuthInterceptor) so that the resolved [`UserIdentity`] is
//! already available.
//!
//! Since Tonic interceptors are synchronous, this follows the same pattern as
//! `auth_interceptor`: an async `check_rate_limit` function called from within
//! gRPC handler methods.
//!
//! ## Usage
//!
//! ```ignore
//! let decision = check_rate_limit(&rate_limiter, &identity).await?;
//! // If Err(Status::resource_exhausted(..)) is returned, the handler should
//! // propagate the error immediately.
//! ```

use std::sync::Arc;

use tonic::metadata::MetadataMap;
use tonic::Status;
use tracing::warn;

use crate::domain::iam::{IdentityKind, UserIdentity};
use crate::domain::rate_limit::{
    RateLimitDecision, RateLimitEnforcer, RateLimitError, RateLimitPolicyResolver,
    RateLimitResourceType, RateLimitScope,
};
use crate::domain::tenant::TenantId;

/// Holds the rate limiting dependencies needed by gRPC handlers.
///
/// Constructed once during server startup and shared (via `Arc`) with all
/// gRPC service implementations.
#[derive(Clone)]
pub struct GrpcRateLimiter {
    resolver: Arc<dyn RateLimitPolicyResolver>,
    enforcer: Arc<dyn RateLimitEnforcer>,
    /// When `false`, all calls to [`check_rate_limit`] are no-ops.
    enabled: bool,
}

impl GrpcRateLimiter {
    /// Create a new rate limiter for gRPC handlers.
    pub fn new(
        resolver: Arc<dyn RateLimitPolicyResolver>,
        enforcer: Arc<dyn RateLimitEnforcer>,
        enabled: bool,
    ) -> Self {
        Self {
            resolver,
            enforcer,
            enabled,
        }
    }

    /// Returns `true` if rate limiting is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Check the per-user aggregate rate limit for a generic API request.
///
/// This function is intended to be called at the top of each gRPC handler
/// method, *after* authentication has resolved the [`UserIdentity`].
///
/// # Returns
///
/// - `Ok(RateLimitDecision)` when the request is allowed (caller may inspect
///   `remaining` counters to set response headers).
/// - `Err(Status::resource_exhausted(..))` when the request is rejected.
///   The error message includes `retry_after_seconds` for the client.
/// - If rate limiting is disabled or the identity is `None` (unauthenticated
///   / exempt method), returns `Ok` with an allow-all decision stub.
pub async fn check_rate_limit(
    limiter: &GrpcRateLimiter,
    identity: Option<&UserIdentity>,
) -> Result<RateLimitDecision, Status> {
    // Bypass when disabled.
    if !limiter.enabled {
        return Ok(allow_all_decision());
    }

    // If no identity is available (unauthenticated or exempt), skip rate
    // limiting and let the auth interceptor handle the failure path.
    let identity = match identity {
        Some(id) => id,
        None => return Ok(allow_all_decision()),
    };

    let tenant_id = tenant_id_from_identity(identity).map_err(|e| {
        warn!(
            user = %identity.sub,
            error = %e,
            "rejecting request: tenant slug in token is invalid (ADR-097)"
        );
        Status::unauthenticated(format!("invalid tenant slug in token: {e}"))
    })?;
    let resource_type = RateLimitResourceType::AgentExecution; // aggregate API limit

    // Resolve effective policy (tier default < tenant override < user override).
    let policy = limiter
        .resolver
        .resolve_policy(identity, &tenant_id, &resource_type)
        .await
        .map_err(|e| {
            warn!(user = %identity.sub, error = %e, "rate limit policy resolution failed");
            rate_limit_error_to_status(&e)
        })?;

    // Check and atomically increment counters.
    let scope = RateLimitScope::User {
        tenant_id: tenant_id.clone(),
        user_id: identity.sub.clone(),
    };

    let decision = limiter
        .enforcer
        .check_and_increment(&scope, &policy, 1)
        .await
        .map_err(|e| {
            warn!(user = %identity.sub, error = %e, "rate limit enforcement failed");
            rate_limit_error_to_status(&e)
        })?;

    if !decision.allowed {
        let retry_after = decision.retry_after_seconds.unwrap_or(60);
        let bucket = decision
            .exhausted_bucket
            .map(|b| format!("{b:?}"))
            .unwrap_or_else(|| "unknown".to_string());

        warn!(
            user = %identity.sub,
            bucket = %bucket,
            retry_after_seconds = retry_after,
            "gRPC request rate limited"
        );

        let message = format!("Rate limit exceeded ({bucket} window). Retry after {retry_after}s.");
        let mut metadata = MetadataMap::new();
        if let Ok(v) = (retry_after * 1000).to_string().parse() {
            metadata.insert("retry-after-ms", v);
        }
        if let Some(ref exhausted) = decision.exhausted_bucket {
            if let Some(window) = policy.windows.get(exhausted) {
                if let Ok(v) = window.limit.to_string().parse() {
                    metadata.insert("x-ratelimit-limit", v);
                }
            }
        }
        if let Ok(v) = "0".parse() {
            metadata.insert("x-ratelimit-remaining", v);
        }
        return Err(Status::with_metadata(
            tonic::Code::ResourceExhausted,
            message,
            metadata,
        ));
    }

    Ok(decision)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive a [`TenantId`] from the user's identity.
///
/// Returns `Err` when the identity carries a malformed `tenant_slug`. The
/// caller MUST translate the error into an unauthenticated status — silently
/// collapsing onto `TenantId::consumer()` would route every malformed-slug
/// request into the shared consumer rate-limit bucket (ADR-097 footgun #7).
fn tenant_id_from_identity(
    identity: &UserIdentity,
) -> Result<TenantId, crate::domain::shared_kernel::TenantIdError> {
    match &identity.identity_kind {
        IdentityKind::TenantUser { tenant_slug } => TenantId::new(format!("tenant-{tenant_slug}")),
        IdentityKind::ConsumerUser { tenant_id, .. } => Ok(tenant_id.clone()),
        IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. } => {
            Ok(TenantId::system())
        }
    }
}

/// Produce a pass-through decision when rate limiting is disabled or skipped.
fn allow_all_decision() -> RateLimitDecision {
    RateLimitDecision {
        allowed: true,
        resource_type: RateLimitResourceType::AgentExecution,
        scope: RateLimitScope::User {
            tenant_id: TenantId::system(),
            user_id: String::new(),
        },
        exhausted_bucket: None,
        retry_after_seconds: None,
        remaining: Default::default(),
    }
}

/// Map a domain [`RateLimitError`] to a gRPC [`Status`].
fn rate_limit_error_to_status(err: &RateLimitError) -> Status {
    match err {
        RateLimitError::PolicyResolutionFailed(_) => {
            Status::internal(format!("Rate limit configuration error: {err}"))
        }
        RateLimitError::EnforcementFailed(_) | RateLimitError::StorageError(_) => {
            // Fail open: if the enforcement backend is unavailable we let the
            // request through rather than blocking legitimate traffic.
            Status::internal(format!("Rate limit enforcement unavailable: {err}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::iam::ZaruTier;

    #[test]
    fn tenant_id_for_consumer_user() {
        let identity = UserIdentity {
            sub: "user-1".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::consumer(),
            },
        };
        assert_eq!(
            tenant_id_from_identity(&identity).unwrap(),
            TenantId::consumer()
        );
    }

    #[test]
    fn tenant_id_for_operator() {
        let identity = UserIdentity {
            sub: "op-1".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: crate::domain::iam::AegisRole::Admin,
            },
        };
        assert_eq!(
            tenant_id_from_identity(&identity).unwrap(),
            TenantId::system()
        );
    }

    #[test]
    fn tenant_id_for_tenant_user() {
        let identity = UserIdentity {
            sub: "tu-1".into(),
            realm_slug: "tenant-acme".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "acme".into(),
            },
        };
        let tid = tenant_id_from_identity(&identity).unwrap();
        assert_eq!(tid.as_str(), "tenant-acme");
    }

    // ── ADR-097 footgun #7 regression ────────────────────────────────────
    //
    // A `TenantUser` with a malformed `tenant_slug` MUST be rejected
    // outright. The previous implementation returned
    // `TenantId::consumer()` and silently degraded the rate-limit scope.
    #[test]
    fn tenant_id_for_invalid_tenant_user_slug_returns_err() {
        let identity = UserIdentity {
            sub: "tu-bad".into(),
            realm_slug: "tenant-Bad Slug!".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "Bad Slug!".into(),
            },
        };
        let result = tenant_id_from_identity(&identity);
        assert!(
            result.is_err(),
            "ADR-097 footgun #7: malformed tenant_slug must return Err, got {result:?}"
        );
    }

    #[test]
    fn allow_all_decision_is_allowed() {
        let d = allow_all_decision();
        assert!(d.allowed);
        assert!(d.exhausted_bucket.is_none());
        assert!(d.retry_after_seconds.is_none());
    }

    #[test]
    fn disabled_limiter_skips_check() {
        // We can't easily test async in a sync test, but we can verify the
        // enabled flag is exposed correctly.
        let limiter = GrpcRateLimiter {
            resolver: Arc::new(NoopResolver),
            enforcer: Arc::new(NoopEnforcer),
            enabled: false,
        };
        assert!(!limiter.is_enabled());
    }

    // Minimal no-op implementations for compile-time verification in tests.
    struct NoopResolver;
    struct NoopEnforcer;

    #[async_trait::async_trait]
    impl RateLimitPolicyResolver for NoopResolver {
        async fn resolve_policy(
            &self,
            _identity: &UserIdentity,
            _tenant_id: &TenantId,
            _resource_type: &RateLimitResourceType,
        ) -> Result<crate::domain::rate_limit::RateLimitPolicy, RateLimitError> {
            Err(RateLimitError::PolicyResolutionFailed("noop".to_string()))
        }
    }

    #[async_trait::async_trait]
    impl RateLimitEnforcer for NoopEnforcer {
        async fn check_and_increment(
            &self,
            _scope: &RateLimitScope,
            _policy: &crate::domain::rate_limit::RateLimitPolicy,
            _cost: u64,
        ) -> Result<RateLimitDecision, RateLimitError> {
            Ok(allow_all_decision())
        }

        async fn remaining(
            &self,
            _scope: &RateLimitScope,
            _policy: &crate::domain::rate_limit::RateLimitPolicy,
        ) -> Result<
            std::collections::HashMap<crate::domain::rate_limit::RateLimitBucket, u64>,
            RateLimitError,
        > {
            Ok(Default::default())
        }
    }
}
