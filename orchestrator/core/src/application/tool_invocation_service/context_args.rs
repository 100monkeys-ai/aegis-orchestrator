// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `contexts` argument of the starting tools (Zaru ADR-0055 D9, D14).
//!
//! `aegis.task.execute`, `aegis.agent.generate`, `aegis.execute.intent`,
//! `aegis.workflow.generate` and `aegis.workflow.run` each take a top-level
//! `contexts: {"<server>": "<binding id>" | null}` beside `attachments`: the
//! person's choice of a credential binding for each remote tool server. It
//! is kept in the execution input's reserved key
//! [`CONTEXTS_INPUT_KEY`], as
//! a caller's `outputs` is, where the starts read it, an agent state or a
//! child inherits it, and the credential path selects by it. The tools'
//! model-facing schemas do not list it: a model never picks a binding (D10).

use serde_json::{Map, Value};

use crate::domain::execution::CONTEXTS_INPUT_KEY;
use crate::domain::seal_session::SealSessionError;

/// The refusal of a `contexts` argument of any other shape.
const CONTEXTS_SHAPE: &str =
    "'contexts' must be an object naming a binding id or null for each server";

/// Parse the `contexts` argument of a starting tool's call.
///
/// `Ok(None)` when it is absent. An object whose every value is a binding
/// id (a UUID) or `null` is answered as given; a server the node does not
/// know is kept and does nothing. Anything else is refused with
/// `InvalidArguments` before anything starts.
pub(super) fn parse_contexts(args: &Value) -> Result<Option<Map<String, Value>>, SealSessionError> {
    let Some(raw) = args.get(CONTEXTS_INPUT_KEY) else {
        return Ok(None);
    };
    let refused = || SealSessionError::InvalidArguments(CONTEXTS_SHAPE.to_string());
    let Value::Object(map) = raw else {
        return Err(refused());
    };
    for choice in map.values() {
        match choice {
            Value::Null => {}
            Value::String(id) if uuid::Uuid::parse_str(id).is_ok() => {}
            _ => return Err(refused()),
        }
    }
    Ok(Some(map.clone()))
}

/// Keep `contexts` in `input`'s reserved key. A non-object input is first
/// wrapped as `{"input": <value>}`, the form the agent's rendering already
/// reads its input from.
pub(super) fn put_contexts(input: &mut Value, contexts: Map<String, Value>) {
    if !input.is_object() {
        let original = std::mem::replace(input, Value::Null);
        *input = serde_json::json!({ "input": original });
    }
    if let Value::Object(map) = input {
        map.insert(CONTEXTS_INPUT_KEY.to_string(), Value::Object(contexts));
    }
}

/// Parse the call's `contexts` and keep it in `input`, as every starting
/// tool does before anything starts.
pub(super) fn carry_contexts(args: &Value, input: &mut Value) -> Result<(), SealSessionError> {
    if let Some(contexts) = parse_contexts(args)? {
        put_contexts(input, contexts);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BINDING: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";

    #[test]
    fn an_absent_argument_is_none_and_leaves_the_input_alone() {
        let mut input = json!({ "topic": "x" });
        assert_eq!(parse_contexts(&json!({})).unwrap(), None);
        carry_contexts(&json!({ "input": {} }), &mut input).unwrap();
        assert_eq!(
            input,
            json!({ "topic": "x" }),
            "the input changed with no contexts"
        );
    }

    #[test]
    fn a_binding_id_and_null_are_kept_as_given_and_an_unknown_server_too() {
        let args = json!({ "contexts": { "nuclear-notes": BINDING, "elsewhere": null } });
        let mut input = json!({ "topic": "x" });
        carry_contexts(&args, &mut input).unwrap();
        assert_eq!(
            input,
            json!({
                "topic": "x",
                "contexts": { "nuclear-notes": BINDING, "elsewhere": null }
            }),
            "the contexts were not kept in the input's reserved key"
        );
    }

    #[test]
    fn a_non_object_input_is_wrapped_before_the_contexts_are_kept() {
        let mut input = json!("convert 43 inches");
        put_contexts(
            &mut input,
            Map::from_iter([("nuclear-notes".to_string(), Value::Null)]),
        );
        assert_eq!(
            input,
            json!({ "input": "convert 43 inches", "contexts": { "nuclear-notes": null } }),
            "a scalar input was not wrapped as {{\"input\": ...}}"
        );
    }

    #[test]
    fn every_other_shape_is_refused_with_the_sentence() {
        for bad in [
            json!({ "contexts": "nuclear-notes" }),
            json!({ "contexts": [BINDING] }),
            json!({ "contexts": null }),
            json!({ "contexts": { "nuclear-notes": "not-a-uuid" } }),
            json!({ "contexts": { "nuclear-notes": 7 } }),
            json!({ "contexts": { "nuclear-notes": { "id": BINDING } } }),
        ] {
            match parse_contexts(&bad) {
                Err(SealSessionError::InvalidArguments(message)) => assert_eq!(
                    message, CONTEXTS_SHAPE,
                    "{bad} was refused with another sentence"
                ),
                other => panic!("{bad} was not refused: {other:?}"),
            }
        }
    }
}
