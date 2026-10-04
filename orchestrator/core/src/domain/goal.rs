// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Goals (AEGIS ADR-131)
//!
//! A [`Goal`] is the user's request, stated once, to which every execution
//! started for it is bound (D1). After every execution turn the
//! orchestrator runs the built-in judge agent `goal-judge` on the goal and
//! its bound executions and stores the result as a [`GoalEvaluation`]
//! (Update U1). The verdict is ADR-017's; [`outcome_of`] reads it against the
//! node configuration's [`GoalsConfig`] (D5).
//!
//! The goal lives outside every container and every model's context: it is
//! a row of the orchestrator's database, and the decision whether a round is
//! granted is the orchestrator's (D1, D6, D10).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::execution::ExecutionId;
use crate::domain::node_config::GoalsConfig;
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;
use crate::domain::validation::GradientResult;

/// The longest statement a goal holds, in characters (D1).
pub const MAX_STATEMENT_CHARS: usize = 32_768;

/// The longest part of a faulted judge's own output an answer shows a reader,
/// in characters. A judge's input is never cut: the judge is given the goal,
/// each output and the answer whole (U14); this bounds only the text quoted
/// back from a judge whose verdict could not be read.
pub const FAULT_OUTPUT_SHOWN_CHARS: usize = 8_192;

/// The signal category whose score decides "cannot be met" (D5).
pub const FEASIBILITY_SIGNAL: &str = "feasibility";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GoalId(pub Uuid);

impl GoalId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for GoalId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for GoalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where a goal stands (D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Open,
    Met,
    CannotBeMet,
    Exhausted,
    Expired,
    Superseded,
    /// A round was not judged, and the goal stopped with a stated reason:
    /// its judge input was larger than the judge's model holds (U16), or the
    /// round repeated the round before it (U17).
    Stopped,
}

impl GoalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Met => "met",
            Self::CannotBeMet => "cannot_be_met",
            Self::Exhausted => "exhausted",
            Self::Expired => "expired",
            Self::Superseded => "superseded",
            Self::Stopped => "stopped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "met" => Some(Self::Met),
            "cannot_be_met" => Some(Self::CannotBeMet),
            "exhausted" => Some(Self::Exhausted),
            "expired" => Some(Self::Expired),
            "superseded" => Some(Self::Superseded),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
}

/// The surface a goal was created from (D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalChannel {
    Web,
    Api,
}

impl GoalChannel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Api => "api",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "web" => Some(Self::Web),
            "api" => Some(Self::Api),
            _ => None,
        }
    }
}

/// What the orchestrator reads from a verdict (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalOutcome {
    Met,
    NotMet,
    CannotBeMet,
}

impl GoalOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Met => "met",
            Self::NotMet => "not_met",
            Self::CannotBeMet => "cannot_be_met",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "met" => Some(Self::Met),
            "not_met" => Some(Self::NotMet),
            "cannot_be_met" => Some(Self::CannotBeMet),
            _ => None,
        }
    }
}

/// Why a round was not judged and its goal stopped (U16, U17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The judge's prompt is larger than its model's context holds (U16).
    TooLarge,
    /// The round's judge input equals the last decided round's (U17).
    RepeatedRound,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::RepeatedRound => "repeated_round",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "too_large" => Some(Self::TooLarge),
            "repeated_round" => Some(Self::RepeatedRound),
            _ => None,
        }
    }
}

/// The SHA-256, in hex, of a judge input without `round` and `rounds_left`
/// (U17): two rounds with the same executions, in the same states, with the
/// same outputs and the same answer have the same digest. serde_json's map
/// keeps its keys sorted, so the serialisation is canonical.
pub fn judge_input_digest(input: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut input = input.clone();
    if let Value::Object(map) = &mut input {
        map.remove("round");
        map.remove("rounds_left");
    }
    let bytes = serde_json::to_vec(&input).unwrap_or_default();
    hex::encode(Sha256::digest(&bytes))
}

/// D5: met at `score ≥ met_min_score` and `confidence ≥ met_min_confidence`;
/// otherwise cannot be met when the `feasibility` signal scores at most
/// `cannot_be_met_max_feasibility`; otherwise not met.
pub fn outcome_of(verdict: &GradientResult, config: &GoalsConfig) -> GoalOutcome {
    if verdict.score >= config.met_min_score && verdict.confidence >= config.met_min_confidence {
        return GoalOutcome::Met;
    }
    let feasibility = verdict
        .signals
        .iter()
        .find(|s| s.category.eq_ignore_ascii_case(FEASIBILITY_SIGNAL))
        .map(|s| s.score);
    match feasibility {
        Some(score) if score <= config.cannot_be_met_max_feasibility => GoalOutcome::CannotBeMet,
        _ => GoalOutcome::NotMet,
    }
}

/// `text` cut to at most `max` characters, on a character boundary.
pub fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => text[..cut].to_string(),
        None => text.to_string(),
    }
}

/// One goal (D1).
#[derive(Debug, Clone, PartialEq)]
pub struct Goal {
    pub id: GoalId,
    pub tenant_id: TenantId,
    pub user_sub: String,
    pub statement: String,
    pub client_ref: String,
    pub channel: GoalChannel,
    pub state: GoalState,
    /// The continuations granted so far; also the number of the round the
    /// next evaluation judges (U2).
    pub rounds: u32,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

impl Goal {
    pub fn is_open(&self) -> bool {
        self.state == GoalState::Open
    }

    /// Whether `tenant_id` and `user_sub` name this goal's own user.
    pub fn belongs_to(&self, tenant_id: &TenantId, user_sub: &str) -> bool {
        &self.tenant_id == tenant_id && self.user_sub == user_sub
    }

    /// Open past its lifetime at `now` (D7).
    pub fn has_outlived(&self, lifetime_seconds: u64, now: DateTime<Utc>) -> bool {
        self.is_open()
            && now.signed_duration_since(self.created_at).num_seconds() >= lifetime_seconds as i64
    }
}

/// Which table a bound execution lives in (D1, U1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundKind {
    /// A row of `executions`: started by `aegis.agent.generate` or
    /// `aegis.task.execute`.
    Agent,
    /// A row of `workflow_executions`: started by `aegis.workflow.generate`
    /// or `aegis.execute.intent`.
    Workflow,
}

/// One execution bound to a goal.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundExecution {
    pub execution_id: ExecutionId,
    pub kind: BoundKind,
    pub started_at: DateTime<Utc>,
}

/// One evaluation of a goal: one `goal-judge` execution and what came of it
/// (U1). A round is decided by the one evaluation with an outcome and no
/// `waiting_on` (U2, U3).
#[derive(Debug, Clone, PartialEq)]
pub struct GoalEvaluation {
    pub id: Uuid,
    pub goal_id: GoalId,
    pub round: u32,
    /// 1, or 2 for the judge re-run after a fault (U4).
    pub attempt: u32,
    pub judge_execution_id: Option<ExecutionId>,
    pub companion_answer: String,
    /// The verdict (score, confidence, reasoning, signals), or the judge
    /// fault (`{"fault": ...}`).
    pub verdict: Option<Value>,
    pub outcome: Option<GoalOutcome>,
    pub r#continue: bool,
    /// `{"kind": "approval", "approval_ids": [...]}` when the evaluation
    /// waited on an approval (U3).
    pub waiting_on: Option<Value>,
    /// The answer `aegis.goal.evaluate` returned (D6).
    pub answer: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    /// Why the round was not judged and the goal stopped; no judge ran for
    /// an evaluation that carries it (U16, U17).
    pub stop_reason: Option<StopReason>,
    /// [`judge_input_digest`] of the input this evaluation's round was
    /// judged on; none on an evaluation stored before migration 040 (U17).
    pub input_digest: Option<String>,
}

impl GoalEvaluation {
    /// The judge has not finished.
    pub fn is_running(&self) -> bool {
        self.decided_at.is_none()
    }

    /// This evaluation decided its round: by an outcome with no
    /// `waiting_on`, or by stopping the goal (U17).
    pub fn decides_round(&self) -> bool {
        self.decided_at.is_some()
            && self.waiting_on.is_none()
            && (self.outcome.is_some() || self.stop_reason.is_some())
    }

    /// The judge's verdict could not be read (ADR-017's Update of 2026-10-01).
    pub fn is_fault(&self) -> bool {
        self.decided_at.is_some() && self.outcome.is_none() && self.stop_reason.is_none()
    }
}

/// The durable store of goals, their evaluations and their bindings (D1, U1).
#[async_trait]
pub trait GoalRepository: Send + Sync {
    async fn insert_goal(&self, goal: &Goal) -> Result<(), RepositoryError>;

    async fn find_goal(&self, id: GoalId) -> Result<Option<Goal>, RepositoryError>;

    /// The user's open goals under `client_ref`.
    async fn list_open_for_client_ref(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        client_ref: &str,
    ) -> Result<Vec<Goal>, RepositoryError>;

    /// Close an open goal; `false` when it was no longer open.
    async fn close_goal(
        &self,
        id: GoalId,
        state: GoalState,
        closed_at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError>;

    /// Grant a continuation: `rounds` from `expected_rounds` to one more, on
    /// an open goal; `false` when the goal moved meanwhile.
    async fn grant_round(&self, id: GoalId, expected_rounds: u32) -> Result<bool, RepositoryError>;

    /// Write `goal_id` on an execution's row; `false` when there is no row.
    async fn bind_execution(
        &self,
        goal_id: GoalId,
        execution_id: ExecutionId,
    ) -> Result<bool, RepositoryError>;

    /// Write `goal_id` on a workflow execution's row; `false` when there is
    /// no row.
    async fn bind_workflow_execution(
        &self,
        goal_id: GoalId,
        execution_id: ExecutionId,
    ) -> Result<bool, RepositoryError>;

    /// Every execution bound to the goal, oldest first.
    async fn list_bound(&self, goal_id: GoalId) -> Result<Vec<BoundExecution>, RepositoryError>;

    async fn insert_evaluation(&self, evaluation: &GoalEvaluation) -> Result<(), RepositoryError>;

    /// Record how an evaluation ended (its verdict, outcome, continue,
    /// waiting_on, answer and decided_at). `false` when it would decide a
    /// round another evaluation already decided.
    async fn finish_evaluation(&self, evaluation: &GoalEvaluation)
        -> Result<bool, RepositoryError>;

    /// Every evaluation of the goal, oldest first.
    async fn list_evaluations(
        &self,
        goal_id: GoalId,
    ) -> Result<Vec<GoalEvaluation>, RepositoryError>;

    /// The open goals created before `cutoff` (the expiry sweep, D7).
    async fn list_open_created_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<Goal>, RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::validation::ValidationSignal;

    fn verdict(score: f64, confidence: f64, feasibility: Option<f64>) -> GradientResult {
        GradientResult {
            score,
            confidence,
            reasoning: "a reason".to_string(),
            signals: feasibility
                .map(|f| {
                    vec![ValidationSignal {
                        category: "feasibility".to_string(),
                        score: f,
                        message: "m".to_string(),
                    }]
                })
                .unwrap_or_default(),
            metadata: Default::default(),
        }
    }

    #[test]
    fn met_at_085_and_075_and_not_below() {
        let c = GoalsConfig::default();
        assert_eq!(outcome_of(&verdict(0.85, 0.75, None), &c), GoalOutcome::Met);
        assert_eq!(
            outcome_of(&verdict(0.849, 0.75, None), &c),
            GoalOutcome::NotMet
        );
        assert_eq!(
            outcome_of(&verdict(0.85, 0.749, None), &c),
            GoalOutcome::NotMet
        );
    }

    #[test]
    fn cannot_be_met_at_feasibility_02_and_not_above() {
        let c = GoalsConfig::default();
        assert_eq!(
            outcome_of(&verdict(0.4, 0.9, Some(0.2)), &c),
            GoalOutcome::CannotBeMet
        );
        assert_eq!(
            outcome_of(&verdict(0.4, 0.9, Some(0.21)), &c),
            GoalOutcome::NotMet
        );
        // A met verdict is met whatever its feasibility signal says.
        assert_eq!(
            outcome_of(&verdict(0.9, 0.9, Some(0.0)), &c),
            GoalOutcome::Met
        );
    }

    #[test]
    fn truncation_keeps_whole_characters() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("abc", 8), "abc");
    }

    #[test]
    fn a_goal_outlives_its_lifetime_at_1800_s() {
        let created = Utc::now();
        let goal = Goal {
            id: GoalId::new(),
            tenant_id: TenantId::default(),
            user_sub: "u".to_string(),
            statement: "s".to_string(),
            client_ref: "c".to_string(),
            channel: GoalChannel::Web,
            state: GoalState::Open,
            rounds: 0,
            created_at: created,
            closed_at: None,
        };
        assert!(!goal.has_outlived(1800, created + chrono::Duration::seconds(1799)));
        assert!(goal.has_outlived(1800, created + chrono::Duration::seconds(1800)));
    }
}
