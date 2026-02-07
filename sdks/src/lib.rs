// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

/// AEGIS Rust SDK
/// 
/// Build secure, autonomous agents with the AEGIS runtime.

pub mod client;
pub mod manifest;
pub mod types;

pub use client::AegisClient;
pub use manifest::AgentManifest;
pub use types::*;
