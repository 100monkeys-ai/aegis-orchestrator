// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Policy Application Service — BC-4
//!
//! Application service for validating and enforcing agent security policies.
//! Bridges the manifest's `spec.security` stanza to the infrastructure-level
//! runtime enforcement (container network rules, filesystem mount constraints).
//!
//! Also acts as the orchestrator's entry point for **SMCP `SecurityContext`**
//! management (create, update, list named permission boundaries).
//!
//! ## Relationships
//! - Consumes `PolicyValidator` domain service for structural validation
//! - Consumes `PolicyEnforcer` infrastructure service for runtime enforcement
//! - Publishes `PolicyViolationAttempted` / `PolicyViolationBlocked` events
//!
//! See AGENTS.md §BC-4 Security Policy Context, ADR-035 (SMCP).

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
