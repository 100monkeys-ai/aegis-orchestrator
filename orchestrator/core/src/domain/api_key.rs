// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::api_scope::ApiScope;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApiKeyId(pub Uuid);

impl ApiKeyId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ApiKeyId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ApiKeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyStatus {
    Active,
    Revoked,
}

/// Metadata record for an operator API key.
/// The actual key value is stored in OpenBao KV2 at user-namespace/api-keys/{id}.
/// PostgreSQL stores this struct (metadata only — never the key value).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: ApiKeyId,
    pub user_id: String,
    pub name: String,
    /// SHA-256 hex digest of the full key value (for verification at request time).
    pub key_hash: String,
    pub scopes: Vec<ApiScope>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub status: ApiKeyStatus,
}

impl ApiKey {
    pub fn new(
        user_id: String,
        name: String,
        key_hash: String,
        scopes: Vec<ApiScope>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id: ApiKeyId::new(),
            user_id,
            name,
            key_hash,
            scopes,
            expires_at,
            last_used_at: None,
            created_at: Utc::now(),
            status: ApiKeyStatus::Active,
        }
    }

    pub fn is_active(&self) -> bool {
        if self.status != ApiKeyStatus::Active {
            return false;
        }
        if let Some(expires_at) = self.expires_at {
            return Utc::now() < expires_at;
        }
        true
    }

    pub fn revoke(&mut self) {
        self.status = ApiKeyStatus::Revoked;
    }
}

/// Lightweight metadata returned by the API — never includes the key value.
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiKeyMetadata {
    pub id: ApiKeyId,
    pub name: String,
    pub scopes: Vec<ApiScope>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub status: ApiKeyStatus,
}

impl From<&ApiKey> for ApiKeyMetadata {
    fn from(key: &ApiKey) -> Self {
        Self {
            id: key.id.clone(),
            name: key.name.clone(),
            scopes: key.scopes.clone(),
            expires_at: key.expires_at,
            last_used_at: key.last_used_at,
            created_at: key.created_at,
            status: key.status.clone(),
        }
    }
}
