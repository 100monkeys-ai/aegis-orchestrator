// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The tool invocation service's half of goals (AEGIS ADR-131): the tools
//! `aegis.goal.create`, `aegis.goal.evaluate` and `aegis.goal.status` (D2),
//! the optional `goal_id` of the four tools that start work (D2, U6), and the
//! [`GoalWorld`] a goal is judged in: the bound executions' rows, their
//! pending approvals, and the `goal-judge` execution, run as the goal's user
//! under the caller's security context (D3, D4).

use super::*;
use crate::application::goal_service::{
    DispatchFact, ExecutionView, GoalCaller, GoalError, GoalService, GoalWorld, JudgeProgress,
    GOAL_JUDGE_AGENT_NAME,
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
                    iteration: iteration.number,
                    tool: step.tool_name.clone(),
                    status: step.status.clone(),
                })
        })
        .collect();
    let executed = dispatches.iter().filter(|d| d.executed()).count();
    (executed, dispatches)
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
                    produced_files: Some(exec.produced_files().to_vec()),
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
                Some(ExecutionView {
                    execution_id: wf.id,
                    kind,
                    agent_or_workflow: name,
                    status: status_name(&wf.status),
                    started_at: wf.started_at,
                    ended_at: terminal.then_some(wf.last_transition_at),
                    iterations: None,
                    // U30: a workflow or intent execution keeps no record of
                    // its own tool calls or files: unknown, never zero.
                    tool_calls_executed: None,
                    dispatches: None,
                    produced_files: None,
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
