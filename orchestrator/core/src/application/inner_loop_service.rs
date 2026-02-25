// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Inner Loop Gateway Service (BC-2 Execution, ADR-038)
//!
//! Application service implementing the Agent Inner Loop described in ADR-038.
//! This is the single entry-point for agent code: `bootstrap.py` makes one
//! long-running `POST /v1/dispatch-gateway` request and the orchestrator handles
//! the entire tool-call cycle internally.
//!
//! ## Inner Loop Flow
//!
//! ```text
//! bootstrap.py ─── POST /v1/dispatch-gateway ───►  InnerLoopService::generate()
//!                                                 │
//!                                           ┌─────┴─────┐
//!                                           │ 1. Inject  │
//!                                           │   tool     │
//!                                           │   schemas  │
//!                                           └─────┬─────┘
//!                                                 ▼
//!                                           ┌───────────┐
//!                                     ┌────►│ 2. Call    │
//!                                     │     │   LLM     │
//!                                     │     │   Proxy   │
//!                                     │     └─────┬─────┘
//!                                     │           │
//!                                     │     ┌─────┴─────┐
//!                                     │     │ Tool call? │
//!                                     │     └──┬────┬───┘
//!                                     │   yes  │    │  no (final text)
//!                                     │        ▼    │
//!                                     │  ┌──────────┐│
//!                                     │  │ 3. Execute││
//!                                     │  │   tool    ││
//!                                     │  │   via     ││
//!                                     │  │ ToolInvoc.││
//!                                     │  └─────┬────┘│
//!                                     │        │     │
//!                                     │  ┌─────┴───┐ │
//!                                     └──│ 4.Append │ │
//!                                        │  result  │ │
//!                                        │  to conv │ │
//!                                        └─────────┘ │
//!                                                    ▼
//!                                           ┌───────────┐
//!                                           │ 5. Return  │
//!                                           │   final    │
//!                                           │   text to  │
//!                                           │   agent    │
//!                                           └───────────┘
//! ```
//!
//! ## Phase Note
//!
//! ⚠️ Phase 1 — The LLM call uses the existing `LlmClient` infrastructure.
//! Tool execution uses `ToolInvocationService::invoke_tool_internal()` which
//! bypasses SMCP signature checks because the orchestrator itself is initiating
//! the call on behalf of the agent's LLM output.
//!
//! See ADR-038, AGENTS.md §Inner Loop.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::application::tool_invocation_service::ToolInvocationService;
use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::llm::{ChatMessage, GenerationOptions, ToolSchema};
use crate::infrastructure::llm::registry::ProviderRegistry;

fn default_model_alias() -> String {
    "default".to_string()
}

/// Maximum number of tool-call iterations before the inner loop is forcibly
/// terminated. Prevents runaway loops from consuming unbounded resources.
const MAX_INNER_LOOP_ITERATIONS: usize = 50;

/// Request from `bootstrap.py` to the `/v1/dispatch-gateway` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InnerLoopRequest {
    /// AEGIS `AgentId` (UUID string) of the calling agent.
    pub agent_id: String,
    /// AEGIS `ExecutionId` (UUID string) of the current execution.
    pub execution_id: String,
    /// The agent's prompt / system message.
    pub prompt: String,
    /// Model alias to use (from `spec.runtime.model` in the agent manifest).
    /// Defaults to `"default"` if omitted.
    #[serde(default = "default_model_alias")]
    pub model_alias: String,
    /// Conversation history (optional, for multi-turn).
    #[serde(default)]
    pub messages: Vec<ConversationMessage>,
}

/// A single message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    /// Role: "system", "user", "assistant", or "tool".
    pub role: String,
    /// Message content.
    pub content: String,
    /// Tool call ID (for role="tool" messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Response returned to `bootstrap.py` after the inner loop completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InnerLoopResponse {
    /// The LLM's final text output (after all tool calls are resolved).
    pub content: String,
    /// Number of inner loop iterations (tool calls) that were executed.
    pub tool_calls_executed: usize,
    /// The full conversation history including tool calls and results.
    pub conversation: Vec<ConversationMessage>,
}

/// Represents a tool call extracted from an LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool call ID for correlation.
    pub id: String,
    /// Tool name (e.g. "fs.read", "web-search.search").
    pub name: String,
    /// Tool arguments as JSON.
    pub arguments: Value,
}

/// LLM response that may contain either a final text or tool calls.
#[derive(Debug, Clone)]
pub enum LlmOutput {
    /// The LLM returned a final text answer — loop terminates.
    FinalText(String),
    /// The LLM requested one or more tool calls — loop continues.
    ToolCalls(Vec<ToolCall>),
}

/// Application service implementing the ADR-038 Inner Loop Gateway.
///
/// Orchestrates the cycle of LLM calls and tool executions until the LLM
/// produces a final text output. This is the only service that `bootstrap.py`
/// interacts with — agents never call tool servers or LLM providers directly.
pub struct InnerLoopService {
    tool_invocation_service: Arc<ToolInvocationService>,
    provider_registry: Arc<ProviderRegistry>,
}

impl InnerLoopService {
    pub fn new(
        tool_invocation_service: Arc<ToolInvocationService>,
        provider_registry: Arc<ProviderRegistry>,
    ) -> Self {
        Self {
            tool_invocation_service,
            provider_registry,
        }
    }

    /// Execute the full inner loop: LLM ↔ tool call cycle.
    ///
    /// This method is the single entry-point for agent code. It:
    /// 1. Injects available MCP tool schemas into the LLM prompt
    /// 2. Calls the LLM proxy
    /// 3. If the LLM returns tool calls, executes them via `ToolInvocationService`
    /// 4. Appends tool results to the conversation and re-calls the LLM
    /// 5. Repeats until the LLM returns a final text output
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Agent/Execution IDs are invalid
    /// - LLM proxy call fails
    /// - Tool execution fails
    /// - Maximum iteration count is exceeded
    pub async fn generate(&self, request: InnerLoopRequest) -> anyhow::Result<InnerLoopResponse> {
        let agent_id = AgentId::from_string(&request.agent_id)?;
        let execution_id = ExecutionId(uuid::Uuid::parse_str(&request.execution_id)?);

        // 1. Get available tools to inject into LLM context
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

        // 2. Build initial conversation
        let mut conversation = request.messages.clone();
        if conversation.is_empty() {
            conversation.push(ConversationMessage {
                role: "user".to_string(),
                content: request.prompt.clone(),
                tool_call_id: None,
            });
        }

        let mut tool_calls_executed: usize = 0;

        // 3. Inner loop: call LLM, execute tools, repeat
        for _iteration in 0..MAX_INNER_LOOP_ITERATIONS {
            // Call LLM with current conversation and tool schemas
            let llm_output = self
                .call_llm(&request.model_alias, &conversation, &tool_schemas)
                .await?;

            match llm_output {
                LlmOutput::FinalText(text) => {
                    // LLM returned a final text — append and return
                    conversation.push(ConversationMessage {
                        role: "assistant".to_string(),
                        content: text.clone(),
                        tool_call_id: None,
                    });

                    return Ok(InnerLoopResponse {
                        content: text,
                        tool_calls_executed,
                        conversation,
                    });
                }
                LlmOutput::ToolCalls(tool_calls) => {
                    // LLM requested tool calls — execute each one
                    tracing::info!(
                        agent_id = %request.agent_id,
                        execution_id = %request.execution_id,
                        tool_count = tool_calls.len(),
                        "Inner loop: executing tool calls"
                    );

                    // Record the assistant's tool-call message
                    let tool_call_summary: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments.to_string(),
                                }
                            })
                        })
                        .collect();

                    conversation.push(ConversationMessage {
                        role: "assistant".to_string(),
                        content: serde_json::to_string(&tool_call_summary).unwrap_or_default(),
                        tool_call_id: None,
                    });

                    // Execute each tool call and append results
                    for tool_call in &tool_calls {
                        let result = self
                            .tool_invocation_service
                            .invoke_tool_internal(
                                &agent_id,
                                execution_id,
                                tool_call.name.clone(),
                                tool_call.arguments.clone(),
                            )
                            .await;

                        let tool_result = match result {
                            Ok(value) => serde_json::to_string(&value).unwrap_or_default(),
                            Err(e) => format!("Tool execution error: {}", e),
                        };

                        conversation.push(ConversationMessage {
                            role: "tool".to_string(),
                            content: tool_result,
                            tool_call_id: Some(tool_call.id.clone()),
                        });

                        tool_calls_executed += 1;
                    }
                }
            }
        }

        // Safety: if we exhaust iterations, return the last conversation state
        anyhow::bail!(
            "Inner loop exceeded maximum iterations ({}) for agent {} execution {}",
            MAX_INNER_LOOP_ITERATIONS,
            request.agent_id,
            request.execution_id
        )
    }

    /// Call the LLM provider and parse the response into `LlmOutput`.
    ///
    /// Maps `ConversationMessage` → `ChatMessage` and `tool_schemas` (OpenAI-format
    /// JSON) → `ToolSchema` domain objects, then delegates to the `ProviderRegistry`.
    async fn call_llm(
        &self,
        model_alias: &str,
        conversation: &[ConversationMessage],
        tool_schemas: &[Value],
    ) -> anyhow::Result<LlmOutput> {
        // Map ConversationMessage → ChatMessage (same fields, different type)
        let chat_messages: Vec<ChatMessage> = conversation
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: m.content.clone(),
                tool_call_id: m.tool_call_id.clone(),
            })
            .collect();

        // Extract ToolSchema from OpenAI-format {"type":"function","function":{...}} objects
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::mcp::ToolRegistry;
    use crate::infrastructure::llm::registry::ProviderRegistry;
    use crate::infrastructure::smcp::middleware::SmcpMiddleware;
    use crate::infrastructure::smcp::session_repository::InMemorySmcpSessionRepository;
    use crate::infrastructure::tool_router::{InMemoryToolRegistry, ToolRouter};

    fn make_empty_registry() -> Arc<ProviderRegistry> {
        use crate::domain::node_config::{
            LLMSelection, ManifestMetadata, NodeConfigManifest, NodeConfigSpec, NodeIdentity,
            NodeType,
        };
        let config = NodeConfigManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "NodeConfig".to_string(),
            metadata: ManifestMetadata {
                name: "test".to_string(),
                version: None,
                labels: None,
            },
            spec: NodeConfigSpec {
                node: NodeIdentity {
                    id: "test".to_string(),
                    node_type: NodeType::Edge,
                    region: None,
                    tags: vec![],
                    resources: None,
                },
                llm_providers: vec![],
                llm_selection: LLMSelection::default(),
                runtime: crate::domain::node_config::RuntimeConfig::default(),
                network: None,
                observability: None,
                storage: None,
                mcp_servers: None,
                smcp: None,
                security_contexts: None,
                database: None,
                temporal: None,
                cortex: None,
            },
        };
        Arc::new(ProviderRegistry::from_config(&config).unwrap())
    }

    fn make_service() -> InnerLoopService {
        let repo: Arc<dyn crate::domain::smcp_session_repository::SmcpSessionRepository> =
            Arc::new(InMemorySmcpSessionRepository::new());
        let registry: Arc<dyn ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers));
        let middleware = Arc::new(SmcpMiddleware::new());
        let tool_service = Arc::new(ToolInvocationService::new(repo, middleware, router));
        InnerLoopService::new(tool_service, make_empty_registry())
    }

    /// Without a configured LLM provider the inner loop should fail gracefully.
    #[tokio::test]
    async fn test_inner_loop_fails_without_provider() {
        let service = make_service();
        let request = InnerLoopRequest {
            agent_id: uuid::Uuid::new_v4().to_string(),
            execution_id: uuid::Uuid::new_v4().to_string(),
            prompt: "Hello, world!".to_string(),
            model_alias: "default".to_string(),
            messages: vec![],
        };
        // Expect an error because no providers are registered
        let result = service.generate(request).await;
        assert!(
            result.is_err(),
            "Expected error without LLM provider configured"
        );
    }

    #[tokio::test]
    async fn test_inner_loop_request_serialization() {
        let request = InnerLoopRequest {
            agent_id: "test-agent-id".to_string(),
            execution_id: "test-exec-id".to_string(),
            prompt: "Test prompt".to_string(),
            model_alias: "default".to_string(),
            messages: vec![ConversationMessage {
                role: "system".to_string(),
                content: "You are a helpful assistant.".to_string(),
                tool_call_id: None,
            }],
        };
        let json = serde_json::to_string(&request).unwrap();
        let deserialized: InnerLoopRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.agent_id, "test-agent-id");
        assert_eq!(deserialized.model_alias, "default");
        assert_eq!(deserialized.messages.len(), 1);
    }

    #[test]
    fn test_model_alias_default() {
        let json = r#"{"agent_id":"a","execution_id":"b","prompt":"hi"}"#;
        let req: InnerLoopRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model_alias, "default");
    }

    #[test]
    fn test_tool_schema_extraction_from_json() {
        let tool_json = serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file from the workspace",
                "parameters": {"type": "object", "properties": {}}
            }
        });
        let schemas: Vec<ToolSchema> = vec![&tool_json]
            .into_iter()
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
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "read_file");
    }
}
