// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Fleet Operations Domain (ADR-117 §D)
//!
//! Multi-target dispatch primitives for Edge Mode.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

use crate::domain::edge::EdgeTarget;
use crate::domain::shared_kernel::NodeId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FleetCommandId(pub Uuid);

impl FleetCommandId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for FleetCommandId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for FleetCommandId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FleetMode {
    Sequential,
    Parallel,
    Rolling { batch: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailurePolicy {
    FailFast,
    ContinueOnError,
    StopAfter(usize),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetDispatchPolicy {
    pub mode: FleetMode,
    pub max_concurrency: Option<usize>,
    pub failure_policy: FailurePolicy,
    pub require_min_targets: Option<usize>,
    #[serde(with = "humantime_serde")]
    pub per_target_deadline: Duration,
}

impl Default for FleetDispatchPolicy {
    fn default() -> Self {
        Self {
            mode: FleetMode::Sequential,
            max_concurrency: None,
            failure_policy: FailurePolicy::FailFast,
            require_min_targets: None,
            per_target_deadline: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    Disconnected,
    RevokedDuringDispatch,
    CrossTenant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetSummary {
    pub ok: usize,
    pub err: usize,
    pub timed_out: usize,
    pub skipped: usize,
}

/// In-memory descriptor of a running fleet operation. Persistence is not
/// required — fleet bookkeeping is transient per ADR-117.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetCommand {
    pub fleet_command_id: FleetCommandId,
    pub target: EdgeTarget,
    pub tool_name: String,
    pub policy: FleetDispatchPolicy,
    pub started_at: DateTime<Utc>,
    pub targets_resolved: Vec<NodeId>,
    pub targets_skipped: Vec<(NodeId, SkipReason)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerNodeOutcome {
    pub node_id: NodeId,
    pub ok: bool,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetExecutionResult {
    pub fleet_command_id: FleetCommandId,
    pub targets_resolved: Vec<NodeId>,
    pub targets_skipped: Vec<(NodeId, SkipReason)>,
    pub per_node: Vec<PerNodeOutcome>,
    pub summary: FleetSummary,
    pub cancelled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_conservative() {
        let p = FleetDispatchPolicy::default();
        assert!(matches!(p.mode, FleetMode::Sequential));
        assert!(matches!(p.failure_policy, FailurePolicy::FailFast));
        assert_eq!(p.per_target_deadline, Duration::from_secs(60));
    }
}
