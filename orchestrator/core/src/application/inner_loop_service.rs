// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Inner Loop Gateway Service (BC-2 Execution, ADR-038)
//!
//! Application service implementing the Agent Inner Loop described in ADR-038 and ADR-040.
//! This is the entry-point for agent code: `bootstrap.py` makes `POST /v1/dispatch-gateway`
//! requests sending `AgentMessage`, and receives `OrchestratorMessage` in return.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::application::execution::ExecutionService;
use crate::application::tool_invocation_service::ToolInvocationService;
use crate::domain::agent::AgentId;
use crate::domain::dispatch::{
    AgentMessage, ConversationMessage, DispatchAction, DispatchId, OrchestratorMessage, ToolCall,
};
use crate::domain::execution::{ExecutionId, Iteration, TrajectoryStep};
use crate::domain::goal::{judge_context_or_default, JudgeContextSource};
use crate::domain::iam::UserIdentity;
use crate::domain::llm::{ChatMessage, GenerationOptions, ToolSchema};
use crate::domain::tenant::TenantId;
use crate::infrastructure::llm::registry::{ApiKeySource, ProviderRegistry};

/// The share of the alias's prompt limit one command's output may take in
/// the conversation: an eighth.
const COMMAND_OUTPUT_SHARE_OF_PROMPT_LIMIT: usize = 8;

/// Maximum number of tool-call iterations before the inner loop is forcibly terminated.
const MAX_INNER_LOOP_ITERATIONS: usize = 50;

/// System-level guidance injected at the start of every conversation.
///
/// Instructs the agent to use the most specific available tool for each task rather than
/// routing everything through `cmd.run`. Using `cmd.run` to circumvent a purpose-built tool
/// (e.g. running `cat` instead of `fs.read`, or `grep` instead of `fs.grep`) is treated as a
/// policy violation and may cause the execution to be terminated.
///
/// Also communicates that the tool list is policy-scoped: if a purpose-built tool is absent
/// from the context, that operation is explicitly blocked by policy and must not be attempted
/// via `cmd.run` or any other workaround.
const TOOL_USE_POLICY_SYSTEM_MESSAGE: &str = "\
You are an autonomous agent. Use the tools available in your context to complete your task.\n\
\n\
Purpose-built tools are provided for common operations. When a dedicated tool exists for a \
task, you MUST use it. Using a general shell execution tool to perform an operation that a \
dedicated tool covers is a policy violation and will terminate the execution. Specifically:\n\
- To read file contents, use the dedicated file-read tool — never shell commands like cat or head.\n\
- To write or create files, use the dedicated file-write tool — never shell redirection.\n\
- To search file contents, use the dedicated search tool — never shell commands like grep.\n\
- To find files by name, use the dedicated glob tool — never shell commands like find or ls.\n\
- To create directories, use the dedicated directory-creation tool — never shell mkdir.\n\
- To delete files, use the dedicated delete tool — never shell rm.\n\
- Reserve general shell execution strictly for tasks that no dedicated tool can accomplish.\n\
\n\
If a dedicated tool for an operation is absent from your context, that operation is blocked \
by security policy. Do not attempt it via shell execution or any other workaround — doing so \
is a policy violation and will terminate the execution.\n\
\n\
Additional tools may be present from MCP servers configured for this execution. Always prefer \
the most specific tool available.";

#[derive(Debug, Clone)]
pub enum LlmOutput {
    FinalText(String),
    ToolCalls(Vec<ToolCall>),
}

#[derive(Debug, Clone)]
struct ExecutionContext {
    agent_id: AgentId,
    model_alias: String,
    iteration_number: u8,
    conversation: Vec<ConversationMessage>,
    iterations: usize,
    pending_dispatch_id: Option<DispatchId>,
    pending_tool_call_id: Option<String>,
    trajectory: Vec<TrajectoryStep>,
    /// Real user identity for per-user rate limiting (ADR-072 Step 2).
    user_identity: Option<UserIdentity>,
    /// Tenant identity for scoped rate limiting (ADR-072 Step 2).
    tenant_id: TenantId,
    /// Security context name bound to this execution, used to scope available tools.
    security_context_name: String,
    /// The agent's `llm_timeout_seconds`, read once at Generate: the bound on
    /// each model call of this loop.
    llm_timeout_seconds: u64,
    /// Count of in-flight `cmd.run` dispatches for this execution.
    /// Used to enforce `Capability.max_concurrent`.
    active_dispatch_count: u32,
    /// When this try's time runs out: its iteration's bound counted from the
    /// iteration's start, or the execution's bound when that comes first.
    deadline: Option<chrono::DateTime<chrono::Utc>>,
    /// The seconds of the try kept after a command for the model to read its
    /// result and act on it ([`command_margin_secs`]).
    command_margin_secs: u64,
    /// The command in flight, as asked for and as dispatched.
    pending_command: Option<PendingCommand>,
}

/// A dispatched command: the timeout the model asked for, the one it was
/// given, and its output cap.
#[derive(Debug, Clone)]
struct PendingCommand {
    requested_timeout_secs: u32,
    timeout_secs: u32,
    max_output_bytes: u64,
}

pub struct InnerLoopService {
    tool_invocation_service: Arc<ToolInvocationService>,
    execution_service: Arc<dyn ExecutionService>,
    provider_registry: Arc<ProviderRegistry>,
    active_executions: RwLock<HashMap<String, ExecutionContext>>,
    /// Optional rate limit enforcer for LLM call/token quotas (ADR-072).
    rate_limit_enforcer: Option<Arc<dyn crate::domain::rate_limit::RateLimitEnforcer>>,
    /// Optional rate limit policy resolver (ADR-072).
    rate_limit_resolver: Option<Arc<dyn crate::domain::rate_limit::RateLimitPolicyResolver>>,
    /// The alias table's room per alias (AEGIS ADR-131 U16a's measure).
    context_source: Option<Arc<dyn JudgeContextSource>>,
}

impl InnerLoopService {
    pub fn new(
        tool_invocation_service: Arc<ToolInvocationService>,
        execution_service: Arc<dyn ExecutionService>,
        provider_registry: Arc<ProviderRegistry>,
    ) -> Self {
        Self {
            tool_invocation_service,
            execution_service,
            provider_registry,
            active_executions: RwLock::new(HashMap::new()),
            rate_limit_enforcer: None,
            rate_limit_resolver: None,
            context_source: None,
        }
    }

    /// The alias table the daemon hands its judges (AEGIS ADR-131 U16a).
    pub fn with_context_source(mut self, source: Arc<dyn JudgeContextSource>) -> Self {
        self.context_source = Some(source);
        self
    }

    /// The bytes of one command's output the model's conversation is given:
    /// an eighth of the alias's prompt limit (`context_window -
    /// max_output_tokens`, U16a's measure, read from the alias table), so one
    /// output never takes more than an eighth of what the model always holds
    /// and a try's prompt, its history and several outputs still fit.
    pub fn conversation_output_bound(&self, alias: &str) -> usize {
        judge_context_or_default(self.context_source.as_deref(), alias).prompt_limit_bytes()
            / COMMAND_OUTPUT_SHARE_OF_PROMPT_LIMIT
    }

    /// Store the try's trajectory as it stands, so a try that fails or is
    /// cut off leaves on its iteration what it did (retry-knows-what-failed,
    /// (a)); before this the trajectory was stored only on a final answer.
    async fn persist_trajectory(&self, execution_id_str: &str, ctx: &ExecutionContext) {
        let Ok(uuid) = uuid::Uuid::parse_str(execution_id_str) else {
            return;
        };
        if let Err(e) = self
            .execution_service
            .store_iteration_trajectory(
                ExecutionId(uuid),
                ctx.iteration_number,
                ctx.trajectory.clone(),
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution_id_str,
                iteration = ctx.iteration_number,
                error = %e,
                "Failed to persist inner-loop trajectory"
            );
        }
    }

    /// Attach rate limiting enforcement for LLM call and token quotas (ADR-072).
    pub fn with_rate_limiting(
        mut self,
        enforcer: Arc<dyn crate::domain::rate_limit::RateLimitEnforcer>,
        resolver: Arc<dyn crate::domain::rate_limit::RateLimitPolicyResolver>,
    ) -> Self {
        self.rate_limit_enforcer = Some(enforcer);
        self.rate_limit_resolver = Some(resolver);
        self
    }

    pub async fn handle_agent_message(
        &self,
        message: AgentMessage,
    ) -> anyhow::Result<OrchestratorMessage> {
        self.handle_agent_message_with_identity(message, None, None)
            .await
    }

    /// Handle an agent message with optional caller identity for per-user rate limiting (ADR-072).
    pub async fn handle_agent_message_with_identity(
        &self,
        message: AgentMessage,
        user_identity: Option<UserIdentity>,
        tenant_id_hint: Option<TenantId>,
    ) -> anyhow::Result<OrchestratorMessage> {
        match message {
            AgentMessage::Generate {
                agent_id,
                execution_id,
                iteration_number,
                prompt,
                messages,
                model_alias,
            } => {
                let parsed_agent_id = AgentId::from_string(&agent_id)?;
                let mut conversation = messages.clone();
                let prompt_is_ours = conversation.is_empty();
                if conversation.is_empty() {
                    // Prepend tool-use policy guidance as a system message so the agent
                    // knows to use purpose-built tools rather than routing through cmd.run.
                    conversation.push(ConversationMessage {
                        role: "system".to_string(),
                        content: TOOL_USE_POLICY_SYSTEM_MESSAGE.to_string(),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                    conversation.push(ConversationMessage {
                        role: "user".to_string(),
                        content: prompt.clone(),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                } else if !conversation.iter().any(|m| m.role == "system") {
                    // If the caller supplied prior messages but no system message, prepend one.
                    conversation.insert(
                        0,
                        ConversationMessage {
                            role: "system".to_string(),
                            content: TOOL_USE_POLICY_SYSTEM_MESSAGE.to_string(),
                            tool_call_id: None,
                            tool_calls: None,
                        },
                    );
                }

                let execution_id_uuid = uuid::Uuid::parse_str(&execution_id)?;
                // Load the execution record to extract both security_context_name and
                // the canonical tenant_id stored on the record. The hint from the caller
                // is used as a fallback when the unscoped lookup is unavailable (e.g.
                // in-memory test doubles), but the record's own tenant_id is authoritative.
                let exec_record = self
                    .execution_service
                    .get_execution_unscoped(ExecutionId(execution_id_uuid))
                    .await;
                let (security_context_name, tenant_id) = match exec_record {
                    Ok(ref e) => (e.security_context_name.clone(), e.tenant_id.clone()),
                    Err(_) => (
                        "aegis-system-agent-runtime".to_string(),
                        tenant_id_hint.unwrap_or_else(TenantId::system),
                    ),
                };

                let llm_timeout_seconds = self
                    .tool_invocation_service
                    .agent_llm_timeout_seconds(&tenant_id, parsed_agent_id)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "loading agent {} for its llm_timeout_seconds: {e}",
                            parsed_agent_id.0
                        )
                    })?;

                // The try's clock (retry-knows-what-failed, (b)): the supervisor
                // ends the iteration at its bound, counted from the iteration's
                // start as the record holds it, and the execution at its own.
                let iteration_bound = self
                    .tool_invocation_service
                    .agent_iteration_timeout(&tenant_id, parsed_agent_id)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "loading agent {} for its iteration_timeout: {e}",
                            parsed_agent_id.0
                        )
                    })?;
                let now = chrono::Utc::now();
                let iteration_bound_delta =
                    chrono::Duration::seconds(iteration_bound.as_secs() as i64);
                let deadline = match exec_record {
                    Ok(ref e) => {
                        let started = e
                            .iterations
                            .iter()
                            .find(|i| i.number == iteration_number)
                            .map(|i| i.started_at)
                            .unwrap_or(now);
                        let mut deadline = started + iteration_bound_delta;
                        if let Some(bound) = e.timeout_seconds {
                            deadline = deadline
                                .min(e.started_at + chrono::Duration::seconds(bound as i64));
                        }
                        deadline
                    }
                    Err(_) => now + iteration_bound_delta,
                };

                // What the earlier tries did (retry-knows-what-failed, (a)),
                // from the trajectory the record keeps of each.
                if prompt_is_ours {
                    if let Ok(ref e) = exec_record {
                        let section = previous_tries_section(
                            &e.iterations,
                            iteration_number,
                            self.conversation_output_bound(&model_alias),
                        );
                        if let (false, Some(user)) = (
                            section.is_empty(),
                            conversation.iter_mut().find(|m| m.role == "user"),
                        ) {
                            user.content.push_str("\n\n");
                            user.content.push_str(&section);
                        }
                    }
                }

                self.active_executions.write().await.insert(
                    execution_id.clone(),
                    ExecutionContext {
                        agent_id: parsed_agent_id,
                        model_alias,
                        iteration_number,
                        conversation,
                        iterations: 0,
                        pending_dispatch_id: None,
                        pending_tool_call_id: None,
                        trajectory: Vec::new(),
                        user_identity: user_identity.clone(),
                        tenant_id,
                        security_context_name,
                        llm_timeout_seconds,
                        active_dispatch_count: 0,
                        deadline: Some(deadline),
                        command_margin_secs: command_margin_secs(
                            llm_timeout_seconds,
                            iteration_bound,
                        ),
                        pending_command: None,
                    },
                );

                self.advance_loop(&execution_id).await
            }
            AgentMessage::DispatchResult {
                execution_id,
                dispatch_id,
                exit_code,
                stdout,
                stderr,
                duration_ms,
                truncated,
            } => {
                let mut ctx = {
                    let mut lock = self.active_executions.write().await;
                    lock.remove(&execution_id).ok_or_else(|| {
                        anyhow::anyhow!("Unknown or expired execution_id: {execution_id}")
                    })?
                };

                if Some(dispatch_id) != ctx.pending_dispatch_id {
                    anyhow::bail!("Mismatched dispatch_id for execution_id: {execution_id}");
                }

                let tool_call_id = ctx.pending_tool_call_id.clone().unwrap_or_default();

                // The result as produced, kept whole in the trajectory for the
                // person and the judges (AEGIS ADR-131 U19); the model's
                // conversation is given a copy bounded by its alias's room.
                let pending = ctx.pending_command.take();
                let result_json = command_result_as_produced(
                    exit_code,
                    &stdout,
                    &stderr,
                    truncated,
                    pending.as_ref(),
                );
                let seen = conversation_copy(
                    &result_json,
                    self.conversation_output_bound(&ctx.model_alias),
                );
                let _ = duration_ms; // used by future CommandExecutionCompleted event emission

                if let Some(step) = ctx.trajectory.last_mut() {
                    step.status = if exit_code == 0 {
                        "succeeded".to_string()
                    } else {
                        "failed".to_string()
                    };
                    step.result_json = Some(result_json.to_string());
                    if exit_code != 0 {
                        step.error = Some(stderr.clone());
                    }
                }

                ctx.conversation.push(ConversationMessage {
                    role: "tool".to_string(),
                    content: seen.to_string(),
                    tool_call_id: Some(tool_call_id),
                    tool_calls: None,
                });
                self.persist_trajectory(&execution_id, &ctx).await;

                ctx.pending_dispatch_id = None;
                ctx.pending_tool_call_id = None;
                ctx.active_dispatch_count = ctx.active_dispatch_count.saturating_sub(1);

                self.active_executions
                    .write()
                    .await
                    .insert(execution_id.clone(), ctx);

                self.advance_loop(&execution_id).await
            }
        }
    }

    async fn advance_loop(&self, execution_id_str: &str) -> anyhow::Result<OrchestratorMessage> {
        loop {
            let mut ctx = {
                let lock = self.active_executions.read().await;
                lock.get(execution_id_str).cloned().ok_or_else(|| {
                    anyhow::anyhow!("Execution context not found for {execution_id_str}")
                })?
            };

            if ctx.iterations >= MAX_INNER_LOOP_ITERATIONS {
                self.active_executions
                    .write()
                    .await
                    .remove(execution_id_str);
                anyhow::bail!("Inner loop exceeded max iterations ({MAX_INNER_LOOP_ITERATIONS})");
            }

            // The run's own list (AEGIS ADR-132 G5, H4): with the remote
            // servers' tools its person bound and granted this agent; every
            // other tool as the agent's context list gives it.
            let available_tools = self
                .tool_invocation_service
                .get_available_tools_for_agent_run(
                    &ctx.tenant_id,
                    ctx.agent_id,
                    ExecutionId(uuid::Uuid::parse_str(execution_id_str)?),
                    &ctx.security_context_name,
                )
                .await
                .unwrap_or_default();

            let tool_schemas: Vec<Value> = available_tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": &t.name,
                            "description": &t.description,
                            "parameters": &t.input_schema,
                        }
                    })
                })
                .collect();

            let llm_output = self
                .call_llm(
                    &ctx.model_alias,
                    &ctx.conversation,
                    &tool_schemas,
                    ctx.user_identity.as_ref(),
                    &ctx.tenant_id,
                    ctx.llm_timeout_seconds,
                )
                .await?;

            match llm_output {
                LlmOutput::FinalText(text) => {
                    tracing::debug!(
                        execution_id = %execution_id_str,
                        iterations = ctx.iterations,
                        "LLM produced final text response (inner loop complete)"
                    );

                    ctx.conversation.push(ConversationMessage {
                        role: "assistant".to_string(),
                        content: text.clone(),
                        tool_call_id: None,
                        tool_calls: None,
                    });

                    let final_msg = OrchestratorMessage::Final {
                        content: text,
                        tool_calls_executed: ctx.iterations as u32,
                        conversation: ctx.conversation.clone(),
                        trajectory: ctx.trajectory.clone(),
                    };

                    self.persist_trajectory(execution_id_str, &ctx).await;

                    self.active_executions
                        .write()
                        .await
                        .remove(execution_id_str);
                    return Ok(final_msg);
                }
                LlmOutput::ToolCalls(tool_calls) => {
                    ctx.iterations += 1;

                    tracing::debug!(
                        execution_id = %execution_id_str,
                        iteration = ctx.iterations,
                        tool_count = tool_calls.len(),
                        tools = ?tool_calls.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
                        "LLM requested tool calls"
                    );

                    ctx.conversation.push(ConversationMessage {
                        role: "assistant".to_string(),
                        content: "".to_string(),
                        tool_call_id: None,
                        tool_calls: Some(tool_calls.clone()),
                    });

                    // Update memory before executing so changes aren't lost if we yield execution
                    self.active_executions
                        .write()
                        .await
                        .insert(execution_id_str.to_string(), ctx.clone());

                    for tool_call in tool_calls {
                        let step = TrajectoryStep {
                            tool_name: tool_call.name.clone(),
                            arguments_json: serde_json::to_string(&tool_call.arguments)
                                .unwrap_or_else(|_| "{}".to_string()),
                            status: "pending".to_string(),
                            result_json: None,
                            error: None,
                        };
                        tracing::debug!(
                            execution_id = %execution_id_str,
                            tool = %tool_call.name,
                            tool_call_id = %tool_call.id,
                            arguments = %tool_call.arguments,
                            "Invoking tool"
                        );

                        let exec_result = self
                            .tool_invocation_service
                            .invoke_tool_internal(
                                &ctx.agent_id,
                                ExecutionId(uuid::Uuid::parse_str(execution_id_str)?),
                                ctx.tenant_id.clone(),
                                ctx.iteration_number,
                                ctx.trajectory.clone(),
                                tool_call.name.clone(),
                                tool_call.arguments.clone(),
                            )
                            .await;

                        match exec_result {
                            Ok(crate::application::tool_invocation_service::ToolInvocationResult::DispatchRequired(action)) => {
                                tracing::debug!(
                                    execution_id = %execution_id_str,
                                    tool = %tool_call.name,
                                    tool_call_id = %tool_call.id,
                                    "Tool requires dispatch (yielding inner loop)"
                                );

                                let dispatch_id = DispatchId::new();

                                let mut next_ctx = {
                                    let lock = self.active_executions.read().await;
                                    lock.get(execution_id_str)
                                        .cloned()
                                        .ok_or_else(|| anyhow::anyhow!(
                                            "execution context for '{execution_id_str}' not found in active_executions"
                                        ))?
                                };

                                // Concurrency gate: enforce max_concurrent from the matching Capability.
                                if let Some(max) = self
                                    .tool_invocation_service
                                    .get_cmd_run_max_concurrent(
                                        &next_ctx.tenant_id,
                                        &next_ctx.security_context_name,
                                    )
                                    .await
                                    .unwrap_or(None)
                                {
                                    if next_ctx.active_dispatch_count >= max {
                                        anyhow::bail!(
                                            "PolicyViolation: ConcurrentExecLimitExceeded (limit={max}, active={})",
                                            next_ctx.active_dispatch_count
                                        );
                                    }
                                }

                                // The command ends inside the try (retry-knows-what-failed, (b)).
                                let now = chrono::Utc::now();
                                let (action, pending) = match fit_command_to_try(
                                    action,
                                    next_ctx.deadline,
                                    next_ctx.command_margin_secs,
                                    now,
                                ) {
                                    Ok(fitted) => fitted,
                                    Err(refusal) => {
                                        let mut step = step;
                                        step.status = "refused".to_string();
                                        step.error = Some(refusal.clone());
                                        next_ctx.trajectory.push(step);
                                        next_ctx.conversation.push(ConversationMessage {
                                            role: "tool".to_string(),
                                            content: refusal,
                                            tool_call_id: Some(tool_call.id.clone()),
                                            tool_calls: None,
                                        });
                                        self.persist_trajectory(execution_id_str, &next_ctx).await;
                                        self.active_executions.write().await.insert(execution_id_str.to_string(), next_ctx);
                                        continue;
                                    }
                                };

                                next_ctx.active_dispatch_count =
                                    next_ctx.active_dispatch_count.saturating_add(1);
                                let mut step = step;
                                step.status = "dispatched".to_string();
                                step.result_json = Some(
                                    serde_json::json!({
                                        "running_since": now.to_rfc3339(),
                                        "timeout_secs": pending.timeout_secs,
                                    })
                                    .to_string(),
                                );
                                next_ctx.trajectory.push(step);
                                next_ctx.pending_dispatch_id = Some(dispatch_id);
                                next_ctx.pending_tool_call_id = Some(tool_call.id.clone());
                                next_ctx.pending_command = Some(pending);
                                self.persist_trajectory(execution_id_str, &next_ctx).await;
                                self.active_executions.write().await.insert(execution_id_str.to_string(), next_ctx);

                                return Ok(OrchestratorMessage::Dispatch {
                                    dispatch_id,
                                    action,
                                });
                            }
                            Ok(crate::application::tool_invocation_service::ToolInvocationResult::Direct(value)) => {
                                tracing::debug!(
                                    execution_id = %execution_id_str,
                                    tool = %tool_call.name,
                                    tool_call_id = %tool_call.id,
                                    result = %serde_json::to_string(&value).unwrap_or_default(),
                                    "Tool returned direct result"
                                );

                                let tool_result = serde_json::to_string(&value).unwrap_or_default();
                                let mut next_ctx = {
                                    let lock = self.active_executions.read().await;
                                    lock.get(execution_id_str)
                                        .cloned()
                                        .ok_or_else(|| anyhow::anyhow!(
                                            "execution context for '{execution_id_str}' not found in active_executions"
                                        ))?
                                };
                                let mut step = step;
                                step.status = "succeeded".to_string();
                                step.result_json = Some(tool_result.clone());
                                next_ctx.trajectory.push(step);
                                next_ctx.conversation.push(ConversationMessage {
                                    role: "tool".to_string(),
                                    content: tool_result,
                                    tool_call_id: Some(tool_call.id.clone()),
                                    tool_calls: None,
                                });
                                self.persist_trajectory(execution_id_str, &next_ctx).await;
                                self.active_executions.write().await.insert(execution_id_str.to_string(), next_ctx);
                            }
                            Err(e) => {
                                // Differentiate fatal, policy-feedback, and recoverable errors.
                                let classification = classify_seal_error(&e);

                                if classification == SealErrorClass::Fatal {
                                    tracing::error!(
                                        tool = %tool_call.name,
                                        error = %e,
                                        "Fatal tool validation/policy error — terminating inner loop"
                                    );
                                    if let Some(mut failed_ctx) = self
                                        .active_executions
                                        .write()
                                        .await
                                        .remove(execution_id_str)
                                    {
                                        let mut step = step;
                                        step.status = "fatal".to_string();
                                        step.error = Some(e.to_string());
                                        failed_ctx.trajectory.push(step);
                                        self.persist_trajectory(execution_id_str, &failed_ctx).await;
                                    }
                                    anyhow::bail!(
                                        "Tool '{}' terminated with fatal error: {}",
                                        tool_call.name,
                                        e
                                    );
                                }

                                // Policy violations and not-found errors are fed back as
                                // explicit "tool not available" messages so the LLM stops
                                // retrying the denied tool (ADR-005 iterative refinement).
                                let tool_result = if classification == SealErrorClass::PolicyFeedback {
                                    tracing::warn!(
                                        execution_id = %execution_id_str,
                                        tool = %tool_call.name,
                                        tool_call_id = %tool_call.id,
                                        error = %e,
                                        "Policy/not-found error — feeding back to LLM as tool error"
                                    );
                                    policy_feedback_message(&tool_call.name)
                                } else {
                                    // Recoverable errors (e.g. MalformedPayload, SessionExpired)
                                    // are fed back to the LLM as tool error messages so it can
                                    // adjust its approach.
                                    tracing::debug!(
                                        execution_id = %execution_id_str,
                                        tool = %tool_call.name,
                                        tool_call_id = %tool_call.id,
                                        error = %e,
                                        "Tool returned recoverable error — feeding back to LLM"
                                    );
                                    format!("Tool execution error: {e}")
                                };
                                let mut next_ctx = {
                                    let lock = self.active_executions.read().await;
                                    lock.get(execution_id_str)
                                        .cloned()
                                        .ok_or_else(|| anyhow::anyhow!(
                                            "execution context for '{execution_id_str}' not found in active_executions"
                                        ))?
                                };
                                let mut step = step;
                                step.status = "failed".to_string();
                                step.error = Some(e.to_string());
                                next_ctx.trajectory.push(step);
                                next_ctx.conversation.push(ConversationMessage {
                                    role: "tool".to_string(),
                                    content: tool_result,
                                    tool_call_id: Some(tool_call.id.clone()),
                                    tool_calls: None,
                                });
                                self.persist_trajectory(execution_id_str, &next_ctx).await;
                                self.active_executions.write().await.insert(execution_id_str.to_string(), next_ctx);
                            }
                        }
                    }
                }
            }
        }
    }

    async fn call_llm(
        &self,
        model_alias: &str,
        conversation: &[ConversationMessage],
        tool_schemas: &[Value],
        user_identity: Option<&UserIdentity>,
        // ADR-097 footgun #8: the parent execution's tenant is REQUIRED
        // here. Previously this was `Option<&TenantId>` and the body
        // silently fell back to `TenantId::consumer()` when None — that
        // routed every system-tier inner-loop's LLM rate-limit counter
        // into the shared consumer bucket. The parent execution is
        // always tenant-scoped (`InnerLoopContext::tenant_id`); if a
        // caller has no tenant, that's an upstream bug and we want a
        // type error rather than a silent footgun.
        tenant_id: &TenantId,
        llm_timeout_seconds: u64,
    ) -> anyhow::Result<LlmOutput> {
        let chat_messages: Vec<ChatMessage> = conversation
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: m.content.clone(),
                tool_call_id: m.tool_call_id.clone(),
                tool_calls: m.tool_calls.as_ref().map(|tcs| {
                    tcs.iter()
                        .map(|tc| crate::domain::llm::ChatToolCall {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                        })
                        .collect()
                }),
            })
            .collect();

        tracing::debug!(
            tool_count = tool_schemas.len(),
            "Converting tool schemas for LLM call"
        );
        let schemas: Vec<ToolSchema> = tool_schemas
            .iter()
            .filter_map(|v| {
                let f = v.get("function")?;
                Some(ToolSchema {
                    name: f.get("name")?.as_str()?.to_string(),
                    description: f
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    parameters: f
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
                })
            })
            .collect();

        let options = GenerationOptions::default();

        // BYOK exemption (ADR-072): users who bring their own API key consume their
        // own provider quota, so platform LlmCall/LlmToken rate limits are skipped.
        let is_byok =
            self.provider_registry.key_source_for_alias(model_alias) == ApiKeySource::User;

        // Rate limit check: LlmCall (ADR-072)
        // When real UserIdentity is available, enforce per-user quotas.
        // Otherwise fall back to tenant-scoped enforcement.
        if !is_byok {
            if let (Some(enforcer), Some(resolver)) =
                (&self.rate_limit_enforcer, &self.rate_limit_resolver)
            {
                use crate::domain::rate_limit::{RateLimitResourceType, RateLimitScope};

                // Use real identity for per-user rate limiting when available (ADR-072 Step 2).
                // Falls back to synthetic tenant-scoped identity for callers that don't
                // have identity yet.
                let fallback_identity;
                let effective_identity = match user_identity {
                    Some(id) => id,
                    None => {
                        fallback_identity = crate::domain::iam::UserIdentity {
                            sub: "inner-loop".to_string(),
                            realm_slug: "aegis-system".to_string(),
                            email: None,
                            email_verified: false,
                            name: None,
                            identity_kind: crate::domain::iam::IdentityKind::TenantUser {
                                tenant_slug: "aegis-system".to_string(),
                            },
                        };
                        &fallback_identity
                    }
                };
                let scope = if user_identity.is_some() {
                    RateLimitScope::User {
                        tenant_id: tenant_id.clone(),
                        user_id: effective_identity.sub.clone(),
                    }
                } else {
                    RateLimitScope::Tenant {
                        tenant_id: tenant_id.clone(),
                    }
                };
                let resource_type = RateLimitResourceType::LlmCall;

                match resolver
                    .resolve_policy(effective_identity, tenant_id, &resource_type)
                    .await
                {
                    Ok(policy) => match enforcer.check_and_increment(&scope, &policy, 1).await {
                        Ok(decision) if !decision.allowed => {
                            let retry_hint = decision
                                .retry_after_seconds
                                .map(|s| format!(", retry after {s}s"))
                                .unwrap_or_default();
                            tracing::warn!(
                                model_alias = %model_alias,
                                bucket = ?decision.exhausted_bucket,
                                "Rate limit exceeded for LlmCall"
                            );
                            anyhow::bail!("Rate limit exceeded for LLM calls{retry_hint}");
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "Rate limit enforcement error for LlmCall (allowing call)"
                            );
                        }
                        Ok(_) => {} // allowed
                    },
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Rate limit policy resolution failed for LlmCall (allowing call)"
                        );
                    }
                }
            }
        } else {
            tracing::debug!(
                model_alias = %model_alias,
                "BYOK detected — skipping LlmCall rate limit check (ADR-072)"
            );
        }

        tracing::info!(
            model_alias = %model_alias,
            message_count = chat_messages.len(),
            tool_schema_count = schemas.len(),
            byok = is_byok,
            "calling LLM provider"
        );
        let llm_started_at = std::time::Instant::now();

        let llm_result = generate_within_llm_timeout(
            &self.provider_registry,
            model_alias,
            &chat_messages,
            &schemas,
            &options,
            llm_timeout_seconds,
        )
        .await;

        let llm_elapsed_ms = llm_started_at.elapsed().as_millis() as u64;
        match &llm_result {
            Ok(crate::domain::llm::ChatResponse::FinalText(r)) => {
                tracing::info!(
                    outcome = "ok",
                    response_kind = "final_text",
                    elapsed_ms = llm_elapsed_ms,
                    tokens_in = r.usage.prompt_tokens,
                    tokens_out = r.usage.completion_tokens,
                    "LLM provider responded"
                );
            }
            Ok(crate::domain::llm::ChatResponse::ToolCalls(calls)) => {
                tracing::info!(
                    outcome = "ok",
                    response_kind = "tool_calls",
                    elapsed_ms = llm_elapsed_ms,
                    tool_call_count = calls.len(),
                    "LLM provider responded"
                );
            }
            Err(e) => {
                tracing::info!(
                    outcome = "error",
                    elapsed_ms = llm_elapsed_ms,
                    error = %e,
                    "LLM provider responded"
                );
            }
        }

        match llm_result {
            Ok(crate::domain::llm::ChatResponse::FinalText(r)) => {
                // Post-call rate limit: LlmToken (ADR-072)
                // BYOK users are exempt — they consume their own provider quota.
                if !is_byok {
                    if let (Some(enforcer), Some(resolver)) =
                        (&self.rate_limit_enforcer, &self.rate_limit_resolver)
                    {
                        use crate::domain::rate_limit::{RateLimitResourceType, RateLimitScope};

                        // Use real identity for per-user token accounting when available (ADR-072 Step 2).
                        let fallback_identity;
                        let effective_identity = match user_identity {
                            Some(id) => id,
                            None => {
                                fallback_identity = crate::domain::iam::UserIdentity {
                                    sub: "inner-loop".to_string(),
                                    realm_slug: "aegis-system".to_string(),
                                    email: None,
                                    email_verified: false,
                                    name: None,
                                    identity_kind: crate::domain::iam::IdentityKind::TenantUser {
                                        tenant_slug: "aegis-system".to_string(),
                                    },
                                };
                                &fallback_identity
                            }
                        };
                        let scope = if user_identity.is_some() {
                            RateLimitScope::User {
                                tenant_id: tenant_id.clone(),
                                user_id: effective_identity.sub.clone(),
                            }
                        } else {
                            RateLimitScope::Tenant {
                                tenant_id: tenant_id.clone(),
                            }
                        };
                        let resource_type = RateLimitResourceType::LlmToken;
                        let token_cost = u64::from(r.usage.total_tokens);

                        if token_cost > 0 {
                            match resolver
                                .resolve_policy(effective_identity, tenant_id, &resource_type)
                                .await
                            {
                                Ok(policy) => {
                                    if let Err(e) = enforcer
                                        .check_and_increment(&scope, &policy, token_cost)
                                        .await
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            tokens = token_cost,
                                            "Rate limit enforcement error for LlmToken"
                                        );
                                    }
                                    // Note: we do not reject the already-completed response for
                                    // token overage — the tokens have already been consumed.
                                    // The next LlmCall check will catch the overage.
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "Rate limit policy resolution failed for LlmToken"
                                    );
                                }
                            }
                        }
                    }
                }

                Ok(LlmOutput::FinalText(r.text))
            }
            Ok(crate::domain::llm::ChatResponse::ToolCalls(calls)) => {
                let tool_calls = calls
                    .into_iter()
                    .map(|c| ToolCall {
                        id: c.id,
                        name: c.name,
                        arguments: c.arguments,
                    })
                    .collect();
                Ok(LlmOutput::ToolCalls(tool_calls))
            }
            // Preserve the typed `LLMError` through `anyhow` so the dispatch
            // gateway handler can downcast and map to a precise HTTP status
            // and emit a structured `LlmCallFailed` execution event.
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }
}

/// One model call of the inner loop, bounded by the agent's
/// `llm_timeout_seconds` (manifest spec v1: the bound on one LLM call, not on
/// the iteration, whose bound the supervisor enforces). The registry is given
/// the bound, so an alias's fallback is tried inside it (AEGIS ADR-130,
/// Update of 2026-10-05, D2a); a call that has not answered by then is ended
/// with an error naming the field and the seconds.
async fn generate_within_llm_timeout(
    registry: &ProviderRegistry,
    model_alias: &str,
    messages: &[ChatMessage],
    schemas: &[ToolSchema],
    options: &GenerationOptions,
    llm_timeout_seconds: u64,
) -> Result<crate::domain::llm::ChatResponse, crate::infrastructure::llm::registry::ModelCallFailure>
{
    registry
        .generate_chat_within(
            model_alias,
            messages,
            schemas,
            options,
            std::time::Duration::from_secs(llm_timeout_seconds),
        )
        .await
}

// ---------------------------------------------------------------------------
// Error classification helper (extracted for testability)
// ---------------------------------------------------------------------------

/// Classification of a `SealSessionError` for inner-loop error handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SealErrorClass {
    /// Unrecoverable — terminate the inner loop immediately.
    Fatal,
    /// The tool is unavailable due to policy or missing resource — feed a
    /// "tool not available" message back to the LLM so it can adjust.
    PolicyFeedback,
    /// Transient or correctable — feed the raw error back to the LLM.
    Recoverable,
}

/// Classify a `SealSessionError` into one of three buckets used by the
/// inner-loop error handler.
fn classify_seal_error(e: &crate::domain::seal_session::SealSessionError) -> SealErrorClass {
    use crate::domain::seal_session::SealSessionError;
    match e {
        // A refusal whose answer to the HTTP caller was decided where it was
        // built (AEGIS ADR-035, Update of 2026-10-04, R6): the inner loop
        // classifies the error it was built from, so an agent sees the class
        // and the text it saw before.
        SealSessionError::Answered { shown, .. } => classify_seal_error(shown),

        // Truly unrecoverable — crypto, session revocation, misconfiguration.
        SealSessionError::SignatureVerificationFailed(_)
        | SealSessionError::SessionInactive(_)
        | SealSessionError::ConfigurationError(_) => SealErrorClass::Fatal,

        // Policy denial or missing resource — LLM should stop trying this tool.
        SealSessionError::PolicyViolation(_) | SealSessionError::NotFound(_) => {
            SealErrorClass::PolicyFeedback
        }

        // Transient external-service failure (e.g. Brave Search HTTP 429,
        // remote fetch transport error). The LLM should see this as a normal
        // tool error and decide whether to retry, change query, or give up —
        // it MUST NOT terminate the inner loop. Listed explicitly so the
        // contract is grep-able and survives future enum additions.
        SealSessionError::UpstreamUnavailable(_) => SealErrorClass::Recoverable,

        // The LLM produced bad tool arguments (missing field, wrong type,
        // unparseable UUID, unknown tool name). Surface as a tool error so
        // the LLM can correct itself in the next iteration (ADR-005).
        // Listed explicitly so the contract is grep-able.
        SealSessionError::InvalidArguments(_) => SealErrorClass::Recoverable,

        // An internal infrastructure failure (DB error, IO error writing a
        // generated manifest, repository call failure). The LLM should see
        // this as a transient tool error and may retry — terminating the
        // inner loop on a transient infra blip is the same anti-pattern as
        // terminating on a 429.
        SealSessionError::InternalError(_) => SealErrorClass::Recoverable,

        // Tenant mismatch is a hard authorization failure — the caller is
        // attempting to operate on another tenant. Surface as policy
        // feedback so the LLM stops retrying with the wrong tenant_id.
        SealSessionError::TenantMismatch { .. } => SealErrorClass::PolicyFeedback,

        // Everything else (MalformedPayload, SessionExpired, etc.) is recoverable.
        _ => SealErrorClass::Recoverable,
    }
}

/// Build the tool-error message that gets fed back to the LLM for a
/// policy-feedback error.
fn policy_feedback_message(tool_name: &str) -> String {
    format!("Tool '{tool_name}' is not available. Do not retry this tool.")
}

// ---------------------------------------------------------------------------
// A try's clock, a command's result, and what earlier tries did
// (retry-knows-what-failed: AEGIS ADR-005 "Context Injection", ADR-040)
// ---------------------------------------------------------------------------

/// Seconds a try's command result needs to come back to the orchestrator.
const COMMAND_RESULT_TRANSPORT_SECS: u64 = 10;

/// The line the bootstrap ends a timed-out command's stderr with.
const COMMAND_TIMED_OUT: &str = "[AEGIS] Command timed out after";

/// Seconds of a try kept after a command for the model to read its result and
/// act on it: one model call's bound (`llm_timeout_seconds`) and the result's
/// way back, at most half the try, so that a try can always run a command.
fn command_margin_secs(llm_timeout_seconds: u64, iteration_bound: std::time::Duration) -> u64 {
    llm_timeout_seconds
        .saturating_add(COMMAND_RESULT_TRANSPORT_SECS)
        .min(iteration_bound.as_secs() / 2)
}

/// The command with its timeout lowered so that it ends `margin_secs` before
/// the try's `deadline`, and what was asked for; or, when less than a second
/// would be left to it, the text the model is given instead of running it.
fn fit_command_to_try(
    action: DispatchAction,
    deadline: Option<chrono::DateTime<chrono::Utc>>,
    margin_secs: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(DispatchAction, PendingCommand), String> {
    let DispatchAction::Exec {
        command,
        args,
        cwd,
        env_additions,
        timeout_secs,
        max_output_bytes,
    } = action;
    let mut fitted = timeout_secs;
    if let Some(deadline) = deadline {
        let left = (deadline - now).num_seconds();
        let room = left - margin_secs as i64;
        if room < 1 {
            return Err(format!(
                "[AEGIS] cmd.run was not run: this try has {} s left, and the last {margin_secs} s \
                 of a try are kept after a command for you to read its result and act on it. \
                 Answer with what you have.",
                left.max(0)
            ));
        }
        fitted = fitted.min(u32::try_from(room).unwrap_or(u32::MAX));
    }
    Ok((
        DispatchAction::Exec {
            command,
            args,
            cwd,
            env_additions,
            timeout_secs: fitted,
            max_output_bytes,
        },
        PendingCommand {
            requested_timeout_secs: timeout_secs,
            timeout_secs: fitted,
            max_output_bytes,
        },
    ))
}

/// A command's result as it was produced, as the trajectory keeps it.
fn command_result_as_produced(
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    truncated: bool,
    pending: Option<&PendingCommand>,
) -> Value {
    let mut result = serde_json::json!({
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
    });
    if truncated {
        result["truncated"] = serde_json::json!(true);
        let cap = pending
            .map(|p| format!(" ({} bytes)", p.max_output_bytes))
            .unwrap_or_default();
        result["notice"] = serde_json::json!(format!(
            "[AEGIS] The command's output was larger than its max_output_bytes{cap} and was cut \
             where it was produced; the line inside stdout or stderr states its true size and \
             what was kept."
        ));
    }
    if let Some(p) = pending {
        if p.timeout_secs < p.requested_timeout_secs
            && exit_code == -1
            && stderr.contains(COMMAND_TIMED_OUT)
        {
            result["timeout_notice"] = serde_json::json!(format!(
                "[AEGIS] This command's timeout was lowered from {} s to {} s so that it ends \
                 inside this try's time limit, with time left for you to read this result and \
                 act on it; the try itself ends at its limit whatever is running.",
                p.requested_timeout_secs, p.timeout_secs
            ));
        }
    }
    result
}

/// The largest index at most `at` that is a character boundary of `text`.
fn floor_boundary(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The smallest index at least `at` that is a character boundary of `text`.
fn ceil_boundary(text: &str, mut at: usize) -> usize {
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// `text` whole when it fits `budget` bytes, else its head and its tail with
/// a line stating its true size and what is omitted.
fn keep_head_and_tail(text: &str, budget: usize, label: &str) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let head = floor_boundary(text, budget / 2);
    let tail_start = ceil_boundary(text, text.len() - (budget - budget / 2));
    let tail = text.len() - tail_start;
    format!(
        "{}\n[AEGIS] This copy of the {label} is cut to fit the context: it is {} bytes; the \
         first {head} and the last {tail} are shown and the {} between them are omitted here.\n{}",
        &text[..head],
        text.len(),
        text.len() - head - tail,
        &text[tail_start..],
    )
}

/// The copy of a command's result the model's conversation is given: whole
/// when its stdout and stderr fit `bound` bytes together, else each stream's
/// head and tail within it, with the cut stated and where the whole is.
fn conversation_copy(produced: &Value, bound: usize) -> Value {
    let stdout = produced["stdout"].as_str().unwrap_or_default();
    let stderr = produced["stderr"].as_str().unwrap_or_default();
    if stdout.len() + stderr.len() <= bound {
        return produced.clone();
    }
    let mut stderr_budget = stderr.len().min(bound / 2);
    let mut stdout_budget = bound - stderr_budget;
    if stdout.len() < stdout_budget {
        stdout_budget = stdout.len();
        stderr_budget = bound - stdout_budget;
    }
    let mut seen = produced.clone();
    seen["stdout"] = Value::String(keep_head_and_tail(stdout, stdout_budget, "stdout"));
    seen["stderr"] = Value::String(keep_head_and_tail(stderr, stderr_budget, "stderr"));
    seen["context_notice"] = serde_json::json!(format!(
        "[AEGIS] This result is cut to fit your context: a command's output may take {bound} \
         bytes of it, an eighth of what this model always holds. The whole output is kept as it \
         was produced in this step of the execution's trajectory, for the person and the \
         judges. To read more of it, run the command again writing its output to a file under \
         /workspace and read that file in parts."
    ));
    seen
}

/// The tools whose successful call writes or changes the file at `path`.
const FILE_WRITING_TOOLS: &[&str] = &[
    "fs.write",
    "fs.edit",
    "fs.multi_edit",
    "fs.create_dir",
    "fs.delete",
];

/// One step of an earlier try, as the next try is told it.
fn render_step(
    n: usize,
    step: &TrajectoryStep,
    try_ended_at: Option<chrono::DateTime<chrono::Utc>>,
    bound: usize,
) -> String {
    let arguments = keep_head_and_tail(&step.arguments_json, bound, "arguments");
    let mut out = format!("{n}. {} {arguments}\n", step.tool_name);
    let result: Option<Value> = step
        .result_json
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok());
    match step.status.as_str() {
        "dispatched" => {
            let since = result
                .as_ref()
                .and_then(|r| r["running_since"].as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc));
            let ran = since
                .map(|since| {
                    let ended = try_ended_at.unwrap_or_else(chrono::Utc::now);
                    format!(": it had run {} s", (ended - since).num_seconds().max(0))
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "   It was still running when the try's time ran out{ran}. Its output so far did \
                 not reach the orchestrator: the try's container was ended with it.\n"
            ));
        }
        "refused" | "fatal" => {
            let error = step.error.as_deref().unwrap_or_default();
            out.push_str(&format!(
                "   {}: {}\n",
                step.status,
                keep_head_and_tail(error, bound, "error")
            ));
        }
        _ => match result {
            Some(r) if r.get("stdout").is_some() => {
                let seen = conversation_copy(&r, bound);
                out.push_str(&format!("   exit code: {}\n", seen["exit_code"]));
                for stream in ["stdout", "stderr"] {
                    let text = seen[stream].as_str().unwrap_or_default();
                    if !text.is_empty() {
                        out.push_str(&format!("   {stream}:\n{text}\n"));
                    }
                }
                for notice in ["notice", "timeout_notice", "context_notice"] {
                    if let Some(text) = seen[notice].as_str() {
                        out.push_str(&format!("   {text}\n"));
                    }
                }
            }
            _ => {
                let text = step
                    .result_json
                    .as_deref()
                    .or(step.error.as_deref())
                    .unwrap_or_default();
                out.push_str(&format!(
                    "   {}: {}\n",
                    step.status,
                    keep_head_and_tail(text, bound, "result")
                ));
            }
        },
    }
    out
}

/// How an earlier try ended, in words.
fn how_the_try_ended(iteration: &Iteration) -> String {
    use crate::domain::execution::IterationStatus;
    match (&iteration.error, &iteration.status) {
        (Some(error), _) => error.message.clone(),
        (None, IterationStatus::Running) => "it was cut off".to_string(),
        (None, IterationStatus::Failed) => "it failed".to_string(),
        (None, IterationStatus::Success | IterationStatus::Refining) => {
            "it gave an answer, which was not accepted".to_string()
        }
    }
}

/// What the tries of this execution before try `current` did, from the
/// trajectory each left on its iteration: each step with its result as the
/// model saw it (bounded by `bound`, as a command's output is in the
/// conversation), a command still running when its try was cut off named as
/// such with how long it had run, and the files each try wrote. Kept within
/// twice `bound`, the latest steps first. Empty when there is no earlier try.
pub(crate) fn previous_tries_section(
    iterations: &[Iteration],
    current: u8,
    bound: usize,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for iteration in iterations.iter().filter(|i| i.number < current) {
        parts.push(format!(
            "## Try {}\nIt ended: {}\n",
            iteration.number,
            how_the_try_ended(iteration)
        ));
        let steps = iteration.trajectory.as_deref().unwrap_or_default();
        if steps.is_empty() {
            parts.push("It ran no tool.\n".to_string());
        }
        for (n, step) in steps.iter().enumerate() {
            parts.push(render_step(n + 1, step, iteration.ended_at, bound));
        }
        let mut written: Vec<String> = Vec::new();
        for step in steps.iter().filter(|s| {
            s.status == "succeeded" && FILE_WRITING_TOOLS.contains(&s.tool_name.as_str())
        }) {
            if let Some(path) = serde_json::from_str::<Value>(&step.arguments_json)
                .ok()
                .and_then(|a| a["path"].as_str().map(str::to_string))
            {
                if !written.contains(&path) {
                    written.push(path);
                }
            }
        }
        if !written.is_empty() {
            parts.push(format!(
                "Files it wrote or changed: {}\n",
                written.join(", ")
            ));
        }
    }
    if parts.is_empty() {
        return String::new();
    }

    let limit = bound.saturating_mul(2);
    let mut kept: Vec<String> = Vec::new();
    let mut size = 0;
    for part in parts.iter().rev() {
        if size + part.len() > limit && !kept.is_empty() {
            break;
        }
        size += part.len();
        kept.push(keep_head_and_tail(part, limit, "step"));
    }
    kept.reverse();
    let left_out = parts.len() - kept.len();
    let mut section = String::from(
        "# What the previous tries did\n\nThe orchestrator kept each step of the earlier tries \
         of this task. Each result is shown as the model saw it then. Build on what worked and \
         do not repeat what failed in the same way.\n\n",
    );
    if left_out > 0 {
        section.push_str(&format!(
            "({left_out} earlier parts of this account are not shown: they are kept in the \
             execution's trajectory.)\n\n"
        ));
    }
    section.push_str(&kept.join("\n"));
    section
}

#[cfg(test)]
#[path = "inner_loop_service_tests.rs"]
mod daemon_path_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::seal_session::SealSessionError;
    use crate::domain::security_context::PolicyViolation;

    /// A stand-in provider whose every answer takes `delay`.
    struct SlowProvider {
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl crate::domain::llm::LLMProvider for SlowProvider {
        async fn generate(
            &self,
            _prompt: &str,
            _options: &GenerationOptions,
        ) -> Result<crate::domain::llm::GenerationResponse, crate::domain::llm::LLMError> {
            unimplemented!("not used by the inner loop")
        }

        async fn generate_chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _options: &GenerationOptions,
        ) -> Result<crate::domain::llm::ChatResponse, crate::domain::llm::LLMError> {
            tokio::time::sleep(self.delay).await;
            Ok(crate::domain::llm::ChatResponse::ToolCalls(Vec::new()))
        }

        async fn health_check(&self) -> Result<(), crate::domain::llm::LLMError> {
            Ok(())
        }
    }

    fn slow_registry(delay_secs: u64) -> ProviderRegistry {
        // One attempt and an overall budget far above the delay, so only the
        // agent's llm_timeout_seconds can end the call.
        ProviderRegistry::new_for_test(
            Arc::new(SlowProvider {
                delay: std::time::Duration::from_secs(delay_secs),
            }),
            None,
            1,
            0,
            3600,
        )
    }

    /// llm_timeout_seconds bounds one model call on the orchestrator's side:
    /// a call longer than the agent's bound is ended with an error naming the
    /// field and the seconds.
    #[tokio::test(start_paused = true)]
    async fn model_call_longer_than_llm_timeout_seconds_is_ended_naming_it() {
        let registry = slow_registry(500);
        let started = tokio::time::Instant::now();
        let result = generate_within_llm_timeout(
            &registry,
            "default",
            &[],
            &[],
            &GenerationOptions::default(),
            120,
        )
        .await;
        let waited = started.elapsed();
        let err = result.expect_err("a 500 s call must not outlive a 120 s llm_timeout_seconds");
        assert_eq!(
            waited,
            std::time::Duration::from_secs(120),
            "waited {waited:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains("llm_timeout_seconds") && text.contains("120 s"),
            "the error must name llm_timeout_seconds and the seconds: {text}"
        );
        assert!(
            matches!(err.error, crate::domain::llm::LLMError::Network(_)),
            "{err:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn model_call_inside_llm_timeout_seconds_answers() {
        let registry = slow_registry(119);
        let result = generate_within_llm_timeout(
            &registry,
            "default",
            &[],
            &[],
            &GenerationOptions::default(),
            120,
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    // -----------------------------------------------------------------------
    // Regression: PolicyViolation must NOT be classified as fatal (was bail!).
    // Prior to the fix, PolicyViolation triggered `anyhow::bail!` which
    // terminated the inner loop and returned HTTP 500.  ADR-005 requires
    // tool errors to be fed back to the agent for iterative refinement.
    // -----------------------------------------------------------------------

    #[test]
    fn policy_violation_is_classified_as_feedback_not_fatal() {
        let err = SealSessionError::PolicyViolation(PolicyViolation::ToolNotAllowed {
            tool_name: "shell.run".to_string(),
            allowed_tools: vec!["fs.write".to_string()],
        });
        let class = classify_seal_error(&err);
        assert_eq!(
            class,
            SealErrorClass::PolicyFeedback,
            "PolicyViolation must be PolicyFeedback, not Fatal"
        );
    }

    /// AEGIS ADR-035, Update of 2026-10-04, R6: an answered refusal keeps
    /// the class of the error it was built from, whatever the HTTP caller is
    /// told.
    #[test]
    fn an_answered_refusal_keeps_the_class_of_the_error_it_was_built_from() {
        use crate::domain::seal_session::{CallerAnswer, InternalFailure};
        let internal = SealSessionError::InternalError("not found: /x".into());
        assert_eq!(
            classify_seal_error(
                &internal
                    .clone()
                    .answered(CallerAnswer::NotFound("file 'x'".into()))
            ),
            classify_seal_error(&internal),
        );
        let malformed = SealSessionError::MalformedPayload("Failed to load execution".into());
        assert_eq!(
            classify_seal_error(
                &malformed
                    .clone()
                    .answered(CallerAnswer::Internal(InternalFailure::Server))
            ),
            classify_seal_error(&malformed),
        );
    }

    #[test]
    fn not_found_is_classified_as_feedback_not_fatal() {
        let err = SealSessionError::NotFound("judge agent not found".into());
        let class = classify_seal_error(&err);
        assert_eq!(
            class,
            SealErrorClass::PolicyFeedback,
            "NotFound must be PolicyFeedback, not Fatal"
        );
    }

    #[test]
    fn policy_feedback_message_contains_tool_name() {
        let msg = policy_feedback_message("shell.run");
        assert_eq!(
            msg,
            "Tool 'shell.run' is not available. Do not retry this tool."
        );
    }

    // Verify that truly fatal errors are still classified as Fatal.
    #[test]
    fn signature_verification_failed_is_fatal() {
        let err = SealSessionError::SignatureVerificationFailed("bad sig".into());
        assert_eq!(classify_seal_error(&err), SealErrorClass::Fatal);
    }

    #[test]
    fn configuration_error_is_fatal() {
        let err = SealSessionError::ConfigurationError("bad config".into());
        assert_eq!(classify_seal_error(&err), SealErrorClass::Fatal);
    }

    // Verify recoverable errors stay recoverable.
    #[test]
    fn malformed_payload_is_recoverable() {
        let err = SealSessionError::MalformedPayload("missing field".into());
        assert_eq!(classify_seal_error(&err), SealErrorClass::Recoverable);
    }

    #[test]
    fn session_expired_is_recoverable() {
        let err = SealSessionError::SessionExpired;
        assert_eq!(classify_seal_error(&err), SealErrorClass::Recoverable);
    }

    // -----------------------------------------------------------------------
    // Regression: Bug 2 — a transient upstream failure (e.g. Brave Search
    // returning HTTP 429 Too Many Requests) MUST be classified as
    // `Recoverable`, never as `Fatal`. Prior to the fix, web_tools.rs wrapped
    // the 429 in `SealSessionError::SignatureVerificationFailed`, which the
    // classifier then routed to `Fatal`, terminating the inner loop and
    // returning HTTP 500 to the bootstrap caller. After the fix, transient
    // upstream failures use `SealSessionError::UpstreamUnavailable` and are
    // fed back to the LLM so it can adapt (ADR-005 iterative refinement).
    // -----------------------------------------------------------------------
    #[test]
    fn upstream_unavailable_is_recoverable_not_fatal() {
        let err = SealSessionError::UpstreamUnavailable(
            "web.search Brave API returned 429 Too Many Requests".to_string(),
        );
        let class = classify_seal_error(&err);
        assert_eq!(
            class,
            SealErrorClass::Recoverable,
            "Brave 429 (UpstreamUnavailable) must be Recoverable so the inner loop continues, got {class:?}"
        );
        assert_ne!(
            class,
            SealErrorClass::Fatal,
            "Brave 429 (UpstreamUnavailable) must NOT be Fatal — terminating the inner loop on a transient upstream rate-limit is the bug we are guarding against"
        );
    }

    // -----------------------------------------------------------------------
    // Regression: Bug 3 — tool-argument validation failures (e.g. missing
    // `agent_id` on `aegis.task.execute`) were wrapped in
    // `SealSessionError::SignatureVerificationFailed`, which the classifier
    // routed to `Fatal`, terminating the inner loop on a recoverable LLM
    // mistake. After the fix, argument validation failures use
    // `SealSessionError::InvalidArguments` and are classified as
    // `Recoverable` so the LLM can correct its next tool call.
    // -----------------------------------------------------------------------
    #[test]
    fn invalid_arguments_is_recoverable_not_fatal() {
        let err = SealSessionError::InvalidArguments(
            "aegis.task.execute requires 'agent_id' string".to_string(),
        );
        let class = classify_seal_error(&err);
        assert_eq!(
            class,
            SealErrorClass::Recoverable,
            "InvalidArguments must be Recoverable so the LLM can correct itself, got {class:?}"
        );
        assert_ne!(
            class,
            SealErrorClass::Fatal,
            "InvalidArguments must NOT be Fatal — bad LLM tool args should not kill the inner loop"
        );
    }

    #[test]
    fn invalid_arguments_display_does_not_mention_signature() {
        // Operators reading logs must not be misled into chasing a SEAL
        // signature bug when the LLM merely produced bad tool arguments.
        let err = SealSessionError::InvalidArguments(
            "aegis.task.execute requires 'agent_id' string".to_string(),
        );
        let s = err.to_string();
        assert!(
            !s.to_lowercase().contains("signature"),
            "InvalidArguments must not be displayed as a signature failure: {s}"
        );
    }

    #[test]
    fn internal_error_is_recoverable_not_fatal() {
        let err = SealSessionError::InternalError("repository call failed".to_string());
        assert_eq!(
            classify_seal_error(&err),
            SealErrorClass::Recoverable,
            "InternalError must be Recoverable — transient infra blips should not kill the inner loop"
        );
    }

    #[test]
    fn internal_error_display_does_not_mention_signature() {
        let err = SealSessionError::InternalError("Failed to list agents: db timeout".to_string());
        let s = err.to_string();
        assert!(
            !s.to_lowercase().contains("signature"),
            "InternalError must not be displayed as a signature failure: {s}"
        );
    }

    #[test]
    fn tenant_mismatch_is_policy_feedback_not_fatal() {
        let err = SealSessionError::TenantMismatch {
            authenticated: "tenant-a".to_string(),
            requested: "tenant-b".to_string(),
        };
        assert_eq!(
            classify_seal_error(&err),
            SealErrorClass::PolicyFeedback,
            "TenantMismatch must be PolicyFeedback so the LLM stops trying the wrong tenant"
        );
    }

    #[test]
    fn tenant_mismatch_display_does_not_mention_signature() {
        let err = SealSessionError::TenantMismatch {
            authenticated: "tenant-a".to_string(),
            requested: "tenant-b".to_string(),
        };
        let s = err.to_string();
        assert!(
            !s.to_lowercase().contains("signature"),
            "TenantMismatch must not be displayed as a signature failure: {s}"
        );
    }

    // -----------------------------------------------------------------------
    // Regression: Copilot finding — building `aegis.task.execute` with a
    // missing `agent_id` previously produced
    // `SealSessionError::SignatureVerificationFailed`, classified as Fatal.
    // The handler now produces `InvalidArguments`, classified Recoverable.
    // This test pins the specific regression so it cannot silently regress
    // by someone copy-pasting an old call site.
    // -----------------------------------------------------------------------
    #[test]
    fn task_execute_missing_agent_id_is_invalid_arguments_not_signature_failure() {
        let err = SealSessionError::InvalidArguments(
            "aegis.task.execute requires 'agent_id' string".to_string(),
        );
        // Must not be a SignatureVerificationFailed.
        assert!(
            !matches!(err, SealSessionError::SignatureVerificationFailed(_)),
            "missing agent_id must not be wrapped as SignatureVerificationFailed"
        );
        // Must classify as Recoverable so the inner loop continues.
        assert_eq!(classify_seal_error(&err), SealErrorClass::Recoverable);
    }

    #[test]
    fn upstream_unavailable_display_does_not_mention_signature() {
        // Companion to the web_tools test: the Display impl for the new
        // variant must not contain the phrase "Signature verification" so
        // operators reading logs are not misled into chasing a SEAL bug.
        let err =
            SealSessionError::UpstreamUnavailable("web.search Brave API returned 429".to_string());
        let s = err.to_string();
        assert!(
            !s.contains("Signature verification"),
            "UpstreamUnavailable must not be displayed as a signature failure: {s}"
        );
    }
}
