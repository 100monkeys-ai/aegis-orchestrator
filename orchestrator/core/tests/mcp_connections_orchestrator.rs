// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A remote MCP client reaches what its person connected to Zaru (Zaru
//! ADR-0069, Trigger 1): calls through the invoke route on a session attested
//! as the Zaru MCP server attests one (the key's user, the tier context, no
//! execution record).
//!
//! - (a) a person with one mailbox calls `mail.list` with no `mailbox`: the
//!   call answers that mailbox's threads and names it (D4);
//! - (b) a person with no calendar is refused `calendar.list` with D3's
//!   sentence (the listing half is `mcp_connections_orchestrator_listing.rs`);
//! - (e) `mail.send` answers `approval_pending` with D5's sentence, writes its
//!   request for the key's user, and passes as `auto_allowed` under a standing
//!   choice;
//! - (f) every D3 sentence names the Connections page and matches no record
//!   pattern (the pattern of `facing_text_no_record_refs.rs`).
//!
//! (c) is in `gateway_wire_tests.rs`, (d) in `operator_escalation_tests.rs`.
//!
//! The session is an API key with no profile, so (a) and (e) choose the
//! person's mailbox in `_meta.contexts`, as a remote client does through
//! `x-zaru-contexts`: a key reaches a connection only through a profile or
//! bindings the call chose (Zaru ADR-0071 D1, D3), and with neither it is
//! refused (`deny_by_default_orchestrator_3_calls.rs`).

#[path = "support/mcp_connections_orchestrator_fixture.rs"]
mod fixture;
#[path = "support/mail_standins.rs"]
mod mail_standins;

use std::sync::Arc;

use aegis_orchestrator_core::domain::credential::CredentialBindingId;
use aegis_orchestrator_core::domain::seal_session::{
    CallerAnswer, EnvelopeVerifier, SealSessionError,
};
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::tool_approval::{
    ToolApprovalPolicy, ToolApprovalPolicyEffect, ToolApprovalPolicyId, ToolApprovalRepository,
};
use fixture::*;
use mail_standins::{imap_mailbox_standin, PlainConnector};
use serde_json::{json, Value};

/// A call as `POST /v1/seal/invoke` receives it, with its own token.
struct Call {
    token: SensitiveString,
    tool: String,
    arguments: Value,
}

impl EnvelopeVerifier for Call {
    fn security_token(&self) -> &SensitiveString {
        &self.token
    }
    fn verify_signature(&self, _: &[u8]) -> Result<(), SealSessionError> {
        Ok(())
    }
    fn extract_tool_name(&self) -> Option<String> {
        Some(self.tool.clone())
    }
    fn extract_arguments(&self) -> Option<Value> {
        Some(self.arguments.clone())
    }
    fn replay_nonce(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

/// A call of the key's session choosing the person's mailbox in
/// `_meta.contexts` (Zaru ADR-0071 D3).
async fn call_choosing_the_mailbox(
    f: &Fixture,
    tool: &str,
    arguments: Value,
) -> Result<Value, SealSessionError> {
    f.service
        .invoke_tool_with_meta(
            &Call {
                token: f.token.clone().into(),
                tool: tool.to_string(),
                arguments,
            },
            Some(&json!({ "contexts": { "imap": [f.mailbox.0.to_string()] } })),
        )
        .await
}

// ---------------------------------------------------------------------------
// Trigger 1
// ---------------------------------------------------------------------------

/// (a) D4: one mailbox chosen and no `mailbox`: `mail.list` answers that
/// mailbox's threads and names it by its context name.
#[tokio::test]
async fn a_mail_list_with_no_mailbox_answers_the_persons_one_mailbox_and_names_it() {
    let standin = imap_mailbox_standin(LOGIN, PASSWORD, inbox(), "\\Seen \\*").await;
    let f = fixture(standin.port(), Arc::new(PlainConnector)).await;
    let answer = call_choosing_the_mailbox(&f, "mail.list", json!({})).await;
    let answer = match answer {
        Ok(answer) => answer,
        Err(_) => panic!("mail.list with no mailbox was refused: {}", told(&answer)),
    };
    let subjects: Vec<&str> = answer["threads"]
        .as_array()
        .map(|threads| {
            threads
                .iter()
                .filter_map(|t| t["subject"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        subjects,
        vec!["Quarterly figures"],
        "not the mailbox's threads: {answer}"
    );
    assert_eq!(answer["mailbox"], json!(f.mailbox.0.to_string()));
    assert_eq!(
        answer["mailbox_name"],
        json!(MAILBOX_NAME),
        "the answer does not name the mailbox: {answer}"
    );
}

/// (b) D1, D3: a person with no calendar is refused `calendar.list` before
/// anything runs, with D3's sentence under `CREDENTIAL_BINDING_REQUIRED`.
#[tokio::test]
async fn b_a_calendar_call_with_no_calendar_is_refused_with_the_connections_sentence() {
    let f = fixture(1, Arc::new(StopAtTheServer)).await;
    let refused = f
        .call("calendar.list", json!({ "calendar_id": "personal" }))
        .await;
    assert_eq!(
        told(&refused),
        format!(
            "CREDENTIAL_BINDING_REQUIRED: {}",
            not_connected("a calendar")
        )
    );
}

/// (e) D5: `mail.send` through the session waits for the key's user, with
/// D5's sentence and a request written for that user; under a standing
/// choice it passes the gate as `auto_allowed`.
#[tokio::test]
async fn e_a_mail_send_waits_for_the_keys_user_and_passes_under_a_standing_choice() {
    let f = fixture(1, Arc::new(StopAtTheServer)).await;
    let send = json!({
        "mailbox": f.mailbox.0.to_string(),
        "to": ["ann@example.test"],
        "subject": "Figures",
        "body": "Thanks, received."
    });
    let pending = call_choosing_the_mailbox(&f, "mail.send", send.clone()).await;
    let pending = match pending {
        Ok(pending) => pending,
        Err(_) => panic!("mail.send was refused: {}", told(&pending)),
    };
    let mut wrong = Vec::new();
    if pending["status"] != json!("approval_pending") || pending["approval_id"].is_null() {
        wrong.push(format!("not pending: {pending}"));
    }
    if pending["message"] != json!(WAITS) {
        wrong.push(format!("no waiting sentence: {pending}"));
    }
    let rows = f.approvals.list_requests(None).await.unwrap();
    if rows.len() != 1 || rows[0].user_sub != PERSON || rows[0].tool_name != "mail.send" {
        wrong.push(format!(
            "the request is not the key's user's: {:?}",
            rows.iter()
                .map(|r| (r.user_sub.clone(), r.tool_name.clone()))
                .collect::<Vec<_>>()
        ));
    }
    f.approvals
        .insert_policy(&ToolApprovalPolicy {
            id: ToolApprovalPolicyId::new(),
            tenant_id: f.tenant.clone(),
            user_sub: PERSON.to_string(),
            tool_name: "mail.send".to_string(),
            binding_id: Some(f.mailbox.0.to_string()),
            profile_id: None,
            effect: ToolApprovalPolicyEffect::Allow,
            created_at: chrono::Utc::now(),
            created_by: PERSON.to_string(),
            revoked_at: None,
        })
        .await
        .unwrap();
    let allowed = call_choosing_the_mailbox(&f, "mail.send", send).await;
    if !told(&allowed).contains(REACHED) {
        wrong.push(format!(
            "under the standing choice the call did not run: {}",
            told(&allowed)
        ));
    }
    let statuses: Vec<String> = f
        .approvals
        .list_requests(None)
        .await
        .unwrap()
        .iter()
        .map(|r| format!("{:?}", r.status))
        .collect();
    if !statuses.iter().any(|s| s == "AutoAllowed") {
        wrong.push(format!("no auto_allowed request: {statuses:?}"));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// (f) D3: a call for each family the person has not connected is refused
/// under `CREDENTIAL_BINDING_REQUIRED` with its own sentence, naming the
/// Connections page and no record.
#[tokio::test]
async fn f_every_not_connected_sentence_names_the_connections_page_and_no_record() {
    let f = fixture_over(
        Arc::new(NoMailboxes),
        CredentialBindingId::new(),
        Arc::new(StopAtTheServer),
    )
    .await;
    let calls = [
        ("a mailbox", "mail.list", json!({})),
        (
            "a calendar",
            "calendar.list",
            json!({ "calendar_id": "personal" }),
        ),
        ("GitHub", "github.list_issues", json!({})),
        ("Carbon Forge", "nuclear-notes.pages.read", json!({})),
        ("notes-own", "notes-own.search", json!({})),
    ];
    let mut wrong = Vec::new();
    for (what, tool, arguments) in calls {
        let result = f.call(tool, arguments).await;
        let said = told(&result);
        if said != format!("CREDENTIAL_BINDING_REQUIRED: {}", not_connected(what)) {
            wrong.push(format!("{tool}: {said}"));
        }
        if record_pattern().is_match(&said) {
            wrong.push(format!("{tool} names a record: {said}"));
        }
        if let Err(SealSessionError::Answered { answer, .. }) = &result {
            // Zaru ADR-0070 D1: the refusal carries its family's `needs`.
            let bare = match answer {
                CallerAnswer::Needs { answer, .. } => answer.as_ref(),
                other => other,
            };
            if !matches!(bare, CallerAnswer::CredentialBindingRequired { .. }) {
                wrong.push(format!("{tool} answered {answer:?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
