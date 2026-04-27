// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # TenantScope — authoritative tenant binding for tool dispatch
//!
//! `TenantScope` carries the tenant proven by the SEAL session / inner-loop
//! parent execution together with the caller's `IdentityKind`. It is constructed
//! once at the dispatch boundary and threaded through every `aegis.*` tool
//! handler. Tool handlers MUST NOT read `tenant_id` from raw arguments —
//! they MUST call [`crate::application::tool_invocation_service::ToolInvocationService::enforce_tenant_arg`]
//! against the `TenantScope` to either inject the authenticated tenant
//! (when absent) or reject a mismatched caller-supplied value.
//!
//! Per ADR-097 the caller's `tenant_id` is the only source of truth. Per
//! ADR-100 a `ServiceAccount` identity may delegate to a different tenant
//! by supplying the value in `args.tenant_id`; all other identity kinds
//! must match the authenticated tenant exactly.

use super::IdentityKind;
use crate::domain::tenant::TenantId;

/// Authoritative tenant scope for a single tool dispatch.
///
/// Constructed at the SEAL or inner-loop dispatch entry point from the
/// authenticated identity. Treated as immutable for the duration of the
/// dispatch.
#[derive(Debug, Clone)]
pub struct TenantScope {
    /// The tenant proven by the authenticated identity (SEAL session
    /// `tenant_id` or parent-execution `tenant_id`). All `aegis.*` tool
    /// handlers MUST scope their queries to this value.
    pub authenticated_tenant: TenantId,
    /// The kind of identity that authenticated the dispatch. Used to gate
    /// ADR-100 service-account delegation when a tool argument supplies a
    /// `tenant_id` different from `authenticated_tenant`.
    pub identity_kind: IdentityKind,
}

impl TenantScope {
    /// Construct a new `TenantScope` from an already-authenticated tenant
    /// and the caller's identity classification.
    pub fn new(authenticated_tenant: TenantId, identity_kind: IdentityKind) -> Self {
        Self {
            authenticated_tenant,
            identity_kind,
        }
    }

    /// Returns `true` when the caller is a service account permitted to
    /// delegate to a different tenant via the `tenant_id` tool argument
    /// (ADR-100).
    pub fn may_delegate(&self) -> bool {
        matches!(self.identity_kind, IdentityKind::ServiceAccount { .. })
    }
}
