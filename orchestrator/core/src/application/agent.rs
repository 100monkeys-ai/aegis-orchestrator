// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::agent::{Agent, AgentId, AgentManifest};
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait AgentLifecycleService: Send + Sync {
    async fn deploy_agent(&self, manifest: AgentManifest) -> Result<AgentId>;
    async fn get_agent(&self, id: AgentId) -> Result<Agent>;
    async fn update_agent(&self, id: AgentId, manifest: AgentManifest) -> Result<()>;
    async fn delete_agent(&self, id: AgentId) -> Result<()>;
    async fn list_agents(&self) -> Result<Vec<Agent>>;
    async fn lookup_agent(&self, name: &str) -> Result<Option<AgentId>>;
}
