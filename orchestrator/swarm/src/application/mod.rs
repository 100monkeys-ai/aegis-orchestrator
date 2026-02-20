// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Mod
//!
//! Provides mod functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements mod

use crate::domain::SwarmId;
use aegis_core::domain::agent::{AgentId, AgentManifest};
use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct LockToken(pub String);

#[async_trait]
pub trait SwarmService: Send + Sync {
    async fn create_swarm(&self, parent_id: AgentId) -> Result<SwarmId>;
    async fn spawn_child(&self, parent_id: AgentId, manifest: AgentManifest) -> Result<AgentId>;
    async fn send_message(&self, from: AgentId, to: AgentId, payload: Vec<u8>) -> Result<()>;
    async fn acquire_lock(&self, resource: &str) -> Result<LockToken>;
    async fn release_lock(&self, token: LockToken) -> Result<()>;
}
