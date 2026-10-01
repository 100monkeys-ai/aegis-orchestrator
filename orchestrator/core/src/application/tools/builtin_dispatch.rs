// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::dispatch::DispatchAction;
use crate::domain::execution::ExecutionId;
use crate::domain::seal_session::SealSessionError;
use serde_json::Value;
use std::collections::HashMap;

/// Environment variable name prefixes that are always scrubbed before forwarding
/// env_additions to a dispatched exec.
const SCRUB_PREFIXES: &[&str] = &[
    "AWS_",
    "AZURE_",
    "GCP_",
    "GOOGLE_",
    "GITHUB_TOKEN",
    "CI_",
    "AEGIS_SECRET",
    "AEGIS_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
];

/// Encodes a `cmd.run` tool call into a `DispatchAction::Exec`, scrubbing
/// sensitive environment variable additions before dispatch.
pub struct DispatchEncoder;

impl DispatchEncoder {
    /// Filter `env_additions` against built-in prefix denylist and any caller-
    /// supplied exact-match denylist entries.
    pub fn scrub_env(
        env_additions: HashMap<String, String>,
        extra_denylist: &[String],
    ) -> HashMap<String, String> {
        env_additions
            .into_iter()
            .filter(|(key, _)| {
                !SCRUB_PREFIXES.iter().any(|p| key.starts_with(p))
                    && !extra_denylist.iter().any(|d| d == key)
            })
            .collect()
    }

    /// Build a `DispatchAction::Exec` from the raw `cmd.run` tool call JSON args.
    ///
    /// No argument is dropped in silence. `args` and `env_additions` sent as a
    /// JSON-encoded string of the right shape are decoded and used; a field
    /// whose shape cannot be used is refused with a `MalformedPayload` naming
    /// the field and what was received, which the inner loop feeds back to the
    /// model as a tool error. An absent or `null` optional field takes its
    /// default.
    pub fn encode(
        args: &Value,
        env_denylist: &[String],
    ) -> Result<DispatchAction, SealSessionError> {
        let command = match args.get("command") {
            Some(Value::String(s)) => s.clone(),
            None | Some(Value::Null) => {
                return Err(SealSessionError::MalformedPayload(
                    "cmd.run: missing required field 'command'".to_string(),
                ))
            }
            Some(other) => return Err(refuse("command", "a string", other)),
        };

        let extra_args = read_args(args.get("args"))?;

        let cwd = match args.get("cwd") {
            None | Some(Value::Null) => "/workspace".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => return Err(refuse("cwd", "a string", other)),
        };

        let env_additions = read_env_additions(args.get("env_additions"))?;

        let timeout_secs = read_whole_number(
            "timeout_secs",
            args.get("timeout_secs"),
            600,
            u64::from(u32::MAX),
        )? as u32;

        let max_output_bytes = read_whole_number(
            "max_output_bytes",
            args.get("max_output_bytes"),
            1_048_576,
            u64::MAX,
        )?;

        Ok(DispatchAction::Exec {
            command,
            args: extra_args,
            cwd,
            env_additions: Self::scrub_env(env_additions, env_denylist),
            timeout_secs,
            max_output_bytes,
        })
    }
}

/// The longest excerpt of a refused value quoted back in the error.
const RECEIVED_EXCERPT_CHARS: usize = 120;

/// Describe a received JSON value for a refusal: its JSON kind and an
/// excerpt of its text.
fn received(value: &Value) -> String {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    };
    let text = value.to_string();
    let excerpt: String = text.chars().take(RECEIVED_EXCERPT_CHARS).collect();
    let ellipsis = if text.chars().count() > RECEIVED_EXCERPT_CHARS {
        "…"
    } else {
        ""
    };
    format!("{kind}: {excerpt}{ellipsis}")
}

fn refuse(field: &str, expected: &str, value: &Value) -> SealSessionError {
    SealSessionError::MalformedPayload(format!(
        "cmd.run: '{field}' must be {expected}; received {}",
        received(value)
    ))
}

/// `args`: an array of strings, or a string holding the JSON of one.
fn read_args(value: Option<&Value>) -> Result<Vec<String>, SealSessionError> {
    const EXPECTED: &str = "an array of strings, such as [\"-c\", \"env\"]";
    let decoded;
    let array = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(raw @ Value::String(s)) => {
            decoded =
                serde_json::from_str::<Value>(s).map_err(|_| refuse("args", EXPECTED, raw))?;
            match &decoded {
                Value::Array(items) => items,
                _ => return Err(refuse("args", EXPECTED, raw)),
            }
        }
        Some(other) => return Err(refuse("args", EXPECTED, other)),
    };
    array
        .iter()
        .enumerate()
        .map(|(i, item)| match item {
            Value::String(s) => Ok(s.clone()),
            other => Err(refuse(&format!("args[{i}]"), "a string", other)),
        })
        .collect()
}

/// `env_additions`: an object whose values are strings (numbers and booleans
/// are taken as their text), or a string holding the JSON of one.
fn read_env_additions(value: Option<&Value>) -> Result<HashMap<String, String>, SealSessionError> {
    const EXPECTED: &str = "an object of string values, such as {\"NAME\": \"value\"}";
    let decoded;
    let object = match value {
        None | Some(Value::Null) => return Ok(HashMap::new()),
        Some(Value::Object(map)) => map,
        Some(raw @ Value::String(s)) => {
            decoded = serde_json::from_str::<Value>(s)
                .map_err(|_| refuse("env_additions", EXPECTED, raw))?;
            match &decoded {
                Value::Object(map) => map,
                _ => return Err(refuse("env_additions", EXPECTED, raw)),
            }
        }
        Some(other) => return Err(refuse("env_additions", EXPECTED, other)),
    };
    object
        .iter()
        .map(|(key, item)| {
            let text = match item {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                other => {
                    return Err(refuse(
                        &format!("env_additions.{key}"),
                        "a string, number or boolean",
                        other,
                    ))
                }
            };
            Ok((key.clone(), text))
        })
        .collect()
}

/// A whole, non-negative number no greater than `max`, given as a JSON number
/// or as a string of decimal digits.
fn read_whole_number(
    field: &str,
    value: Option<&Value>,
    default: u64,
    max: u64,
) -> Result<u64, SealSessionError> {
    let expected = format!("a whole number from 0 to {max}");
    let number = match value {
        None | Some(Value::Null) => return Ok(default),
        Some(raw @ Value::Number(n)) => n
            .as_u64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= max as f64)
                    .map(|f| f as u64)
            })
            .ok_or_else(|| refuse(field, &expected, raw))?,
        Some(raw @ Value::String(s)) => s
            .trim()
            .parse::<u64>()
            .map_err(|_| refuse(field, &expected, raw))?,
        Some(other) => return Err(refuse(field, &expected, other)),
    };
    if number > max {
        return Err(refuse(field, &expected, value.unwrap_or(&Value::Null)));
    }
    Ok(number)
}

pub fn invoke_cmd_run(
    args: &Value,
    _execution_id: ExecutionId,
) -> Result<ToolInvocationResult, SealSessionError> {
    let action = DispatchEncoder::encode(args, &[])?;
    Ok(ToolInvocationResult::DispatchRequired(action))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Exec {
        args: Vec<String>,
        cwd: String,
        env_additions: HashMap<String, String>,
        timeout_secs: u32,
        max_output_bytes: u64,
    }

    fn encode_ok(raw: Value) -> Exec {
        match DispatchEncoder::encode(&raw, &[]).expect("cmd.run call must encode") {
            DispatchAction::Exec {
                args,
                cwd,
                env_additions,
                timeout_secs,
                max_output_bytes,
                ..
            } => Exec {
                args,
                cwd,
                env_additions,
                timeout_secs,
                max_output_bytes,
            },
        }
    }

    /// The refusal text the model receives, for a call that must be refused.
    fn refusal(raw: Value) -> String {
        match DispatchEncoder::encode(&raw, &[]) {
            Err(SealSessionError::MalformedPayload(msg)) => msg,
            Err(other) => panic!("expected MalformedPayload, got {other:?}"),
            Ok(action) => panic!("expected a refusal, got {action:?}"),
        }
    }

    /// The exact arguments of the model's cmd.run call in production
    /// (execution 7661a39a, 2026-10-01T20:43:08Z, image sha-aa4c7e5).
    #[test]
    fn env_additions_as_json_string_from_production_call_is_decoded() {
        let raw: Value = serde_json::from_str(
            r#"{"command":"python /workspace/solution.py","env_additions":"{\"INTENT_INPUTS\": \"{\\\"text\\\":\\\"100monkeys\\\"}\"}"}"#,
        )
        .unwrap();
        let exec = encode_ok(raw);
        assert_eq!(
            exec.env_additions.get("INTENT_INPUTS").map(String::as_str),
            Some(r#"{"text":"100monkeys"}"#),
            "env_additions: {:?}",
            exec.env_additions
        );
    }

    /// The model's call of 2026-10-01T20:45:12Z in the same execution.
    #[test]
    fn args_as_json_string_of_array_is_decoded() {
        let raw: Value = serde_json::from_str(
            r#"{"args":"[\"-c\", \"env\"]","command":"sh","cwd":"/workspace","env_additions":"{\"INTENT_INPUTS\": \"{\\\"text\\\":\\\"100monkeys\\\"}\"}"}"#,
        )
        .unwrap();
        let exec = encode_ok(raw);
        assert_eq!(exec.args, vec!["-c".to_string(), "env".to_string()]);
        assert_eq!(exec.cwd, "/workspace");
        assert!(exec.env_additions.contains_key("INTENT_INPUTS"));
    }

    #[test]
    fn env_additions_string_that_is_not_a_json_object_is_refused() {
        for bad in [
            json!("INTENT_INPUTS=hello"),
            json!("[\"a\"]"),
            json!("\"x\""),
        ] {
            let msg = refusal(json!({"command": "env", "env_additions": bad}));
            assert!(msg.contains("env_additions"), "{msg}");
        }
    }

    #[test]
    fn args_array_holding_a_number_is_refused() {
        let msg = refusal(json!({"command": "echo", "args": ["a", 1]}));
        assert!(msg.contains("args"), "{msg}");
    }

    #[test]
    fn args_string_that_is_not_a_json_array_of_strings_is_refused() {
        for bad in [json!("-c env"), json!("[1, 2]"), json!("{\"a\":\"b\"}")] {
            let msg = refusal(json!({"command": "sh", "args": bad}));
            assert!(msg.contains("args"), "{msg}");
        }
    }

    #[test]
    fn env_additions_number_and_boolean_values_become_text() {
        let exec = encode_ok(json!({
            "command": "env",
            "env_additions": {"N": 3, "F": 1.5, "B": true}
        }));
        assert_eq!(exec.env_additions.get("N").map(String::as_str), Some("3"));
        assert_eq!(exec.env_additions.get("F").map(String::as_str), Some("1.5"));
        assert_eq!(
            exec.env_additions.get("B").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn env_additions_value_that_is_not_text_number_or_boolean_is_refused() {
        for bad in [json!(null), json!(["x"]), json!({"y": "z"})] {
            let msg = refusal(json!({"command": "env", "env_additions": {"K": bad}}));
            assert!(msg.contains("env_additions"), "{msg}");
            assert!(msg.contains('K'), "{msg}");
        }
    }

    #[test]
    fn timeout_secs_as_numeric_string_is_used() {
        let exec = encode_ok(json!({"command": "sleep", "timeout_secs": "30"}));
        assert_eq!(exec.timeout_secs, 30);
    }

    #[test]
    fn timeout_secs_as_other_string_is_refused() {
        for bad in [json!("thirty"), json!("30s"), json!(true), json!(-1)] {
            let msg = refusal(json!({"command": "sleep", "timeout_secs": bad}));
            assert!(msg.contains("timeout_secs"), "{msg}");
        }
    }

    #[test]
    fn max_output_bytes_of_unusable_shape_is_refused() {
        let msg = refusal(json!({"command": "ls", "max_output_bytes": "lots"}));
        assert!(msg.contains("max_output_bytes"), "{msg}");
    }

    #[test]
    fn cwd_that_is_not_a_string_is_refused() {
        let msg = refusal(json!({"command": "ls", "cwd": 7}));
        assert!(msg.contains("cwd"), "{msg}");
    }

    #[test]
    fn object_and_array_forms_keep_working() {
        let exec = encode_ok(json!({
            "command": "cargo",
            "args": ["build", "--release"],
            "cwd": "/workspace/app",
            "env_additions": {"RUST_LOG": "debug", "AWS_SECRET": "x"},
            "timeout_secs": 120,
            "max_output_bytes": 4096
        }));
        assert_eq!(
            exec.args,
            vec!["build".to_string(), "--release".to_string()]
        );
        assert_eq!(exec.cwd, "/workspace/app");
        assert_eq!(exec.env_additions.len(), 1, "AWS_ prefix is scrubbed");
        assert_eq!(
            exec.env_additions.get("RUST_LOG").map(String::as_str),
            Some("debug")
        );
        assert_eq!(exec.timeout_secs, 120);
        assert_eq!(exec.max_output_bytes, 4096);
    }

    #[test]
    fn absent_and_null_optional_fields_take_their_defaults() {
        for raw in [
            json!({"command": "ls"}),
            json!({"command": "ls", "args": null, "cwd": null, "env_additions": null,
                   "timeout_secs": null, "max_output_bytes": null}),
        ] {
            let exec = encode_ok(raw);
            assert!(exec.args.is_empty());
            assert_eq!(exec.cwd, "/workspace");
            assert!(exec.env_additions.is_empty());
            assert_eq!(exec.timeout_secs, 600);
            assert_eq!(exec.max_output_bytes, 1_048_576);
        }
    }

    /// The inner loop classifies `MalformedPayload` as recoverable and feeds
    /// `Tool execution error: {e}` back to the model; the text it shows names
    /// the field and what was received.
    #[test]
    fn invoke_cmd_run_refusal_text_names_field_and_received_value() {
        let err = match invoke_cmd_run(
            &json!({"command": "env", "env_additions": "INTENT_INPUTS=x"}),
            ExecutionId::new(),
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected a refusal"),
        };
        let shown = format!("Tool execution error: {err}");
        assert!(shown.contains("'env_additions'"), "{shown}");
        assert!(
            shown.contains("received a string: \"INTENT_INPUTS=x\""),
            "{shown}"
        );
    }

    #[test]
    fn command_that_is_not_a_string_is_refused_naming_command() {
        let msg = refusal(json!({"command": ["ls"]}));
        assert!(msg.contains("command"), "{msg}");
    }
}
