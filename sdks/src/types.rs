// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

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
