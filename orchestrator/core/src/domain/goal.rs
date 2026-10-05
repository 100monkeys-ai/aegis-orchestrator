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

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::execution::{ExecutionId, REAPER_MARGIN_SECONDS};
use crate::domain::node_config::{GoalsConfig, LLMProviderConfig};
use crate::domain::repository::RepositoryError;
use crate::domain::supervisor::DEFAULT_EXECUTION_TIMEOUT_SECONDS;
use crate::domain::tenant::TenantId;
use crate::domain::validation::GradientResult;

/// The longest statement a goal holds, in characters (D1).
pub const MAX_STATEMENT_CHARS: usize = 32_768;

/// The `kind` of a [`GoalEvaluation`]'s `waiting_on` while its round waits
/// for bound work still running (U21, U24), and the `waiting_on` of the
/// answer that says so (U22).
pub const WAITING_ON_EXECUTION: &str = "execution";

/// The latest a round waits for one bound execution (U23): its `started_at`
/// plus its recorded `timeout_seconds` (the supervisor's bound, migration
/// 038) plus the container reaper's [`REAPER_MARGIN_SECONDS`], by when the
/// supervisor or the reaper has ended it. An execution with no recorded
/// bound, and a workflow or intent execution, takes the node's
/// [`DEFAULT_EXECUTION_TIMEOUT_SECONDS`] plus the same margin.
pub fn execution_wait_bound(
    started_at: DateTime<Utc>,
    timeout_seconds: Option<u64>,
) -> DateTime<Utc> {
    let seconds = timeout_seconds
        .unwrap_or(DEFAULT_EXECUTION_TIMEOUT_SECONDS)
        .saturating_add(REAPER_MARGIN_SECONDS);
    chrono::Duration::try_seconds(i64::try_from(seconds).unwrap_or(i64::MAX))
        .and_then(|bound| started_at.checked_add_signed(bound))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// A time as a wait's record and answer carry it (U22, U24): RFC 3339 in UTC
/// to the millisecond, so the same instant reads as the same text whichever
/// store it came back from.
pub fn wait_time_text(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// The longest part of a faulted judge's own output an answer shows a reader,
/// in characters. A judge's input is never cut: the judge is given the goal,
/// each output and the answer whole (U14); this bounds only the text quoted
/// back from a judge whose verdict could not be read.
pub const FAULT_OUTPUT_SHOWN_CHARS: usize = 8_192;

/// The output allowance of an alias whose entry names no
/// `max_output_tokens`: `GenerationOptions`' default (`domain/llm.rs`), the
/// one `ModelConfig::max_output_tokens` overrides.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;

/// The bytes a judge's prompt holds beside its input, for goal-judge: its
/// instruction and prompt template (2,777 bytes at this writing, pinned
/// below this bound by the template's test in `cli/src/commands/builtins.rs`)
/// and the chat template's own tokens (U16).
pub const JUDGE_PROMPT_RESERVE_BYTES: usize = 8_192;

/// A judge model's room, read at run time from its alias's entry in the
/// node configuration's alias table (`spec.llm_providers[].models[]`:
/// `context_window` and `max_output_tokens`) (U16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JudgeContext {
    pub context_window: u32,
    pub max_output_tokens: u32,
}

impl JudgeContext {
    /// The stated default for an alias the configuration does not map: 32,768
    /// tokens of context with [`DEFAULT_MAX_OUTPUT_TOKENS`] for output. It is
    /// used with a warning log line; a prompt is never sent unbounded.
    pub const UNCONFIGURED: Self = Self {
        context_window: 32_768,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    };

    /// The prompt, in UTF-8 bytes, that always fits: every token of a
    /// byte-level or byte-fallback vocabulary covers at least one byte, so a
    /// prompt of at most `context_window - max_output_tokens` bytes leaves
    /// the output its allowance.
    pub fn prompt_limit_bytes(&self) -> usize {
        self.context_window.saturating_sub(self.max_output_tokens) as usize
    }
}

/// Where a judge alias's room is read at run time (U16).
pub trait JudgeContextSource: Send + Sync {
    fn judge_context(&self, alias: &str) -> Option<JudgeContext>;
}

/// [`JudgeContextSource`] over the node configuration's alias table. Where
/// several enabled providers map one alias, the entry with the smallest
/// prompt limit is taken, so the prompt fits whichever entry the selection
/// strategy picks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AliasTableJudgeContext {
    by_alias: HashMap<String, JudgeContext>,
}

impl AliasTableJudgeContext {
    pub fn from_providers(providers: &[LLMProviderConfig]) -> Self {
        let mut by_alias: HashMap<String, JudgeContext> = HashMap::new();
        for model in providers
            .iter()
            .filter(|p| p.enabled)
            .flat_map(|p| p.models.iter())
        {
            let context = JudgeContext {
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
            };
            by_alias
                .entry(model.alias.clone())
                .and_modify(|held| {
                    if context.prompt_limit_bytes() < held.prompt_limit_bytes() {
                        *held = context;
                    }
                })
                .or_insert(context);
        }
        Self { by_alias }
    }
}

impl JudgeContextSource for AliasTableJudgeContext {
    fn judge_context(&self, alias: &str) -> Option<JudgeContext> {
        self.by_alias.get(alias).copied()
    }
}

/// The alias's room from `source`, or [`JudgeContext::UNCONFIGURED`] with a
/// warning when no source is given or it does not map the alias.
pub fn judge_context_or_default(
    source: Option<&dyn JudgeContextSource>,
    alias: &str,
) -> JudgeContext {
    match source.and_then(|s| s.judge_context(alias)) {
        Some(context) => context,
        None => {
            tracing::warn!(
                alias,
                context_window = JudgeContext::UNCONFIGURED.context_window,
                max_output_tokens = JudgeContext::UNCONFIGURED.max_output_tokens,
                "No configured context for the judge's alias: its prompt is bounded by the \
                 stated default"
            );
            JudgeContext::UNCONFIGURED
        }
    }
}

/// A judge's input larger than its model holds (U16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverLimit {
    /// The input's size in characters.
    pub size_chars: usize,
    /// The whole prompt's size in UTF-8 bytes.
    pub size_bytes: usize,
    /// The prompt limit in bytes ([`JudgeContext::prompt_limit_bytes`]).
    pub limit_bytes: usize,
}

impl OverLimit {
    /// Measure a prompt of `reserve_bytes` beside `input_text` against
    /// `context`; `None` when it fits.
    pub fn check(input_text: &str, reserve_bytes: usize, context: JudgeContext) -> Option<Self> {
        let size_bytes = input_text.len().saturating_add(reserve_bytes);
        let limit_bytes = context.prompt_limit_bytes();
        (size_bytes > limit_bytes).then(|| Self {
            size_chars: input_text.chars().count(),
            size_bytes,
            limit_bytes,
        })
    }

    /// The sentence a person reads.
    pub fn sentence(&self) -> String {
        format!(
            "The input is too large to judge: {} characters ({} bytes) against a limit of {} \
             bytes.",
            self.size_chars, self.size_bytes, self.limit_bytes
        )
    }
}

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

    /// The goal's own time at `now` (U29): the time since `created_at`
    /// less the time it spent waiting on work it started ([`waited`]). A goal
    /// that never waited has spent all of its time as its own.
    pub fn active_time(
        &self,
        evaluations: &[GoalEvaluation],
        now: DateTime<Utc>,
    ) -> chrono::Duration {
        now.signed_duration_since(self.created_at) - waited(evaluations, now)
    }

    /// Open past its lifetime at `now` (D7, U29): its own time, the time
    /// between its waits, has reached `lifetime_seconds`. `evaluations` are
    /// the goal's own.
    pub fn has_outlived(
        &self,
        lifetime_seconds: u64,
        evaluations: &[GoalEvaluation],
        now: DateTime<Utc>,
    ) -> bool {
        self.is_open()
            && self.active_time(evaluations, now).num_seconds()
                >= i64::try_from(lifetime_seconds).unwrap_or(i64::MAX)
    }
}

/// The time a goal has spent waiting on work it started, at `now` (U29), from
/// its wait rows (U24): each wait counts from when it began to when it ended,
/// or to `now` while it is open, and never past its `wait_until` (U23), so a
/// wait whose evaluations stopped coming counts no further than its work can
/// run. A round's waits are counted once where they overlap, and a round is
/// credited at most its longest single wait bound (`wait_until` less the
/// wait's start): however many times a round waits again after an
/// approval's answer, it is credited no more than one wait of its work.
pub fn waited(evaluations: &[GoalEvaluation], now: DateTime<Utc>) -> chrono::Duration {
    let mut rounds: HashMap<u32, Vec<&GoalEvaluation>> = HashMap::new();
    for wait in evaluations.iter().filter(|e| e.is_wait()) {
        rounds.entry(wait.round).or_default().push(wait);
    }
    let zero = chrono::Duration::zero();
    rounds
        .values()
        .map(|waits| {
            let mut spans: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
            let mut cap = zero;
            for wait in waits {
                let began = wait.created_at;
                let until = wait.wait_until().unwrap_or(began);
                cap = cap.max(until - began);
                let ended = wait.decided_at.unwrap_or(now).min(now).min(until);
                if ended > began {
                    spans.push((began, ended));
                }
            }
            spans.sort();
            let mut total = zero;
            let mut current: Option<(DateTime<Utc>, DateTime<Utc>)> = None;
            for (began, ended) in spans {
                current = match current {
                    Some((b, e)) if began <= e => Some((b, e.max(ended))),
                    Some((b, e)) => {
                        total += e - b;
                        Some((began, ended))
                    }
                    None => Some((began, ended)),
                };
            }
            if let Some((b, e)) = current {
                total += e - b;
            }
            total.min(cap.max(zero))
        })
        .fold(zero, |sum, round| sum + round)
}

/// The bound on one goal-judge execution, in seconds: the template's
/// `spec.security.resources.timeout: 300s` (D3). With the reaper's
/// [`REAPER_MARGIN_SECONDS`] it bounds how long a judge started after a wait
/// holds the goal against the expiry sweep (U29).
pub const GOAL_JUDGE_TIMEOUT_SECONDS: u64 = 300;

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
///
/// A **wait** is an evaluation with no judge execution whose `waiting_on` is
/// `{"kind": "execution", "execution_ids", "wait_until"}` (U24): the round
/// waited for bound work still running. Its `created_at` is when the wait
/// began, its `decided_at` when it ended (null while it waits); it has no
/// verdict and no outcome, never decides a round, and is never a judge in
/// flight or a judge fault.
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
    /// waited on an approval (U3); `{"kind": "execution", "execution_ids":
    /// [...], "wait_until": ...}` on a wait (U24).
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
    /// A wait for bound work still running (U24), open or ended.
    pub fn is_wait(&self) -> bool {
        self.judge_execution_id.is_none()
            && self
                .waiting_on
                .as_ref()
                .and_then(|w| w.get("kind"))
                .and_then(Value::as_str)
                == Some(WAITING_ON_EXECUTION)
    }

    /// A wait that has not ended (U24).
    pub fn is_open_wait(&self) -> bool {
        self.is_wait() && self.decided_at.is_none()
    }

    /// A wait's `wait_until` (U23, U24); `None` on any other evaluation.
    pub fn wait_until(&self) -> Option<DateTime<Utc>> {
        if !self.is_wait() {
            return None;
        }
        self.waiting_on
            .as_ref()?
            .get("wait_until")?
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
    }

    /// The judge has not finished. A wait names no judge and is never one
    /// (U24).
    pub fn is_running(&self) -> bool {
        self.judge_execution_id.is_some() && self.decided_at.is_none()
    }

    /// This evaluation decided its round: by an outcome with no
    /// `waiting_on`, or by stopping the goal (U17).
    pub fn decides_round(&self) -> bool {
        self.decided_at.is_some()
            && self.waiting_on.is_none()
            && (self.outcome.is_some() || self.stop_reason.is_some())
    }

    /// The judge's verdict could not be read (ADR-017's Update of 2026-10-01).
    /// Only an evaluation that names its judge execution can be one; an
    /// ended wait is not (U24).
    pub fn is_fault(&self) -> bool {
        self.judge_execution_id.is_some()
            && self.decided_at.is_some()
            && self.outcome.is_none()
            && self.stop_reason.is_none()
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

    /// The open goals created before `cutoff` (the expiry sweep, D7), less
    /// those whose current round holds an open wait whose `wait_until` is
    /// after `now` (U25).
    async fn list_open_created_before(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Goal>, RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::validation::ValidationSignal;
    use chrono::SubsecRound;

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
        assert!(!goal.has_outlived(1800, &[], created + chrono::Duration::seconds(1799)));
        assert!(goal.has_outlived(1800, &[], created + chrono::Duration::seconds(1800)));
    }

    /// A wait of `round` that began `began` seconds after `t0`, ended `ended`
    /// seconds after it (open when none), with its `wait_until` `until`.
    fn wait_span(
        t0: DateTime<Utc>,
        round: u32,
        began: i64,
        ended: Option<i64>,
        until: i64,
    ) -> GoalEvaluation {
        let at = |s: i64| t0 + chrono::Duration::seconds(s);
        GoalEvaluation {
            round,
            waiting_on: Some(serde_json::json!({
                "kind": WAITING_ON_EXECUTION,
                "execution_ids": [ExecutionId::new().to_string()],
                "wait_until": wait_time_text(at(until)),
            })),
            created_at: at(began),
            decided_at: ended.map(at),
            ..wait_row(false)
        }
    }

    /// U29: the time a goal waited is each wait from its start to its end,
    /// to `now` while open, never past its `wait_until`; a round's
    /// overlapping waits count once, and a round is credited at most its
    /// longest single wait bound; a judge's evaluation is never a wait.
    #[test]
    fn the_time_waited_is_each_wait_clipped_to_its_bound_and_capped_per_round() {
        let t0 = Utc::now().trunc_subsecs(3);
        let at = |s: i64| t0 + chrono::Duration::seconds(s);
        let secs = |e: &[GoalEvaluation], now: i64| waited(e, at(now)).num_seconds();

        let ended = [wait_span(t0, 0, 100, Some(700), 2_500)];
        assert_eq!(secs(&ended, 3_000), 600, "began to ended");
        let open = [wait_span(t0, 0, 100, None, 2_500)];
        assert_eq!(secs(&open, 1_000), 900, "began to now while open");
        assert_eq!(secs(&open, 9_000), 2_400, "never past its wait_until");

        let again = [
            wait_span(t0, 1, 0, Some(1_000), 1_200),
            wait_span(t0, 1, 500, Some(1_500), 1_600),
            wait_span(t0, 1, 2_000, Some(2_600), 3_100),
        ];
        assert_eq!(
            secs(&again, 4_000),
            1_200,
            "1,500 + 600 counted once is 2,100, capped at the round's longest bound 1,200"
        );
        let rounds = [
            ended[0].clone(),
            wait_span(t0, 1, 3_000, Some(3_100), 5_000),
        ];
        assert_eq!(secs(&rounds, 6_000), 700, "rounds add up");

        let mut judge = ended[0].clone();
        judge.judge_execution_id = Some(ExecutionId::new());
        assert_eq!(secs(&[judge], 3_000), 0, "a judge is not a wait");

        let goal = Goal {
            id: GoalId::new(),
            tenant_id: TenantId::default(),
            user_sub: "u".to_string(),
            statement: "s".to_string(),
            client_ref: "c".to_string(),
            channel: GoalChannel::Web,
            state: GoalState::Open,
            rounds: 0,
            created_at: t0,
            closed_at: None,
        };
        assert!(
            !goal.has_outlived(1800, &ended, at(2_399)),
            "1,799 s of its own"
        );
        assert!(goal.has_outlived(1800, &ended, at(2_400)));
    }

    /// U23: an execution's wait ends at its start plus its recorded bound
    /// plus the reaper's 600 s; with no recorded bound (and for a workflow
    /// or intent execution), at its start plus the node's 1,800 s plus 600 s.
    #[test]
    fn an_executions_wait_bound_is_its_time_limit_plus_the_reapers_margin() {
        let started = DateTime::parse_from_rfc3339("2026-10-05T02:48:01Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            wait_time_text(execution_wait_bound(started, Some(600))),
            "2026-10-05T03:08:01.000Z"
        );
        assert_eq!(
            wait_time_text(execution_wait_bound(started, None)),
            "2026-10-05T03:28:01.000Z"
        );
        assert_eq!(
            execution_wait_bound(started, Some(u64::MAX)),
            DateTime::<Utc>::MAX_UTC,
            "a bound past the calendar is the latest time, never a panic"
        );
    }

    fn wait_row(decided: bool) -> GoalEvaluation {
        let began = Utc::now();
        GoalEvaluation {
            id: Uuid::new_v4(),
            goal_id: GoalId::new(),
            round: 0,
            attempt: 1,
            judge_execution_id: None,
            companion_answer: "Dispatching it now.".to_string(),
            verdict: None,
            outcome: None,
            r#continue: false,
            waiting_on: Some(serde_json::json!({
                "kind": WAITING_ON_EXECUTION,
                "execution_ids": [ExecutionId::new().to_string()],
                "wait_until": "2026-10-05T03:28:01.000Z",
            })),
            answer: None,
            created_at: began,
            decided_at: decided.then_some(began),
            stop_reason: None,
            input_digest: None,
        }
    }

    /// U24: a wait, open or ended, is never a judge in flight, never a judge
    /// fault and never decides its round.
    #[test]
    fn a_wait_is_neither_a_judge_in_flight_nor_a_fault_nor_a_decision() {
        let open = wait_row(false);
        assert!(open.is_wait() && open.is_open_wait());
        assert!(!open.is_running(), "an open wait is no judge in flight");
        assert!(!open.is_fault());
        assert!(!open.decides_round());
        assert_eq!(
            open.wait_until().map(wait_time_text).as_deref(),
            Some("2026-10-05T03:28:01.000Z")
        );

        let ended = wait_row(true);
        assert!(ended.is_wait() && !ended.is_open_wait());
        assert!(!ended.is_running());
        assert!(!ended.is_fault(), "an ended wait is no judge fault");
        assert!(!ended.decides_round());

        // A judge's evaluation is not a wait, and reads as before.
        let mut judged = wait_row(false);
        judged.judge_execution_id = Some(ExecutionId::new());
        judged.waiting_on = None;
        assert!(!judged.is_wait() && judged.is_running());
        judged.decided_at = Some(Utc::now());
        assert!(
            judged.is_fault(),
            "a judge ended with no outcome is a fault"
        );
        assert_eq!(judged.wait_until(), None);
    }

    fn providers(yaml: &str) -> Vec<LLMProviderConfig> {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// U16: the alias table gives each alias its context and output
    /// allowance; an entry without `max_output_tokens` takes the default;
    /// a disabled provider is not read; of two entries for one alias the
    /// smaller prompt limit holds.
    #[test]
    fn the_alias_table_gives_each_judge_alias_its_room() {
        let table = AliasTableJudgeContext::from_providers(&providers(
            "- name: a
  type: openai-compatible
  endpoint: https://a.invalid/v1
  models:
    - {alias: judge, model: m1, capabilities: [chat], context_window: 256000, max_output_tokens: 16384}
    - {alias: tool-judge, model: m2, capabilities: [chat], context_window: 24000, max_output_tokens: 4096}
    - {alias: plain, model: m3, capabilities: [chat], context_window: 100000}
- name: b
  type: openai-compatible
  endpoint: https://b.invalid/v1
  models:
    - {alias: judge, model: m4, capabilities: [chat], context_window: 128000, max_output_tokens: 16384}
- name: c
  type: openai-compatible
  endpoint: https://c.invalid/v1
  enabled: false
  models:
    - {alias: tool-judge, model: m5, capabilities: [chat], context_window: 1000, max_output_tokens: 500}
",
        ));
        assert_eq!(
            table.judge_context("judge").unwrap().prompt_limit_bytes(),
            128_000 - 16_384
        );
        assert_eq!(
            table
                .judge_context("tool-judge")
                .unwrap()
                .prompt_limit_bytes(),
            19_904
        );
        assert_eq!(
            table.judge_context("plain"),
            Some(JudgeContext {
                context_window: 100_000,
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS
            })
        );
        assert_eq!(table.judge_context("absent"), None);
        assert_eq!(
            judge_context_or_default(Some(&table), "absent"),
            JudgeContext::UNCONFIGURED
        );
        assert_eq!(
            judge_context_or_default(None, "judge"),
            JudgeContext::UNCONFIGURED
        );
    }

    #[test]
    fn over_limit_counts_bytes_and_characters_and_fits_at_the_limit() {
        let room = JudgeContext {
            context_window: 1_000,
            max_output_tokens: 100,
        };
        assert_eq!(OverLimit::check(&"a".repeat(800), 100, room), None);
        let over = OverLimit::check(&"é".repeat(401), 100, room).unwrap();
        assert_eq!(
            (over.size_chars, over.size_bytes, over.limit_bytes),
            (401, 902, 900)
        );
        assert_eq!(
            over.sentence(),
            "The input is too large to judge: 401 characters (902 bytes) against a limit of 900 \
             bytes."
        );
    }
}
