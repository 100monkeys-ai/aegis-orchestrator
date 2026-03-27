use super::*;

impl ToolInvocationService {
    pub(super) fn publish_invocation_requested(
        &self,
        invocation_id: ToolInvocationId,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        tool_name: &str,
        arguments: &Value,
    ) {
        self.event_bus
            .publish_mcp_event(MCPToolEvent::InvocationRequested {
                invocation_id,
                execution_id,
                agent_id,
                tool_name: tool_name.to_string(),
                arguments: arguments.clone(),
                requested_at: Utc::now(),
            });
    }

    pub(super) fn publish_invocation_started(
        &self,
        invocation_id: ToolInvocationId,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        server_id: ToolServerId,
        tool_name: &str,
    ) {
        self.event_bus
            .publish_mcp_event(MCPToolEvent::InvocationStarted {
                invocation_id,
                execution_id,
                agent_id,
                server_id,
                tool_name: tool_name.to_string(),
                started_at: Utc::now(),
            });
    }

    pub(super) fn publish_invocation_completed(
        &self,
        invocation_id: ToolInvocationId,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        result: &Value,
        started: Instant,
    ) {
        self.event_bus
            .publish_mcp_event(MCPToolEvent::InvocationCompleted {
                invocation_id,
                execution_id,
                agent_id,
                result: result.clone(),
                duration_ms: started.elapsed().as_millis() as u64,
                completed_at: Utc::now(),
            });
    }

    pub(super) fn publish_invocation_failed(
        &self,
        invocation_id: ToolInvocationId,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        message: String,
    ) {
        self.event_bus
            .publish_mcp_event(MCPToolEvent::InvocationFailed {
                invocation_id,
                execution_id,
                agent_id,
                error: MCPError {
                    code: -32000,
                    message,
                    data: None,
                },
                failed_at: Utc::now(),
            });
    }

    pub(super) fn parse_json_or_string(raw: &str) -> Value {
        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
    }

    pub(super) fn compact_string_preview(value: &str, limit: usize) -> Value {
        if value.len() <= limit {
            Value::String(value.to_string())
        } else {
            let preview: String = value.chars().take(limit).collect();
            Value::String(format!("{preview}...<truncated>"))
        }
    }

    pub(super) fn compact_json_value(value: &Value) -> Value {
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
            Value::String(value) => {
                Self::compact_string_preview(value, COMPACT_STRING_PREVIEW_LIMIT)
            }
            Value::Array(items) => {
                let serialized_len = serde_json::to_string(value).unwrap_or_default().len();
                if serialized_len <= COMPACT_JSON_INLINE_LIMIT {
                    value.clone()
                } else {
                    serde_json::json!({
                        "kind": "array",
                        "length": items.len(),
                        "items": items
                            .iter()
                            .take(3)
                            .map(Self::compact_json_value)
                            .collect::<Vec<_>>()
                    })
                }
            }
            Value::Object(map) => {
                let serialized_len = serde_json::to_string(value).unwrap_or_default().len();
                if serialized_len <= COMPACT_JSON_INLINE_LIMIT {
                    value.clone()
                } else {
                    let mut keys = map.keys().cloned().collect::<Vec<String>>();
                    keys.sort();
                    serde_json::json!({
                        "kind": "object",
                        "key_count": map.len(),
                        "keys": keys.into_iter().take(8).collect::<Vec<_>>()
                    })
                }
            }
        }
    }

    pub(super) fn compact_tool_arguments(tool_name: &str, arguments: &Value) -> Value {
        match tool_name {
            "aegis.schema.get" => serde_json::json!({
                "key": arguments.get("key").and_then(Value::as_str).unwrap_or("unknown"),
            }),
            "aegis.schema.validate" => {
                let manifest_yaml = arguments
                    .get("manifest_yaml")
                    .and_then(Value::as_str)
                    .unwrap_or("");

                serde_json::json!({
                    "kind": arguments.get("kind").and_then(Value::as_str).unwrap_or("unknown"),
                    "manifest_present": !manifest_yaml.is_empty(),
                    "manifest_bytes": manifest_yaml.len(),
                })
            }
            "aegis.agent.create"
            | "aegis.agent.update"
            | "aegis.workflow.create"
            | "aegis.workflow.update" => {
                let manifest_yaml = arguments
                    .get("manifest_yaml")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut compact = serde_json::Map::new();

                for key in [
                    "force",
                    "judge_agents",
                    "min_score",
                    "min_confidence",
                    "task_context",
                ] {
                    if let Some(value) = arguments.get(key) {
                        compact.insert(key.to_string(), Self::compact_json_value(value));
                    }
                }

                compact.insert(
                    "manifest_summary".to_string(),
                    serde_json::json!({
                        "present": !manifest_yaml.is_empty(),
                        "bytes": manifest_yaml.len(),
                        "preview": Self::compact_string_preview(
                            manifest_yaml,
                            COMPACT_STRING_PREVIEW_LIMIT
                        ),
                    }),
                );

                Value::Object(compact)
            }
            _ => Self::compact_json_value(arguments),
        }
    }

    pub(super) fn compact_tool_result(
        tool_name: &str,
        arguments: &Value,
        result: Option<&Value>,
    ) -> Value {
        let Some(result) = result else {
            return Value::Null;
        };

        match tool_name {
            "aegis.schema.get" => {
                let schema_key = arguments
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");

                if let Some(error) = result.get("error").and_then(Value::as_str) {
                    return serde_json::json!({
                        "result_kind": "error",
                        "schema_key": schema_key,
                        "error": Self::compact_string_preview(error, COMPACT_STRING_PREVIEW_LIMIT),
                        "supported_keys": result.get("supported_keys").cloned(),
                    });
                }

                serde_json::json!({
                    "result_kind": "schema",
                    "schema_key": schema_key,
                    "schema_id": result.get("$id").and_then(Value::as_str),
                    "title": result.get("title").and_then(Value::as_str),
                    "schema_type": result.get("type").and_then(Value::as_str),
                    "property_count": result.get("properties").and_then(Value::as_object).map(|m| m.len()),
                    "definition_count": result.get("definitions").and_then(Value::as_object).map(|m| m.len()),
                })
            }
            "aegis.schema.validate" => {
                let errors = result
                    .get("errors")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                serde_json::json!({
                    "valid": result.get("valid").and_then(Value::as_bool).unwrap_or(false),
                    "errors_count": errors.len(),
                    "errors": errors
                        .into_iter()
                        .take(COMPACT_ERROR_PREVIEW_LIMIT)
                        .map(|error| Self::compact_json_value(&error))
                        .collect::<Vec<_>>(),
                })
            }
            _ => Self::compact_json_value(result),
        }
    }

    pub(super) fn compact_tool_call(step: &TrajectoryStep) -> Value {
        let arguments = Self::parse_json_or_string(&step.arguments_json);
        let result = step.result_json.as_deref().map(Self::parse_json_or_string);

        serde_json::json!({
            "tool_name": step.tool_name,
            "arguments_summary": Self::compact_tool_arguments(&step.tool_name, &arguments),
            "status": step.status,
            "result_summary": Self::compact_tool_result(&step.tool_name, &arguments, result.as_ref()),
            "error": step.error.as_ref().map(|error| Self::compact_string_preview(error, COMPACT_STRING_PREVIEW_LIMIT)),
        })
    }

    pub(super) fn build_tool_audit_history(
        execution_id: crate::domain::execution::ExecutionId,
        iteration_number: u8,
        tool_audit_history: &[TrajectoryStep],
    ) -> Value {
        let tool_calls: Vec<Value> = tool_audit_history
            .iter()
            .map(Self::compact_tool_call)
            .collect();

        let latest_schema_validate = tool_calls
            .iter()
            .rev()
            .find(|step| {
                step.get("tool_name").and_then(|v| v.as_str()) == Some("aegis.schema.validate")
            })
            .cloned();
        let latest_schema_get = tool_calls
            .iter()
            .rev()
            .find(|step| step.get("tool_name").and_then(|v| v.as_str()) == Some("aegis.schema.get"))
            .cloned();

        serde_json::json!({
            "execution_id": execution_id.to_string(),
            "available": true,
            "iteration_number": iteration_number,
            "tool_calls": tool_calls,
            "latest_schema_get": latest_schema_get,
            "latest_schema_validate": latest_schema_validate,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_semantic_judge_payload(
        execution_id: crate::domain::execution::ExecutionId,
        task: String,
        tool_name: &str,
        arguments: &Value,
        available_tools: Vec<String>,
        worker_mounts: Vec<String>,
        criteria: &str,
        validation_context: &str,
        iteration_number: u8,
        tool_audit_history: &[TrajectoryStep],
    ) -> Value {
        let tool_audit_history =
            Self::build_tool_audit_history(execution_id, iteration_number, tool_audit_history);

        serde_json::json!({
            "execution_id": execution_id.to_string(),
            "task": task,
            "proposed_tool_call": {
                "name": tool_name,
                "arguments": Self::compact_tool_arguments(tool_name, arguments),
            },
            "available_tools": available_tools,
            "worker_mounts": worker_mounts,
            "tool_audit_history": tool_audit_history,
            "criteria": criteria,
            "validation_context": validation_context,
        })
    }

    pub(super) fn map_policy_violation(violation: &PolicyViolation) -> (ViolationType, String) {
        match violation {
            PolicyViolation::ToolNotAllowed {
                tool_name,
                allowed_tools,
            } => (
                ViolationType::ToolNotAllowed,
                format!("Tool '{tool_name}' is not allowed. Allowed: {allowed_tools:?}"),
            ),
            PolicyViolation::ToolExplicitlyDenied { tool_name } => (
                ViolationType::ToolExplicitlyDenied,
                format!("Tool '{tool_name}' is explicitly denied"),
            ),
            PolicyViolation::RateLimitExceeded {
                resource_type,
                bucket,
                limit,
                current,
                retry_after_seconds,
            } => (
                ViolationType::RateLimitExceeded,
                format!(
                    "Rate limit exceeded: resource={resource_type}, bucket={bucket}, limit={limit}, current={current}, retry_after={retry_after_seconds}s"
                ),
            ),
            PolicyViolation::PathOutsideBoundary {
                path,
                allowed_paths,
            } => (
                ViolationType::PathOutsideBoundary,
                format!(
                    "Path '{}' outside boundary {:?}",
                    path.display(),
                    allowed_paths
                ),
            ),
            PolicyViolation::PathTraversalAttempt { path } => (
                ViolationType::PathTraversalAttempt,
                format!("Path traversal attempt at '{}'", path.display()),
            ),
            PolicyViolation::DomainNotAllowed {
                domain,
                allowed_domains,
            } => (
                ViolationType::DomainNotAllowed,
                format!("Domain '{domain}' not allowed. Allowed: {allowed_domains:?}"),
            ),
            PolicyViolation::MissingRequiredArgument(arg) => (
                ViolationType::MissingRequiredArgument,
                format!("Missing required argument '{arg}'"),
            ),
            PolicyViolation::TimeoutExceeded {
                tool_name,
                max_duration,
            } => (
                ViolationType::TimeoutExceeded,
                format!("Tool '{tool_name}' exceeded timeout {max_duration:?}"),
            ),
        }
    }
}
