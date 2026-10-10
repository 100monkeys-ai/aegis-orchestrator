// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `aegis.schedule.*` tools as a model sees them (AEGIS ADR-139 N11):
//! the eight are advertised with their schemas; `contexts` and
//! `repositories` are in none of them; `list`, `get` and `runs` skip the
//! judge and the rest do not; none waits at the approval gate; each
//! declares its required arguments.

use aegis_orchestrator_core::domain::mcp::ToolInputContract;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use serde_json::Value;

const TOOLS: [&str; 8] = [
    "aegis.schedule.create",
    "aegis.schedule.list",
    "aegis.schedule.get",
    "aegis.schedule.update",
    "aegis.schedule.pause",
    "aegis.schedule.resume",
    "aegis.schedule.delete",
    "aegis.schedule.runs",
];

fn router() -> ToolRouter {
    ToolRouter::new(ToolRouter::builtin_dispatchers())
}

/// Every property name anywhere in `schema`.
fn property_names(schema: &Value, names: &mut Vec<String>) {
    match schema {
        Value::Object(map) => {
            if let Some(Value::Object(properties)) = map.get("properties") {
                names.extend(properties.keys().cloned());
            }
            for value in map.values() {
                property_names(value, names);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| property_names(v, names)),
        _ => {}
    }
}

/// Each tool is advertised with a schema whose `required` is the input
/// contract's, and the timing, target and input it takes; no schema offers
/// `contexts` or `repositories`, the client's reserved dispatch keys.
#[tokio::test]
async fn the_eight_tools_are_advertised_with_their_schemas_and_no_reserved_key() {
    let tools = router().list_tools().await.unwrap();
    let mut wrong = Vec::new();
    for name in TOOLS {
        let Some(tool) = tools.iter().find(|t| t.name == name) else {
            wrong.push(format!("{name} is not advertised"));
            continue;
        };
        if tool.description.is_empty() {
            wrong.push(format!("{name} has no description"));
        }
        let required: Vec<&str> = tool.input_schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if required != ToolInputContract::required_fields(name) {
            wrong.push(format!(
                "{name}: the schema requires {required:?}, the contract {:?}",
                ToolInputContract::required_fields(name)
            ));
        }
        let mut names = Vec::new();
        property_names(&tool.input_schema, &mut names);
        for reserved in ["contexts", "repositories"] {
            if names.iter().any(|n| n == reserved) {
                wrong.push(format!("{name}'s schema offers '{reserved}'"));
            }
        }
        let takes = |keys: &[&str]| keys.iter().all(|k| names.iter().any(|n| n == k));
        let expected: &[&str] = match name {
            "aegis.schedule.create" => &[
                "name",
                "target_kind",
                "target",
                "version",
                "intent",
                "input",
                "attachments",
                "at",
                "recurrence",
                "cron",
                "timezone",
                "jitter_seconds",
            ],
            "aegis.schedule.update" => &[
                "schedule_id",
                "name",
                "target_kind",
                "target",
                "version",
                "intent",
                "input",
                "attachments",
                "at",
                "recurrence",
            ],
            "aegis.schedule.list" => &[],
            "aegis.schedule.runs" => &["schedule_id", "limit"],
            _ => &["schedule_id"],
        };
        if !takes(expected) {
            wrong.push(format!("{name} takes {names:?}, not all of {expected:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "the schedule tools as advertised: {wrong:#?}"
    );
}

/// `list`, `get` and `runs` skip the inner-loop judge and the rest are
/// judged; no schedule tool waits at the approval gate (a run's outward
/// calls do).
#[tokio::test]
async fn only_the_reads_skip_the_judge_and_none_is_gated() {
    let router = router();
    let mut answered = Vec::new();
    for name in TOOLS {
        answered.push((
            name,
            router.is_skip_judge(name).await,
            router.requires_approval(name),
        ));
    }
    let expected: Vec<_> = TOOLS
        .iter()
        .map(|name| {
            (
                *name,
                matches!(
                    *name,
                    "aegis.schedule.list" | "aegis.schedule.get" | "aegis.schedule.runs"
                ),
                false,
            )
        })
        .collect();
    assert_eq!(
        answered, expected,
        "(tool, skips the judge, gated) is not as N11 has it"
    );
}

/// A call missing a required argument is refused before anything runs.
#[test]
fn a_call_missing_its_required_argument_is_refused() {
    let refused: Vec<bool> = TOOLS
        .iter()
        .map(|name| ToolInputContract::validate(name, &serde_json::json!({})).is_err())
        .collect();
    assert_eq!(
        refused,
        vec![true, false, true, true, true, true, true, true],
        "an empty call of each tool (list needs nothing)"
    );
}

/// Run now: `aegis.schedule.run_now` is advertised with its description and
/// a schema requiring `schedule_id`, its input contract requires
/// `schedule_id`, and it is judged and ungated as pause and resume are.
#[tokio::test]
async fn run_now_is_advertised_with_its_description_and_requires_schedule_id() {
    const NAME: &str = "aegis.schedule.run_now";
    let router = router();
    let tools = router.list_tools().await.unwrap();
    let advertised = tools.iter().find(|t| t.name == NAME).map(|tool| {
        (
            tool.description.clone(),
            tool.input_schema["required"].clone(),
            tool.input_schema["properties"]["schedule_id"]["type"].clone(),
        )
    });
    assert_eq!(
        (
            advertised,
            ToolInputContract::required_fields(NAME).to_vec(),
            router.is_skip_judge(NAME).await,
            router.requires_approval(NAME),
        ),
        (
            Some((
                "Starts one run of a schedule now by schedule_id, whether it is active or \
                 paused, as its timed runs start; refused while its last run is still running."
                    .to_string(),
                serde_json::json!(["schedule_id"]),
                serde_json::json!("string"),
            )),
            vec!["schedule_id"],
            false,
            false,
        ),
        "aegis.schedule.run_now is not advertised as (description, schema required, \
         schedule_id type), with its contract, judged and ungated"
    );
}
