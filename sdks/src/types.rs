// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Types
//!
//! Provides types functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** Implements types

use serde::{Deserialize, Serialize};

/// Common types used across the SDK.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Cold,
    Warm,
    Hot,
    Failed,
    Terminated,
}
