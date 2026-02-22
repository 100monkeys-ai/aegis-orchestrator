// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # `aegis-sdk` — Rust SDK for AEGIS Agent Development (BC-10)
//!
//! Provides a fluent, type-safe API for declaring agent manifests, submitting
//! executions, and watching iteration progress from Rust applications.
//!
//! ## Crate Layout
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`client`] | [`AegisClient`] — HTTP/gRPC orchestrator client |
//! | [`types`] | SDK-specific value objects (execution watchers, etc.) |
//!
//! ## Manifest Re-exports
//!
//! The SDK re-exports all manifest types directly from `aegis-core` so that
//! SDK consumers have a single import path (e.g. `aegis_sdk::AgentManifest`)
//! and always stay in sync with the orchestrator's canonical type definitions.
//!
//! See AGENTS.md §BC-10 Client SDK Context.

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
