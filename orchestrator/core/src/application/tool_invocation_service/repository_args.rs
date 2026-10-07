// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `repositories` argument of the starting tools (AEGIS ADR-136 G3).
//!
//! `aegis.task.execute`, `aegis.agent.generate`, `aegis.execute.intent`,
//! `aegis.workflow.generate` and `aegis.workflow.run` each take a top-level
//! `repositories: [{"binding_id": "<git repository binding id>", "branch":
//! "<work branch>"?}]` beside `contexts`: the person's repositories for the
//! run. It is kept in the execution input's reserved key
//! [`REPOSITORIES_INPUT_KEY`], as `contexts` is, where the starts read it and
//! an agent state or a child inherits it. The tools' model-facing schemas do
//! not list it.

use serde_json::Value;

use crate::domain::git_repo::{parse_run_repositories, REPOSITORIES_INPUT_KEY};
use crate::domain::seal_session::SealSessionError;

/// Parse the `repositories` argument of a starting tool's call.
///
/// `Ok(None)` when it is absent. A list G3 admits is answered as given;
/// anything else is refused with `InvalidArguments` before anything starts.
pub(super) fn parse_repositories(args: &Value) -> Result<Option<Value>, SealSessionError> {
    let Some(raw) = args.get(REPOSITORIES_INPUT_KEY) else {
        return Ok(None);
    };
    parse_run_repositories(raw)
        .map_err(|sentence| SealSessionError::InvalidArguments(sentence.to_string()))?;
    Ok(Some(raw.clone()))
}

/// Keep `repositories` in `input`'s reserved key. A non-object input is
/// first wrapped as `{"input": <value>}`, as `contexts` is.
pub(super) fn put_repositories(input: &mut Value, repositories: Value) {
    if !input.is_object() {
        let original = std::mem::replace(input, Value::Null);
        *input = serde_json::json!({ "input": original });
    }
    if let Value::Object(map) = input {
        map.insert(REPOSITORIES_INPUT_KEY.to_string(), repositories);
    }
}

/// Parse the call's `repositories` and keep it in `input`, as every
/// starting tool does before anything starts.
pub(super) fn carry_repositories(args: &Value, input: &mut Value) -> Result<(), SealSessionError> {
    if let Some(repositories) = parse_repositories(args)? {
        put_repositories(input, repositories);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BINDING: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";

    #[test]
    fn an_absent_argument_leaves_the_input_alone() {
        let mut input = json!({ "topic": "x" });
        carry_repositories(&json!({ "input": {} }), &mut input).unwrap();
        assert_eq!(
            input,
            json!({ "topic": "x" }),
            "the input changed with no repositories"
        );
    }

    #[test]
    fn a_list_of_bindings_is_kept_in_the_reserved_key_as_given() {
        let args = json!({ "repositories": [
            { "binding_id": BINDING },
            { "binding_id": BINDING, "branch": "feature/x" }
        ] });
        let mut input = json!("fix the bug");
        carry_repositories(&args, &mut input).unwrap();
        assert_eq!(
            input,
            json!({ "input": "fix the bug", "repositories": args["repositories"] }),
            "the repositories were not kept in the input's reserved key"
        );
    }

    /// AEGIS ADR-136 G3: any other shape is refused with the sentence.
    #[test]
    fn any_other_shape_is_refused_with_the_sentence() {
        for bad in [
            json!({ "repositories": { "binding_id": BINDING } }),
            json!({ "repositories": [BINDING] }),
            json!({ "repositories": [{ "binding_id": "not-a-uuid" }] }),
            json!({ "repositories": [{ "binding_id": BINDING, "branch": 7 }] }),
            json!({ "repositories": [{ "binding_id": BINDING, "branch": "a..b" }] }),
            json!({ "repositories": [{ "binding_id": BINDING, "ref": "main" }] }),
            json!({ "repositories": [{}] }),
            json!({ "repositories": null }),
        ] {
            match parse_repositories(&bad) {
                Err(SealSessionError::InvalidArguments(sentence)) => assert_eq!(
                    sentence,
                    "'repositories' must be a list of objects naming a binding_id and, optionally, a branch",
                    "the refusal of {bad} is not G3's sentence"
                ),
                other => panic!("{bad} was not refused with G3's sentence: {other:?}"),
            }
        }
    }
}
