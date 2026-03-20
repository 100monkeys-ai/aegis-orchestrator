// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Canonical tenant identifier used across bounded contexts.

use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_LOCAL_TENANT: &str = "local";

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

    pub fn local_default() -> Self {
        Self(DEFAULT_LOCAL_TENANT.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(value: &str) -> Result<(), TenantIdError> {
        if value.is_empty() {
            return Err(TenantIdError::Empty);
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
        Self::local_default()
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
}
