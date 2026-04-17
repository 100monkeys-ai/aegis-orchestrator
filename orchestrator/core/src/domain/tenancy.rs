// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tenant Entity (ADR-056)
//!
//! Represents a provisioned tenant in the AEGIS platform. Each tenant maps 1:1
//! to a Keycloak realm and an OpenBao namespace.

use crate::domain::tenant::TenantId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Tenant lifecycle status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    Active,
    Suspended,
    Deleted,
}

/// Tenant tier classification (ADR-097)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantTier {
    Free,
    Pro,
    Business,
    Enterprise,
    System,
}

/// Tenant kind classification (ADR-056, extended by ADR-111).
///
/// Identifies the provenance and ownership model of a tenant.
///
/// - `Consumer` — per-user tenant provisioned under ADR-097 (`u-{uuid}`).
/// - `Enterprise` — enterprise tenant with a dedicated Keycloak realm (ADR-041).
/// - `Team` — shared tenant owned by a team; owns a Stripe subscription whose
///   seats cover active members. See ADR-111.
/// - `System` — platform-internal tenant (`aegis-system`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantKind {
    Consumer,
    Enterprise,
    Team,
    System,
}

impl TenantKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TenantKind::Consumer => "consumer",
            TenantKind::Enterprise => "enterprise",
            TenantKind::Team => "team",
            TenantKind::System => "system",
        }
    }
}

/// Discriminant for per-tenant resource quota types (ADR-056).
///
/// Used in `TenantEvent::TenantQuotaExceeded` and `TenantEvent::TenantQuotaUpdated`
/// to identify which quota was violated or changed without relying on stringly-typed fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantQuotaKind {
    ConcurrentExecutions,
    TotalAgents,
    StorageGb,
}

/// Per-tenant resource quotas
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantQuotas {
    pub max_concurrent_executions: u32,
    pub max_agents: u32,
    pub max_storage_gb: f64,
}

impl TenantQuotas {
    /// Return tier-specific default quotas (ADR-097).
    pub fn for_tier(tier: &TenantTier) -> Self {
        match tier {
            TenantTier::Free => Self {
                max_concurrent_executions: 2,
                max_agents: 5,
                max_storage_gb: 1.0,
            },
            TenantTier::Pro => Self {
                max_concurrent_executions: 10,
                max_agents: 50,
                max_storage_gb: 25.0,
            },
            TenantTier::Business => Self {
                max_concurrent_executions: 25,
                max_agents: 200,
                max_storage_gb: 50.0,
            },
            TenantTier::Enterprise => Self {
                max_concurrent_executions: 50,
                max_agents: 500,
                max_storage_gb: 100.0,
            },
            TenantTier::System => Self {
                max_concurrent_executions: u32::MAX,
                max_agents: u32::MAX,
                max_storage_gb: f64::MAX,
            },
        }
    }
}

impl Default for TenantQuotas {
    fn default() -> Self {
        Self {
            max_concurrent_executions: 50,
            max_agents: 500,
            max_storage_gb: 100.0,
        }
    }
}

/// Tenant aggregate — provisioned platform tenant
///
/// Each tenant owns a Keycloak realm and an OpenBao namespace.
/// Data isolation is enforced at the repository layer via `tenant_id` columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub slug: TenantId,
    pub display_name: String,
    pub status: TenantStatus,
    pub tier: TenantTier,
    pub keycloak_realm: String,
    pub openbao_namespace: String,
    pub quotas: TenantQuotas,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Tenant {
    pub fn new(
        slug: TenantId,
        display_name: String,
        tier: TenantTier,
        keycloak_realm: String,
        openbao_namespace: String,
    ) -> Self {
        let now = Utc::now();
        Self {
            slug,
            display_name,
            status: TenantStatus::Active,
            tier,
            keycloak_realm,
            openbao_namespace,
            quotas: TenantQuotas::default(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.status == TenantStatus::Active
    }

    pub fn suspend(&mut self) {
        self.status = TenantStatus::Suspended;
        self.updated_at = Utc::now();
    }

    pub fn soft_delete(&mut self) {
        self.status = TenantStatus::Deleted;
        self.deleted_at = Some(Utc::now());
        self.updated_at = Utc::now();
    }

    /// Return `true` if this tenant is a team tenant (ADR-111).
    ///
    /// Team tenants are identified by the `t-{uuid}` slug prefix, matching the
    /// slug convention set out in ADR-111 §Decision. The prefix is the single
    /// source of truth — team tenants are materialized with `TenantKind::Team`
    /// and their slug always matches this pattern.
    pub fn is_team(&self) -> bool {
        self.slug.as_str().starts_with("t-")
    }

    /// Derive the [`TenantKind`] of this tenant from its slug (ADR-111).
    ///
    /// The kind is not persisted as a column on `tenants` today; it is
    /// reconstructed deterministically from the slug prefix. This matches the
    /// naming convention established by ADR-056 (`aegis-system`, `zaru-consumer`)
    /// and extended by ADR-097 (`u-{uuid}`) and ADR-111 (`t-{uuid}`).
    pub fn kind(&self) -> TenantKind {
        let s = self.slug.as_str();
        if s == "aegis-system" {
            TenantKind::System
        } else if s == "zaru-consumer" || s.starts_with("u-") {
            TenantKind::Consumer
        } else if s.starts_with("t-") {
            TenantKind::Team
        } else {
            TenantKind::Enterprise
        }
    }
}
