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
    AgentMessage, ConversationMessage, DispatchId, OrchestratorMessage, ToolCall,
};
use crate::domain::execution::{ExecutionId, TrajectoryStep};
use crate::domain::iam::UserIdentity;
use crate::domain::llm::{ChatMessage, GenerationOptions, LLMError, ToolSchema};
use crate::domain::tenant::TenantId;
use crate::infrastructure::llm::registry::{ApiKeySource, ProviderRegistry};

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
                duration_ms: _,
                truncated: _,
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

                let result_json = serde_json::json!({
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                });

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
                    content: result_json.to_string(),
                    tool_call_id: Some(tool_call_id),
                    tool_calls: None,
                });

                ctx.pending_dispatch_id = None;
                ctx.pending_tool_call_id = None;

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

            let available_tools = self
                .tool_invocation_service
                .get_available_tools_for_agent_in_context(
                    &ctx.tenant_id,
                    ctx.agent_id,
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
                    Some(&ctx.tenant_id),
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

                    let execution_id = ExecutionId(uuid::Uuid::parse_str(execution_id_str)?);
                    if let Err(e) = self
                        .execution_service
                        .store_iteration_trajectory(
                            execution_id,
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
                                let mut step = step;
                                step.status = "dispatched".to_string();
                                next_ctx.trajectory.push(step);
                                next_ctx.pending_dispatch_id = Some(dispatch_id);
                                next_ctx.pending_tool_call_id = Some(tool_call.id.clone());
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
                                self.active_executions.write().await.insert(execution_id_str.to_string(), next_ctx);
                            }
                            Err(e) => {
                                // Differentiate fatal policy/validation errors from
                                // recoverable tool execution errors.
                                //
                                // The following are configuration or security errors that will
                                // never self-resolve by retrying. Fail the execution
                                // immediately instead of looping forever (ADR-049).
                                //   - SignatureVerificationFailed: Ed25519 crypto / validation rejection
                                //   - PolicyViolation: security context denial
                                //   - SessionInactive: session expired or revoked
                                //   - NotFound: missing agent/resource (e.g. judge agent not found)
                                //   - ConfigurationError: misconfigured security context or gateway
                                use crate::domain::seal_session::SealSessionError;
                                let is_fatal = matches!(
                                    &e,
                                    SealSessionError::SignatureVerificationFailed(_)
                                        | SealSessionError::PolicyViolation(_)
                                        | SealSessionError::SessionInactive(_)
                                        | SealSessionError::NotFound(_)
                                        | SealSessionError::ConfigurationError(_)
                                );

                                if is_fatal {
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
                                    }
                                    anyhow::bail!(
                                        "Tool '{}' terminated with fatal error: {}",
                                        tool_call.name,
                                        e
                                    );
                                }

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
                                let tool_result = format!("Tool execution error: {e}");
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
        tenant_id: Option<&TenantId>,
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
                            identity_kind: crate::domain::iam::IdentityKind::TenantUser {
                                tenant_slug: "aegis-system".to_string(),
                            },
                        };
                        &fallback_identity
                    }
                };
                let effective_tenant_id = tenant_id.cloned().unwrap_or_else(TenantId::consumer);
                let scope = if user_identity.is_some() {
                    RateLimitScope::User {
                        user_id: effective_identity.sub.clone(),
                    }
                } else {
                    RateLimitScope::Tenant {
                        tenant_id: effective_tenant_id.clone(),
                    }
                };
                let resource_type = RateLimitResourceType::LlmCall;

                match resolver
                    .resolve_policy(effective_identity, &effective_tenant_id, &resource_type)
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

        match self
            .provider_registry
            .generate_chat(model_alias, &chat_messages, &schemas, &options)
            .await
        {
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
                                    identity_kind: crate::domain::iam::IdentityKind::TenantUser {
                                        tenant_slug: "aegis-system".to_string(),
                                    },
                                };
                                &fallback_identity
                            }
                        };
                        let effective_tenant_id =
                            tenant_id.cloned().unwrap_or_else(TenantId::consumer);
                        let scope = if user_identity.is_some() {
                            RateLimitScope::User {
                                user_id: effective_identity.sub.clone(),
                            }
                        } else {
                            RateLimitScope::Tenant {
                                tenant_id: effective_tenant_id.clone(),
                            }
                        };
                        let resource_type = RateLimitResourceType::LlmToken;
                        let token_cost = u64::from(r.usage.total_tokens);

                        if token_cost > 0 {
                            match resolver
                                .resolve_policy(
                                    effective_identity,
                                    &effective_tenant_id,
                                    &resource_type,
                                )
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
            Err(LLMError::ServiceUnavailable(msg)) => {
                Err(anyhow::anyhow!("LLM service unavailable: {msg}"))
            }
            Err(e) => Err(anyhow::anyhow!("LLM call failed: {e}")),
        }
    }
}
