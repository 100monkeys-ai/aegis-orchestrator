// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::security_context::SecurityContext;

pub struct InMemorySecurityContextRepository {
    contexts: Arc<RwLock<HashMap<String, SecurityContext>>>,
}

impl InMemorySecurityContextRepository {
    pub fn new() -> Self {
        Self {
            contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemorySecurityContextRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecurityContextRepository for InMemorySecurityContextRepository {
    async fn find_by_name(&self, name: &str) -> Result<Option<SecurityContext>> {
        let guard = self.contexts.read().await;
        Ok(guard.get(name).cloned())
    }

    async fn save(&self, context: SecurityContext) -> Result<()> {
        let mut guard = self.contexts.write().await;
        guard.insert(context.name.clone(), context);
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<SecurityContext>> {
        let guard = self.contexts.read().await;
        Ok(guard.values().cloned().collect())
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let mut guard = self.contexts.write().await;
        Ok(guard.remove(name).is_some())
    }
}
