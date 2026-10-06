// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The tool invocation service's half of goals (AEGIS ADR-131): the tools
//! `aegis.goal.create`, `aegis.goal.evaluate`, `aegis.goal.status` (D2) and
//! `aegis.goal.cancel` (U33),
//! the optional `goal_id` of the four tools that start work (D2, U6), and the
//! [`GoalWorld`] a goal is judged in: the bound executions' rows, their
//! pending approvals, and the `goal-judge` execution, run as the goal's user
//! under the caller's security context (D3, D4).

use super::*;
use crate::application::goal_service::{
    DispatchFact, ExecutionView, GoalCaller, GoalError, GoalService, GoalWorld, JudgeProgress,
    ProducedFileFact, GOAL_JUDGE_AGENT_NAME,
};
use crate::domain::execution::{Execution, ExecutionId, ExecutionStatus};
use crate::domain::goal::{
    execution_wait_bound, BoundExecution, BoundKind, Goal, GoalChannel, GoalId,
};
use crate::domain::iam::{TenantScope, UserIdentity};
use crate::domain::tool_approval::ToolApprovalStatus;

/// The built-in workflow `aegis.execute.intent` starts: a goal reads its
/// executions as kind `intent` (D4).
const INTENT_WORKFLOW_NAME: &str = "builtin-intent-to-execution";
/// The built-in workflow `aegis.workflow.generate` starts.
const WORKFLOW_GENERATOR_NAME: &str = "builtin-workflow-generator";

/// The argument the four starting tools read and never advertise (U6).
pub(super) const GOAL_ID_ARG: &str = "goal_id";

fn refusal(tool: &str, error: &GoalError) -> ToolInvocationResult {
    ToolInvocationResult::Direct(serde_json::json!({
        "tool": tool,
        "error": error.code(),
        "message": error.to_string(),
    }))
}

fn status_name(status: &ExecutionStatus) -> String {
    format!("{status:?}").to_lowercase()
}

impl ToolInvocationService {
    /// AEGIS ADR-131: hold goals and judge them. Without it the three goal
    /// tools answer that goals are not configured, and a `goal_id` on a
    /// starting tool is refused.
    pub fn with_goals(mut self, service: Arc<GoalService>) -> Self {
        self.goal_service = Some(service);
        self
    }

    /// The goal service, when goals are configured: the REST cancel routes
    /// end a cancelled execution's goal through it (U33a).
    pub fn goal_service(&self) -> Option<&Arc<GoalService>> {
        self.goal_service.as_ref()
    }

    fn goals(&self, tool: &str) -> Result<&Arc<GoalService>, SealSessionError> {
        self.goal_service.as_ref().ok_or_else(|| {
            SealSessionError::ConfigurationError(format!("{tool}: goals are not configured"))
        })
    }

    /// A goal belongs to a user: the call's identity and its tenant.
    fn goal_caller(
        tool: &str,
        caller_identity: Option<&UserIdentity>,
        tenant_scope: &TenantScope,
    ) -> Result<GoalCaller, SealSessionError> {
        let identity = caller_identity.ok_or_else(|| {
            SealSessionError::InvalidArguments(format!(
                "{tool}: a goal belongs to a user, and this call has no user identity"
            ))
        })?;
        Ok(GoalCaller {
            tenant_id: tenant_scope.authenticated_tenant.clone(),
            user_sub: identity.sub.clone(),
        })
    }

    fn goal_id_arg(tool: &str, args: &Value) -> Result<Option<GoalId>, SealSessionError> {
        match args.get(GOAL_ID_ARG) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(raw)) => GoalId::from_string(raw).map(Some).map_err(|e| {
                SealSessionError::InvalidArguments(format!("{tool}: invalid goal_id '{raw}': {e}"))
            }),
            Some(_) => Err(SealSessionError::InvalidArguments(format!(
                "{tool}: goal_id must be a string"
            ))),
        }
    }

    // ── The four starting tools (D2, U6) ───────────────────────────────────

    /// The open goal a starting tool's call names, checked before anything
    /// starts. `Ok(Err(refusal))` is the tool's answer `goal_not_open`.
    pub(super) async fn goal_for_start(
        &self,
        tool: &str,
        args: &Value,
        caller_identity: Option<&UserIdentity>,
        tenant_scope: &TenantScope,
    ) -> Result<Result<Option<GoalId>, ToolInvocationResult>, SealSessionError> {
        let Some(goal_id) = Self::goal_id_arg(tool, args)? else {
            return Ok(Ok(None));
        };
        let Some(goals) = self.goal_service.as_ref() else {
            return Ok(Err(refusal(tool, &GoalError::NotOpen)));
        };
        let Some(identity) = caller_identity else {
            return Ok(Err(refusal(tool, &GoalError::NotOpen)));
        };
        let caller = GoalCaller {
            tenant_id: tenant_scope.authenticated_tenant.clone(),
            user_sub: identity.sub.clone(),
        };
        match goals.open_goal_for(&caller, goal_id).await {
            Ok(goal) => Ok(Ok(Some(goal.id))),
            Err(e @ GoalError::Repository(_)) => {
                Err(SealSessionError::InternalError(e.to_string()))
            }
            Err(e) => Ok(Err(refusal(tool, &e))),
        }
    }

    /// Write the goal on the execution just started for it.
    pub(super) async fn bind_to_goal(
        &self,
        goal_id: Option<GoalId>,
        execution_id: &str,
        kind: BoundKind,
    ) {
        let (Some(goal_id), Some(goals)) = (goal_id, self.goal_service.as_ref()) else {
            return;
        };
        let Ok(uuid) = uuid::Uuid::parse_str(execution_id) else {
            tracing::warn!(
                execution_id,
                "A started execution's id is not a UUID; not bound"
            );
            return;
        };
        if let Err(e) = goals.bind(goal_id, ExecutionId(uuid), kind).await {
            tracing::warn!(goal_id = %goal_id, execution_id, error = %e, "Failed to bind an execution to its goal");
        }
    }

    // ── aegis.goal.create, .evaluate, .status (D2, U8) ────────────────────

    pub(super) async fn invoke_aegis_goal_create_tool(
        &self,
        args: &Value,
        caller_identity: Option<&UserIdentity>,
        tenant_scope: &TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        const TOOL: &str = "aegis.goal.create";
        let goals = self.goals(TOOL)?;
        let caller = Self::goal_caller(TOOL, caller_identity, tenant_scope)?;
        let field = |name: &str| {
            args.get(name).and_then(Value::as_str).ok_or_else(|| {
                SealSessionError::InvalidArguments(format!(
                    "{TOOL}: required field '{name}' is missing or not a string"
                ))
            })
        };
        let statement = field("statement")?;
        let client_ref = field("client_ref")?;
        let channel_raw = field("channel")?;
        let channel = GoalChannel::parse(channel_raw).ok_or_else(|| {
            SealSessionError::InvalidArguments(format!(
                "{TOOL}: channel must be \"web\" or \"api\", got \"{channel_raw}\""
            ))
        })?;
        match goals.create(&caller, statement, client_ref, channel).await {
            Ok(goal) => Ok(ToolInvocationResult::Direct(
                serde_json::json!({ "goal_id": goal.id.to_string() }),
            )),
            Err(e @ GoalError::Repository(_)) => {
                Err(SealSessionError::InternalError(e.to_string()))
            }
            Err(e) => Ok(refusal(TOOL, &e)),
        }
    }

    pub(super) async fn invoke_aegis_goal_evaluate_tool(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
        caller_identity: Option<&UserIdentity>,
        tenant_scope: &TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        const TOOL: &str = "aegis.goal.evaluate";
        let goals = self.goals(TOOL)?;
        let caller = Self::goal_caller(TOOL, caller_identity, tenant_scope)?;
        let goal_id = Self::goal_id_arg(TOOL, args)?.ok_or_else(|| {
            SealSessionError::InvalidArguments(format!(
                "{TOOL}: required field 'goal_id' is missing"
            ))
        })?;
        let companion_answer = args
            .get("companion_answer")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let round =
            match args.get("round") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(
                    || {
                        SealSessionError::InvalidArguments(format!(
                            "{TOOL}: round must be a non-negative integer"
                        ))
                    },
                )?),
            };
        let world = GoalCallWorld {
            service: self,
            identity: caller_identity.cloned(),
            security_context_name: security_context.name.clone(),
        };
        match goals
            .evaluate(&world, &caller, goal_id, companion_answer, round)
            .await
        {
            Ok(answer) => Ok(ToolInvocationResult::Direct(answer)),
            Err(e @ GoalError::Repository(_)) => {
                Err(SealSessionError::InternalError(e.to_string()))
            }
            Err(e) => Ok(refusal(TOOL, &e)),
        }
    }

    pub(super) async fn invoke_aegis_goal_status_tool(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
        caller_identity: Option<&UserIdentity>,
        tenant_scope: &TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        const TOOL: &str = "aegis.goal.status";
        let goals = self.goals(TOOL)?;
        let caller = Self::goal_caller(TOOL, caller_identity, tenant_scope)?;
        let goal_id = Self::goal_id_arg(TOOL, args)?.ok_or_else(|| {
            SealSessionError::InvalidArguments(format!(
                "{TOOL}: required field 'goal_id' is missing"
            ))
        })?;
        let world = GoalCallWorld {
            service: self,
            identity: caller_identity.cloned(),
            security_context_name: security_context.name.clone(),
        };
        match goals.status(&world, &caller, goal_id).await {
            Ok(status) => Ok(ToolInvocationResult::Direct(status)),
            Err(e @ GoalError::Repository(_)) => {
                Err(SealSessionError::InternalError(e.to_string()))
            }
            Err(e) => Ok(refusal(TOOL, &e)),
        }
    }

    /// `aegis.goal.cancel` (U33): the person ends the goal. `reason` is
    /// optional; the goal keeps it, or
    /// [`crate::domain::goal::DEFAULT_CANCEL_REASON`].
    pub(super) async fn invoke_aegis_goal_cancel_tool(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
        caller_identity: Option<&UserIdentity>,
        tenant_scope: &TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        const TOOL: &str = "aegis.goal.cancel";
        let goals = self.goals(TOOL)?;
        let caller = Self::goal_caller(TOOL, caller_identity, tenant_scope)?;
        let goal_id = Self::goal_id_arg(TOOL, args)?.ok_or_else(|| {
            SealSessionError::InvalidArguments(format!(
                "{TOOL}: required field 'goal_id' is missing"
            ))
        })?;
        let reason = match args.get("reason") {
            None | Some(Value::Null) => None,
            Some(Value::String(reason)) => Some(reason.as_str()),
            Some(_) => {
                return Err(SealSessionError::InvalidArguments(format!(
                    "{TOOL}: reason must be a string"
                )))
            }
        };
        let world = GoalCallWorld {
            service: self,
            identity: caller_identity.cloned(),
            security_context_name: security_context.name.clone(),
        };
        match goals.cancel(&world, &caller, goal_id, reason).await {
            Ok(answer) => Ok(ToolInvocationResult::Direct(answer)),
            Err(e @ GoalError::Repository(_)) => {
                Err(SealSessionError::InternalError(e.to_string()))
            }
            Err(e) => Ok(refusal(TOOL, &e)),
        }
    }

    /// U32: the facts of the workflow or intent execution `id`, from its
    /// step executions read each on its own (U32a). `None` when it has none,
    /// when no execution store is configured, or when they cannot be listed:
    /// unknown, never zero.
    async fn workflow_step_facts(&self, tenant: &TenantId, id: ExecutionId) -> Option<StepFacts> {
        let Some(executions) = self.execution_repository.as_ref() else {
            tracing::warn!(
                workflow_execution_id = %id,
                "goal judge input: no execution repository configured; a workflow's facts are unknown"
            );
            return None;
        };
        match executions
            .read_steps_of_workflow_execution_for_tenant(tenant, id.0)
            .await
        {
            Ok(steps) => {
                let steps: Vec<Result<Execution, ExecutionId>> = steps
                    .into_iter()
                    .map(|step| {
                        step.map_err(|(step_id, error)| {
                            tracing::warn!(
                                workflow_execution_id = %id,
                                step_execution_id = %step_id,
                                %error,
                                "goal judge input: a step execution could not be read"
                            );
                            step_id
                        })
                    })
                    .collect();
                step_facts(&steps)
            }
            Err(error) => {
                tracing::warn!(
                    workflow_execution_id = %id,
                    %error,
                    "goal judge input: a workflow's step executions could not be listed; its facts are unknown"
                );
                None
            }
        }
    }

    async fn workflow_name(
        &self,
        tenant: &TenantId,
        id: crate::domain::workflow::WorkflowId,
    ) -> String {
        let Some(repo) = self.workflow_repository.as_ref() else {
            return id.0.to_string();
        };
        if let Ok(Some(workflow)) = repo.find_by_id_for_tenant(tenant, id).await {
            return workflow.metadata.name;
        }
        // A built-in workflow is the platform's, not the tenant's: it is
        // found by its name, as the tools that start it find it.
        for name in [INTENT_WORKFLOW_NAME, WORKFLOW_GENERATOR_NAME] {
            if let Ok(Some(workflow)) = repo.resolve_by_name(tenant, name).await {
                if workflow.id == id {
                    return name.to_string();
                }
            }
        }
        id.0.to_string()
    }
}

/// U30: each tool call the execution's tries made, oldest first, from the
/// stored trajectory, and how many of them ran.
fn run_dispatches(exec: &Execution) -> (usize, Vec<DispatchFact>) {
    let dispatches: Vec<DispatchFact> = exec
        .iterations()
        .iter()
        .flat_map(|iteration| {
            iteration
                .trajectory
                .iter()
                .flatten()
                .map(move |step| DispatchFact {
                    execution_id: None,
                    iteration: iteration.number,
                    tool: step.tool_name.clone(),
                    status: step.status.clone(),
                })
        })
        .collect();
    let executed = dispatches.iter().filter(|d| d.executed()).count();
    (executed, dispatches)
}

/// U30: the files the execution produced, as the judge is given them.
fn run_produced_files(exec: &Execution) -> Vec<ProducedFileFact> {
    exec.produced_files()
        .iter()
        .map(|file| ProducedFileFact {
            execution_id: None,
            file: file.clone(),
        })
        .collect()
}

/// The facts of a workflow or intent execution's run (U32): its step
/// executions' facts.
#[derive(Debug, Default, PartialEq)]
struct StepFacts {
    tool_calls_executed: usize,
    dispatches: Vec<DispatchFact>,
    produced_files: Vec<ProducedFileFact>,
    steps_unread: usize,
}

/// U32: fold a workflow or intent execution's step executions, as read in
/// their start order, into its facts. A step that ended (completed, failed
/// or cancelled) adds its executed calls to the count and its dispatches and
/// files to the lists, each naming the step (U32b, U32d); one still pending
/// or running adds nothing, so the count and the lists agree. A step whose
/// record could not be read leaves the lists as they are and is counted in
/// `steps_unread` (U32a, U32c). `None` when there are no steps: unknown,
/// never zero.
fn step_facts(steps: &[Result<Execution, ExecutionId>]) -> Option<StepFacts> {
    if steps.is_empty() {
        return None;
    }
    let mut facts = StepFacts::default();
    for step in steps {
        let Ok(step) = step else {
            facts.steps_unread += 1;
            continue;
        };
        if !matches!(
            step.status,
            ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled
        ) {
            continue;
        }
        let (executed, dispatches) = run_dispatches(step);
        facts.tool_calls_executed += executed;
        facts
            .dispatches
            .extend(dispatches.into_iter().map(|dispatch| DispatchFact {
                execution_id: Some(step.id),
                ..dispatch
            }));
        facts
            .produced_files
            .extend(
                run_produced_files(step)
                    .into_iter()
                    .map(|file| ProducedFileFact {
                        execution_id: Some(step.id),
                        ..file
                    }),
            );
    }
    Some(facts)
}

/// One goal call's view of the orchestrator, as the goal's user.
struct GoalCallWorld<'a> {
    service: &'a ToolInvocationService,
    identity: Option<UserIdentity>,
    security_context_name: String,
}

#[async_trait::async_trait]
impl GoalWorld for GoalCallWorld<'_> {
    async fn read_execution(&self, goal: &Goal, bound: &BoundExecution) -> Option<ExecutionView> {
        let tenant = &goal.tenant_id;
        match bound.kind {
            BoundKind::Agent => {
                let exec = self
                    .service
                    .execution_service
                    .get_execution_for_tenant(tenant, bound.execution_id)
                    .await
                    .ok()?;
                let name = self
                    .service
                    .agent_lifecycle
                    .get_agent_visible(tenant, exec.agent_id)
                    .await
                    .map(|a| a.manifest.metadata.name)
                    .unwrap_or_else(|_| exec.agent_id.0.to_string());
                let last = exec.iterations().last();
                // U30: what the orchestrator recorded it doing, beside its words.
                let (tool_calls_executed, dispatches) = run_dispatches(&exec);
                Some(ExecutionView {
                    execution_id: exec.id,
                    kind: "agent",
                    agent_or_workflow: name,
                    status: status_name(&exec.status),
                    started_at: exec.started_at,
                    ended_at: exec.ended_at,
                    iterations: Some(exec.iterations().len()),
                    tool_calls_executed: Some(tool_calls_executed),
                    dispatches: Some(dispatches),
                    produced_files: Some(run_produced_files(&exec)),
                    steps_unread: None,
                    last_output: last.and_then(|i| i.output.clone()),
                    last_error: last
                        .and_then(|i| i.error.as_ref().map(|e| format!("{e:?}")))
                        .or_else(|| exec.error.clone()),
                    // U23: its recorded time limit, or the node's default.
                    bound_until: execution_wait_bound(exec.started_at, exec.timeout_seconds),
                })
            }
            BoundKind::Workflow => {
                let repo = self.service.workflow_execution_repo.as_ref()?;
                let wf = repo
                    .find_by_id_for_tenant(tenant, bound.execution_id)
                    .await
                    .ok()??;
                let name = self.service.workflow_name(tenant, wf.workflow_id).await;
                let kind = if name == INTENT_WORKFLOW_NAME {
                    "intent"
                } else {
                    "workflow"
                };
                let output = wf
                    .final_output
                    .clone()
                    .or_else(|| wf.blackboard.data().get("final_result").cloned())
                    .map(|v| match v {
                        Value::String(s) => s,
                        other => other.to_string(),
                    });
                let error = wf.blackboard.data().get("failure_reason").map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
                let terminal = matches!(
                    wf.status,
                    ExecutionStatus::Completed
                        | ExecutionStatus::Failed
                        | ExecutionStatus::Cancelled
                );
                // U32: what its step executions were recorded doing.
                let facts = self.service.workflow_step_facts(tenant, wf.id).await;
                Some(ExecutionView {
                    execution_id: wf.id,
                    kind,
                    agent_or_workflow: name,
                    status: status_name(&wf.status),
                    started_at: wf.started_at,
                    ended_at: terminal.then_some(wf.last_transition_at),
                    iterations: None,
                    tool_calls_executed: facts.as_ref().map(|f| f.tool_calls_executed),
                    steps_unread: facts.as_ref().map(|f| f.steps_unread),
                    dispatches: facts.as_ref().map(|f| f.dispatches.clone()),
                    produced_files: facts.map(|f| f.produced_files),
                    last_output: output,
                    last_error: error,
                    // U23: a workflow or intent execution records no time
                    // limit of its own; it takes the node's default.
                    bound_until: execution_wait_bound(wf.started_at, None),
                })
            }
        }
    }

    async fn pending_approvals(
        &self,
        goal: &Goal,
        execution_ids: &[ExecutionId],
    ) -> Vec<(ExecutionId, String)> {
        let Some(approvals) = self.service.tool_approval_service.as_ref() else {
            return Vec::new();
        };
        match approvals
            .list_for_user(
                &goal.tenant_id,
                &goal.user_sub,
                Some(ToolApprovalStatus::Pending),
            )
            .await
        {
            Ok(requests) => requests
                .into_iter()
                .filter(|r| execution_ids.contains(&r.execution_id))
                .map(|r| (r.execution_id, r.id.to_string()))
                .collect(),
            Err(e) => {
                tracing::warn!(goal_id = %goal.id, error = %e, "Failed to read a goal's pending approvals");
                Vec::new()
            }
        }
    }

    async fn start_judge(&self, goal: &Goal, mut input: Value) -> Result<ExecutionId, String> {
        let agent_id = self
            .service
            .agent_lifecycle
            .lookup_agent_visible_for_tenant(&goal.tenant_id, GOAL_JUDGE_AGENT_NAME)
            .await
            .map_err(|e| format!("{GOAL_JUDGE_AGENT_NAME} lookup failed: {e}"))?
            .ok_or_else(|| {
                format!("the built-in agent '{GOAL_JUDGE_AGENT_NAME}' is not deployed")
            })?;
        if let Some(map) = input.as_object_mut() {
            map.entry("tenant_id")
                .or_insert_with(|| Value::String(goal.tenant_id.to_string()));
        }
        self.service
            .execution_service
            .start_execution(
                agent_id,
                ExecutionInput {
                    intent: None,
                    input,
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: Vec::new(),
                },
                self.security_context_name.clone(),
                self.identity.as_ref(),
            )
            .await
            .map_err(|e| format!("{GOAL_JUDGE_AGENT_NAME} could not be started: {e}"))
    }

    async fn judge_progress(&self, goal: &Goal, judge_execution_id: ExecutionId) -> JudgeProgress {
        let exec = match self
            .service
            .execution_service
            .get_execution_for_tenant(&goal.tenant_id, judge_execution_id)
            .await
        {
            Ok(exec) => exec,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to read the goal-judge execution; still waiting");
                return JudgeProgress::Running;
            }
        };
        match exec.status {
            ExecutionStatus::Completed => JudgeProgress::Completed(
                exec.iterations()
                    .last()
                    .and_then(|i| i.output.clone())
                    .unwrap_or_default(),
            ),
            ExecutionStatus::Failed | ExecutionStatus::Cancelled => JudgeProgress::Ended(
                exec.error
                    .clone()
                    .unwrap_or_else(|| status_name(&exec.status)),
            ),
            _ => JudgeProgress::Running,
        }
    }

    /// U33, U33a: an agent execution (bound, or the round's judge) is
    /// cancelled as `aegis.task.cancel` cancels one, a workflow or intent
    /// execution as `aegis.workflow.cancel` cancels one, in the goal's
    /// tenant.
    async fn cancel_execution(&self, goal: &Goal, bound: &BoundExecution) -> Result<(), String> {
        match bound.kind {
            BoundKind::Agent => self
                .service
                .execution_service
                .cancel_execution_for_tenant(&goal.tenant_id, bound.execution_id)
                .await
                .map_err(|e| e.to_string()),
            BoundKind::Workflow => {
                let port = self
                    .service
                    .workflow_execution_control
                    .as_ref()
                    .ok_or_else(|| "workflow execution control is not configured".to_string())?;
                port.cancel_workflow_execution(&goal.tenant_id, bound.execution_id)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod run_facts_tests {
    use super::*;
    use crate::domain::agent::AgentId;
    use crate::domain::execution::{ExecutionInput, IterationError, TrajectoryStep};

    fn step(tool: &str, status: &str) -> TrajectoryStep {
        TrajectoryStep {
            tool_name: tool.to_string(),
            arguments_json: "{}".to_string(),
            status: status.to_string(),
            result_json: None,
            error: None,
        }
    }

    /// AEGIS ADR-131 U30: an agent execution's dispatches are read from
    /// every try's stored trajectory, oldest first, each with its try, its
    /// tool and its status verbatim; a call refused before it ran or still
    /// pending is listed but not counted as executed.
    #[test]
    fn an_agent_executions_dispatches_are_read_from_every_tries_trajectory() {
        let input = ExecutionInput {
            intent: None,
            input: serde_json::json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        };
        let mut exec = Execution::new_with_id(
            ExecutionId::new(),
            AgentId::new(),
            input,
            5,
            "zaru-free".into(),
        );
        exec.start();
        exec.start_iteration("try 1".to_string()).unwrap();
        exec.store_iteration_trajectory(
            1,
            vec![step("fs.write", "succeeded"), step("cmd.run", "failed")],
        )
        .unwrap();
        exec.fail_iteration(IterationError {
            message: "declared output /workspace/x.pdf does not exist".to_string(),
            details: None,
        });
        exec.start_iteration("try 2".to_string()).unwrap();
        exec.store_iteration_trajectory(
            2,
            vec![
                step("cmd.run", "refused"),
                step("cmd.run", "fatal"),
                step("fs.read", "pending"),
                step("cmd.run", "dispatched"),
            ],
        )
        .unwrap();
        exec.complete_iteration("/workspace/x.pdf".to_string());
        exec.complete();

        let (executed, dispatches) = run_dispatches(&exec);
        let read: Vec<(u8, &str, &str)> = dispatches
            .iter()
            .map(|d| (d.iteration, d.tool.as_str(), d.status.as_str()))
            .collect();
        let mut complaints = Vec::new();
        let expected = vec![
            (1, "fs.write", "succeeded"),
            (1, "cmd.run", "failed"),
            (2, "cmd.run", "refused"),
            (2, "cmd.run", "fatal"),
            (2, "fs.read", "pending"),
            (2, "cmd.run", "dispatched"),
        ];
        if read != expected {
            complaints.push(format!(
                "the dispatches read are {read:?}, not every try's calls in order {expected:?}"
            ));
        }
        if executed != 4 {
            complaints.push(format!(
                "{executed} calls counted as executed, not 4 (refused and pending did not run)"
            ));
        }
        assert!(complaints.is_empty(), "U30: {complaints:#?}");
    }
}

/// AEGIS ADR-131 U32 and U32a to U32d, through the real types the daemon
/// uses: the goal service's `judge_input` over the world a goal is judged in
/// (`GoalCallWorld`), reading a workflow execution from the workflow
/// execution store and its step executions from the execution store.
#[cfg(test)]
mod step_facts_tests {
    use super::*;
    use crate::domain::agent::AgentId;
    use crate::domain::events::ExecutionEvent;
    use crate::domain::execution::{ExecutionInput, Iteration, ProducedFile, TrajectoryStep};
    use crate::domain::goal::AliasTableJudgeContext;
    use crate::domain::node_config::GoalsConfig;
    use crate::domain::repository::{
        ExecutionRepository, RepositoryError, WorkflowExecutionRepository,
    };
    use crate::domain::workflow::{Blackboard, StateName, WorkflowExecution, WorkflowId};
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
    use crate::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
        InMemoryWorkflowExecutionRepository,
    };
    use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
    use crate::infrastructure::storage::LocalHostStorageProvider;
    use chrono::{DateTime, Utc};
    use futures::Stream;
    use serde_json::json;
    use std::collections::HashMap;
    use std::pin::Pin;

    const USER: &str = "user-a";

    fn tenant() -> TenantId {
        TenantId::for_consumer_user(USER).unwrap()
    }

    /// The execution service is not read for a workflow execution.
    struct NoExecutions;

    #[async_trait::async_trait]
    impl ExecutionService for NoExecutions {
        async fn start_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&UserIdentity>,
        ) -> anyhow::Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn start_execution_with_id(
            &self,
            _: ExecutionId,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&UserIdentity>,
        ) -> anyhow::Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn start_child_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: ExecutionId,
        ) -> anyhow::Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn get_execution_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<Execution> {
            anyhow::bail!("not exercised")
        }
        async fn get_execution_unscoped(&self, _: ExecutionId) -> anyhow::Result<Execution> {
            anyhow::bail!("not exercised")
        }
        async fn get_iterations_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<Vec<Iteration>> {
            anyhow::bail!("not exercised")
        }
        async fn cancel_execution_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn stream_execution(
            &self,
            _: ExecutionId,
        ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>>
        {
            anyhow::bail!("not exercised")
        }
        async fn stream_agent_events(
            &self,
            _: AgentId,
        ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<DomainEvent>> + Send>>>
        {
            anyhow::bail!("not exercised")
        }
        async fn list_executions_for_tenant(
            &self,
            _: &TenantId,
            _: Option<AgentId>,
            _: Option<WorkflowId>,
            _: usize,
        ) -> anyhow::Result<Vec<Execution>> {
            anyhow::bail!("not exercised")
        }
        async fn delete_execution_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn record_llm_interaction(
            &self,
            _: ExecutionId,
            _: u8,
            _: crate::domain::execution::LlmInteraction,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn store_iteration_trajectory(
            &self,
            _: ExecutionId,
            _: u8,
            _: Vec<TrajectoryStep>,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
    }

    struct NoOpPublisher;

    #[async_trait::async_trait]
    impl crate::domain::fsal::EventPublisher for NoOpPublisher {
        async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
    }

    /// A step store whose row for `unread` cannot be read (U32a): it answers
    /// that step as its id with the error, the others whole.
    struct OneUnreadable {
        steps: Vec<Execution>,
        unread: ExecutionId,
    }

    fn not_exercised<T>() -> Result<T, RepositoryError> {
        Err(RepositoryError::Unknown("not exercised".to_string()))
    }

    #[async_trait::async_trait]
    impl ExecutionRepository for OneUnreadable {
        async fn save_for_tenant(
            &self,
            _: &TenantId,
            _: &Execution,
        ) -> Result<(), RepositoryError> {
            not_exercised()
        }
        async fn find_by_id_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> Result<Option<Execution>, RepositoryError> {
            not_exercised()
        }
        async fn find_by_agent_for_tenant(
            &self,
            _: &TenantId,
            _: AgentId,
            _: usize,
        ) -> Result<Vec<Execution>, RepositoryError> {
            not_exercised()
        }
        async fn find_by_workflow_for_tenant(
            &self,
            _: &TenantId,
            _: WorkflowId,
            _: usize,
        ) -> Result<Vec<Execution>, RepositoryError> {
            not_exercised()
        }
        async fn find_by_workflow_execution_for_tenant(
            &self,
            _: &TenantId,
            _: uuid::Uuid,
        ) -> Result<Vec<Execution>, RepositoryError> {
            Err(RepositoryError::Serialization(format!(
                "Failed to deserialize iterations of {}",
                self.unread
            )))
        }
        async fn read_steps_of_workflow_execution_for_tenant(
            &self,
            _: &TenantId,
            _: uuid::Uuid,
        ) -> Result<Vec<Result<Execution, (ExecutionId, RepositoryError)>>, RepositoryError>
        {
            Ok(self
                .steps
                .iter()
                .map(|step| {
                    if step.id == self.unread {
                        Err((
                            step.id,
                            RepositoryError::Serialization(
                                "Failed to deserialize iterations".to_string(),
                            ),
                        ))
                    } else {
                        Ok(step.clone())
                    }
                })
                .collect())
        }
        async fn find_recent_for_tenant(
            &self,
            _: &TenantId,
            _: usize,
        ) -> Result<Vec<Execution>, RepositoryError> {
            not_exercised()
        }
        async fn list_recent_all_paginated(
            &self,
            _: usize,
            _: usize,
        ) -> Result<Vec<Execution>, RepositoryError> {
            not_exercised()
        }
        async fn delete_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> Result<(), RepositoryError> {
            not_exercised()
        }
        async fn count_by_agent_for_tenant(
            &self,
            _: &TenantId,
            _: AgentId,
        ) -> Result<i64, RepositoryError> {
            not_exercised()
        }
        async fn find_by_id_unscoped(
            &self,
            _: ExecutionId,
        ) -> Result<Option<Execution>, RepositoryError> {
            not_exercised()
        }
        async fn count_running(&self, _: &TenantId) -> Result<u64, RepositoryError> {
            not_exercised()
        }
    }

    /// A workflow execution of the user's, ended `completed`.
    fn workflow_execution() -> WorkflowExecution {
        let now = Utc::now();
        WorkflowExecution {
            id: ExecutionId::new(),
            workflow_id: WorkflowId::from_uuid(uuid::Uuid::new_v4()),
            tenant_id: tenant(),
            status: ExecutionStatus::Completed,
            current_state: StateName::new("DONE").unwrap(),
            blackboard: Blackboard::new(),
            input: json!({}),
            state_outputs: HashMap::new(),
            final_output: Some(json!("The report is at /workspace/report.pdf.")),
            started_at: now,
            last_transition_at: now,
            initiating_user_sub: Some(USER.to_string()),
        }
    }

    /// A step execution of `workflow`, started `offset` seconds after
    /// `base`, whose one try made `calls` (tool, status) and left `files`,
    /// ended with `status`.
    fn step(
        workflow: &WorkflowExecution,
        base: DateTime<Utc>,
        offset: i64,
        calls: &[(&str, &str)],
        files: &[&str],
        status: ExecutionStatus,
    ) -> Execution {
        let input = ExecutionInput {
            intent: None,
            input: json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: Some(workflow.id.0),
            attachments: Vec::new(),
        };
        let mut exec = Execution::new_with_id(
            ExecutionId::new(),
            AgentId::new(),
            input,
            5,
            "zaru-free".into(),
        );
        exec.tenant_id = tenant();
        exec.start();
        exec.started_at = base + chrono::Duration::seconds(offset);
        exec.start_iteration("the step's try".to_string()).unwrap();
        exec.store_iteration_trajectory(
            1,
            calls
                .iter()
                .map(|(tool, status)| TrajectoryStep {
                    tool_name: tool.to_string(),
                    arguments_json: "{}".to_string(),
                    status: status.to_string(),
                    result_json: None,
                    error: None,
                })
                .collect(),
        )
        .unwrap();
        exec.store_produced_files(
            1,
            files
                .iter()
                .map(|path| ProducedFile {
                    path: path.to_string(),
                    size_bytes: 21,
                    content_type: "application/pdf".to_string(),
                    volume_id: None,
                    path_in_volume: None,
                })
                .collect(),
        )
        .unwrap();
        exec.complete_iteration("step done".to_string());
        match status {
            ExecutionStatus::Completed => exec.complete(),
            ExecutionStatus::Failed => exec.fail("the step failed".to_string()),
            other => exec.status = other,
        }
        exec
    }

    /// The goal service and the world, over `executions` as the step store,
    /// with `workflow` stored and bound to a new goal; `room` sets the
    /// judge alias's `(context_window, max_output_tokens)`.
    async fn judge_input_for(
        workflow: &WorkflowExecution,
        executions: Arc<dyn ExecutionRepository>,
        room: Option<(u32, u32)>,
    ) -> Value {
        let router = Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers()));
        let storage_root =
            std::env::temp_dir().join(format!("aegis-step-facts-{}", uuid::Uuid::new_v4()));
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
            Arc::new(InMemoryVolumeRepository::new()),
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoOpPublisher),
        ));
        let event_bus = Arc::new(EventBus::new(16));
        let workflows = Arc::new(InMemoryWorkflowExecutionRepository::new());
        workflows
            .save_for_tenant(&tenant(), workflow)
            .await
            .unwrap();
        let service = ToolInvocationService::new(
            Arc::new(InMemorySealSessionRepository::new()),
            Arc::new(
                crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
            ),
            Arc::new(SealMiddleware::new()),
            router,
            fsal,
            NfsVolumeRegistry::new(),
            Arc::new(InMemoryAgentRepository::new()),
            Arc::new(NoExecutions),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
            event_bus.clone(),
            None,
        )
        .with_workflow_execution_repo(workflows)
        .with_execution_repository(executions);
        let mut goals = GoalService::new(
            Arc::new(InMemoryGoalRepository::new()),
            event_bus,
            GoalsConfig::default(),
        );
        if let Some((context_window, max_output_tokens)) = room {
            let providers: Vec<crate::domain::node_config::LLMProviderConfig> =
                serde_yaml::from_str(&format!(
                    "- name: workers-ai\n  type: openai-compatible\n  endpoint: https://example.invalid/v1\n  \
                     models:\n    - alias: judge\n      model: gemma\n      capabilities: [chat]\n      \
                     context_window: {context_window}\n      max_output_tokens: {max_output_tokens}\n"
                ))
                .unwrap();
            goals = goals
                .with_judge_context(Arc::new(AliasTableJudgeContext::from_providers(&providers)));
        }
        let caller = GoalCaller {
            tenant_id: tenant(),
            user_sub: USER.to_string(),
        };
        let goal = goals
            .create(
                &caller,
                "Write the quarterly report as a PDF.",
                "conversation-1",
                GoalChannel::Web,
            )
            .await
            .unwrap();
        goals
            .bind(goal.id, workflow.id, BoundKind::Workflow)
            .await
            .unwrap();
        let world = GoalCallWorld {
            service: &service,
            identity: None,
            security_context_name: "zaru-free".to_string(),
        };
        goals
            .judge_input(&world, &goal, "The report is at /workspace/report.pdf.")
            .await
            .unwrap()
    }

    async fn stored(steps: &[Execution]) -> Arc<dyn ExecutionRepository> {
        let store = Arc::new(InMemoryExecutionRepository::new());
        // Stored latest first: the order read is the steps' start order.
        for step in steps.iter().rev() {
            store.save_for_tenant(&tenant(), step).await.unwrap();
        }
        store
    }

    /// U32, U32b, U32d: a workflow with two completed steps (one with a
    /// produced file and two dispatches, one with none) and one failed step
    /// is judged on its steps' facts: the count summed, the lists
    /// concatenated in start order, each entry naming its step, none unread.
    #[tokio::test]
    async fn a_workflows_judge_input_carries_its_steps_facts_summed_and_in_start_order() {
        let workflow = workflow_execution();
        let base = workflow.started_at;
        let writer = step(
            &workflow,
            base,
            1,
            &[("fs.write", "succeeded"), ("cmd.run", "succeeded")],
            &["/workspace/report.pdf"],
            ExecutionStatus::Completed,
        );
        let quiet = step(&workflow, base, 2, &[], &[], ExecutionStatus::Completed);
        let failed = step(
            &workflow,
            base,
            3,
            &[("cmd.run", "failed")],
            &[],
            ExecutionStatus::Failed,
        );
        let input = judge_input_for(
            &workflow,
            stored(&[writer.clone(), quiet.clone(), failed.clone()]).await,
            None,
        )
        .await;
        let run = &input["executions"][0];
        println!("U32 goal-judge input, a workflow of three steps: {run}");
        let mut complaints = Vec::new();
        if run["tool_calls_executed"] != json!(3) {
            complaints.push(format!(
                "tool_calls_executed is {}, not 3 (2 + 0 + 1 over the steps)",
                run["tool_calls_executed"]
            ));
        }
        let dispatches = json!([
            {"execution_id": writer.id.to_string(), "iteration": 1, "tool": "fs.write", "status": "succeeded"},
            {"execution_id": writer.id.to_string(), "iteration": 1, "tool": "cmd.run", "status": "succeeded"},
            {"execution_id": failed.id.to_string(), "iteration": 1, "tool": "cmd.run", "status": "failed"},
        ]);
        if run["dispatches"] != dispatches {
            complaints.push(format!(
                "dispatches are {}, not the steps' in start order, each naming its step: {dispatches}",
                run["dispatches"]
            ));
        }
        let produced = json!([
            {"execution_id": writer.id.to_string(), "path": "/workspace/report.pdf", "size_bytes": 21, "content_type": "application/pdf"},
        ]);
        if run["produced_files"] != produced {
            complaints.push(format!(
                "produced_files are {}, not the steps' naming their step: {produced}",
                run["produced_files"]
            ));
        }
        if run["steps_unread"] != json!(0) {
            complaints.push(format!("steps_unread is {}, not 0", run["steps_unread"]));
        }
        assert!(complaints.is_empty(), "U32: {complaints:#?}\n{run}");
    }

    /// U32, U32a: a step whose row cannot be read leaves the lists as the
    /// other steps make them and is counted in `steps_unread`.
    #[tokio::test]
    async fn an_unreadable_step_is_counted_in_steps_unread_and_the_others_facts_stand() {
        let workflow = workflow_execution();
        let base = workflow.started_at;
        let writer = step(
            &workflow,
            base,
            1,
            &[("fs.write", "succeeded"), ("cmd.run", "succeeded")],
            &["/workspace/report.pdf"],
            ExecutionStatus::Completed,
        );
        let unread = step(
            &workflow,
            base,
            2,
            &[("cmd.run", "succeeded")],
            &[],
            ExecutionStatus::Completed,
        );
        let failed = step(
            &workflow,
            base,
            3,
            &[("cmd.run", "failed")],
            &[],
            ExecutionStatus::Failed,
        );
        let store = Arc::new(OneUnreadable {
            steps: vec![writer.clone(), unread.clone(), failed.clone()],
            unread: unread.id,
        });
        let input = judge_input_for(&workflow, store, None).await;
        let run = &input["executions"][0];
        println!("U32a goal-judge input, one step unreadable: {run}");
        let mut complaints = Vec::new();
        if run["steps_unread"] != json!(1) {
            complaints.push(format!("steps_unread is {}, not 1", run["steps_unread"]));
        }
        if run["tool_calls_executed"] != json!(3) {
            complaints.push(format!(
                "tool_calls_executed is {}, not 3 (the two steps read)",
                run["tool_calls_executed"]
            ));
        }
        let named: Vec<&str> = run["dispatches"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["execution_id"].as_str())
            .collect();
        let expected = [
            writer.id.to_string(),
            writer.id.to_string(),
            failed.id.to_string(),
        ];
        if named != expected {
            complaints.push(format!(
                "the dispatches name {named:?}, not the steps read in start order {expected:?}"
            ));
        }
        if run["produced_files"][0]["execution_id"] != json!(writer.id.to_string()) {
            complaints.push(format!(
                "produced_files are {}, not the writer's file",
                run["produced_files"]
            ));
        }
        assert!(complaints.is_empty(), "U32a: {complaints:#?}\n{run}");
    }

    /// U32: a workflow with no step executions keeps its facts null:
    /// unknown, never zero.
    #[tokio::test]
    async fn a_workflow_with_no_step_executions_keeps_null_facts() {
        let workflow = workflow_execution();
        let input = judge_input_for(&workflow, stored(&[]).await, None).await;
        let run = &input["executions"][0];
        println!("U32 goal-judge input, a workflow with no step executions: {run}");
        let mut complaints = Vec::new();
        for field in [
            "tool_calls_executed",
            "dispatches",
            "produced_files",
            "steps_unread",
        ] {
            if !run[field].is_null() {
                complaints.push(format!("{field} is {}, not null (unknown)", run[field]));
            }
        }
        assert!(complaints.is_empty(), "U32: {complaints:#?}\n{run}");
    }

    /// U32b: a step that has not ended adds nothing to the count or the
    /// lists, so the two agree.
    #[tokio::test]
    async fn a_step_still_running_adds_nothing_to_the_count_or_the_lists() {
        let workflow = workflow_execution();
        let base = workflow.started_at;
        let done = step(
            &workflow,
            base,
            1,
            &[("fs.write", "succeeded")],
            &[],
            ExecutionStatus::Completed,
        );
        let running = step(
            &workflow,
            base,
            2,
            &[("cmd.run", "succeeded")],
            &[],
            ExecutionStatus::Running,
        );
        let input = judge_input_for(&workflow, stored(&[done.clone(), running]).await, None).await;
        let run = &input["executions"][0];
        println!("U32b goal-judge input, one step still running: {run}");
        let mut complaints = Vec::new();
        if run["tool_calls_executed"] != json!(1) {
            complaints.push(format!(
                "tool_calls_executed is {}, not 1 (the ended step's)",
                run["tool_calls_executed"]
            ));
        }
        if run["dispatches"]
            != json!([{"execution_id": done.id.to_string(), "iteration": 1, "tool": "fs.write", "status": "succeeded"}])
        {
            complaints.push(format!(
                "dispatches are {}, not the ended step's only",
                run["dispatches"]
            ));
        }
        assert!(complaints.is_empty(), "U32b: {complaints:#?}\n{run}");
    }

    /// U32 under U16a (J2): a workflow's lists, whose entries name several
    /// step executions, cut to the judge's room keep whole entries in start
    /// order, each naming its step, and carry `*_omitted`; the count stays
    /// whole.
    #[tokio::test]
    async fn a_workflows_lists_cut_to_the_room_keep_whole_entries_naming_their_steps() {
        let workflow = workflow_execution();
        let base = workflow.started_at;
        let calls: Vec<(&str, &str)> = (0..50)
            .map(|i| {
                if i % 2 == 0 {
                    ("fs.write", "succeeded")
                } else {
                    ("cmd.run", "succeeded")
                }
            })
            .collect();
        let steps: Vec<Execution> = (0..6)
            .map(|i| {
                step(
                    &workflow,
                    base,
                    i + 1,
                    &calls,
                    &["/workspace/report.pdf"],
                    ExecutionStatus::Completed,
                )
            })
            .collect();
        // 30,000 and 4,000: a prompt limit of 26,000 bytes, 17,808 for the
        // input beside goal-judge's reserve.
        let input = judge_input_for(&workflow, stored(&steps).await, Some((30_000, 4_000))).await;
        let size = serde_json::to_string(&input).unwrap().len();
        let run = &input["executions"][0];
        let kept: Vec<Value> = run["dispatches"].as_array().cloned().unwrap_or_default();
        println!(
            "U32 under U16a: input {size} bytes against a room of 17808; tool_calls_executed {}, \
             produced_files {}, dispatches kept {}, dispatches_omitted {}, first kept {}, last kept {}",
            run["tool_calls_executed"],
            run["produced_files"],
            kept.len(),
            run["dispatches_omitted"],
            kept.first().cloned().unwrap_or(Value::Null),
            kept.last().cloned().unwrap_or(Value::Null),
        );
        let mut complaints = Vec::new();
        if size > 17_808 {
            complaints.push(format!("the input is {size} bytes, over the room"));
        }
        if run["tool_calls_executed"] != json!(300) {
            complaints.push(format!(
                "tool_calls_executed is {}, not the whole count 300",
                run["tool_calls_executed"]
            ));
        }
        if run["produced_files"].as_array().map(Vec::len) != Some(6) {
            complaints.push(format!(
                "produced_files are {}, not the six whole",
                run["produced_files"]
            ));
        }
        if kept.is_empty() || kept.len() >= 300 {
            complaints.push(format!("{} dispatches kept, not a cut prefix", kept.len()));
        }
        let whole: Vec<Value> = steps
            .iter()
            .flat_map(|s| {
                calls.iter().map(move |(tool, status)| {
                    json!({"execution_id": s.id.to_string(), "iteration": 1, "tool": tool, "status": status})
                })
            })
            .collect();
        if kept[..] != whole[..kept.len().min(whole.len())] {
            complaints.push(
                "the dispatches kept are not the first ones, whole, each naming its step"
                    .to_string(),
            );
        }
        let named: std::collections::BTreeSet<&str> = kept
            .iter()
            .filter_map(|d| d["execution_id"].as_str())
            .collect();
        if named.len() < 2 {
            complaints.push(format!(
                "the dispatches kept name {} step(s), not several",
                named.len()
            ));
        }
        if run["dispatches_omitted"] != json!(300 - kept.len()) {
            complaints.push(format!(
                "dispatches_omitted is {}, not {}",
                run["dispatches_omitted"],
                300 - kept.len()
            ));
        }
        assert!(complaints.is_empty(), "U32 under U16a: {complaints:#?}");
    }
}

/// AEGIS ADR-131 U33 and U33a in the tool path, through the real handlers of
/// a `ToolInvocationService` with a real `GoalService` over the in-memory
/// store and the real `GoalCallWorld`: `aegis.goal.cancel` ends the goal and
/// cancels its work by kind; `aegis.task.cancel` and `aegis.workflow.cancel`
/// of a bound execution close its goal with the reason before they answer,
/// and a following starting call is refused `goal_not_open`.
#[cfg(test)]
mod cancel_path_tests {
    use super::*;
    use crate::application::agent::AgentLifecycleService;
    use crate::application::ports::WorkflowExecutionControlPort;
    use crate::domain::agent::{Agent, AgentId, AgentManifest};
    use crate::domain::events::ExecutionEvent;
    use crate::domain::execution::{ExecutionInput, Iteration};
    use crate::domain::goal::{GoalRepository, GoalState};
    use crate::domain::iam::{IdentityKind, ZaruTier};
    use crate::domain::node_config::GoalsConfig;
    use crate::domain::repository::AgentVersion;
    use crate::domain::security_context::SecurityContext;
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
    use crate::infrastructure::repositories::InMemoryVolumeRepository;
    use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
    use crate::infrastructure::storage::LocalHostStorageProvider;
    use crate::infrastructure::tool_router::ToolRouter;
    use futures::Stream;
    use serde_json::json;
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;

    const OWNER: &str = "1a2b0000-owner";

    fn identity(sub: &str) -> UserIdentity {
        UserIdentity {
            sub: sub.to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::for_consumer_user(sub).unwrap(),
            },
        }
    }

    fn scope(sub: &str) -> TenantScope {
        TenantScope::new(
            TenantId::for_consumer_user(sub).unwrap(),
            identity(sub).identity_kind,
        )
    }

    fn zaru_free() -> SecurityContext {
        SecurityContext {
            name: "zaru-free".to_string(),
            description: "consumer".to_string(),
            capabilities: vec![],
            deny_list: vec![],
            metadata: crate::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        }
    }

    /// Starts an execution and keeps it; a cancel ends one not yet ended, as
    /// the real service does.
    #[derive(Default)]
    struct Executions {
        started: StdMutex<HashMap<ExecutionId, Execution>>,
        cancels: StdMutex<Vec<ExecutionId>>,
    }

    #[async_trait::async_trait]
    impl ExecutionService for Executions {
        async fn start_execution(
            &self,
            agent_id: AgentId,
            input: ExecutionInput,
            security_context_name: String,
            identity: Option<&UserIdentity>,
        ) -> anyhow::Result<ExecutionId> {
            let id = ExecutionId::new();
            let mut e = Execution::new_with_id(id, agent_id, input, 5, security_context_name);
            if let Some(identity) = identity {
                e.tenant_id = TenantId::for_consumer_user(&identity.sub).unwrap();
            }
            e.start();
            self.started.lock().unwrap().insert(id, e);
            Ok(id)
        }
        async fn start_execution_with_id(
            &self,
            _: ExecutionId,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&UserIdentity>,
        ) -> anyhow::Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn start_child_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: ExecutionId,
        ) -> anyhow::Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn get_execution_for_tenant(
            &self,
            tenant: &TenantId,
            id: ExecutionId,
        ) -> anyhow::Result<Execution> {
            self.started
                .lock()
                .unwrap()
                .get(&id)
                .filter(|e| &e.tenant_id == tenant)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Execution not found"))
        }
        async fn get_execution_unscoped(&self, _: ExecutionId) -> anyhow::Result<Execution> {
            anyhow::bail!("not exercised")
        }
        async fn get_iterations_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<Vec<Iteration>> {
            anyhow::bail!("not exercised")
        }
        async fn cancel_execution_for_tenant(
            &self,
            tenant: &TenantId,
            id: ExecutionId,
        ) -> anyhow::Result<()> {
            let mut started = self.started.lock().unwrap();
            let e = started
                .get_mut(&id)
                .filter(|e| &e.tenant_id == tenant)
                .ok_or_else(|| anyhow::anyhow!("Execution not found"))?;
            if !e.is_completed() {
                e.status = ExecutionStatus::Cancelled;
            }
            self.cancels.lock().unwrap().push(id);
            Ok(())
        }
        async fn stream_execution(
            &self,
            _: ExecutionId,
        ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>>
        {
            anyhow::bail!("not exercised")
        }
        async fn stream_agent_events(
            &self,
            _: AgentId,
        ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<DomainEvent>> + Send>>>
        {
            anyhow::bail!("not exercised")
        }
        async fn list_executions_for_tenant(
            &self,
            _: &TenantId,
            _: Option<AgentId>,
            _: Option<crate::domain::workflow::WorkflowId>,
            _: usize,
        ) -> anyhow::Result<Vec<Execution>> {
            anyhow::bail!("not exercised")
        }
        async fn delete_execution_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn record_llm_interaction(
            &self,
            _: ExecutionId,
            _: u8,
            _: crate::domain::execution::LlmInteraction,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn store_iteration_trajectory(
            &self,
            _: ExecutionId,
            _: u8,
            _: Vec<crate::domain::execution::TrajectoryStep>,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
    }

    /// Every agent name resolves to one agent, which has no manifest to read.
    struct OneAgent(AgentId);

    #[async_trait::async_trait]
    impl AgentLifecycleService for OneAgent {
        async fn deploy_agent_for_tenant(
            &self,
            _: &TenantId,
            _: AgentManifest,
            _: bool,
            _: crate::domain::agent::AgentScope,
            _: Option<&UserIdentity>,
        ) -> anyhow::Result<AgentId> {
            anyhow::bail!("not exercised")
        }
        async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> anyhow::Result<Agent> {
            anyhow::bail!("no manifest")
        }
        async fn update_agent_for_tenant(
            &self,
            _: &TenantId,
            _: AgentId,
            _: AgentManifest,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn list_agents_for_tenant(&self, _: &TenantId) -> anyhow::Result<Vec<Agent>> {
            Ok(vec![])
        }
        async fn lookup_agent_for_tenant(
            &self,
            _: &TenantId,
            _: &str,
        ) -> anyhow::Result<Option<AgentId>> {
            Ok(Some(self.0))
        }
        async fn lookup_agent_visible_for_tenant(
            &self,
            _: &TenantId,
            _: &str,
        ) -> anyhow::Result<Option<AgentId>> {
            Ok(Some(self.0))
        }
        async fn lookup_agent_for_tenant_with_version(
            &self,
            _: &TenantId,
            _: &str,
            _: &str,
        ) -> anyhow::Result<Option<AgentId>> {
            Ok(Some(self.0))
        }
        async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> anyhow::Result<Vec<Agent>> {
            Ok(vec![])
        }
        async fn list_versions_for_tenant(
            &self,
            _: &TenantId,
            _: AgentId,
        ) -> anyhow::Result<Vec<AgentVersion>> {
            Ok(vec![])
        }
    }

    /// Records each workflow cancel, as the daemon's port would request it.
    #[derive(Default)]
    struct WorkflowControl {
        cancels: StdMutex<Vec<ExecutionId>>,
    }

    #[async_trait::async_trait]
    impl WorkflowExecutionControlPort for WorkflowControl {
        async fn cancel_workflow_execution(
            &self,
            _: &TenantId,
            execution_id: ExecutionId,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.cancels.lock().unwrap().push(execution_id);
            Ok(())
        }
        async fn signal_workflow_execution(
            &self,
            _: &TenantId,
            _: ExecutionId,
            _: &str,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Err("not exercised".into())
        }
        async fn remove_workflow_execution(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Err("not exercised".into())
        }
    }

    struct NoOpPublisher;

    #[async_trait::async_trait]
    impl crate::domain::fsal::EventPublisher for NoOpPublisher {
        async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
    }

    struct Harness {
        service: ToolInvocationService,
        executions: Arc<Executions>,
        workflows: Arc<WorkflowControl>,
        goals: Arc<InMemoryGoalRepository>,
    }

    fn harness() -> Harness {
        let router = Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers()));
        let storage_root =
            std::env::temp_dir().join(format!("aegis-goal-cancel-{}", uuid::Uuid::new_v4()));
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
            Arc::new(InMemoryVolumeRepository::new()),
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoOpPublisher),
        ));
        let event_bus = Arc::new(EventBus::new(256));
        let executions = Arc::new(Executions::default());
        let workflows = Arc::new(WorkflowControl::default());
        let goals = Arc::new(InMemoryGoalRepository::new());
        let goal_service = Arc::new(GoalService::new(
            goals.clone(),
            event_bus.clone(),
            GoalsConfig::default(),
        ));
        let service = ToolInvocationService::new(
            Arc::new(InMemorySealSessionRepository::new()),
            Arc::new(
                crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
            ),
            Arc::new(SealMiddleware::new()),
            router,
            fsal,
            NfsVolumeRegistry::new(),
            Arc::new(OneAgent(AgentId::new())),
            executions.clone(),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
            event_bus,
            None,
        )
        .with_goals(goal_service)
        .with_workflow_execution_control(workflows.clone());
        Harness {
            service,
            executions,
            workflows,
            goals,
        }
    }

    fn direct(result: ToolInvocationResult) -> Value {
        match result {
            ToolInvocationResult::Direct(v) => v,
            other => panic!("answers directly, not {other:?}"),
        }
    }

    impl Harness {
        async fn create(&self) -> GoalId {
            let answer = direct(
                self.service
                    .invoke_aegis_goal_create_tool(
                        &json!({
                            "statement": "Solve the routing problem.",
                            "client_ref": "conversation-1",
                            "channel": "web",
                        }),
                        Some(&identity(OWNER)),
                        &scope(OWNER),
                    )
                    .await
                    .unwrap(),
            );
            GoalId::from_string(answer["goal_id"].as_str().unwrap()).unwrap()
        }

        async fn task_execute(&self, goal_id: GoalId) -> Value {
            let mut args = json!({
                "agent_id": "vrp-solver-agent",
                "input": {"prompt": "solve it"},
                "goal_id": goal_id.to_string(),
            });
            direct(
                self.service
                    .invoke_aegis_task_execute_tool(
                        &mut args,
                        &zaru_free(),
                        Some(&identity(OWNER)),
                        &scope(OWNER),
                    )
                    .await
                    .unwrap(),
            )
        }

        /// A running agent execution started for the goal.
        async fn running_agent(&self, goal_id: GoalId) -> ExecutionId {
            let started = self.task_execute(goal_id).await;
            assert_eq!(started["status"], "started", "{started}");
            ExecutionId::from_string(started["execution_id"].as_str().unwrap()).unwrap()
        }

        /// A workflow execution bound to the goal, as a starting tool binds one.
        async fn bound_workflow(&self, goal_id: GoalId) -> ExecutionId {
            let id = ExecutionId::new();
            assert!(self
                .goals
                .bind_workflow_execution(goal_id, id)
                .await
                .unwrap());
            id
        }

        async fn goal_cancel(&self, args: Value) -> Value {
            direct(
                self.service
                    .invoke_aegis_goal_cancel_tool(
                        &args,
                        &zaru_free(),
                        Some(&identity(OWNER)),
                        &scope(OWNER),
                    )
                    .await
                    .unwrap(),
            )
        }

        async fn state(&self, goal_id: GoalId) -> (GoalState, Option<String>) {
            let goal = self.goals.find_goal(goal_id).await.unwrap().unwrap();
            (goal.state, goal.closed_reason)
        }
    }

    #[tokio::test]
    async fn goal_cancel_ends_the_goal_and_cancels_its_agent_and_workflow_executions() {
        let h = harness();
        let goal = h.create().await;
        let agent = h.running_agent(goal).await;
        let workflow = h.bound_workflow(goal).await;

        let answer = h
            .goal_cancel(json!({"goal_id": goal.to_string(), "reason": "Stop it."}))
            .await;
        println!("aegis.goal.cancel answered: {answer}");
        assert_eq!(answer["state"], "cancelled", "{answer}");
        assert_eq!(answer["closed_reason"], "Stop it.");
        assert_eq!(answer["continue"], false);
        assert_eq!(
            *h.executions.cancels.lock().unwrap(),
            vec![agent],
            "the agent execution is cancelled as aegis.task.cancel cancels one"
        );
        assert_eq!(
            *h.workflows.cancels.lock().unwrap(),
            vec![workflow],
            "the workflow execution is cancelled as aegis.workflow.cancel cancels one"
        );
        assert_eq!(
            h.state(goal).await,
            (GoalState::Cancelled, Some("Stop it.".to_string()))
        );
        let refused = h.task_execute(goal).await;
        assert_eq!(refused["error"], "goal_not_open", "{refused}");
    }

    #[tokio::test]
    async fn goal_cancel_refuses_another_users_goal_as_not_found() {
        let h = harness();
        let goal = h.create().await;
        let other = "3c4d0000-other";
        let answer = direct(
            h.service
                .invoke_aegis_goal_cancel_tool(
                    &json!({"goal_id": goal.to_string()}),
                    &zaru_free(),
                    Some(&identity(other)),
                    &scope(other),
                )
                .await
                .unwrap(),
        );
        assert_eq!(answer["error"], "goal_not_found", "{answer}");
        assert_eq!(h.state(goal).await.0, GoalState::Open);
    }

    #[tokio::test]
    async fn task_cancel_of_a_bound_execution_closes_its_goal_before_it_answers() {
        let h = harness();
        let goal = h.create().await;
        let agent = h.running_agent(goal).await;
        let mut args = json!({"execution_id": agent.to_string()});
        let answer = direct(
            h.service
                .invoke_aegis_task_cancel_tool(&mut args, &scope(OWNER))
                .await
                .unwrap(),
        );
        assert_eq!(answer["cancelled"], true, "{answer}");
        let (state, reason) = h.state(goal).await;
        println!(
            "after aegis.task.cancel: the goal is {} ({reason:?})",
            state.as_str()
        );
        assert_eq!(state, GoalState::Cancelled);
        assert_eq!(reason, Some(format!("its execution {agent} was cancelled")));
        let refused = h.task_execute(goal).await;
        assert_eq!(refused["error"], "goal_not_open", "{refused}");
    }

    #[tokio::test]
    async fn workflow_cancel_of_a_bound_execution_closes_its_goal_before_it_answers() {
        let h = harness();
        let goal = h.create().await;
        let workflow = h.bound_workflow(goal).await;
        let mut args = json!({"execution_id": workflow.to_string()});
        let answer = direct(
            h.service
                .invoke_aegis_workflow_cancel_tool(&mut args, &scope(OWNER))
                .await
                .unwrap(),
        );
        assert_eq!(answer["cancelled"], true, "{answer}");
        let (state, reason) = h.state(goal).await;
        println!(
            "after aegis.workflow.cancel: the goal is {} ({reason:?})",
            state.as_str()
        );
        assert_eq!(state, GoalState::Cancelled);
        assert_eq!(
            reason,
            Some(format!("its execution {workflow} was cancelled"))
        );
        let refused = h.task_execute(goal).await;
        assert_eq!(refused["error"], "goal_not_open", "{refused}");
    }

    #[tokio::test]
    async fn the_cancel_tool_is_listed_with_goal_id_required_and_an_optional_reason() {
        let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
        let tools = router.list_tools().await.unwrap();
        let cancel = tools
            .iter()
            .find(|t| t.name == "aegis.goal.cancel")
            .expect("aegis.goal.cancel is listed");
        assert_eq!(cancel.input_schema["required"], json!(["goal_id"]));
        assert_eq!(
            cancel.input_schema["properties"]["reason"]["type"],
            "string"
        );
        assert!(router.is_skip_judge("aegis.goal.cancel").await);
    }
}
