// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use async_trait::async_trait;
use anyhow::Result;

use super::security_context::SecurityContext;

/// Protocol layer Security Context repository
#[async_trait]
pub trait SecurityContextRepository: Send + Sync {
    async fn find_by_name(&self, name: &str) -> Result<Option<SecurityContext>>;
    async fn save(&self, context: SecurityContext) -> Result<()>;
    async fn list_all(&self) -> Result<Vec<SecurityContext>>;
}
