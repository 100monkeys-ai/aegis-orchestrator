// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Inner Loop Gateway Service (BC-2 Execution, ADR-038)
//!
//! Application service implementing the Agent Inner Loop described in ADR-038 and ADR-040.
//! This is the entry-point for agent code: `bootstrap.py` makes `POST /v1/dispatch-gateway`
//! requests sending `AgentMessage`, and receives `OrchestratorMessage` in return.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::application::tool_invocation_service::ToolInvocationService;
use crate::domain::agent::AgentId;
use crate::domain::dispatch::{AgentMessage, DispatchId, OrchestratorMessage};
use crate::domain::execution::ExecutionId;
use crate::domain::llm::{ChatMessage, GenerationOptions, ToolSchema};
use crate::infrastructure::llm::registry::ProviderRegistry;

/// Maximum number of tool-call iterations before the inner loop is forcibly terminated.
const MAX_INNER_LOOP_ITERATIONS: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub enum LlmOutput {
    FinalText(String),
    ToolCalls(Vec<ToolCall>),
}

#[derive(Debug, Clone)]
struct ExecutionContext {
    agent_id: AgentId,
    model_alias: String,
    conversation: Vec<ConversationMessage>,
    iterations: usize,
    pending_dispatch_id: Option<DispatchId>,
    pending_tool_call_id: Option<String>,
}

pub struct InnerLoopService {
    tool_invocation_service: Arc<ToolInvocationService>,
    provider_registry: Arc<ProviderRegistry>,
    active_executions: RwLock<HashMap<String, ExecutionContext>>,
}

impl InnerLoopService {
    pub fn new(
        tool_invocation_service: Arc<ToolInvocationService>,
        provider_registry: Arc<ProviderRegistry>,
    ) -> Self {
        Self {
            tool_invocation_service,
            provider_registry,
            active_executions: RwLock::new(HashMap::new()),
        }
    }

    pub async fn handle_agent_message(
        &self,
        message: AgentMessage,
    ) -> anyhow::Result<OrchestratorMessage> {
        match message {
            AgentMessage::Generate {
                agent_id,
                execution_id,
                iteration_number: _,
                prompt,
                messages,
                model_alias,
            } => {
                let parsed_agent_id = AgentId::from_string(&agent_id)?;
                let mut conversation = messages.clone();
                if conversation.is_empty() {
                    conversation.push(ConversationMessage {
                        role: "user".to_string(),
                        content: prompt.clone(),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                }

                self.active_executions.write().await.insert(
                    execution_id.clone(),
                    ExecutionContext {
                        agent_id: parsed_agent_id,
                        model_alias,
                        conversation,
                        iterations: 0,
                        pending_dispatch_id: None,
                        pending_tool_call_id: None,
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
                        anyhow::anyhow!("Unknown or expired execution_id: {}", execution_id)
                    })?
                };

                if Some(dispatch_id) != ctx.pending_dispatch_id {
                    anyhow::bail!("Mismatched dispatch_id for execution_id: {}", execution_id);
                }

                let tool_call_id = ctx.pending_tool_call_id.clone().unwrap_or_default();

                let result_json = serde_json::json!({
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                });

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
                    anyhow::anyhow!("Execution context not found for {}", execution_id_str)
                })?
            };

            if ctx.iterations >= MAX_INNER_LOOP_ITERATIONS {
                self.active_executions
                    .write()
                    .await
                    .remove(execution_id_str);
                anyhow::bail!(
                    "Inner loop exceeded max iterations ({})",
                    MAX_INNER_LOOP_ITERATIONS
                );
            }

            let available_tools = self
                .tool_invocation_service
                .get_available_tools()
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
                .call_llm(&ctx.model_alias, &ctx.conversation, &tool_schemas)
                .await?;

            match llm_output {
                LlmOutput::FinalText(text) => {
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
                    };

                    self.active_executions
                        .write()
                        .await
                        .remove(execution_id_str);
                    return Ok(final_msg);
                }
                LlmOutput::ToolCalls(tool_calls) => {
                    ctx.iterations += 1;

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
                        let exec_result = self
                            .tool_invocation_service
                            .invoke_tool_internal(
                                &ctx.agent_id,
                                ExecutionId(uuid::Uuid::parse_str(execution_id_str)?),
                                tool_call.name.clone(),
                                tool_call.arguments.clone(),
                            )
                            .await;

                        match exec_result {
                            Ok(crate::application::tool_invocation_service::ToolInvocationResult::DispatchRequired(action)) => {
                                let dispatch_id = DispatchId::new();

                                let mut next_ctx = self.active_executions.read().await.get(execution_id_str).unwrap().clone();
                                next_ctx.pending_dispatch_id = Some(dispatch_id);
                                next_ctx.pending_tool_call_id = Some(tool_call.id.clone());
                                self.active_executions.write().await.insert(execution_id_str.to_string(), next_ctx);

                                return Ok(OrchestratorMessage::Dispatch {
                                    dispatch_id,
                                    action,
                                });
                            }
                            Ok(crate::application::tool_invocation_service::ToolInvocationResult::Direct(value)) => {
                                let tool_result = serde_json::to_string(&value).unwrap_or_default();
                                let mut next_ctx = self.active_executions.read().await.get(execution_id_str).unwrap().clone();
                                next_ctx.conversation.push(ConversationMessage {
                                    role: "tool".to_string(),
                                    content: tool_result,
                                    tool_call_id: Some(tool_call.id.clone()),
                                    tool_calls: None,
                                });
                                self.active_executions.write().await.insert(execution_id_str.to_string(), next_ctx);
                            }
                            Err(e) => {
                                let tool_result = format!("Tool execution error: {}", e);
                                let mut next_ctx = self.active_executions.read().await.get(execution_id_str).unwrap().clone();
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
                    parameters: f.get("parameters")?.clone(),
                })
            })
            .collect();

        let options = GenerationOptions::default();

        match self
            .provider_registry
            .generate_chat(model_alias, &chat_messages, &schemas, &options)
            .await
        {
            Ok(crate::domain::llm::ChatResponse::FinalText(r)) => Ok(LlmOutput::FinalText(r.text)),
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
            Err(e) => Err(anyhow::anyhow!("LLM call failed: {}", e)),
        }
    }
}
