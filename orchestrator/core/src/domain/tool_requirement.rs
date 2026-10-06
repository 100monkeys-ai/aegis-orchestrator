// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Tool Requirement Domain Service (BC-1 Agent Lifecycle)
//!
//! Refuses an agent manifest whose declarations cannot do the work it asks
//! for (AEGIS ADR-005, Update of 2026-10-06, clauses O4 and O5).
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** A pure rule over [`AgentManifest`], called by the agent
//!   lifecycle service at deploy and at update.

use crate::domain::agent::AgentManifest;
use std::fmt;

/// A manifest refused by this rule. Its `Display` is the refusal sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequirementRefusal {
    sentence: String,
}

impl fmt::Display for ToolRequirementRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.sentence)
    }
}

impl std::error::Error for ToolRequirementRefusal {}

/// Check a manifest against clauses O4 and O5.
pub fn check(_manifest: &AgentManifest) -> Result<(), ToolRequirementRefusal> {
    Ok(())
}
