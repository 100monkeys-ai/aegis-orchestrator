// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Policy
//!
//! Provides policy functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements policy

use crate::domain::policy::{SecurityPolicy, NetworkPolicy, FilesystemPolicy, ResourceLimits};
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct NetworkRequest {
    pub host: String,
    // port, protocol, etc.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    Read,
    Write,
}

#[derive(Debug, Clone)]
pub struct ResourceUsage {
    pub cpu_us: u64,
    pub memory_mb: u64,
    pub duration_seconds: u64,
}

#[async_trait]
pub trait PolicyService: Send + Sync {
    async fn validate_policy(&self, policy: &SecurityPolicy) -> Result<()>;
    async fn enforce_network(&self, policy: &NetworkPolicy, request: &NetworkRequest) -> Result<()>;
    async fn enforce_filesystem(&self, policy: &FilesystemPolicy, path: &Path, mode: AccessMode) -> Result<()>;
    async fn check_resources(&self, limits: &ResourceLimits, usage: &ResourceUsage) -> Result<()>;
}
