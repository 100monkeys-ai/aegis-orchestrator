// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Lib
//!
//! Provides lib functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** Implements lib

/// AEGIS Rust SDK
/// 
/// Build secure, autonomous agents with the AEGIS runtime.

pub mod client;
pub mod types;

// Re-export core domain types for manifest (single source of truth)
pub use aegis_core::domain::agent::{
    AgentManifest, ManifestMetadata, AgentSpec, RuntimeConfig, TaskConfig,
    SecurityConfig, NetworkPolicy, FilesystemPolicy, ResourceLimits,
    ExecutionStrategy, ValidationConfig, ContextItem, AdvancedConfig,
    SemanticValidation, SystemValidation, OutputValidation, ScriptValidation,
    DeliveryConfig, DeliveryDestination, FallbackBehavior,
};

pub use client::AegisClient;
pub use types::*;
