// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # TenantScope — authoritative tenant binding for tool dispatch
//!
//! `TenantScope` carries the tenant proven by the SEAL session / inner-loop
//! parent execution together with the caller's `IdentityKind`. It is constructed
//! once at the dispatch boundary and threaded through every `aegis.*` tool
//! handler. Tool handlers MUST NOT read `tenant_id` from raw arguments —
//! they MUST call `ToolInvocationService::enforce_tenant_arg`
//! against the `TenantScope` to either inject the authenticated tenant
//! (when absent) or reject a mismatched caller-supplied value.
//!
//! Per ADR-097 the caller's `tenant_id` is the only source of truth. Per
//! ADR-100 a `ServiceAccount` identity may delegate to a different tenant
//! by supplying the value in `args.tenant_id`; all other identity kinds
//! must match the authenticated tenant exactly.

use super::{AegisRole, IdentityKind};
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
    /// The operator escalation the dispatch runs under, when its SEAL
    /// session was attested by an escalated API key (AEGIS ADR-129 D14,
    /// D17). `authenticated_tenant` is then the key's home tenant.
    pub operator_escalation: Option<EscalationScope>,
}

/// What an active operator escalation adds to a dispatch (AEGIS ADR-129
/// D17): reads of every tenant's executions, and, for `aegis:admin` alone,
/// naming one other tenant in `tenant_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct EscalationScope {
    pub aegis_role: AegisRole,
}

impl TenantScope {
    /// Construct a new `TenantScope` from an already-authenticated tenant
    /// and the caller's identity classification.
    pub fn new(authenticated_tenant: TenantId, identity_kind: IdentityKind) -> Self {
        Self {
            authenticated_tenant,
            identity_kind,
            operator_escalation: None,
        }
    }

    /// The same scope, running under an active operator escalation.
    pub fn with_operator_escalation(mut self, escalation: EscalationScope) -> Self {
        self.operator_escalation = Some(escalation);
        self
    }

    /// Whether the dispatch runs under an active operator escalation
    /// (AEGIS ADR-129 D17: reads of any tenant's execution by its id, and
    /// the all-tenant list).
    pub fn is_escalated(&self) -> bool {
        self.operator_escalation.is_some()
    }

    /// Whether the caller may name another tenant in `tenant_id` as an
    /// escalated operator: `aegis:admin` only, the tool-path equivalent of
    /// `X-Aegis-Tenant` (AEGIS ADR-129 D17; ADR-056).
    pub fn may_name_tenant_as_admin(&self) -> bool {
        matches!(
            self.operator_escalation,
            Some(EscalationScope {
                aegis_role: AegisRole::Admin
            })
        )
    }

    /// Returns `true` when the caller is a service account permitted to
    /// delegate to a different tenant via the `tenant_id` tool argument
    /// (ADR-100).
    pub fn may_delegate(&self) -> bool {
        matches!(self.identity_kind, IdentityKind::ServiceAccount { .. })
    }
}
