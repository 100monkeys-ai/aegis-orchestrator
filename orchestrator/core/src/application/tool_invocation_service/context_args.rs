// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `contexts` argument of the starting tools (Zaru ADR-0055 D9, D14;
//! AEGIS ADR-132 Update (13) S11a).
//!
//! `aegis.task.execute`, `aegis.agent.generate`, `aegis.execute.intent`,
//! `aegis.workflow.generate` and `aegis.workflow.run` each take a top-level
//! `contexts: {"<server>": "<binding id>" | ["<binding id>", ...] | null}`
//! beside `attachments`: the person's choice of credential bindings for each
//! remote tool server, any number per server. It is kept in the execution
//! input's reserved key [`CONTEXTS_INPUT_KEY`], as
//! a caller's `outputs` is, where the starts read it, an agent state or a
//! child inherits it, and the credential path selects by it. The tools'
//! model-facing schemas do not list it: a model never picks a binding (D10).

use serde_json::{Map, Value};

use crate::domain::execution::{check_contexts_shape, CONTEXTS_INPUT_KEY, CONVERSATION_INPUT_KEY};
use crate::domain::seal_session::SealSessionError;

/// Parse the `contexts` argument of a starting tool's call, or a call's
/// `_meta.contexts`.
///
/// `Ok(None)` when it is absent. An object whose every value is `null`, a
/// binding id (a UUID) or a non-empty list of distinct binding ids is
/// answered as given; a server the node does not know is kept and does
/// nothing. Anything else is refused with `InvalidArguments` before
/// anything starts.
pub(super) fn parse_contexts(args: &Value) -> Result<Option<Map<String, Value>>, SealSessionError> {
    let Some(raw) = args.get(CONTEXTS_INPUT_KEY) else {
        return Ok(None);
    };
    let refused = |sentence: &str| SealSessionError::InvalidArguments(sentence.to_string());
    check_contexts_shape(raw).map_err(refused)?;
    let Value::Object(map) = raw else {
        return Err(refused(crate::domain::execution::CONTEXTS_SHAPE));
    };
    Ok(Some(map.clone()))
}

/// The key of a call's `_meta` naming the Zaru conversation the call was
/// made in (AEGIS ADR-126, Update of 2026-10-07 (2), clause 2).
pub(super) const CONVERSATION_META_KEY: &str = "conversation_id";

/// The refusal of a `_meta.conversation_id` that is not a string holding a
/// UUID, before anything runs.
pub(super) const CONVERSATION_ID_SHAPE: &str =
    "'_meta.conversation_id' must be a conversation id (a UUID)";

/// Parse a call's `_meta.conversation_id`: `Ok(None)` when absent, the id
/// as sent when it is a string holding a UUID, refused with
/// [`CONVERSATION_ID_SHAPE`] otherwise.
pub(super) fn parse_conversation_id(meta: &Value) -> Result<Option<String>, SealSessionError> {
    let Some(raw) = meta.get(CONVERSATION_META_KEY) else {
        return Ok(None);
    };
    match raw {
        Value::String(id) if uuid::Uuid::parse_str(id).is_ok() => Ok(Some(id.clone())),
        _ => Err(SealSessionError::InvalidArguments(
            CONVERSATION_ID_SHAPE.to_string(),
        )),
    }
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

/// Keep the conversation a starting call names in its `_meta` in `input`'s
/// reserved key [`CONVERSATION_INPUT_KEY`] (AEGIS ADR-126, Update of
/// 2026-10-07 (2), clauses 3 and 3a). Any `conversation_id` the call's input
/// already carries is removed first, so only `_meta` names a run's
/// conversation; with none named, the input keeps none. A non-object input
/// is first wrapped as `{"input": <value>}`, as [`put_contexts`] does.
pub(super) fn put_conversation(input: &mut Value, conversation: Option<&str>) {
    if !input.is_object() {
        if conversation.is_none() {
            return;
        }
        let original = std::mem::replace(input, Value::Null);
        *input = serde_json::json!({ "input": original });
    }
    if let Value::Object(map) = input {
        map.remove(CONVERSATION_INPUT_KEY);
        if let Some(conversation) = conversation {
            map.insert(
                CONVERSATION_INPUT_KEY.to_string(),
                Value::String(conversation.to_string()),
            );
        }
    }
}

/// Keep the conversation the facade wrote into a starting call's `args`
/// (the call's `_meta.conversation_id` on a session with no execution
/// record, clauses 3 and 3b) in `input`, as [`carry_contexts`] keeps the
/// call's `contexts`; with none written, `input` keeps none (clause 3a).
pub(super) fn carry_conversation(args: &Value, input: &mut Value) {
    let conversation = args
        .get(CONVERSATION_INPUT_KEY)
        .and_then(Value::as_str)
        .map(str::to_string);
    put_conversation(input, conversation.as_deref());
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

    const CONVERSATION: &str = "6c1f0b52-8a3e-4d7b-9f21-0e5d4c3b2a19";

    /// ADR-126, Update of 2026-10-07 (2), clauses 3 and 3a: the call's
    /// conversation is kept in the reserved key, a scalar input is wrapped,
    /// and a `conversation_id` the input carried is replaced, or removed
    /// when the call names none.
    #[test]
    fn put_conversation_keeps_only_the_calls_conversation() {
        let mut complaints = Vec::new();
        let planted = "0b0b0b0b-1c1c-4d4d-8e8e-2f2f2f2f2f2f";
        for (mut input, conversation, expected) in [
            (
                json!({ "topic": "x" }),
                Some(CONVERSATION),
                json!({ "topic": "x", "conversation_id": CONVERSATION }),
            ),
            (
                json!("convert 43 inches"),
                Some(CONVERSATION),
                json!({ "input": "convert 43 inches", "conversation_id": CONVERSATION }),
            ),
            (
                json!({ "topic": "x", "conversation_id": planted }),
                Some(CONVERSATION),
                json!({ "topic": "x", "conversation_id": CONVERSATION }),
            ),
            (
                json!({ "topic": "x", "conversation_id": planted }),
                None,
                json!({ "topic": "x" }),
            ),
            (json!({ "topic": "x" }), None, json!({ "topic": "x" })),
        ] {
            let before = input.clone();
            put_conversation(&mut input, conversation);
            if input != expected {
                complaints.push(format!(
                    "{before} with {conversation:?} became {input}, not {expected}"
                ));
            }
        }
        assert!(
            complaints.is_empty(),
            "the input does not carry only the call's conversation:\n{}",
            complaints.join("\n")
        );
    }

    const SECOND: &str = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";

    /// AEGIS ADR-132 Update (13) S11a: the refusal's words.
    const SENTENCE: &str =
        "'contexts' must be an object naming, for each server, a binding id, a list of binding ids, or null";

    /// S11a: a non-empty list of distinct binding ids is kept as given, in
    /// the person's order, beside a bare id and `null`.
    #[test]
    fn a_list_of_binding_ids_is_kept_as_given() {
        let args = json!({ "contexts": {
            "nuclear-notes": [SECOND, BINDING],
            "imap": [BINDING],
            "github": null,
            "elsewhere": BINDING
        } });
        let mut input = json!({ "topic": "x" });
        match carry_contexts(&args, &mut input) {
            Ok(()) => assert_eq!(
                input["contexts"], args["contexts"],
                "the list was not kept as given"
            ),
            Err(e) => panic!("a list of binding ids was refused: {e:?}"),
        }
    }

    /// S11a: every other shape, an empty list and a repeated id included, is
    /// refused with the sentence; a top-level list stays refused.
    #[test]
    fn every_other_shape_is_refused_with_the_sentence() {
        let mut complaints = Vec::new();
        for bad in [
            json!({ "contexts": "nuclear-notes" }),
            json!({ "contexts": [BINDING] }),
            json!({ "contexts": null }),
            json!({ "contexts": { "nuclear-notes": "not-a-uuid" } }),
            json!({ "contexts": { "nuclear-notes": 7 } }),
            json!({ "contexts": { "nuclear-notes": { "id": BINDING } } }),
            json!({ "contexts": { "nuclear-notes": [] } }),
            json!({ "contexts": { "nuclear-notes": [BINDING, BINDING] } }),
            json!({ "contexts": { "nuclear-notes": [BINDING, "not-a-uuid"] } }),
            json!({ "contexts": { "nuclear-notes": [BINDING, null] } }),
            json!({ "contexts": { "nuclear-notes": [[BINDING]] } }),
        ] {
            match parse_contexts(&bad) {
                Err(SealSessionError::InvalidArguments(message)) if message == SENTENCE => {}
                Err(SealSessionError::InvalidArguments(message)) => {
                    complaints.push(format!("{bad} was refused with \"{message}\""))
                }
                other => complaints.push(format!("{bad} was not refused: {other:?}")),
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }
}
