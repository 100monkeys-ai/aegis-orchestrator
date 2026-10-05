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
//! Running work (U21 to U26): while a bound execution has not ended and is
//! inside its bound (its time limit plus the reaper's margin), no judge
//! starts and no round is decided; the call holds, then answers `judging`
//! with `waiting_on: "execution"`, and the wait is a row of its own. Past
//! its lifetime, a goal whose round waited is judged once when the wait
//! ends, and its not_met closes it `expired`.
//!
//! What the service needs from the rest of the orchestrator (the bound
//! executions' records, their pending approvals, and the judge's execution)
//! comes through [`GoalWorld`], which the tool invocation service implements
//! per call, as the goal's user.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::domain::events::GoalEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::goal::{
    execution_wait_bound, judge_context_or_default, judge_input_digest, outcome_of, truncate_chars,
    wait_time_text, BoundExecution, BoundKind, Goal, GoalChannel, GoalEvaluation, GoalId,
    GoalOutcome, GoalRepository, GoalState, JudgeContextSource, OverLimit, StopReason,
    FAULT_OUTPUT_SHOWN_CHARS, GOAL_JUDGE_TIMEOUT_SECONDS, JUDGE_PROMPT_RESERVE_BYTES,
    MAX_STATEMENT_CHARS, WAITING_ON_EXECUTION,
};
use crate::domain::node_config::GoalsConfig;
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;
use crate::domain::validation::{read_judge_verdict, GradientResult, JudgeFault};
use crate::infrastructure::event_bus::EventBus;

/// The built-in judge agent every goal is judged by (D3).
pub const GOAL_JUDGE_AGENT_NAME: &str = "goal-judge";

/// The alias `goal-judge` runs on (D3; `cli/templates/agents/goal-judge.yaml`
/// `spec.runtime.model`, pinned by the template's test). Its room is read
/// from the alias table at run time (U16).
pub const GOAL_JUDGE_ALIAS: &str = "judge";

/// The longest one `aegis.goal.evaluate` call waits for the judge before it
/// answers `judging` (U7).
pub const EVALUATE_WAIT_BOUND: Duration = Duration::from_secs(45);

/// How often a waiting evaluation reads the judge's execution.
pub const JUDGE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often an evaluation holding for bound work still running reads the
/// executions' rows (U22): coarse beside an iteration's minutes.
pub const EXECUTION_POLL_INTERVAL: Duration = Duration::from_secs(5);

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
    /// The latest a round waits for this execution (U23):
    /// [`crate::domain::goal::execution_wait_bound`] of its start and its
    /// recorded time limit.
    pub bound_until: DateTime<Utc>,
}

impl ExecutionView {
    /// The execution has not ended and is inside its bound at `now` (U21,
    /// U23): a round waits for it. Past its bound it is judged as it stands,
    /// whatever its row still says.
    pub fn runs_at(&self, now: DateTime<Utc>) -> bool {
        matches!(self.status.as_str(), "pending" | "running") && now < self.bound_until
    }
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
    execution_poll_interval: Duration,
    /// The alias table's room for `goal-judge`'s alias (U16); `None` bounds
    /// its prompt by [`crate::domain::goal::JudgeContext::UNCONFIGURED`].
    judge_context: Option<Arc<dyn JudgeContextSource>>,
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
            execution_poll_interval: EXECUTION_POLL_INTERVAL,
            judge_context: None,
        }
    }

    /// Read the judge alias's context and output allowance from `source`
    /// at every evaluation (U16): the daemon passes the node configuration's
    /// alias table, so a different judge model changes the limit with no
    /// change of code.
    pub fn with_judge_context(mut self, source: Arc<dyn JudgeContextSource>) -> Self {
        self.judge_context = Some(source);
        self
    }

    pub fn with_clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Wait at most `bound` per evaluation, reading the judge, and the bound
    /// executions while they run, every `poll_interval` (tests shorten the
    /// 45 s of U7 and U22, and the 500 ms and 5 s reads).
    pub fn with_wait(mut self, bound: Duration, poll_interval: Duration) -> Self {
        self.wait_bound = bound;
        self.poll_interval = poll_interval;
        self.execution_poll_interval = poll_interval;
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
    /// it: another user's goal, a closed goal, a goal open past its lifetime
    /// (which is closed `expired` here), or a goal waiting on its running
    /// work (U29: work binds between waits, so no wait is lengthened by work
    /// started during it) is refused `goal_not_open`.
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
        let evaluations = self.repo.list_evaluations(goal.id).await?;
        if goal.has_outlived(self.config.lifetime_seconds, &evaluations, now) {
            // U25: a goal whose round waited stays open for its one late
            // verdict, and binds no new work past its lifetime.
            if !round_waited(&evaluations, goal.rounds) {
                self.close(&goal, GoalState::Expired, now).await?;
            }
            return Err(GoalError::NotOpen);
        }
        if !goal.is_open() || waits_ahead(&evaluations, goal.rounds, now) {
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
        // D7, U25, U29: past its lifetime (its own time, between its waits)
        // a goal closes expired, unless its current round waited for its
        // running work: that round is judged once when the wait ends (and
        // its not_met closes it expired).
        if goal.has_outlived(self.config.lifetime_seconds, &evaluations, now)
            && !round_waited(&evaluations, goal.rounds)
        {
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
        // U7, U22: one call holds at most `wait_bound`, for the work and
        // then for the judge.
        let deadline = tokio::time::Instant::now() + self.wait_bound;
        if let Some(waiting) = self
            .wait_for_work(world, &goal, companion_answer, &evaluations, deadline)
            .await?
        {
            return Ok(waiting);
        }
        // U14: the answer is judged and stored as the person received it.
        self.judge_round(world, goal, companion_answer, evaluations, deadline)
            .await
    }

    /// U21 to U24, U26: before a round's judge starts, hold while a bound
    /// execution has not ended and is inside its bound (U23), reading the
    /// executions every `execution_poll_interval`, up to `deadline`. `None`
    /// when the round is to be judged now: nothing runs (any more), a judge
    /// is already in flight for the round, or a bound execution holds a
    /// pending approval, which keeps its precedence (D6, U3, U10). Otherwise
    /// the not-decided answer with `waiting_on: "execution"` (U22); the wait
    /// is one `goal_evaluations` row per round, reused by later calls and
    /// closed when the wait ends (U24). No judge starts, no verdict is
    /// stored, no round is decided and nothing is published while it waits.
    async fn wait_for_work(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
        companion_answer: &str,
        evaluations: &[GoalEvaluation],
        deadline: tokio::time::Instant,
    ) -> Result<Option<Value>, GoalError> {
        let round = goal.rounds;
        if evaluations
            .iter()
            .any(|e| e.round == round && e.is_running())
        {
            return Ok(None);
        }
        let mut open = evaluations
            .iter()
            .find(|e| e.round == round && e.is_open_wait())
            .cloned();
        loop {
            let now = (self.clock)();
            let bound = self.repo.list_bound(goal.id).await?;
            let approval_pending = !world
                .pending_approvals(goal, &directly_bound(&bound))
                .await
                .is_empty();
            let mut running = Vec::new();
            if !approval_pending {
                for b in &bound {
                    if let Some(view) = world.read_execution(goal, b).await {
                        if view.runs_at(now) {
                            running.push(view);
                        }
                    }
                }
            }
            let Some(wait_until) = running.iter().map(|v| v.bound_until).max() else {
                if let Some(mut wait) = open.take() {
                    wait.decided_at = Some(now);
                    self.repo.finish_evaluation(&wait).await?;
                    tracing::info!(
                        goal_id = %goal.id,
                        round,
                        approval_pending,
                        "The goal's wait for its running work ended"
                    );
                }
                return Ok(None);
            };
            let execution_ids: Vec<String> =
                running.iter().map(|v| v.execution_id.to_string()).collect();
            let waiting_on = json!({
                "kind": WAITING_ON_EXECUTION,
                "execution_ids": execution_ids,
                "wait_until": wait_time_text(wait_until),
            });
            match open.as_mut() {
                None => {
                    // Stored to the millisecond, so `waiting_since` reads the
                    // same from every store and every later call.
                    let began = now.trunc_subsecs(3);
                    let wait = GoalEvaluation {
                        id: Uuid::new_v4(),
                        goal_id: goal.id,
                        round,
                        attempt: 1,
                        judge_execution_id: None,
                        companion_answer: companion_answer.to_string(),
                        verdict: None,
                        outcome: None,
                        r#continue: false,
                        waiting_on: Some(waiting_on),
                        answer: None,
                        created_at: began,
                        decided_at: None,
                        stop_reason: None,
                        input_digest: None,
                    };
                    self.repo.insert_evaluation(&wait).await?;
                    tracing::info!(
                        goal_id = %goal.id,
                        round,
                        wait_until = %wait_time_text(wait_until),
                        "The goal waits for its running work before its round is judged"
                    );
                    open = Some(wait);
                }
                Some(wait) if wait.waiting_on.as_ref() != Some(&waiting_on) => {
                    wait.waiting_on = Some(waiting_on);
                    self.repo.finish_evaluation(wait).await?;
                }
                Some(_) => {}
            }
            let held = tokio::time::Instant::now();
            if held >= deadline {
                let since = open
                    .as_ref()
                    .map(|w| wait_time_text(w.created_at))
                    .unwrap_or_default();
                return Ok(Some(json!({
                    "state": "judging",
                    "goal_id": goal.id.to_string(),
                    "round": round,
                    "waiting_on": WAITING_ON_EXECUTION,
                    "execution_ids": execution_ids,
                    "waiting_since": since,
                    "wait_until": wait_time_text(wait_until),
                })));
            }
            tokio::time::sleep(self.execution_poll_interval.min(deadline - held)).await;
        }
    }

    async fn judge_round(
        &self,
        world: &dyn GoalWorld,
        goal: Goal,
        companion_answer: &str,
        evaluations: Vec<GoalEvaluation>,
        deadline: tokio::time::Instant,
    ) -> Result<Value, GoalError> {
        let round = goal.rounds;
        // A wait (U24) is no attempt of the judge's.
        let attempt = match evaluations
            .iter()
            .rev()
            .find(|e| e.round == round && !e.is_wait())
        {
            Some(e) if e.is_running() => Attempt::Started(e.clone()),
            Some(e) if e.is_fault() && e.attempt == 1 => {
                let answer = e.companion_answer.clone();
                self.start_attempt(world, &goal, 2, &answer, &evaluations)
                    .await?
            }
            _ => {
                self.start_attempt(world, &goal, 1, companion_answer, &evaluations)
                    .await?
            }
        };
        let mut current = match attempt {
            Attempt::Started(evaluation) => evaluation,
            Attempt::Stopped(answer) => return Ok(answer),
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
            current = match self
                .start_attempt(world, &goal, 2, &answer, &evaluations)
                .await?
            {
                Attempt::Started(evaluation) => evaluation,
                Attempt::Stopped(answer) => return Ok(answer),
            };
        }
    }

    /// Start `goal-judge` on the round, or stop the goal with a stated
    /// reason when the round's input repeats the last decided round's (U17).
    async fn start_attempt(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
        attempt: u32,
        companion_answer: &str,
        evaluations: &[GoalEvaluation],
    ) -> Result<Attempt, GoalError> {
        let input = self.judge_input(world, goal, companion_answer).await?;
        let digest = judge_input_digest(&input);
        // U16: a prompt the judge's model cannot hold is never sent and
        // never cut; the round ends with the sizes, in a sentence.
        let input_text = serde_json::to_string(&input).unwrap_or_default();
        let context = judge_context_or_default(self.judge_context.as_deref(), GOAL_JUDGE_ALIAS);
        if let Some(over) = OverLimit::check(&input_text, JUDGE_PROMPT_RESERVE_BYTES, context) {
            return self
                .stop_round(
                    world,
                    goal,
                    companion_answer,
                    digest,
                    StopReason::TooLarge,
                    over.sentence(),
                    json!({
                        "size_chars": over.size_chars,
                        "size_bytes": over.size_bytes,
                        "limit_bytes": over.limit_bytes,
                    }),
                )
                .await
                .map(Attempt::Stopped);
        }
        if attempt == 1 && repeats_the_last_round(goal, evaluations, &digest) {
            let previous = goal.rounds.saturating_sub(1);
            let reasoning = format!(
                "Round {} brought nothing new: the same executions, the same outputs and the \
                 same answer as round {previous}, so it was not judged again and the goal \
                 stopped.",
                goal.rounds
            );
            return self
                .stop_round(
                    world,
                    goal,
                    companion_answer,
                    digest,
                    StopReason::RepeatedRound,
                    reasoning,
                    json!({}),
                )
                .await
                .map(Attempt::Stopped);
        }
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
            stop_reason: None,
            input_digest: Some(digest),
        };
        self.repo.insert_evaluation(&evaluation).await?;
        tracing::info!(
            goal_id = %goal.id,
            round = goal.rounds,
            attempt,
            judge_execution_id = %judge,
            "goal-judge started"
        );
        Ok(Attempt::Started(evaluation))
    }

    /// Decide the round with no judge run and close the goal `stopped`
    /// (U16, U17): the answer carries `stopped` with the reason and its
    /// sizes, `outcome` null, `continue: false`, and `verdict.reasoning` the
    /// plain sentence a person reads.
    #[allow(clippy::too_many_arguments)]
    async fn stop_round(
        &self,
        world: &dyn GoalWorld,
        goal: &Goal,
        companion_answer: &str,
        digest: String,
        reason: StopReason,
        reasoning: String,
        mut stopped: Value,
    ) -> Result<Value, GoalError> {
        stopped["reason"] = json!(reason.as_str());
        let verdict = json!({
            "score": Value::Null,
            "confidence": Value::Null,
            "reasoning": reasoning,
            "signals": [],
        });
        let answer = json!({
            "goal_id": goal.id.to_string(),
            "state": GoalState::Stopped.as_str(),
            "round": goal.rounds,
            "rounds_left": self.config.max_continuations.saturating_sub(goal.rounds),
            "verdict": verdict,
            "outcome": Value::Null,
            "continue": false,
            "stopped": stopped,
            "executions": self.execution_summaries(world, goal).await?,
        });
        let now = (self.clock)();
        let evaluation = GoalEvaluation {
            id: Uuid::new_v4(),
            goal_id: goal.id,
            round: goal.rounds,
            attempt: 1,
            judge_execution_id: None,
            companion_answer: companion_answer.to_string(),
            verdict: Some(verdict),
            outcome: None,
            r#continue: false,
            waiting_on: None,
            answer: Some(answer.clone()),
            created_at: now,
            decided_at: Some(now),
            stop_reason: Some(reason),
            input_digest: Some(digest),
        };
        if let Err(e) = self.repo.insert_evaluation(&evaluation).await {
            // Another call decided this round first: its answer stands.
            let stored = self
                .repo
                .list_evaluations(goal.id)
                .await?
                .into_iter()
                .find(|e| e.round == goal.rounds && e.decides_round())
                .and_then(|e| e.answer);
            return stored.ok_or(GoalError::Repository(e));
        }
        tracing::warn!(
            goal_id = %goal.id,
            round = goal.rounds,
            reason = reason.as_str(),
            "Goal stopped: its round was not judged"
        );
        self.close(goal, GoalState::Stopped, now).await?;
        Ok(answer)
    }

    /// D4: the goal verbatim, every bound execution oldest first as the
    /// orchestrator's rows hold it, its last output whole, the companion's
    /// answer whole (U14: nothing a judge is given is cut), the round and the
    /// rounds left. `approval_pending` covers executions bound directly
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
                "last_output": view.last_output,
                "last_error": view.last_error,
                "approval_pending": approvals,
            }));
        }
        Ok(json!({
            "goal": goal.statement,
            "executions": executions,
            "companion_answer": companion_answer,
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
        // U25: past the lifetime no round is granted, so no approval holds
        // one; the late round's not_met closes the goal expired.
        if outcome == GoalOutcome::NotMet && !self.is_late_round(goal).await? {
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
        let late = outcome == GoalOutcome::NotMet && self.is_late_round(goal).await?;
        let (granted, closes) = match outcome {
            GoalOutcome::Met => (false, Some(GoalState::Met)),
            GoalOutcome::CannotBeMet => (false, Some(GoalState::CannotBeMet)),
            // U25: a round that waited past the lifetime grants nothing.
            GoalOutcome::NotMet if late => (false, Some(GoalState::Expired)),
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

    /// The goal, its bound executions, every verdict and every wait for
    /// running work (U24), read-only. Another user's goal is answered as not
    /// found.
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
        let evaluations = self.repo.list_evaluations(goal.id).await?;
        // U24: each wait for bound work still running, beside the verdicts.
        let waits: Vec<Value> = evaluations
            .iter()
            .filter(|e| e.is_wait())
            .map(|e| {
                let waiting_on = e.waiting_on.as_ref();
                json!({
                    "round": e.round,
                    "execution_ids": waiting_on
                        .and_then(|w| w.get("execution_ids").cloned())
                        .unwrap_or_else(|| json!([])),
                    "began_at": wait_time_text(e.created_at),
                    "ended_at": e.decided_at.map(wait_time_text),
                    "wait_until": e.wait_until().map(wait_time_text),
                })
            })
            .collect();
        let verdicts: Vec<Value> = evaluations
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
                    // U16, U17: why the round was not judged, when it was not.
                    "stop_reason": e.stop_reason.map(|r| r.as_str()),
                    // When this attempt's goal-judge execution was started,
                    // and when its outcome was stored (null while judging).
                    "judge_started_at": e.created_at,
                    "decided_at": e.decided_at,
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
            "waits": waits,
        }))
    }

    // ── The lifetime (D7) ───────────────────────────────────────────────────

    /// Close every goal open past its lifetime at `now`; returns how many.
    /// The store lists the goals whose wall time has passed the lifetime and
    /// that hold no open wait still inside its bound (U25); of those, a goal
    /// whose own time (U29) has not passed it stays open, and so does a goal
    /// whose current round waited and whose judge is in flight inside its
    /// bound: that late round is judged once (U25) and its verdict closes
    /// the goal.
    pub async fn close_expired(&self, now: DateTime<Utc>) -> Result<usize, GoalError> {
        let cutoff = now - chrono::Duration::seconds(self.config.lifetime_seconds as i64);
        let mut closed = 0;
        for goal in self.repo.list_open_created_before(cutoff, now).await? {
            let evaluations = self.repo.list_evaluations(goal.id).await?;
            if !goal.has_outlived(self.config.lifetime_seconds, &evaluations, now)
                || judge_in_flight_after_wait(&evaluations, goal.rounds, now)
            {
                continue;
            }
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

    /// U25: the goal is past its lifetime and its current round waited for
    /// its running work, so the round is judged once and grants nothing.
    async fn is_late_round(&self, goal: &Goal) -> Result<bool, GoalError> {
        let evaluations = self.repo.list_evaluations(goal.id).await?;
        Ok(
            goal.has_outlived(self.config.lifetime_seconds, &evaluations, (self.clock)())
                && round_waited(&evaluations, goal.rounds),
        )
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

/// What starting a round's judgment came to.
enum Attempt {
    /// `goal-judge` runs; this is its evaluation.
    Started(GoalEvaluation),
    /// The round was not judged and the goal stopped; this is the answer.
    Stopped(Value),
}

/// U17: the round's judge input equals the input the last decided round was
/// judged on. An evaluation stored before migration 040 has no digest and
/// never matches, so its goal is judged as before.
fn repeats_the_last_round(goal: &Goal, evaluations: &[GoalEvaluation], digest: &str) -> bool {
    let Some(previous) = goal.rounds.checked_sub(1) else {
        return false;
    };
    evaluations.iter().any(|e| {
        e.round == previous
            && e.decides_round()
            && e.stop_reason.is_none()
            && e.input_digest.as_deref() == Some(digest)
    })
}

/// U25: the round waited for bound work still running (a wait row of that
/// round, open or ended). Bounded: a wait ends by U23, work binds only to an
/// open goal inside its lifetime, and rounds stay at most D7's.
fn round_waited(evaluations: &[GoalEvaluation], round: u32) -> bool {
    evaluations.iter().any(|e| e.round == round && e.is_wait())
}

/// U25, U29: the round holds an open wait whose `wait_until` is still ahead
/// at `now`.
fn waits_ahead(evaluations: &[GoalEvaluation], round: u32, now: DateTime<Utc>) -> bool {
    evaluations.iter().any(|e| {
        e.round == round && e.is_open_wait() && e.wait_until().is_some_and(|until| until > now)
    })
}

/// The Low row of AEGIS known-defects-7 (U25, U29): the round waited for its
/// work and a judge started for it is still running at `now`, inside its
/// bound: goal-judge's [`GOAL_JUDGE_TIMEOUT_SECONDS`] plus the reaper's
/// margin from when it started, so a judge that never reports holds the goal
/// no longer than a judge can run.
fn judge_in_flight_after_wait(
    evaluations: &[GoalEvaluation],
    round: u32,
    now: DateTime<Utc>,
) -> bool {
    round_waited(evaluations, round)
        && evaluations.iter().any(|e| {
            e.round == round
                && e.is_running()
                && now < execution_wait_bound(e.created_at, Some(GOAL_JUDGE_TIMEOUT_SECONDS))
        })
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
            "output": truncate_chars(&fault.output, FAULT_OUTPUT_SHOWN_CHARS),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::goal::{AliasTableJudgeContext, JudgeContext};
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
    use std::collections::{HashMap, VecDeque};
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
        /// Every bound execution's last output; 10,000 characters when unset.
        output: Mutex<Option<String>>,
        /// A bound execution's status; `completed` when unset.
        states: Mutex<HashMap<ExecutionId, String>>,
        /// A bound execution's recorded time limit; none when unset.
        timeouts: Mutex<HashMap<ExecutionId, u64>>,
        /// After this many reads of a bound execution, every execution still
        /// `running` takes this status (the work ends inside a call).
        end_after_reads: Mutex<Option<(usize, &'static str)>>,
        reads: Mutex<usize>,
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
        /// The bound execution `id` stands at `status`.
        fn set_status(&self, id: ExecutionId, status: &str) {
            self.states.lock().unwrap().insert(id, status.to_string());
        }
    }

    #[async_trait]
    impl GoalWorld for World {
        async fn read_execution(&self, _: &Goal, b: &BoundExecution) -> Option<ExecutionView> {
            {
                let mut reads = self.reads.lock().unwrap();
                *reads += 1;
                if let Some((after, status)) = *self.end_after_reads.lock().unwrap() {
                    if *reads >= after {
                        for state in self.states.lock().unwrap().values_mut() {
                            if state == "running" {
                                *state = status.to_string();
                            }
                        }
                    }
                }
            }
            let status = self
                .states
                .lock()
                .unwrap()
                .get(&b.execution_id)
                .cloned()
                .unwrap_or_else(|| "completed".to_string());
            let timeout = self.timeouts.lock().unwrap().get(&b.execution_id).copied();
            let ended = !matches!(status.as_str(), "pending" | "running");
            Some(ExecutionView {
                execution_id: b.execution_id,
                kind: if b.kind == BoundKind::Agent {
                    "agent"
                } else {
                    "workflow"
                },
                agent_or_workflow: "palindrome-checker".to_string(),
                status,
                started_at: b.started_at,
                ended_at: ended.then_some(b.started_at),
                bound_until: crate::domain::goal::execution_wait_bound(b.started_at, timeout),
                iterations: Some(1),
                last_output: Some(
                    self.output
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| "x".repeat(10_000)),
                ),
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

    /// A companion answer that differs from round to round, as a round that
    /// did something new answers (U17 stops a round that repeats the last).
    fn answer_of_round(round: Option<u32>) -> String {
        format!("Dispatching it now (round {}).", round.unwrap_or(0))
    }

    /// The alias table as a node configuration writes it, with `judge` at
    /// `context_window` and `max_output_tokens` (production's entry at
    /// `aegis-platform-deployment` d27c51d is 256,000 and 16,384).
    fn alias_table(context_window: u32, max_output_tokens: u32) -> AliasTableJudgeContext {
        let providers: Vec<crate::domain::node_config::LLMProviderConfig> =
            serde_yaml::from_str(&format!(
                "- name: workers-ai\n  type: openai-compatible\n  endpoint: https://example.invalid/v1\n  \
                 models:\n    - alias: judge\n      model: gemma\n      capabilities: [chat]\n      \
                 context_window: {context_window}\n      max_output_tokens: {max_output_tokens}\n"
            ))
            .unwrap();
        AliasTableJudgeContext::from_providers(&providers)
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
                .with_wait(Duration::from_millis(60), Duration::from_millis(5))
                .with_judge_context(Arc::new(alias_table(256_000, 16_384)));
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
                .evaluate(world, &self.caller, goal.id, &answer_of_round(round), round)
                .await
                .unwrap()
        }

        async fn stored(&self, goal: &Goal) -> Goal {
            self.repo.find_goal(goal.id).await.unwrap().unwrap()
        }

        async fn bound(&self, goal: &Goal) -> Vec<ExecutionId> {
            self.repo
                .list_bound(goal.id)
                .await
                .unwrap()
                .into_iter()
                .map(|b| b.execution_id)
                .collect()
        }

        async fn waits(&self, goal: &Goal) -> Vec<GoalEvaluation> {
            self.repo
                .list_evaluations(goal.id)
                .await
                .unwrap()
                .into_iter()
                .filter(GoalEvaluation::is_wait)
                .collect()
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
            input["companion_answer"], long_answer,
            "U14: the answer reaches the judge whole"
        );
        let execution = &input["executions"][0];
        assert_eq!(execution["kind"], "agent");
        assert_eq!(execution["status"], "completed");
        assert_eq!(
            execution["last_output"],
            "x".repeat(10_000),
            "U14: the output reaches the judge whole"
        );
        assert_eq!(execution["approval_pending"], json!([]));
    }

    /// A long, complete report (synthetic text, 13,010 characters, the size
    /// of the answer judged on 2026-10-04 19:20-19:28 UTC) ending on its own
    /// closing sentence.
    fn complete_report_of_13010_chars() -> String {
        let closing = " End of the report: every section above is complete.";
        let paragraph = "Section: the parts, their ratings and why each was chosen. ";
        let mut report = String::new();
        while report.chars().count() + paragraph.len() <= 13_010 - closing.len() {
            report.push_str(paragraph);
        }
        while report.chars().count() < 13_010 - closing.len() {
            report.push('.');
        }
        report.push_str(closing);
        assert_eq!(report.chars().count(), 13_010);
        report
    }

    /// U14 (AEGIS ADR-131, Update of 2026-10-04 (5)): on 2026-10-04 goal-judge
    /// was given an answer and an output cut to 8,192 characters and judged
    /// the cut as the answer's own defect four rounds running. The judge is
    /// given both whole, ending on their closing sentence, and the stored
    /// evaluation holds the answer whole.
    #[tokio::test]
    async fn a_long_complete_answer_and_output_reach_the_judge_whole_and_are_stored_whole() {
        let h = Harness::new();
        let goal = h.goal().await;
        let report = complete_report_of_13010_chars();
        let world = World::scripted(vec![says(0.95, 0.9, 0.9)]);
        *world.output.lock().unwrap() = Some(report.clone());
        let answer = h
            .service
            .evaluate(&world, &h.caller, goal.id, &report, None)
            .await
            .unwrap();
        assert_eq!(answer["outcome"], "met");
        let input = world.last_input();
        let judged_answer = input["companion_answer"].as_str().unwrap();
        assert_eq!(judged_answer, report);
        assert!(judged_answer.ends_with("every section above is complete."));
        let judged_output = input["executions"][0]["last_output"].as_str().unwrap();
        assert_eq!(judged_output, report);
        let stored = h.repo.list_evaluations(goal.id).await.unwrap();
        assert_eq!(stored[0].companion_answer, report, "stored whole");
    }

    /// An evaluation stored before U14, its answer cut to 8,192 characters,
    /// reads as stored: a repeated evaluation of its decided round answers the
    /// stored answer and starts no judge, and the stored text is not
    /// presented as whole.
    #[tokio::test]
    async fn an_evaluation_stored_cut_before_u14_reads_as_stored() {
        let h = Harness::new();
        let goal = h.goal().await;
        let cut = "a".repeat(8_192);
        let stored_answer = json!({"goal_id": goal.id.to_string(), "state": "open",
            "round": 1, "continue": true, "outcome": "not_met"});
        let old = GoalEvaluation {
            id: Uuid::new_v4(),
            goal_id: goal.id,
            round: 0,
            attempt: 1,
            judge_execution_id: Some(ExecutionId::new()),
            companion_answer: cut.clone(),
            verdict: Some(json!({"score": 0.0, "confidence": 1.0,
                "reasoning": "The answer is truncated.", "signals": []})),
            outcome: Some(GoalOutcome::NotMet),
            r#continue: true,
            waiting_on: None,
            answer: Some(stored_answer.clone()),
            created_at: h.now(),
            decided_at: Some(h.now()),
            stop_reason: None,
            input_digest: None,
        };
        h.repo.insert_evaluation(&old).await.unwrap();
        let world = World::scripted(vec![]);
        let again = h
            .service
            .evaluate(&world, &h.caller, goal.id, &"a".repeat(13_010), Some(0))
            .await
            .unwrap();
        assert_eq!(again, stored_answer);
        assert_eq!(world.judges_started(), 0);
        let stored = h.repo.list_evaluations(goal.id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].companion_answer, cut,
            "kept as stored, not rewritten"
        );
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

    /// AEGIS operations/known-defects-7: aegis.goal.status verdicts carried
    /// no time, so the time to a verdict was measurable only to the polling
    /// interval. Each verdict carries the two stored times: when its
    /// goal-judge execution was started and when its outcome was stored.
    #[tokio::test]
    async fn status_lists_each_verdicts_judge_start_and_decision_times() {
        let h = Harness::new();
        let goal = h.goal().await;
        let judged_at = Utc::now() + chrono::Duration::seconds(42);
        *h.now.lock().unwrap() = judged_at;
        let world = World::scripted(vec![says(0.4, 0.9, 0.9)]);
        h.evaluate(&world, &goal, None).await;
        let status = h.service.status(&world, &h.caller, goal.id).await.unwrap();
        let verdict = &status["verdicts"][0];
        assert_eq!(verdict["judge_started_at"], json!(judged_at));
        assert_eq!(verdict["decided_at"], json!(judged_at));
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

    // ── U17: a round that repeats the last one stops the goal ─────────────

    /// AEGIS ADR-131 U17: a round whose judge input (the executions, their
    /// states and outputs, and the answer) equals the last decided round's
    /// is not judged again: the goal closes `stopped`, the answer says why in
    /// plain words, and no round is spent.
    #[tokio::test]
    async fn a_round_that_repeats_the_last_one_stops_the_goal_and_says_why_with_no_judge() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.0, 1.0, 0.9), says(0.0, 1.0, 0.9)]);
        let same = "Here is the full report again.";
        let first = h
            .service
            .evaluate(&world, &h.caller, goal.id, same, None)
            .await
            .unwrap();
        assert_eq!(first["continue"], true);
        let second = h
            .service
            .evaluate(&world, &h.caller, goal.id, same, Some(1))
            .await
            .unwrap();
        assert_eq!(
            world.judges_started(),
            1,
            "the repeated round is not judged"
        );
        assert_eq!(second["state"], "stopped");
        assert_eq!(second["continue"], false);
        assert_eq!(second["outcome"], Value::Null);
        assert_eq!(second["stopped"]["reason"], "repeated_round");
        let reasoning = second["verdict"]["reasoning"].as_str().unwrap();
        assert_eq!(
            reasoning,
            "Round 1 brought nothing new: the same executions, the same outputs and the same \
             answer as round 0, so it was not judged again and the goal stopped."
        );
        assert_eq!(h.stored(&goal).await.state.as_str(), "stopped");
        assert_eq!(h.stored(&goal).await.rounds, 1, "no round is spent");

        let again = h
            .service
            .evaluate(&world, &h.caller, goal.id, same, Some(1))
            .await
            .unwrap();
        assert_eq!(again, second, "the stopped round answers the same");
        assert_eq!(world.judges_started(), 1);

        let status = h.service.status(&world, &h.caller, goal.id).await.unwrap();
        assert_eq!(status["state"], "stopped");
        let verdicts = status["verdicts"].as_array().unwrap();
        assert_eq!(verdicts.len(), 2);
        assert_eq!(verdicts[1]["stop_reason"], "repeated_round");
        assert_eq!(verdicts[1]["reasoning"], reasoning);
    }

    /// A round with a new answer, or whose executions changed, is judged.
    #[tokio::test]
    async fn a_round_with_something_new_is_judged() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![
            says(0.0, 1.0, 0.9),
            says(0.0, 1.0, 0.9),
            says(0.9, 0.9, 0.9),
        ]);
        h.evaluate(&world, &goal, None).await;
        let answer = h.evaluate(&world, &goal, Some(1)).await;
        assert_eq!(answer["state"], "open", "a new answer is judged");
        assert_eq!(world.judges_started(), 2);
        // The same answer as round 1, but an execution's output changed.
        *world.output.lock().unwrap() = Some("a new result".to_string());
        let answer = h
            .service
            .evaluate(
                &world,
                &h.caller,
                goal.id,
                &answer_of_round(Some(1)),
                Some(2),
            )
            .await
            .unwrap();
        assert_eq!(answer["outcome"], "met");
        assert_eq!(world.judges_started(), 3);
    }

    /// An evaluation stored before migration 040 has no input digest: the
    /// next round is judged as before, whatever its input.
    #[tokio::test]
    async fn a_round_after_an_evaluation_stored_before_040_is_judged() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.0, 1.0, 0.9), says(0.0, 1.0, 0.9)]);
        let same = "Here is the full report again.";
        h.service
            .evaluate(&world, &h.caller, goal.id, same, None)
            .await
            .unwrap();
        // As a row written before 040 reads: no digest.
        let mut old = h.repo.list_evaluations(goal.id).await.unwrap().remove(0);
        old.input_digest = None;
        h.repo.finish_evaluation(&old).await.unwrap();
        let answer = h
            .service
            .evaluate(&world, &h.caller, goal.id, same, Some(1))
            .await
            .unwrap();
        assert_eq!(answer["state"], "open");
        assert_eq!(world.judges_started(), 2);
    }

    // ── U16: a judge whose model cannot hold its input does not judge ─────

    fn service_with(h: &Harness, source: Option<AliasTableJudgeContext>) -> GoalService {
        let clock = h.now.clone();
        let service = GoalService::new(h.repo.clone(), h.bus.clone(), GoalsConfig::default())
            .with_clock(move || *clock.lock().unwrap())
            .with_wait(Duration::from_millis(60), Duration::from_millis(5));
        match source {
            Some(source) => service.with_judge_context(Arc::new(source)),
            None => service,
        }
    }

    /// AEGIS ADR-131 U16: an input whose prompt is larger than the judge
    /// alias's context less its output allowance starts no judge and is
    /// never cut: the goal stops with the sizes in the answer and a sentence.
    #[tokio::test]
    async fn an_input_larger_than_the_judges_context_starts_no_judge_and_stops_with_the_sizes() {
        let h = Harness::new();
        let goal = h.goal().await;
        let service = service_with(&h, Some(alias_table(30_000, 4_000)));
        let report = complete_report_of_13010_chars();
        let world = World::scripted(vec![says(0.9, 0.9, 0.9)]);
        *world.output.lock().unwrap() = Some(report.clone());
        let input = service.judge_input(&world, &goal, &report).await.unwrap();
        let input_text = serde_json::to_string(&input).unwrap();
        let answer = service
            .evaluate(&world, &h.caller, goal.id, &report, None)
            .await
            .unwrap();
        assert_eq!(world.judges_started(), 0, "no judge reads a fragment");
        assert_eq!(answer["state"], "stopped");
        assert_eq!(answer["continue"], false);
        assert_eq!(answer["stopped"]["reason"], "too_large");
        let size_bytes = input_text.len() + JUDGE_PROMPT_RESERVE_BYTES;
        assert_eq!(answer["stopped"]["size_bytes"], size_bytes);
        assert_eq!(answer["stopped"]["size_chars"], input_text.chars().count());
        assert_eq!(answer["stopped"]["limit_bytes"], 26_000);
        assert_eq!(
            answer["verdict"]["reasoning"],
            format!(
                "The input is too large to judge: {} characters ({size_bytes} bytes) against a \
                 limit of 26000 bytes.",
                input_text.chars().count()
            )
        );
        assert_eq!(h.stored(&goal).await.state, GoalState::Stopped);
        let stored = h.repo.list_evaluations(goal.id).await.unwrap();
        assert_eq!(stored[0].stop_reason, Some(StopReason::TooLarge));
        assert_eq!(stored[0].companion_answer, report, "stored whole, not cut");
    }

    /// The limit is read from the alias table at each evaluation: the same
    /// input is judged under production's `judge` entry (256,000 and 16,384).
    #[tokio::test]
    async fn the_limit_follows_the_alias_table_with_no_change_of_code() {
        let h = Harness::new();
        let goal = h.goal().await;
        let service = service_with(&h, Some(alias_table(256_000, 16_384)));
        let report = complete_report_of_13010_chars();
        let world = World::scripted(vec![says(0.9, 0.9, 0.9)]);
        *world.output.lock().unwrap() = Some(report.clone());
        let answer = service
            .evaluate(&world, &h.caller, goal.id, &report, None)
            .await
            .unwrap();
        assert_eq!(answer["outcome"], "met");
        assert_eq!(world.judges_started(), 1);
    }

    /// With no configured context for the alias, the stated default (32,768
    /// tokens, 8,192 for output) bounds the prompt: never an unbounded send.
    #[tokio::test]
    async fn an_unconfigured_alias_is_bounded_by_the_stated_default() {
        assert_eq!(JudgeContext::UNCONFIGURED.prompt_limit_bytes(), 24_576);
        let h = Harness::new();
        let goal = h.goal().await;
        let service = service_with(&h, None);
        let world = World::scripted(vec![says(0.9, 0.9, 0.9)]);
        let answer = service
            .evaluate(&world, &h.caller, goal.id, &"a".repeat(20_000), None)
            .await
            .unwrap();
        assert_eq!(answer["stopped"]["reason"], "too_large");
        assert_eq!(answer["stopped"]["limit_bytes"], 24_576);
        assert_eq!(world.judges_started(), 0);
    }

    // ── U21 to U23, U26: work still running is waited for, never judged ──

    /// The answer a deployed runner reads as "not decided, ask again with the
    /// same round" (U7): `state: "judging"`, and U22's fields beside it.
    fn assert_waiting_answer(answer: &Value, goal: &Goal, round: u32, ids: &[ExecutionId]) {
        assert_eq!(answer["state"], "judging", "{answer}");
        assert_eq!(answer["goal_id"], goal.id.to_string());
        assert_eq!(answer["round"], round);
        assert_eq!(answer["waiting_on"], WAITING_ON_EXECUTION);
        let listed: Vec<String> = ids.iter().map(ToString::to_string).collect();
        assert_eq!(answer["execution_ids"], json!(listed));
        for field in ["waiting_since", "wait_until"] {
            let text = answer[field].as_str().unwrap_or_default();
            assert!(
                DateTime::parse_from_rfc3339(text).is_ok(),
                "{field} is an RFC 3339 time: {answer}"
            );
        }
        for absent in ["verdict", "outcome", "continue", "rounds_left"] {
            assert!(answer.get(absent).is_none(), "no {absent}: {answer}");
        }
    }

    /// The reproduction (AEGIS ADR-131 U21, agentic-solver-watch, execution
    /// b912e29a): the goal was judged four times while its execution still
    /// ran, each answer "still running" spent a round, and the goal closed
    /// exhausted while the work ran on. An execution running across four
    /// evaluations starts no judge and decides no round, and one wait row
    /// holds the wait; when it ends, the round is judged once on it as it
    /// ended, and that not_met is one real round.
    #[tokio::test]
    async fn the_reproduction_work_running_across_four_evaluations_is_waited_for_and_then_judged_once(
    ) {
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![says(0.0, 1.0, 0.9)]);
        world.set_status(solver, "running");

        let mut since = None;
        for call in 1..=4 {
            let answer = h.evaluate(&world, &goal, Some(0)).await;
            assert_waiting_answer(&answer, &goal, 0, &[solver]);
            let this = answer["waiting_since"].clone();
            assert_eq!(
                since.get_or_insert(this.clone()),
                &this,
                "call {call}: one wait"
            );
        }
        assert_eq!(world.judges_started(), 0, "no judge while the work runs");
        let stored = h.stored(&goal).await;
        assert_eq!((stored.state, stored.rounds), (GoalState::Open, 0));
        let evaluations = h.repo.list_evaluations(goal.id).await.unwrap();
        assert_eq!(evaluations.len(), 1, "one wait row, no verdict");
        assert!(evaluations[0].is_open_wait());
        assert_eq!(
            json!(wait_time_text(evaluations[0].created_at)),
            since.clone().unwrap()
        );

        world.set_status(solver, "failed");
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(world.judges_started(), 1, "judged once when it ended");
        assert_eq!(world.last_input()["executions"][0]["status"], "failed");
        assert_eq!(answer["outcome"], "not_met");
        assert_eq!(answer["continue"], true);
        assert_eq!(h.stored(&goal).await.rounds, 1, "one real round");
        let waits = h.waits(&goal).await;
        assert_eq!(waits.len(), 1);
        assert!(waits[0].decided_at.is_some(), "the wait row is closed");
    }

    /// U21: an execution that failed before the evaluation is judged at once
    /// as failed; its not_met is a real round, and no wait is stored.
    #[tokio::test]
    async fn an_execution_already_failed_is_judged_at_once_and_its_not_met_costs_one_round() {
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        world_failed_then_judged(&h, &goal, solver).await;
    }

    async fn world_failed_then_judged(h: &Harness, goal: &Goal, solver: ExecutionId) {
        let world = World::scripted(vec![says(0.0, 1.0, 0.9)]);
        world.set_status(solver, "failed");
        let answer = h.evaluate(&world, goal, None).await;
        assert_eq!(world.judges_started(), 1);
        assert_eq!(world.last_input()["executions"][0]["status"], "failed");
        assert_eq!(answer["outcome"], "not_met");
        assert_eq!(answer["continue"], true);
        assert_eq!(h.stored(goal).await.rounds, 1);
        assert!(h.waits(goal).await.is_empty(), "no wait");
    }

    /// U23: past its start plus its recorded time limit plus the reaper's
    /// 600 s, a row still `running` is judged as it stands; with no recorded
    /// limit, the node's 1,800 s plus 600 s. No wait outlasts its bound.
    #[tokio::test]
    async fn past_its_bound_a_running_execution_is_judged_on_what_exists() {
        for (timeout, bound) in [(Some(300u64), 900i64), (None, 2_400)] {
            let h = Harness::new();
            let goal = h.goal().await;
            let solver = h.bound(&goal).await[0];
            let world = World::scripted(vec![says(0.0, 1.0, 0.9)]);
            world.set_status(solver, "running");
            if let Some(t) = timeout {
                world.timeouts.lock().unwrap().insert(solver, t);
            }
            let answer = h.evaluate(&world, &goal, None).await;
            assert_eq!(answer["state"], "judging");
            let wait_until = answer["wait_until"].as_str().unwrap().to_string();
            let started = h.repo.list_bound(goal.id).await.unwrap()[0].started_at;
            assert_eq!(
                wait_until,
                wait_time_text(started + chrono::Duration::seconds(bound)),
                "wait_until is the execution's bound"
            );
            assert_eq!(world.judges_started(), 0);
            if timeout.is_none() {
                // 2,400 s is past the goal's lifetime: U25's test.
                continue;
            }

            // Past the bound (the clock runs from before the binding).
            h.advance(bound + 1);
            let answer = h.evaluate(&world, &goal, Some(0)).await;
            assert_eq!(world.judges_started(), 1, "{timeout:?}: judged");
            assert_eq!(world.last_input()["executions"][0]["status"], "running");
            assert_eq!(answer["outcome"], "not_met");
            assert!(h.waits(&goal).await[0].decided_at.is_some());
        }
    }

    /// U21: a pending approval keeps its precedence. A running execution
    /// holding one is judged and answered as D6, U3 and U10 say, with no
    /// wait; and an approval asked during a wait ends the wait the same way.
    #[tokio::test]
    async fn a_pending_approval_on_running_work_is_answered_as_before_with_no_wait() {
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![says(0.4, 0.9, 0.9)]);
        world.set_status(solver, "running");
        world
            .pending
            .lock()
            .unwrap()
            .push((solver, "approval-1".to_string()));
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["waiting_on"], "approval");
        assert_eq!(answer["approval_ids"], json!(["approval-1"]));
        assert_eq!(answer["continue"], false);
        assert_eq!(answer["round"], 0);
        assert_eq!(world.judges_started(), 1);
        assert!(h.waits(&goal).await.is_empty(), "no wait");

        // The work runs with no approval: it waits; then an approval is
        // asked: the wait ends and the approval is answered as before.
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![says(0.4, 0.9, 0.9)]);
        world.set_status(solver, "running");
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["waiting_on"], WAITING_ON_EXECUTION);
        world
            .pending
            .lock()
            .unwrap()
            .push((solver, "approval-2".to_string()));
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["waiting_on"], "approval");
        assert_eq!(answer["approval_ids"], json!(["approval-2"]));
        assert_eq!(world.judges_started(), 1);
        let waits = h.waits(&goal).await;
        assert_eq!(waits.len(), 1);
        assert!(waits[0].decided_at.is_some(), "the wait ended");
        assert_eq!(h.stored(&goal).await.rounds, 0, "no round is decided");
    }

    /// U26: several bound executions give one verdict after all have ended;
    /// one that ended early is not judged alone meanwhile.
    #[tokio::test]
    async fn several_executions_are_judged_once_after_every_one_has_ended() {
        let h = Harness::new();
        let goal = h.goal().await;
        h.service
            .bind(goal.id, ExecutionId::new(), BoundKind::Agent)
            .await
            .unwrap();
        let [creator, solver] = h.bound(&goal).await[..] else {
            panic!("two bound executions");
        };
        let world = World::scripted(vec![says(0.95, 0.9, 0.9)]);
        world.set_status(creator, "completed");
        world.set_status(solver, "running");
        let answer = h.evaluate(&world, &goal, None).await;
        assert_waiting_answer(&answer, &goal, 0, &[solver]);
        assert_eq!(
            world.judges_started(),
            0,
            "the ended one is not judged alone"
        );

        world.set_status(solver, "completed");
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["outcome"], "met");
        assert_eq!(world.judges_started(), 1, "one verdict");
        assert_eq!(
            world.last_input()["executions"].as_array().unwrap().len(),
            2
        );
    }

    /// U22: one call holds while the work runs, reading the executions, and
    /// judges in the same call when they end inside its hold.
    #[tokio::test]
    async fn work_that_ends_inside_the_hold_is_judged_in_the_same_call() {
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![says(0.95, 0.9, 0.9)]);
        world.set_status(solver, "running");
        *world.end_after_reads.lock().unwrap() = Some((3, "completed"));
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["outcome"], "met", "{answer}");
        assert_eq!(world.judges_started(), 1);
        let waits = h.waits(&goal).await;
        assert_eq!(waits.len(), 1, "the wait is recorded");
        assert!(waits[0].decided_at.is_some(), "and ended");
    }

    /// U24: aegis.goal.status lists each wait as `waits: [{round,
    /// execution_ids, began_at, ended_at, wait_until}]` beside `verdicts`,
    /// which keep their form and never list a wait. `began_at` is the
    /// answer's `waiting_since`; `ended_at` is null while it waits.
    #[tokio::test]
    async fn status_lists_each_wait_beside_the_verdicts() {
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![says(0.4, 0.9, 0.9)]);
        world.set_status(solver, "running");
        let waiting = h.evaluate(&world, &goal, None).await;

        let status = h.service.status(&world, &h.caller, goal.id).await.unwrap();
        assert_eq!(status["verdicts"], json!([]), "a wait is no verdict");
        assert_eq!(
            status["waits"],
            json!([{
                "round": 0,
                "execution_ids": [solver.to_string()],
                "began_at": waiting["waiting_since"],
                "ended_at": Value::Null,
                "wait_until": waiting["wait_until"],
            }])
        );

        world.set_status(solver, "completed");
        h.evaluate(&world, &goal, Some(0)).await;
        let status = h.service.status(&world, &h.caller, goal.id).await.unwrap();
        let waits = status["waits"].as_array().unwrap();
        assert_eq!(waits.len(), 1);
        let ended = waits[0]["ended_at"].as_str().unwrap();
        assert!(DateTime::parse_from_rfc3339(ended).is_ok(), "{ended}");
        let verdicts = status["verdicts"].as_array().unwrap();
        assert_eq!(verdicts.len(), 1, "the round's one verdict");
        assert_eq!(verdicts[0]["outcome"], "not_met");
        assert_eq!(verdicts[0]["waiting_on"], Value::Null);

        // A goal that never waited lists no wait.
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.4, 0.9, 0.9)]);
        h.evaluate(&world, &goal, None).await;
        let status = h.service.status(&world, &h.caller, goal.id).await.unwrap();
        assert_eq!(status["waits"], json!([]));
    }

    // ── U25: the lifetime and a wait ──────────────────────────────────────

    /// U25 with U29: a goal whose round waits inside its bound is not closed
    /// expired past 1,800 s of wall time, by an evaluation or by the sweep,
    /// and binds no new work while it waits; the time it waited does not
    /// count against its lifetime, so when the work ends its not_met grants
    /// a round as any other does.
    #[tokio::test]
    async fn a_goal_waiting_past_1800_s_of_wall_time_stays_open_and_its_not_met_grants_a_round() {
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![says(0.0, 1.0, 0.9)]);
        world.set_status(solver, "running");
        h.evaluate(&world, &goal, None).await;

        h.advance(1_801);
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_waiting_answer(&answer, &goal, 0, &[solver]);
        assert_eq!(h.service.close_expired(h.now()).await.unwrap(), 0);
        assert_eq!(h.stored(&goal).await.state, GoalState::Open);
        let err = h
            .service
            .open_goal_for(&h.caller, goal.id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), GOAL_NOT_OPEN, "no new work while it waits");
        assert_eq!(
            h.stored(&goal).await.state,
            GoalState::Open,
            "and not closed"
        );

        world.set_status(solver, "failed");
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(world.judges_started(), 1, "judged once");
        assert_eq!(answer["outcome"], "not_met");
        assert_eq!(answer["state"], "open");
        assert_eq!(answer["continue"], true);
        let stored = h.stored(&goal).await;
        assert_eq!((stored.state, stored.rounds), (GoalState::Open, 1));
        assert!(
            h.service.open_goal_for(&h.caller, goal.id).await.is_ok(),
            "the next round's work is admitted"
        );
    }

    /// U25 with U23 and U29: a wait whose bound passes with the row still
    /// running is judged on what exists, and its not_met grants a round; met
    /// closes met; and when the goal's own time between waits passes its
    /// lifetime while the round's judge runs after a wait, the round is
    /// judged once and grants nothing, so no pending approval holds one.
    #[tokio::test]
    async fn a_wait_ending_after_1800_s_of_wall_time_is_judged_once_on_what_exists() {
        // The bound passes with the row still running: judged, a round.
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![says(0.0, 1.0, 0.9)]);
        world.set_status(solver, "running");
        h.evaluate(&world, &goal, None).await;
        h.advance(2_401);
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(world.judges_started(), 1);
        assert_eq!(world.last_input()["executions"][0]["status"], "running");
        assert_eq!(answer["state"], "open", "{answer}");
        assert_eq!(answer["continue"], true);
        assert_eq!(h.stored(&goal).await.rounds, 1);

        // The work completes after 1,800 s of wall time: met closes met.
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![says(0.95, 0.9, 0.9)]);
        world.set_status(solver, "running");
        h.evaluate(&world, &goal, None).await;
        h.advance(1_900);
        world.set_status(solver, "completed");
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["outcome"], "met");
        assert_eq!(h.stored(&goal).await.state, GoalState::Met);

        // 1,790 s between waits, then a short wait, then 20 s of judging:
        // past the lifetime, the late round grants nothing, and an approval
        // pending when it is judged holds nothing.
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![Judge::Runs]);
        h.advance(1_790);
        world.set_status(solver, "running");
        let answer = h.evaluate(&world, &goal, None).await;
        assert_waiting_answer(&answer, &goal, 0, &[solver]);
        h.advance(5);
        world.set_status(solver, "failed");
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["state"], "judging", "{answer}");
        h.advance(20);
        world
            .pending
            .lock()
            .unwrap()
            .push((solver, "approval-late".to_string()));
        world.finish_running(says(0.4, 0.9, 0.9));
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["state"], "expired", "{answer}");
        assert_eq!(answer["continue"], false);
        assert!(answer.get("waiting_on").is_none());
        assert_eq!(world.judges_started(), 1, "judged once");
    }

    /// The Low row (AEGIS known-defects-7, line 70): the sweep does not close
    /// a goal whose late judge is in flight after a wait; that judge's
    /// verdict decides the round once. A judge in flight is bounded by its
    /// own limit (goal-judge's 300 s, D3) plus the reaper's margin: past it
    /// the sweep closes the goal.
    #[tokio::test]
    async fn the_sweep_does_not_close_a_goal_whose_judge_runs_after_a_wait() {
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![Judge::Runs]);
        h.advance(1_790);
        world.set_status(solver, "running");
        h.evaluate(&world, &goal, None).await;
        world.set_status(solver, "failed");
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["state"], "judging", "{answer}");
        h.advance(20);
        assert_eq!(
            h.service.close_expired(h.now()).await.unwrap(),
            0,
            "the judge after the wait is in flight"
        );
        assert_eq!(h.stored(&goal).await.state, GoalState::Open);
        world.finish_running(says(0.0, 1.0, 0.9));
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["outcome"], "not_met");
        assert_eq!(answer["state"], "expired", "judged once, late: {answer}");
        assert_eq!(world.judges_started(), 1);

        // A judge that never finishes holds the goal only to its bound.
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![Judge::Runs]);
        h.advance(1_790);
        world.set_status(solver, "running");
        h.evaluate(&world, &goal, None).await;
        world.set_status(solver, "failed");
        h.evaluate(&world, &goal, Some(0)).await;
        h.advance(899);
        assert_eq!(h.service.close_expired(h.now()).await.unwrap(), 0);
        h.advance(2);
        assert_eq!(h.service.close_expired(h.now()).await.unwrap(), 1);
        assert_eq!(h.stored(&goal).await.state, GoalState::Expired);
    }

    /// U29 for a goal that never waits: its lifetime is its wall time, as
    /// D7 says. Judged at 900 s, its not_met grants a round; at 1,800 s it
    /// closes expired at its next evaluation and is not judged again.
    #[tokio::test]
    async fn a_goal_that_never_waits_expires_at_1800_s_as_before() {
        let h = Harness::new();
        let goal = h.goal().await;
        let world = World::scripted(vec![says(0.0, 1.0, 0.9)]);
        h.advance(900);
        let answer = h.evaluate(&world, &goal, None).await;
        assert_eq!(answer["continue"], true);
        assert!(h.waits(&goal).await.is_empty(), "it never waited");
        h.advance(899);
        assert!(h.service.open_goal_for(&h.caller, goal.id).await.is_ok());
        assert_eq!(h.service.close_expired(h.now()).await.unwrap(), 0);
        h.advance(1);
        let answer = h.evaluate(&world, &goal, Some(1)).await;
        assert_eq!(answer["state"], "expired");
        assert_eq!(answer["continue"], false);
        assert_eq!(world.judges_started(), 1);
    }

    /// U29 when nobody asks again: a goal whose wait is never ended by an
    /// evaluation (its runner gone) is credited the wait only up to its
    /// `wait_until`, and the sweep closes it once its own time passes the
    /// lifetime: by its wait's bound plus the lifetime.
    #[tokio::test]
    async fn an_abandoned_wait_is_credited_to_its_bound_and_the_sweep_closes_the_goal() {
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![]);
        world.set_status(solver, "running");
        h.evaluate(&world, &goal, None).await;
        h.advance(2_400 + 1_799);
        assert_eq!(h.service.close_expired(h.now()).await.unwrap(), 0);
        h.advance(2);
        assert_eq!(h.service.close_expired(h.now()).await.unwrap(), 1);
        assert_eq!(h.stored(&goal).await.state, GoalState::Expired);
    }

    /// U25: a goal past its lifetime whose current round did not wait closes
    /// expired as D7 says, even when its earlier round waited.
    #[tokio::test]
    async fn a_goal_past_its_lifetime_whose_round_did_not_wait_closes_expired() {
        let h = Harness::new();
        let goal = h.goal().await;
        let solver = h.bound(&goal).await[0];
        let world = World::scripted(vec![says(0.0, 1.0, 0.9)]);
        world.set_status(solver, "running");
        h.evaluate(&world, &goal, None).await;
        world.set_status(solver, "failed");
        let answer = h.evaluate(&world, &goal, Some(0)).await;
        assert_eq!(answer["continue"], true, "round 0 waited, then was judged");
        h.advance(1_801);
        let answer = h.evaluate(&world, &goal, Some(1)).await;
        assert_eq!(answer["state"], "expired");
        assert_eq!(world.judges_started(), 1, "round 1 is not judged");
    }
}
