// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Goal service (AEGIS ADR-131)
//!
//! Holds a [`Goal`] for its user (D1), binds the executions started for it
//! (D2, U6), and after every execution turn runs the built-in judge agent
//! `goal-judge` on the goal and its bound executions (D3, D4), reads the
//! verdict (D5) and answers whether a round is granted (D6), inside the node
//! configuration's bounds (D7), publishing every evaluation and closing
//! (D8). It decides and never drives the caller (D10).
//!
//! Rounds (U2): a goal's `rounds` is the number of continuations granted and
//! also the number of the round its next evaluation judges, from 0. An
//! evaluation names the round it asks about; a decided round answers its
//! stored answer and grants nothing again; the current round is judged; any
//! other round is refused with `goal_round_mismatch`. An evaluation that
//! waits on an approval decides no round (U3). A judge whose verdict cannot
//! be read, or whose execution ends without one, is run once more; a second
//! fault decides the round `not_met` with the fault as its verdict (D5,
//! ADR-017's Update of 2026-10-01; U4).
//!
//! What the service needs from the rest of the orchestrator (the bound
//! executions' records, their pending approvals, and the judge's execution)
//! comes through [`GoalWorld`], which the tool invocation service implements
//! per call, as the goal's user.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::domain::events::GoalEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::goal::{
    outcome_of, truncate_chars, BoundExecution, BoundKind, Goal, GoalChannel, GoalEvaluation,
    GoalId, GoalOutcome, GoalRepository, GoalState, MAX_JUDGED_TEXT_CHARS, MAX_STATEMENT_CHARS,
};
use crate::domain::node_config::GoalsConfig;
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;
use crate::domain::validation::{read_judge_verdict, GradientResult, JudgeFault};
use crate::infrastructure::event_bus::EventBus;

/// The built-in judge agent every goal is judged by (D3).
pub const GOAL_JUDGE_AGENT_NAME: &str = "goal-judge";

/// The longest one `aegis.goal.evaluate` call waits for the judge before it
/// answers `judging` (U7).
pub const EVALUATE_WAIT_BOUND: Duration = Duration::from_secs(45);

/// How often a waiting evaluation reads the judge's execution.
pub const JUDGE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often the daemon closes goals open past their lifetime (D7: ADR-126
/// D3's sweep interval).
pub const EXPIRY_SWEEP_INTERVAL: Duration = Duration::from_secs(600);

pub const GOAL_NOT_OPEN: &str = "goal_not_open";
pub const GOAL_ROUND_MISMATCH: &str = "goal_round_mismatch";
pub const GOAL_NOT_FOUND: &str = "goal_not_found";
pub const GOAL_INVALID: &str = "goal_invalid";
pub const GOAL_JUDGE_UNAVAILABLE: &str = "goal_judge_unavailable";

#[derive(Debug, thiserror::Error)]
pub enum GoalError {
    /// No goal of this caller by that id (another user's is answered the
    /// same way), for a read.
    #[error("{GOAL_NOT_FOUND}: no such goal for this user")]
    NotFound,
    /// Another user's goal, or a goal that is not open (D2, U6).
    #[error("{GOAL_NOT_OPEN}: the goal is not an open goal of this user")]
    NotOpen,
    /// The round asked about is neither decided nor the current one (U2).
    #[error("{GOAL_ROUND_MISMATCH}: round {asked} is neither decided nor the goal's current round {current}")]
    RoundMismatch { current: u32, asked: u32 },
    /// The request breaks a limit of D1.
    #[error("{GOAL_INVALID}: {0}")]
    Invalid(String),
    /// The judge could not be started.
    #[error("{GOAL_JUDGE_UNAVAILABLE}: {0}")]
    JudgeUnavailable(String),
    #[error("goal store: {0}")]
    Repository(#[from] RepositoryError),
}

impl GoalError {
    /// The error code a caller reads.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => GOAL_NOT_FOUND,
            Self::NotOpen => GOAL_NOT_OPEN,
            Self::RoundMismatch { .. } => GOAL_ROUND_MISMATCH,
            Self::Invalid(_) => GOAL_INVALID,
            Self::JudgeUnavailable(_) => GOAL_JUDGE_UNAVAILABLE,
            Self::Repository(_) => "goal_store_error",
        }
    }
}

/// The goal's user, as the call presents it.
#[derive(Debug, Clone)]
pub struct GoalCaller {
    pub tenant_id: TenantId,
    pub user_sub: String,
}

/// What the orchestrator's own rows say of one bound execution (D4).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionView {
    pub execution_id: ExecutionId,
    /// `agent`, `workflow` or `intent`.
    pub kind: &'static str,
    pub agent_or_workflow: String,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub iterations: Option<usize>,
    pub last_output: Option<String>,
    pub last_error: Option<String>,
}

/// Where the judge's execution stands.
#[derive(Debug, Clone, PartialEq)]
pub enum JudgeProgress {
    Running,
    /// Completed, with its last output.
    Completed(String),
    /// Failed or cancelled, with why.
    Ended(String),
}

/// The rest of the orchestrator, as one goal evaluation sees it.
#[async_trait]
pub trait GoalWorld: Send + Sync {
    /// The bound execution's record, read from the orchestrator's rows.
    async fn read_execution(&self, goal: &Goal, bound: &BoundExecution) -> Option<ExecutionView>;

    /// The ids of the ADR-126 requests still pending of the goal's user
    /// whose execution is one of `execution_ids`, each with its execution.
    async fn pending_approvals(
        &self,
        goal: &Goal,
        execution_ids: &[ExecutionId],
    ) -> Vec<(ExecutionId, String)>;

    /// Start `goal-judge` with `input`, as the goal's user.
    async fn start_judge(&self, goal: &Goal, input: Value) -> Result<ExecutionId, String>;

    /// Where the judge's execution stands.
    async fn judge_progress(&self, goal: &Goal, judge_execution_id: ExecutionId) -> JudgeProgress;
}

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub struct GoalService {
    repo: Arc<dyn GoalRepository>,
    event_bus: Arc<EventBus>,
    config: GoalsConfig,
    clock: Clock,
    wait_bound: Duration,
    poll_interval: Duration,
}

impl GoalService {
    pub fn new(
        repo: Arc<dyn GoalRepository>,
        event_bus: Arc<EventBus>,
        config: GoalsConfig,
    ) -> Self {
        Self {
            repo,
            event_bus,
            config,
            clock: Arc::new(Utc::now),
            wait_bound: EVALUATE_WAIT_BOUND,
            poll_interval: JUDGE_POLL_INTERVAL,
        }
    }

    pub fn with_clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Wait at most `bound` per evaluation, reading the judge every
    /// `poll_interval` (tests shorten the 45 s of U7).
    pub fn with_wait(mut self, bound: Duration, poll_interval: Duration) -> Self {
        self.wait_bound = bound;
        self.poll_interval = poll_interval;
        self
    }

    pub fn config(&self) -> &GoalsConfig {
        &self.config
    }

    // ── aegis.goal.create (D2) ──────────────────────────────────────────────

    /// Create a goal for the caller. An open goal of the caller under the
    /// same `client_ref` is first closed `superseded`.
    pub async fn create(
        &self,
        caller: &GoalCaller,
        statement: &str,
        client_ref: &str,
        channel: GoalChannel,
    ) -> Result<Goal, GoalError> {
        if statement.trim().is_empty() {
            return Err(GoalError::Invalid(
                "statement must not be empty".to_string(),
            ));
        }
        let chars = statement.chars().count();
        if chars > MAX_STATEMENT_CHARS {
            return Err(GoalError::Invalid(format!(
                "statement holds {chars} characters, above the {MAX_STATEMENT_CHARS} a goal holds"
            )));
        }
        if client_ref.trim().is_empty() {
            return Err(GoalError::Invalid(
                "client_ref must not be empty".to_string(),
            ));
        }
        let now = (self.clock)();
        for open in self
            .repo
            .list_open_for_client_ref(&caller.tenant_id, &caller.user_sub, client_ref)
            .await?
        {
            self.close(&open, GoalState::Superseded, now).await?;
        }
        let goal = Goal {
            id: GoalId::new(),
            tenant_id: caller.tenant_id.clone(),
            user_sub: caller.user_sub.clone(),
            statement: statement.to_string(),
            client_ref: client_ref.to_string(),
            channel,
            state: GoalState::Open,
            rounds: 0,
            created_at: now,
            closed_at: None,
        };
        self.repo.insert_goal(&goal).await?;
        tracing::info!(goal_id = %goal.id, client_ref, "Goal created");
        Ok(goal)
    }

    // ── goal_id on the four starting tools (D2, U6) ────────────────────────

    /// The caller's open goal by `goal_id`, before anything is started for
    /// it: another user's goal, a closed goal, or a goal open past its
    /// lifetime (which is closed `expired` here) is refused `goal_not_open`.
    pub async fn open_goal_for(
        &self,
        caller: &GoalCaller,
        goal_id: GoalId,
    ) -> Result<Goal, GoalError> {
        let goal = match self.repo.find_goal(goal_id).await? {
            Some(goal) if goal.belongs_to(&caller.tenant_id, &caller.user_sub) => goal,
            _ => return Err(GoalError::NotOpen),
        };
        let now = (self.clock)();
        if goal.has_outlived(self.config.lifetime_seconds, now) {
            self.close(&goal, GoalState::Expired, now).await?;
            return Err(GoalError::NotOpen);
        }
        if !goal.is_open() {
            return Err(GoalError::NotOpen);
        }
        Ok(goal)
    }

    /// Write the goal on an execution started for it.
    pub async fn bind(
        &self,
        goal_id: GoalId,
        execution_id: ExecutionId,
        kind: BoundKind,
    ) -> Result<(), GoalError> {
        let bound = match kind {
            BoundKind::Agent => self.repo.bind_execution(goal_id, execution_id).await?,
            BoundKind::Workflow => {
                self.repo
                    .bind_workflow_execution(goal_id, execution_id)
                    .await?
            }
        };
        if !bound {
            tracing::warn!(
                goal_id = %goal_id,
                execution_id = %execution_id,
                "No row to bind to its goal: the execution was not stored at its start"
            );
        }
        Ok(())
    }

    // ── aegis.goal.evaluate (D4 to D7, U2 to U4, U7) ───────────────────────

    /// Evaluate the goal after an execution turn: the stored answer of a
    /// decided round, the closed state of a closed goal, or the judge's
    /// verdict on the current round and whether a round is granted.
    pub async fn evaluate(
        &self,
        world: &dyn GoalWorld,
        caller: &GoalCaller,
        goal_id: GoalId,
        companion_answer: &str,
        round: Option<u32>,
    ) -> Result<Value, GoalError> {
        let mut goal = match self.repo.find_goal(goal_id).await? {
            Some(goal) if goal.belongs_to(&caller.tenant_id, &caller.user_sub) => goal,
            _ => return Err(GoalError::NotOpen),
        };
        let asked = round.unwrap_or(0);
        let evaluations = self.repo.list_evaluations(goal_id).await?;
        if let Some(decided) = evaluations
            .iter()
            .find(|e| e.round == asked && e.decides_round())
        {
            if let Some(answer) = &decided.answer {
                return Ok(answer.clone());
            }
        }

        let now = (self.clock)();
        if goal.has_outlived(self.config.lifetime_seconds, now) {
            self.close(&goal, GoalState::Expired, now).await?;
            goal = self.reload(goal_id).await?;
        }
        if !goal.is_open() {
            return self.closed_answer(world, &goal).await;
        }
        if asked != goal.rounds {
            return Err(GoalError::RoundMismatch {
                current: goal.rounds,
                asked,
            });
        }
        let answer = truncate_chars(companion_answer, MAX_JUDGED_TEXT_CHARS);
        self.judge_round(world, goal, &answer, evaluations).await
    }

    async fn judge_round(
        &self,
        world: &dyn GoalWorld,
        goal: Goal,
        companion_answer: &str,
        evaluations: Vec<GoalEvaluation>,
    ) -> Result<Value, GoalError> {
        let round = goal.rounds;
        let deadline = tokio::time::Instant::now() + self.wait_bound;
        let mut current = match evaluations.iter().rev().find(|e| e.round == round) {
            Some(e) if e.is_running() => e.clone(),
            Some(e) if e.is_fault() && e.attempt == 1 => {
                self.start_attempt(world, &goal, 2, &e.companion_answer)
                    .await?
            }
            _ => {
                self.start_attempt(world, &goal, 1, companion_answer)
                    .await?
            }
        };
        loop {
            let judge = current
                .judge_execution_id
                .expect("a running evaluation names its judge execution");
            let fault = match world.judge_progress(&goal, judge).await {
                JudgeProgress::Running => {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        return Ok(json!({
                            "state": "judging",
                            "goal_id": goal.id.to_string(),
                            "round": round,
                        }));
                    }
                    tokio::time::sleep(self.poll_interval.min(deadline - now)).await;
                    continue;
                }
                JudgeProgress::Completed(output) => {
                    match read_judge_verdict(GOAL_JUDGE_AGENT_NAME, &output) {
                        Ok(verdict) => return self.decide(world, &goal, current, &verdict).await,
                        Err(fault) => fault,
                    }
                }
                JudgeProgress::Ended(reason) => JudgeFault {
                    judge_agent: GOAL_JUDGE_AGENT_NAME.to_string(),
                    reason: format!("its execution ended without a verdict: {reason}"),
                    output: String::new(),
                },
            };
            tracing::warn!(
                goal_id = %goal.id,
                round,
                attempt = current.attempt,
                reason = %fault.reason,
                "goal-judge verdict cannot be read"
            );
            if current.attempt >= 2 {
                return self.decide_on_fault(world, &goal, current, &fault).await;
            }
            current.verdict = Some(fault_json(&fault));
            current.decided_at = Some((self.clock)());
            self.repo.finish_evaluation(&current).await?;
            let answer = current.companion_answer.clone();
            current = self.start_attempt(world, &goal, 2, &answer).await?;
        }
    }

    async fn start_attempt(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
        attempt: u32,
        companion_answer: &str,
    ) -> Result<GoalEvaluation, GoalError> {
        let input = self.judge_input(world, goal, companion_answer).await?;
        let judge = world
            .start_judge(goal, input)
            .await
            .map_err(GoalError::JudgeUnavailable)?;
        let evaluation = GoalEvaluation {
            id: Uuid::new_v4(),
            goal_id: goal.id,
            round: goal.rounds,
            attempt,
            judge_execution_id: Some(judge),
            companion_answer: companion_answer.to_string(),
            verdict: None,
            outcome: None,
            r#continue: false,
            waiting_on: None,
            answer: None,
            created_at: (self.clock)(),
            decided_at: None,
        };
        self.repo.insert_evaluation(&evaluation).await?;
        tracing::info!(
            goal_id = %goal.id,
            round = goal.rounds,
            attempt,
            judge_execution_id = %judge,
            "goal-judge started"
        );
        Ok(evaluation)
    }

    /// D4: the goal verbatim, every bound execution oldest first as the
    /// orchestrator's rows hold it, the companion's answer, the round and
    /// the rounds left. `approval_pending` covers executions bound directly
    /// to the goal only (U5).
    pub async fn judge_input(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
        companion_answer: &str,
    ) -> Result<Value, GoalError> {
        let bound = self.repo.list_bound(goal.id).await?;
        let pending = world.pending_approvals(goal, &directly_bound(&bound)).await;
        let mut executions = Vec::with_capacity(bound.len());
        for b in &bound {
            let Some(view) = world.read_execution(goal, b).await else {
                continue;
            };
            let approvals: Vec<&String> = pending
                .iter()
                .filter(|(id, _)| *id == view.execution_id)
                .map(|(_, approval)| approval)
                .collect();
            executions.push(json!({
                "execution_id": view.execution_id.to_string(),
                "kind": view.kind,
                "agent_or_workflow": view.agent_or_workflow,
                "status": view.status,
                "started_at": view.started_at,
                "ended_at": view.ended_at,
                "iterations": view.iterations,
                "last_output": view
                    .last_output
                    .as_deref()
                    .map(|o| truncate_chars(o, MAX_JUDGED_TEXT_CHARS)),
                "last_error": view.last_error,
                "approval_pending": approvals,
            }));
        }
        Ok(json!({
            "goal": goal.statement,
            "executions": executions,
            "companion_answer": truncate_chars(companion_answer, MAX_JUDGED_TEXT_CHARS),
            "round": goal.rounds,
            "rounds_left": self.config.max_continuations.saturating_sub(goal.rounds),
        }))
    }

    /// D5 and D6 on a verdict read.
    async fn decide(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
        mut evaluation: GoalEvaluation,
        verdict: &GradientResult,
    ) -> Result<Value, GoalError> {
        let outcome = outcome_of(verdict, &self.config);
        let verdict_json = verdict_json(verdict);
        if outcome == GoalOutcome::NotMet {
            let bound = self.repo.list_bound(goal.id).await?;
            let pending = world.pending_approvals(goal, &directly_bound(&bound)).await;
            if !pending.is_empty() {
                // D6, U3: no round while an approval is pending; the round
                // is judged again at the next evaluation.
                let approval_ids: Vec<String> = pending.into_iter().map(|(_, id)| id).collect();
                let mut answer = self
                    .answer(
                        world,
                        goal,
                        goal.state,
                        goal.rounds,
                        &verdict_json,
                        outcome,
                        false,
                    )
                    .await?;
                answer["waiting_on"] = json!("approval");
                answer["approval_ids"] = json!(approval_ids);
                evaluation.verdict = Some(verdict_json);
                evaluation.outcome = Some(outcome);
                evaluation.r#continue = false;
                evaluation.waiting_on =
                    Some(json!({"kind": "approval", "approval_ids": approval_ids}));
                evaluation.answer = Some(answer.clone());
                evaluation.decided_at = Some((self.clock)());
                self.repo.finish_evaluation(&evaluation).await?;
                self.publish_evaluated(goal, verdict.score, verdict.confidence, outcome, false);
                return Ok(answer);
            }
        }
        self.decide_round(
            world,
            goal,
            evaluation,
            verdict_json,
            verdict.score,
            verdict.confidence,
            outcome,
        )
        .await
    }

    /// A second judge fault decides the round `not_met`, with the fault as
    /// its verdict (D5, U4).
    async fn decide_on_fault(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
        evaluation: GoalEvaluation,
        fault: &JudgeFault,
    ) -> Result<Value, GoalError> {
        let mut verdict = json!({
            "score": 0.0,
            "confidence": 0.0,
            "reasoning": format!(
                "{GOAL_JUDGE_AGENT_NAME} faulted twice, on two fresh runs; the second: {}",
                fault.reason
            ),
            "signals": [],
        });
        verdict["fault"] = fault_json(fault)["fault"].clone();
        self.decide_round(
            world,
            goal,
            evaluation,
            verdict,
            0.0,
            0.0,
            GoalOutcome::NotMet,
        )
        .await
    }

    /// Decide the round: store the evaluation as the round's decision, then
    /// grant a continuation or close the goal (D6, D7).
    #[allow(clippy::too_many_arguments)]
    async fn decide_round(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
        mut evaluation: GoalEvaluation,
        verdict_json: Value,
        score: f64,
        confidence: f64,
        outcome: GoalOutcome,
    ) -> Result<Value, GoalError> {
        let (granted, closes) = match outcome {
            GoalOutcome::Met => (false, Some(GoalState::Met)),
            GoalOutcome::CannotBeMet => (false, Some(GoalState::CannotBeMet)),
            GoalOutcome::NotMet if goal.rounds < self.config.max_continuations => (true, None),
            GoalOutcome::NotMet => (false, Some(GoalState::Exhausted)),
        };
        let rounds_after = goal.rounds + u32::from(granted);
        let answer = self
            .answer(
                world,
                goal,
                closes.unwrap_or(GoalState::Open),
                rounds_after,
                &verdict_json,
                outcome,
                granted,
            )
            .await?;
        evaluation.verdict = Some(verdict_json);
        evaluation.outcome = Some(outcome);
        evaluation.r#continue = granted;
        evaluation.waiting_on = None;
        evaluation.answer = Some(answer.clone());
        let now = (self.clock)();
        evaluation.decided_at = Some(now);
        if !self.repo.finish_evaluation(&evaluation).await? {
            // Another call decided this round first: its answer stands, and
            // the round is granted once (D6).
            let stored = self
                .repo
                .list_evaluations(goal.id)
                .await?
                .into_iter()
                .find(|e| e.round == goal.rounds && e.decides_round())
                .and_then(|e| e.answer);
            if let Some(stored) = stored {
                return Ok(stored);
            }
        }
        self.publish_evaluated(goal, score, confidence, outcome, granted);
        if granted && !self.repo.grant_round(goal.id, goal.rounds).await? {
            tracing::warn!(goal_id = %goal.id, "The goal moved before its round was granted");
        }
        if let Some(state) = closes {
            self.close(goal, state, now).await?;
        }
        Ok(answer)
    }

    /// D6's answer.
    #[allow(clippy::too_many_arguments)]
    async fn answer(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
        state: GoalState,
        rounds: u32,
        verdict: &Value,
        outcome: GoalOutcome,
        granted: bool,
    ) -> Result<Value, GoalError> {
        Ok(json!({
            "goal_id": goal.id.to_string(),
            "state": state.as_str(),
            "round": rounds,
            "rounds_left": self.config.max_continuations.saturating_sub(rounds),
            "verdict": verdict,
            "outcome": outcome.as_str(),
            "continue": granted,
            "executions": self.execution_summaries(world, goal).await?,
        }))
    }

    /// D10: a closed goal answers its state and `continue: false`.
    async fn closed_answer(&self, world: &dyn GoalWorld, goal: &Goal) -> Result<Value, GoalError> {
        Ok(json!({
            "goal_id": goal.id.to_string(),
            "state": goal.state.as_str(),
            "round": goal.rounds,
            "rounds_left": self.config.max_continuations.saturating_sub(goal.rounds),
            "continue": false,
            "executions": self.execution_summaries(world, goal).await?,
        }))
    }

    async fn execution_summaries(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
    ) -> Result<Vec<Value>, GoalError> {
        let mut out = Vec::new();
        for b in self.repo.list_bound(goal.id).await? {
            if let Some(view) = world.read_execution(goal, &b).await {
                out.push(json!({
                    "execution_id": view.execution_id.to_string(),
                    "status": view.status,
                    "agent_or_workflow": view.agent_or_workflow,
                }));
            }
        }
        Ok(out)
    }

    // ── aegis.goal.status (D2, U8) ─────────────────────────────────────────

    /// The goal, its bound executions and every verdict, read-only. Another
    /// user's goal is answered as not found.
    pub async fn status(
        &self,
        world: &dyn GoalWorld,
        caller: &GoalCaller,
        goal_id: GoalId,
    ) -> Result<Value, GoalError> {
        let goal = match self.repo.find_goal(goal_id).await? {
            Some(goal) if goal.belongs_to(&caller.tenant_id, &caller.user_sub) => goal,
            _ => return Err(GoalError::NotFound),
        };
        let mut executions = Vec::new();
        for b in self.repo.list_bound(goal.id).await? {
            if let Some(view) = world.read_execution(&goal, &b).await {
                executions.push(json!({
                    "execution_id": view.execution_id.to_string(),
                    "kind": view.kind,
                    "agent_or_workflow": view.agent_or_workflow,
                    "status": view.status,
                    "started_at": view.started_at,
                    "ended_at": view.ended_at,
                }));
            }
        }
        let verdicts: Vec<Value> = self
            .repo
            .list_evaluations(goal.id)
            .await?
            .into_iter()
            .filter_map(|e| {
                let verdict = e.verdict?;
                Some(json!({
                    "round": e.round,
                    "attempt": e.attempt,
                    "score": verdict.get("score").cloned().unwrap_or(Value::Null),
                    "confidence": verdict.get("confidence").cloned().unwrap_or(Value::Null),
                    "outcome": e.outcome.map(|o| o.as_str()),
                    "reasoning": verdict
                        .get("reasoning")
                        .cloned()
                        .or_else(|| verdict.pointer("/fault/reason").cloned())
                        .unwrap_or(Value::Null),
                    "continue": e.r#continue,
                    "waiting_on": e.waiting_on,
                }))
            })
            .collect();
        Ok(json!({
            "goal_id": goal.id.to_string(),
            "statement": goal.statement,
            "client_ref": goal.client_ref,
            "channel": goal.channel.as_str(),
            "state": goal.state.as_str(),
            "rounds": goal.rounds,
            "created_at": goal.created_at,
            "closed_at": goal.closed_at,
            "executions": executions,
            "verdicts": verdicts,
        }))
    }

    // ── The lifetime (D7) ───────────────────────────────────────────────────

    /// Close every goal open past its lifetime at `now`; returns how many.
    pub async fn close_expired(&self, now: DateTime<Utc>) -> Result<usize, GoalError> {
        let cutoff = now - chrono::Duration::seconds(self.config.lifetime_seconds as i64);
        let mut closed = 0;
        for goal in self.repo.list_open_created_before(cutoff).await? {
            if self.close(&goal, GoalState::Expired, now).await? {
                closed += 1;
            }
        }
        Ok(closed)
    }

    /// Run [`Self::close_expired`] every `interval` for the life of the
    /// process (the daemon passes [`EXPIRY_SWEEP_INTERVAL`]).
    pub fn spawn_expiry_sweep(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let now = (self.clock)();
                match self.close_expired(now).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(expired = n, "Closed goals open past their lifetime"),
                    Err(e) => tracing::warn!(error = %e, "Goal expiry sweep failed"),
                }
            }
        })
    }

    async fn close(
        &self,
        goal: &Goal,
        state: GoalState,
        now: DateTime<Utc>,
    ) -> Result<bool, GoalError> {
        let closed = self.repo.close_goal(goal.id, state, now).await?;
        if closed {
            tracing::info!(goal_id = %goal.id, state = state.as_str(), "Goal closed");
            self.event_bus.publish_goal_event(GoalEvent::GoalClosed {
                goal_id: goal.id,
                tenant_id: goal.tenant_id.clone(),
                state,
                closed_at: now,
            });
        }
        Ok(closed)
    }

    async fn reload(&self, goal_id: GoalId) -> Result<Goal, GoalError> {
        self.repo
            .find_goal(goal_id)
            .await?
            .ok_or(GoalError::NotFound)
    }

    fn publish_evaluated(
        &self,
        goal: &Goal,
        score: f64,
        confidence: f64,
        outcome: GoalOutcome,
        granted: bool,
    ) {
        self.event_bus.publish_goal_event(GoalEvent::GoalEvaluated {
            goal_id: goal.id,
            tenant_id: goal.tenant_id.clone(),
            round: goal.rounds,
            score,
            confidence,
            outcome,
            r#continue: granted,
            evaluated_at: (self.clock)(),
        });
    }
}

/// The executions bound directly to the goal, whose own pending approvals
/// the orchestrator finds (U5).
fn directly_bound(bound: &[BoundExecution]) -> Vec<ExecutionId> {
    bound
        .iter()
        .filter(|b| b.kind == BoundKind::Agent)
        .map(|b| b.execution_id)
        .collect()
}

fn verdict_json(verdict: &GradientResult) -> Value {
    json!({
        "score": verdict.score,
        "confidence": verdict.confidence,
        "reasoning": verdict.reasoning,
        "signals": verdict
            .signals
            .iter()
            .map(|s| json!({"category": s.category, "score": s.score, "message": s.message}))
            .collect::<Vec<_>>(),
    })
}

fn fault_json(fault: &JudgeFault) -> Value {
    json!({
        "fault": {
            "judge_agent": fault.judge_agent,
            "reason": fault.reason,
            "output": truncate_chars(&fault.output, MAX_JUDGED_TEXT_CHARS),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// What one judge execution does.
    #[derive(Clone)]
    enum Judge {
        /// Completes with this output.
        Says(String),
        /// Never finishes inside the test.
        Runs,
        /// Fails without a verdict.
        Fails,
    }

    fn says(score: f64, confidence: f64, feasibility: f64) -> Judge {
        Judge::Says(format!(
            "```json\n{}\n```",
            json!({
                "score": score,
                "confidence": confidence,
                "reasoning": "The answer was not relayed to the user yet.",
                "signals": [
                    {"category": "delivered", "score": score, "message": "delivered?"},
                    {"category": "evidence", "score": score, "message": "evidence?"},
                    {"category": "chain", "score": score, "message": "chain?"},
                    {"category": "feasibility", "score": feasibility, "message": "feasible?"},
                    {"category": "alignment", "score": score, "message": "aligned?"},
                ],
            })
        ))
    }

    #[derive(Default)]
    struct World {
        script: Mutex<VecDeque<Judge>>,
        judges: Mutex<Vec<(ExecutionId, Judge, Value)>>,
        pending: Mutex<Vec<(ExecutionId, String)>>,
    }

    impl World {
        fn scripted(judges: Vec<Judge>) -> Self {
            Self {
                script: Mutex::new(judges.into()),
                ..Default::default()
            }
        }
        fn judges_started(&self) -> usize {
            self.judges.lock().unwrap().len()
        }
        fn last_input(&self) -> Value {
            self.judges.lock().unwrap().last().unwrap().2.clone()
        }
        /// The judge already running finishes with `judge`.
        fn finish_running(&self, judge: Judge) {
            self.judges.lock().unwrap().last_mut().unwrap().1 = judge;
        }
    }

    #[async_trait]
    impl GoalWorld for World {
        async fn read_execution(&self, _: &Goal, b: &BoundExecution) -> Option<ExecutionView> {
            Some(ExecutionView {
                execution_id: b.execution_id,
                kind: if b.kind == BoundKind::Agent {
                    "agent"
                } else {
                    "workflow"
                },
                agent_or_workflow: "palindrome-checker".to_string(),
                status: "completed".to_string(),
                started_at: b.started_at,
                ended_at: Some(b.started_at),
                iterations: Some(1),
                last_output: Some("x".repeat(10_000)),
                last_error: None,
            })
        }
        async fn pending_approvals(
            &self,
            _: &Goal,
            ids: &[ExecutionId],
        ) -> Vec<(ExecutionId, String)> {
            self.pending
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| ids.contains(e))
                .cloned()
                .collect()
        }
        async fn start_judge(&self, _: &Goal, input: Value) -> Result<ExecutionId, String> {
            let judge = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .ok_or("no judge scripted")?;
            let id = ExecutionId::new();
            self.judges.lock().unwrap().push((id, judge, input));
            Ok(id)
        }
        async fn judge_progress(&self, _: &Goal, id: ExecutionId) -> JudgeProgress {
            let judges = self.judges.lock().unwrap();
            match &judges.iter().find(|(j, _, _)| *j == id).unwrap().1 {
                Judge::Says(output) => JudgeProgress::Completed(output.clone()),
                Judge::Runs => JudgeProgress::Running,
                Judge::Fails => JudgeProgress::Ended("failed".to_string()),
            }
        }
    }

    struct Harness {
        service: GoalService,
        repo: Arc<InMemoryGoalRepository>,
        bus: Arc<EventBus>,
        now: Arc<Mutex<DateTime<Utc>>>,
        caller: GoalCaller,
    }

    impl Harness {
        fn new() -> Self {
            let repo = Arc::new(InMemoryGoalRepository::new());
            let bus = Arc::new(EventBus::new(256));
            let now = Arc::new(Mutex::new(Utc::now()));
            let clock = now.clone();
            let service = GoalService::new(repo.clone(), bus.clone(), GoalsConfig::default())
                .with_clock(move || *clock.lock().unwrap())
                .with_wait(Duration::from_millis(60), Duration::from_millis(5));
            Self {
                service,
                repo,
                bus,
                now,
                caller: GoalCaller {
                    tenant_id: TenantId::for_consumer_user("user-a").unwrap(),
                    user_sub: "user-a".to_string(),
                },
            }
        }

        fn now(&self) -> DateTime<Utc> {
            *self.now.lock().unwrap()
        }

        fn advance(&self, seconds: i64) {
            *self.now.lock().unwrap() += chrono::Duration::seconds(seconds);
        }

        async fn goal(&self) -> Goal {
            let goal = self
                .service
                .create(
                    &self.caller,
                    "Create a new agent called palindrome-checker, then run it on \"racecar\".",
                    "conversation-1",
                    GoalChannel::Web,
                )
                .await
                .unwrap();
            self.service
                .bind(goal.id, ExecutionId::new(), BoundKind::Agent)
                .await
                .unwrap();
            goal
        }

        async fn evaluate(&self, world: &World, goal: &Goal, round: Option<u32>) -> Value {
            self.service
                .evaluate(world, &self.caller, goal.id, "Dispatching it now.", round)
                .await
                .unwrap()
        }

        async fn stored(&self, goal: &Goal) -> Goal {
            self.repo.find_goal(goal.id).await.unwrap().unwrap()
        }
    }

    // ── The trigger's orchestrator clause (ADR-131 clause 1) ───────────────

    #[tokio::test]
    async fn met_at_score_085_and_confidence_075_closes_the_goal_met() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.85, 0.75, 0.9)]);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["outcome"], "met");
        assert_eq!(answer["state"], "met");
        assert_eq!(answer["continue"], false);
        assert_eq!(h.stored(&goal).await.state, GoalState::Met);

        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.849, 0.75, 0.9)]);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["outcome"], "not_met", "0.849 is below 0.85");
    }

    #[tokio::test]
    async fn not_met_answers_continue_true_and_grants_one_round() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.4, 0.9, 0.9)]);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["outcome"], "not_met");
        assert_eq!(answer["continue"], true);
        assert_eq!(answer["state"], "open");
        assert_eq!(
            answer["round"], 1,
            "the round the next evaluation asks about"
        );
        assert_eq!(answer["rounds_left"], 2);
        assert_eq!(
            answer["verdict"]["reasoning"],
            "The answer was not relayed to the user yet."
        );
        assert_eq!(answer["executions"].as_array().unwrap().len(), 1);
        assert_eq!(h.stored(&goal).await.rounds, 1);
    }

    #[tokio::test]
    async fn the_fourth_evaluation_answers_continue_false_and_the_goal_exhausted() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.4, 0.9, 0.9); 4]);
        for round in 0..3u32 {
            let answer = h.evaluate(&world, &goal, Some(round)).await;
            assert_eq!(answer["continue"], true, "evaluation {}", round + 1);
        }
        let fourth = h.evaluate(&world, &goal, Some(3)).await;
        assert_eq!(fourth["continue"], false);
        assert_eq!(fourth["state"], "exhausted");
        assert_eq!(fourth["rounds_left"], 0);
        assert_eq!(h.stored(&goal).await.state, GoalState::Exhausted);
        assert_eq!(world.judges_started(), 4, "four judged rounds");
    }

    #[tokio::test]
    async fn a_pending_approval_answers_waiting_on_and_continue_false_and_the_round_is_judged_again(
    ) {
        let h = Harness::new();
        let goal = h.goal().await;
        let bound = h.repo.list_bound(goal.id).await.unwrap()[0].execution_id;
        let world = World::scripted(vec![says(0.4, 0.9, 0.9), says(0.4, 0.9, 0.9)]);
        world
            .pending
            .lock()
            .unwrap()
            .push((bound, "approval-1".to_string()));

        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["continue"], false);
        assert_eq!(answer["waiting_on"], "approval");
        assert_eq!(answer["approval_ids"], json!(["approval-1"]));
        assert_eq!(answer["state"], "open");
        assert_eq!(answer["round"], 0, "no round is decided");
        let stored = h.stored(&goal).await;
        assert_eq!((stored.state, stored.rounds), (GoalState::Open, 0));
        assert_eq!(
            world.last_input()["executions"][0]["approval_pending"],
            json!(["approval-1"]),
            "the pending approval is input to the judge (D4, D9)"
        );

        // The person answered: the next evaluation of round 0 judges again.
        world.pending.lock().unwrap().clear();
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["continue"], true);
        assert!(answer.get("waiting_on").is_none());
        assert_eq!(world.judges_started(), 2);
    }

    #[tokio::test]
    async fn a_repeated_evaluation_of_one_round_answers_the_same_and_grants_it_once() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.4, 0.9, 0.9), says(0.4, 0.9, 0.9)]);
        let first = h.evaluate(&world, &goal, None).await;
        let again = h.evaluate(&world, &goal, None).await;
        let and_again = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(first, again);
        assert_eq!(first, and_again);
        assert_eq!(h.stored(&goal).await.rounds, 1, "the round is granted once");
        assert_eq!(world.judges_started(), 1, "the judge ran once");
    }

    #[tokio::test]
    async fn cannot_be_met_at_a_feasibility_of_02_closes_the_goal() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.3, 0.9, 0.2)]);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["outcome"], "cannot_be_met");
        assert_eq!(answer["state"], "cannot_be_met");
        assert_eq!(answer["continue"], false);
        assert_eq!(h.stored(&goal).await.state, GoalState::CannotBeMet);

        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.3, 0.9, 0.21)]);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["outcome"], "not_met", "0.21 is above 0.2");
    }

    #[tokio::test]
    async fn a_goal_open_1800_s_closes_expired_at_its_next_evaluation_and_by_the_sweep() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![]);
        h.advance(1799);
        assert_eq!(h.service.close_expired(h.now()).await.unwrap(), 0);
        h.advance(1);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["state"], "expired");
        assert_eq!(answer["continue"], false);
        assert_eq!(world.judges_started(), 0, "an expired goal is not judged");
        assert_eq!(h.stored(&goal).await.state, GoalState::Expired);

        let other = h.goal().await;
        h.advance(1800);
        assert_eq!(h.service.close_expired(h.now()).await.unwrap(), 1);
        assert_eq!(h.stored(&other).await.state, GoalState::Expired);
    }

    #[tokio::test]
    async fn a_judge_fault_twice_decides_the_round_not_met_with_the_fault() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![
            Judge::Says("no verdict here".to_string()),
            Judge::Fails,
        ]);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(world.judges_started(), 2, "the judge is run once more");
        assert_eq!(answer["outcome"], "not_met");
        assert_eq!(answer["verdict"]["score"], 0.0);
        let reasoning = answer["verdict"]["reasoning"].as_str().unwrap();
        assert!(reasoning.contains("faulted twice"), "{reasoning}");
        assert_eq!(
            answer["continue"], true,
            "a not_met round inside the bounds"
        );
        let evaluations = h.repo.list_evaluations(goal.id).await.unwrap();
        assert_eq!(evaluations.len(), 2);
        assert!(evaluations[0].is_fault());
        assert!(evaluations[1].decides_round());
        assert_eq!(evaluations[1].attempt, 2);
    }

    #[tokio::test]
    async fn one_judge_fault_is_run_once_more_and_its_verdict_decides() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![Judge::Fails, says(0.9, 0.9, 0.9)]);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(world.judges_started(), 2);
        assert_eq!(answer["outcome"], "met");
    }

    #[tokio::test]
    async fn another_users_goal_is_not_open_for_the_caller() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.9, 0.9, 0.9)]);
        let intruder = GoalCaller {
            tenant_id: TenantId::for_consumer_user("user-b").unwrap(),
            user_sub: "user-b".to_string(),
        };
        let err = h
            .service
            .evaluate(&world, &intruder, goal.id, "", None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), GOAL_NOT_OPEN);
        let err = h
            .service
            .open_goal_for(&intruder, goal.id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), GOAL_NOT_OPEN);
        let err = h
            .service
            .status(&world, &intruder, goal.id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), GOAL_NOT_FOUND);
        assert_eq!(world.judges_started(), 0);
    }

    // ── U2, U7, D10, D2, D4, D8 ────────────────────────────────────────────

    #[tokio::test]
    async fn judging_is_answered_at_the_bound_and_the_next_call_reads_the_same_judge() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![Judge::Runs]);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(
            answer,
            json!({"state": "judging", "goal_id": goal.id.to_string(), "round": 0})
        );
        world.finish_running(says(0.9, 0.9, 0.9));
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["outcome"], "met");
        assert_eq!(
            world.judges_started(),
            1,
            "the running judge is read, not started again"
        );
    }

    #[tokio::test]
    async fn a_round_neither_decided_nor_current_is_refused() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![]);
        let err = h
            .service
            .evaluate(&world, &h.caller, goal.id, "", Some(2))
            .await
            .unwrap_err();
        assert_eq!(err.code(), GOAL_ROUND_MISMATCH);
        assert!(matches!(
            err,
            GoalError::RoundMismatch {
                current: 0,
                asked: 2
            }
        ));
    }

    #[tokio::test]
    async fn a_closed_goal_answers_its_state_and_continue_false() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.9, 0.9, 0.9)]);
        h.evaluate(&world, &goal, None).await;
        // A round never decided, asked after the goal closed met.
        let answer = h.evaluate(&world, &goal, Some(1)).await;
        assert_eq!(answer["state"], "met");
        assert_eq!(answer["continue"], false);
        assert!(answer.get("verdict").is_none());
        assert_eq!(world.judges_started(), 1);
        let err = h
            .service
            .open_goal_for(&h.caller, goal.id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), GOAL_NOT_OPEN, "a closed goal binds nothing");
    }

    #[tokio::test]
    async fn create_supersedes_the_open_goal_under_the_same_client_ref() {
        let h = Harness::new();
        let first = h.goal().await;
        let second = h.goal().await;
        assert_eq!(h.stored(&first).await.state, GoalState::Superseded);
        assert_eq!(h.stored(&second).await.state, GoalState::Open);
        let long = "x".repeat(MAX_STATEMENT_CHARS + 1);
        let err = h
            .service
            .create(&h.caller, &long, "c", GoalChannel::Api)
            .await
            .unwrap_err();
        assert_eq!(err.code(), GOAL_INVALID);
    }

    #[tokio::test]
    async fn the_judge_receives_the_goal_verbatim_and_the_executions_from_the_rows() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.4, 0.9, 0.9)]);
        let long_answer = "a".repeat(9_000);
        h.service
            .evaluate(&world, &h.caller, goal.id, &long_answer, None)
            .await
            .unwrap();
        let input = world.last_input();
        assert_eq!(input["goal"], goal.statement.as_str());
        assert_eq!(input["round"], 0);
        assert_eq!(input["rounds_left"], 3);
        assert_eq!(
            input["companion_answer"].as_str().unwrap().chars().count(),
            MAX_JUDGED_TEXT_CHARS
        );
        let execution = &input["executions"][0];
        assert_eq!(execution["kind"], "agent");
        assert_eq!(execution["status"], "completed");
        assert_eq!(
            execution["last_output"].as_str().unwrap().chars().count(),
            MAX_JUDGED_TEXT_CHARS
        );
        assert_eq!(execution["approval_pending"], json!([]));
    }

    #[tokio::test]
    async fn each_evaluation_publishes_goal_evaluated_and_each_closing_goal_closed() {
        let h = Harness::new();
        let mut events = h.bus.subscribe();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.4, 0.9, 0.9), says(0.95, 0.9, 0.9)]);
        h.evaluate(&world, &goal, None).await;
        h.evaluate(&world, &goal, Some(1)).await;
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                DomainEvent::Goal(GoalEvent::GoalEvaluated {
                    round,
                    outcome,
                    r#continue,
                    ..
                }) => seen.push(format!(
                    "evaluated {round} {} {continue_}",
                    outcome.as_str(),
                    continue_ = r#continue
                )),
                DomainEvent::Goal(GoalEvent::GoalClosed { state, .. }) => {
                    seen.push(format!("closed {}", state.as_str()))
                }
                _ => {}
            }
        }
        assert_eq!(
            seen,
            vec![
                "evaluated 0 not_met true",
                "evaluated 1 met false",
                "closed met"
            ]
        );
    }

    #[tokio::test]
    async fn status_lists_the_executions_and_every_verdict_and_changes_nothing() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.4, 0.9, 0.9)]);
        h.evaluate(&world, &goal, None).await;
        let status = h.service.status(&world, &h.caller, goal.id).await.unwrap();
        assert_eq!(status["state"], "open");
        assert_eq!(status["rounds"], 1);
        assert_eq!(status["executions"][0]["kind"], "agent");
        let verdicts = status["verdicts"].as_array().unwrap();
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0]["round"], 0);
        assert_eq!(verdicts[0]["outcome"], "not_met");
        assert_eq!(verdicts[0]["continue"], true);
        let again = h.service.status(&world, &h.caller, goal.id).await.unwrap();
        assert_eq!(status, again);
        assert_eq!(world.judges_started(), 1);
    }
}
