// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Shared Kernel — Cross-Context Identity Types
//!
//! This module contains identity types that are shared across multiple bounded contexts
//! as part of the DDD **Shared Kernel** pattern. These types are intentionally cross-context
//! and are used as opaque correlation handles — they carry no business logic beyond identity.
//!
//! ## What belongs here
//!
//! - UUID newtype wrappers used for cross-context correlation (`AgentId`, `ExecutionId`, etc.)
//! - Simple configuration enums shared across contexts (`ImagePullPolicy`)
//! - The `TenantId` value object (multi-tenant identity with RFC-1123 validation)
//!
//! ## What does NOT belong here
//!
//! - Aggregates, entities, or domain services
//! - Value objects with context-specific business logic
//! - Repository traits or event types
//!
//! See ADR-XXX (Shared Kernel & ACL Decisions) for the architectural rationale.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ============================================================================
// UUID Identity Macro
// ============================================================================

macro_rules! define_uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub struct $name(pub uuid::Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }

            pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
                Ok(Self(uuid::Uuid::parse_str(s)?))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

// ============================================================================
// UUID Identity Types
// ============================================================================

define_uuid_id!(
    /// Unique identifier for an agent definition (BC-1 Agent Lifecycle).
    AgentId
);

define_uuid_id!(
    /// Unique identifier for an execution run (BC-2 Execution).
    ExecutionId
);

define_uuid_id!(
    /// Unique identifier for a storage volume (BC-7 Storage Gateway).
    VolumeId
);

define_uuid_id!(
    /// Unique identifier for a workflow definition (BC-3 Workflow Orchestration).
    WorkflowId
);

define_uuid_id!(
    /// Opaque UUID identifier for a single stimulus ingestion (BC-8 Stimulus-Response).
    StimulusId
);

define_uuid_id!(
    /// Unique stable node identifier (BC-16 Infrastructure & Hosting).
    NodeId
);

define_uuid_id!(
    /// Unique identifier for a dispatch request.
    DispatchId
);

// ============================================================================
// WorkflowId extra methods
// ============================================================================

impl WorkflowId {
    pub fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }

    pub fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }
}

// ============================================================================
// ImagePullPolicy (shared enum)
// ============================================================================

/// Container image pull strategy (Shared Kernel).
///
/// Determines when container runtimes should pull images from registries.
/// Used by both Agent Lifecycle (BC-1) manifests and Execution (BC-2) runtime config.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    Default,
    schemars::JsonSchema,
)]
#[serde(rename_all = "PascalCase")]
pub enum ImagePullPolicy {
    /// Always pull from registry, even if cached locally
    Always,
    /// Use local cache if available; pull only if missing
    #[serde(rename = "IfNotPresent")]
    #[default]
    IfNotPresent,
    /// Never pull; use only cached images (fail if missing)
    Never,
}

impl std::fmt::Display for ImagePullPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImagePullPolicy::Always => write!(f, "Always"),
            ImagePullPolicy::IfNotPresent => write!(f, "IfNotPresent"),
            ImagePullPolicy::Never => write!(f, "Never"),
        }
    }
}

// ============================================================================
// TenantId (multi-tenant identity with RFC-1123 validation)
// ============================================================================

/// Keycloak realm slug for the consumer product (Zaru).
pub const CONSUMER_SLUG: &str = "zaru-consumer";

/// Reserved slug for the AEGIS platform itself (internal operations).
pub const SYSTEM_SLUG: &str = "aegis-system";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantId(String);

impl TenantId {
    pub fn new(value: impl Into<String>) -> Result<Self, TenantIdError> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    pub fn from_string(value: &str) -> Result<Self, TenantIdError> {
        Self::new(value)
    }

    /// Build a `TenantId` from a Keycloak realm slug, applying RFC-1123 label
    /// validation (1-63 chars, lowercase alphanumeric + hyphens).
    pub fn from_realm_slug(slug: impl Into<String>) -> Result<Self, TenantIdError> {
        let slug = slug.into();
        Self::validate(&slug)?;
        Ok(Self(slug))
    }

    /// Extract the tenant slug from a Keycloak issuer URL.
    ///
    /// Expects a URL of the form `https://<host>/realms/<slug>` and validates
    /// the extracted slug with the standard rules.
    pub fn from_issuer_url(issuer: &str) -> Result<Self, TenantIdError> {
        let slug = issuer
            .split("/realms/")
            .nth(1)
            .and_then(|rest| {
                let segment = rest.split('/').next().unwrap_or(rest);
                if segment.is_empty() {
                    None
                } else {
                    Some(segment)
                }
            })
            .ok_or_else(|| TenantIdError::InvalidIssuerUrl(issuer.to_string()))?;
        Self::from_realm_slug(slug)
    }

    /// Well-known consumer tenant (Zaru).
    pub fn consumer() -> Self {
        Self(CONSUMER_SLUG.to_string())
    }

    /// Well-known system tenant (AEGIS internals).
    pub fn system() -> Self {
        Self(SYSTEM_SLUG.to_string())
    }

    /// Generate a per-user tenant slug from a Keycloak `sub` claim (ADR-097).
    /// Format: `u-{uuid_without_hyphens}`
    pub fn for_consumer_user(sub: &str) -> Result<Self, TenantIdError> {
        let slug = format!("u-{}", sub.replace('-', ""));
        Self::new(slug)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_system(&self) -> bool {
        self.0 == SYSTEM_SLUG
    }

    pub fn is_consumer(&self) -> bool {
        self.0 == CONSUMER_SLUG
    }

    fn validate(value: &str) -> Result<(), TenantIdError> {
        if value.is_empty() {
            return Err(TenantIdError::Empty);
        }

        if value.len() > 63 {
            return Err(TenantIdError::InvalidLength { len: value.len() });
        }

        let valid = value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');

        if !valid {
            return Err(TenantIdError::InvalidFormat(value.to_string()));
        }

        if value.starts_with('-') || value.ends_with('-') {
            return Err(TenantIdError::InvalidFormat(value.to_string()));
        }

        Ok(())
    }
}

impl Default for TenantId {
    fn default() -> Self {
        Self::consumer()
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<TenantId> for String {
    fn from(value: TenantId) -> Self {
        value.0
    }
}

#[derive(Debug, Error)]
pub enum TenantIdError {
    #[error("tenant id cannot be empty")]
    Empty,
    #[error("tenant id must be a lowercase slug, got '{0}'")]
    InvalidFormat(String),
    #[error("could not extract realm slug from issuer URL '{0}'")]
    InvalidIssuerUrl(String),
    #[error("tenant id length {len} exceeds RFC-1123 label limit of 63 characters")]
    InvalidLength { len: usize },
}
