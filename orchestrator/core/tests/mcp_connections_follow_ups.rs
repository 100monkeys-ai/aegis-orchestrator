// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `mailbox` and `account` arguments are optional (Zaru ADR-0069 D2,
//! D4): an absent one is filled from the person's own binding before the
//! input contract's check, so neither the advertised schema nor the contract
//! requires it. A call with no binding is still refused with D3's sentence
//! (`mcp_connections_orchestrator.rs`, Trigger 1 (b)), and the fill itself is
//! Trigger 1 (a) there.

use aegis_orchestrator_core::domain::mcp::ToolInputContract;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use serde_json::json;

/// The argument each family's tools name their binding by, and the words its
/// description must carry.
fn binding_argument(tool: &str) -> Option<(&'static str, &'static str)> {
    if tool.starts_with("mail.") {
        Some(("mailbox", "one of your mailboxes"))
    } else if tool.starts_with("calendar.") {
        Some(("account", "one of your calendar accounts"))
    } else {
        None
    }
}

/// Every listed mail and calendar tool declares its binding argument as an
/// optional string, described as naming one of the person's own and as one
/// that may be left out with one connected; its input contract does not
/// require it.
#[tokio::test]
async fn every_mail_and_calendar_tool_leaves_its_mailbox_or_account_optional() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let tools = router.list_tools().await.unwrap();
    let mut seen = 0;
    let mut wrong = Vec::new();
    for tool in &tools {
        let Some((argument, names)) = binding_argument(&tool.name) else {
            continue;
        };
        seen += 1;
        let property = &tool.input_schema["properties"][argument];
        if property["type"] != "string" {
            wrong.push(format!("{} does not declare {argument}", tool.name));
        }
        let description = property["description"].as_str().unwrap_or_default();
        if !description.contains(names) || !description.contains("may be left out") {
            wrong.push(format!("{}'s {argument}: {description:?}", tool.name));
        }
        let schema_required: Vec<&str> = tool.input_schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if schema_required.contains(&argument) {
            wrong.push(format!("{}'s schema requires {argument}", tool.name));
        }
        if ToolInputContract::required_fields(&tool.name).contains(&argument) {
            wrong.push(format!(
                "{}'s input contract requires {argument}",
                tool.name
            ));
        }
    }
    assert!(seen >= 16, "only {seen} mail and calendar tools are listed");
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A call with no `mailbox` (or `account`) passes the input contract, which
/// still holds the tool's other required arguments.
#[test]
fn the_contract_passes_an_absent_mailbox_or_account_and_holds_the_rest() {
    let mut wrong = Vec::new();
    for (tool, args) in [
        ("mail.list", json!({})),
        ("mail.read", json!({"thread_id": "t-1"})),
        (
            "mail.send",
            json!({"to": ["ann@example.test"], "subject": "Hi", "body": "Hello"}),
        ),
        ("calendar.calendars", json!({})),
        ("calendar.list", json!({"calendar_id": "primary"})),
    ] {
        if let Err(refusal) = ToolInputContract::validate(tool, &args) {
            wrong.push(format!("{tool}: {refusal}"));
        }
    }
    for (tool, args, missing) in [
        ("mail.read", json!({}), "thread_id"),
        ("calendar.list", json!({}), "calendar_id"),
    ] {
        match ToolInputContract::validate(tool, &args) {
            Err(refusal) if refusal.contains(missing) => {}
            other => wrong.push(format!("{tool} without {missing}: {other:?}")),
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
