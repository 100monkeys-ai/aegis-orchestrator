// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The mail read tools against a loopback IMAP stand-in (AEGIS ADR-125 D4;
//! its Update of 2026-10-07 clauses 1, 2 and 7).
//!
//! - `mail.list` groups the inbox into threads, newest first.
//! - `mail.read` answers a thread's messages oldest first and sets no
//!   `\Seen` (every body is fetched with `BODY.PEEK`).
//! - `mail.label` stores keywords and `\Flagged` with `UID STORE`, and
//!   refuses a keyword the server will not keep before any store.
//! - Who may use a mailbox: the person, the mailbox's owner, the run's
//!   choice for `imap` and the grant.
//! - The production connector's guard refuses a loopback mail server.
//!
//! The mailbox source here answers one mailbox for its owner; the real
//! source's ownership checks are tested beside it in the crate
//! (`tool_invocation_service/mail_tools_tests.rs`).

#[path = "support/mail_standins.rs"]
mod mail_standins;

use aegis_orchestrator_core::application::credential_service::{
    ToolCallActor, ToolMailbox, ToolMailboxSource,
};
use aegis_orchestrator_core::application::tools::builtin_mail::{
    MailActing, MailTools, CHOSEN_DIFFERENT, NONE_CHOSEN, NOT_GRANTED, NO_PERSON,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, MailSecurity, MailboxSettings,
};
use aegis_orchestrator_core::domain::execution::ContextChoice;
use aegis_orchestrator_core::domain::seal_session::{CallerAnswer, SealSessionError};
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::tenant::TenantId;
use async_trait::async_trait;
use mail_standins::{imap_mailbox_standin, MailboxStandIn, PlainConnector, StoredMessage};
use serde_json::{json, Value};
use std::sync::Arc;

const USER: &str = "mail-owner";
const LOGIN: &str = "owner@example.test";
const PASSWORD: &str = "Mk7-mailbox-password";

/// Answers one mailbox, to its owner only.
struct OneMailbox {
    id: CredentialBindingId,
    port: u16,
    host: String,
    granted: bool,
}

#[async_trait]
impl ToolMailboxSource for OneMailbox {
    async fn tool_mailbox(
        &self,
        actor: &ToolCallActor<'_>,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<Option<ToolMailbox>> {
        if *binding_id != self.id || actor.user_id != USER {
            return Ok(None);
        }
        Ok(Some(ToolMailbox {
            binding_id: self.id,
            settings: MailboxSettings {
                address: LOGIN.to_string(),
                display_name: None,
                imap_host: self.host.clone(),
                imap_port: self.port,
                imap_security: MailSecurity::Tls,
                smtp_host: self.host.clone(),
                smtp_port: 465,
                smtp_security: MailSecurity::Tls,
                username: LOGIN.to_string(),
            },
            password: SensitiveString::new(PASSWORD),
            granted: self.granted,
        }))
    }
}

/// The inbox: thread `<a1@x>` (uids 1 and 3), thread `<b1@x>` (uid 2, unread)
/// and a message with no `Message-ID` (uid 4).
fn inbox() -> Vec<StoredMessage> {
    vec![
        StoredMessage::new(
            1,
            &["\\Seen"],
            "05-Oct-2026 09:00:00 +0000",
            &[
                "Message-ID: <a1@x>",
                "From: Ann <ann@example.test>",
                "To: owner@example.test",
                "Subject: Invoice",
                "Date: Mon, 05 Oct 2026 09:00:00 +0000",
            ],
            "Please find the invoice.",
        ),
        StoredMessage::new(
            2,
            &[],
            "05-Oct-2026 10:00:00 +0000",
            &[
                "Message-ID: <b1@x>",
                "From: Bob <bob@example.test>",
                "To: owner@example.test",
                "Subject: Lunch",
                "Date: Mon, 05 Oct 2026 10:00:00 +0000",
            ],
            "Lunch tomorrow?",
        ),
        StoredMessage::new(
            3,
            &["\\Seen", "zaru/lead"],
            "06-Oct-2026 08:30:00 +0000",
            &[
                "Message-ID: <a2@x>",
                "In-Reply-To: <a1@x>",
                "References: <a1@x>",
                "From: owner@example.test",
                "To: Ann <ann@example.test>, accounts@example.test",
                "Subject: Re: Invoice",
                "Date: Tue, 06 Oct 2026 08:30:00 +0000",
                "Content-Type: multipart/mixed; boundary=\"BB\"",
            ],
            "--BB\nContent-Type: text/plain; charset=utf-8\n\nThanks, paid.\n--BB\nContent-Type: application/pdf; name=\"receipt.pdf\"\nContent-Transfer-Encoding: base64\n\nAAEC\n--BB--\n",
        ),
        StoredMessage::new(
            4,
            &[],
            "06-Oct-2026 12:00:00 +0000",
            &[
                "From: Carol <carol@example.test>",
                "Subject: No id",
                "Date: Tue, 06 Oct 2026 12:00:00 +0000",
            ],
            "Hello.",
        ),
    ]
}

struct Fixture {
    mailbox: MailboxStandIn,
    tools: MailTools,
    id: CredentialBindingId,
}

async fn fixture(permanent_flags: &str, granted: bool) -> Fixture {
    let mailbox = imap_mailbox_standin(LOGIN, PASSWORD, inbox(), permanent_flags).await;
    let id = CredentialBindingId::new();
    let source = Arc::new(OneMailbox {
        id,
        port: mailbox.port(),
        host: "127.0.0.1".to_string(),
        granted,
    });
    Fixture {
        tools: MailTools::with_connector(source, Arc::new(PlainConnector)),
        mailbox,
        id,
    }
}

/// A conversation's call: the owner, no execution record, nothing chosen.
fn conversation() -> MailActing {
    MailActing {
        tenant_id: TenantId::default(),
        user_id: Some(USER.to_string()),
        agent_id: AgentId::new(),
        workflow_id: None,
        choice: ContextChoice::NotGiven,
        has_execution_record: false,
    }
}

/// An agent's run for the owner, nothing chosen.
fn agent_run() -> MailActing {
    MailActing {
        has_execution_record: true,
        ..conversation()
    }
}

/// The sentence a refusal tells its caller.
fn sentence(error: &SealSessionError) -> String {
    match error {
        SealSessionError::Answered {
            answer: CallerAnswer::CredentialBindingRequired { message },
            ..
        } => message.clone(),
        SealSessionError::Answered {
            answer: CallerAnswer::NotFound(message),
            ..
        } => message.clone(),
        SealSessionError::InvalidArguments(message)
        | SealSessionError::UpstreamUnavailable(message) => message.clone(),
        other => format!("{other:?}"),
    }
}

/// Whether `error` is the binding-required refusal.
fn binding_required(error: &SealSessionError) -> bool {
    matches!(
        error,
        SealSessionError::Answered {
            answer: CallerAnswer::CredentialBindingRequired { .. },
            ..
        }
    )
}

async fn call(
    f: &Fixture,
    tool: &str,
    mut args: Value,
    acting: &MailActing,
) -> Result<Value, SealSessionError> {
    args["mailbox"] = json!(f.id.0.to_string());
    f.tools.invoke(tool, &args, acting).await
}

#[tokio::test]
async fn mail_list_answers_the_inbox_grouped_into_threads_newest_first() {
    let f = fixture("\\Seen \\Flagged \\*", false).await;
    let answer = call(&f, "mail.list", json!({}), &conversation())
        .await
        .unwrap();
    let ids: Vec<&str> = answer["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["thread_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["uid:7:4", "<a1@x>", "<b1@x>"],
        "the threads were not grouped newest first: {answer}"
    );
    let invoice = &answer["threads"][1];
    assert_eq!(invoice["message_count"], 2, "{answer}");
    assert_eq!(invoice["unread_count"], 0, "{answer}");
    assert_eq!(invoice["subject"], "Re: Invoice", "{answer}");
    assert_eq!(
        invoice["participants"],
        json!(["Ann <ann@example.test>", "owner@example.test"]),
        "{answer}"
    );
    assert_eq!(invoice["keywords"], json!(["zaru/lead"]), "{answer}");
    assert_eq!(invoice["latest_date"], "2026-10-06T08:30:00Z", "{answer}");
    assert_eq!(answer["threads"][2]["unread_count"], 1, "{answer}");
    assert_eq!(answer["matched_messages"], 4, "{answer}");
    assert_eq!(answer["truncated"], false, "{answer}");
    let commands = f.mailbox.commands();
    assert!(
        commands.iter().any(|c| c.contains(" EXAMINE ")),
        "mail.list did not open the inbox read-only: {commands:?}"
    );
}

#[tokio::test]
async fn mail_list_searches_with_the_query_and_reports_truncation() {
    let f = fixture("\\Seen \\Flagged \\*", false).await;
    let answer = call(
        &f,
        "mail.list",
        json!({"query": "invoice", "unread_only": false, "limit": 1}),
        &conversation(),
    )
    .await
    .unwrap();
    assert_eq!(answer["threads"].as_array().unwrap().len(), 1, "{answer}");
    assert_eq!(answer["threads"][0]["thread_id"], "<a1@x>", "{answer}");
    assert_eq!(answer["matched_messages"], 2, "{answer}");
    let commands = f.mailbox.commands();
    assert!(
        commands
            .iter()
            .any(|c| c.contains("UID SEARCH TEXT \"invoice\"")),
        "the query was not a TEXT search: {commands:?}"
    );
}

#[tokio::test]
async fn mail_read_answers_a_threads_messages_oldest_first_and_marks_nothing_read() {
    let f = fixture("\\Seen \\Flagged \\*", false).await;
    let answer = call(
        &f,
        "mail.read",
        json!({"thread_id": "<a1@x>"}),
        &conversation(),
    )
    .await
    .unwrap();
    let uids: Vec<u64> = answer["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["uid"].as_u64().unwrap())
        .collect();
    assert_eq!(
        uids,
        vec![1, 3],
        "the thread's messages were not oldest first: {answer}"
    );
    let reply = &answer["messages"][1];
    assert_eq!(reply["body_text"], "Thanks, paid.", "{answer}");
    assert_eq!(reply["in_reply_to"], "<a1@x>", "{answer}");
    assert_eq!(
        reply["to"],
        json!(["Ann <ann@example.test>", "accounts@example.test"]),
        "{answer}"
    );
    assert_eq!(
        reply["attachments"],
        json!([{"filename": "receipt.pdf", "content_type": "application/pdf", "size": 3}]),
        "{answer}"
    );

    // An unread thread stays unread.
    let unread = call(
        &f,
        "mail.read",
        json!({"thread_id": "<b1@x>"}),
        &conversation(),
    )
    .await
    .unwrap();
    assert_eq!(unread["messages"][0]["seen"], false, "{unread}");
    assert!(
        !f.mailbox.flags_of(2).iter().any(|x| x == "\\Seen"),
        "mail.read marked the message read: {:?}",
        f.mailbox.flags_of(2)
    );
    let commands = f.mailbox.commands();
    let fetches: Vec<&String> = commands
        .iter()
        .filter(|c| c.contains("UID FETCH"))
        .collect();
    assert!(
        fetches.iter().any(|c| c.contains("BODY.PEEK[]")),
        "no body was fetched with PEEK: {commands:?}"
    );
    assert!(
        !fetches.iter().any(|c| c.contains("BODY[]")),
        "a body was fetched without PEEK: {commands:?}"
    );
}

#[tokio::test]
async fn mail_read_of_a_message_with_no_message_id_reads_it_by_uid() {
    let f = fixture("\\Seen \\Flagged \\*", false).await;
    let answer = call(
        &f,
        "mail.read",
        json!({"thread_id": "uid:7:4"}),
        &conversation(),
    )
    .await
    .unwrap();
    assert_eq!(answer["messages"][0]["uid"], 4, "{answer}");
    assert_eq!(answer["messages"][0]["body_text"], "Hello.", "{answer}");
}

#[tokio::test]
async fn mail_label_stores_the_keywords_and_the_flag_and_takes_them_off_again() {
    let f = fixture("\\Seen \\Flagged \\*", false).await;
    let answer = call(
        &f,
        "mail.label",
        json!({"thread_id": "<a1@x>", "add": ["zaru/triaged"], "flagged": true}),
        &conversation(),
    )
    .await
    .unwrap();
    assert_eq!(answer["message_uids"], json!([1, 3]), "{answer}");
    let commands = f.mailbox.commands();
    assert!(
        commands
            .iter()
            .any(|c| c.ends_with("UID STORE 1,3 +FLAGS (zaru/triaged)")),
        "the keyword was not stored: {commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|c| c.ends_with("UID STORE 1,3 +FLAGS (\\Flagged)")),
        "the flag was not set with +FLAGS (\\Flagged): {commands:?}"
    );
    assert!(f.mailbox.flags_of(3).iter().any(|x| x == "\\Flagged"));
    assert!(f.mailbox.flags_of(1).iter().any(|x| x == "zaru/triaged"));

    call(
        &f,
        "mail.label",
        json!({"thread_id": "<a1@x>", "remove": ["zaru/lead"], "flagged": false}),
        &conversation(),
    )
    .await
    .unwrap();
    let commands = f.mailbox.commands();
    assert!(
        commands
            .iter()
            .any(|c| c.ends_with("UID STORE 1,3 -FLAGS (zaru/lead)")),
        "the keyword was not removed: {commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|c| c.ends_with("UID STORE 1,3 -FLAGS (\\Flagged)")),
        "the flag was not cleared with -FLAGS (\\Flagged): {commands:?}"
    );
    assert!(!f.mailbox.flags_of(3).iter().any(|x| x == "\\Flagged"));
    assert!(
        !commands.iter().any(|c| c.contains(".SILENT")),
        "a store was sent .SILENT: {commands:?}"
    );
}

#[tokio::test]
async fn mail_label_refuses_a_keyword_the_server_will_not_keep_before_any_store() {
    let f = fixture("\\Seen \\Flagged", false).await;
    let error = call(
        &f,
        "mail.label",
        json!({"thread_id": "<a1@x>", "add": ["zaru/triaged"], "flagged": true}),
        &conversation(),
    )
    .await
    .unwrap_err();
    assert!(
        sentence(&error).contains("does not keep the label 'zaru/triaged'"),
        "an unkept keyword was not refused: {error}"
    );
    let commands = f.mailbox.commands();
    assert!(
        !commands.iter().any(|c| c.contains("STORE")),
        "a store was sent before the refusal: {commands:?}"
    );
}

#[tokio::test]
async fn a_call_with_no_person_is_refused_before_any_connection() {
    let f = fixture("\\*", true).await;
    let acting = MailActing {
        user_id: None,
        ..agent_run()
    };
    let error = call(&f, "mail.list", json!({}), &acting).await.unwrap_err();
    assert_eq!(sentence(&error), NO_PERSON, "{error:?}");
    assert!(binding_required(&error), "{error:?}");
    assert_eq!(f.mailbox.connections(), 0);
}

#[tokio::test]
async fn a_mailbox_that_is_not_the_persons_own_is_refused() {
    let f = fixture("\\*", true).await;
    let other = CredentialBindingId::new();
    let error = f
        .tools
        .invoke(
            "mail.list",
            &json!({"mailbox": other.0.to_string()}),
            &conversation(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        sentence(&error),
        format!(
            "'{}' is not an active mailbox connection of yours.",
            other.0
        ),
        "{error:?}"
    );
    let stranger = MailActing {
        user_id: Some("someone-else".to_string()),
        ..conversation()
    };
    let error = call(&f, "mail.list", json!({}), &stranger)
        .await
        .unwrap_err();
    assert!(
        sentence(&error).ends_with("is not an active mailbox connection of yours."),
        "another person's call reached the mailbox: {error:?}"
    );
    let error = f
        .tools
        .invoke(
            "mail.list",
            &json!({"mailbox": "not-a-uuid"}),
            &conversation(),
        )
        .await
        .unwrap_err();
    assert!(
        sentence(&error).ends_with("'not-a-uuid' is not an active mailbox connection of yours."),
        "{error:?}"
    );
    assert_eq!(f.mailbox.connections(), 0);
}

#[tokio::test]
async fn a_chosen_mailbox_other_than_the_argument_or_none_is_refused() {
    let f = fixture("\\*", true).await;
    let chosen_other = MailActing {
        choice: ContextChoice::Binding(CredentialBindingId::new()),
        ..conversation()
    };
    let error = call(&f, "mail.list", json!({}), &chosen_other)
        .await
        .unwrap_err();
    assert_eq!(sentence(&error), CHOSEN_DIFFERENT, "{error:?}");
    let chosen_none = MailActing {
        choice: ContextChoice::None,
        ..agent_run()
    };
    let error = call(&f, "mail.list", json!({}), &chosen_none)
        .await
        .unwrap_err();
    assert_eq!(sentence(&error), NONE_CHOSEN, "{error:?}");
    assert_eq!(f.mailbox.connections(), 0);

    // The chosen mailbox itself is admitted, no grant needed.
    let chosen = MailActing {
        choice: ContextChoice::Binding(f.id),
        ..agent_run()
    };
    let f = fixture("\\*", false).await;
    let chosen = MailActing {
        choice: ContextChoice::Binding(f.id),
        ..chosen
    };
    call(&f, "mail.list", json!({}), &chosen).await.unwrap();
}

#[tokio::test]
async fn an_agents_run_with_nothing_chosen_needs_the_grant() {
    let ungranted = fixture("\\*", false).await;
    let error = call(&ungranted, "mail.list", json!({}), &agent_run())
        .await
        .unwrap_err();
    assert_eq!(sentence(&error), NOT_GRANTED, "{error:?}");
    assert_eq!(ungranted.mailbox.connections(), 0);

    let granted = fixture("\\*", true).await;
    call(&granted, "mail.list", json!({}), &agent_run())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_conversation_with_nothing_chosen_is_admitted_on_ownership_alone() {
    let f = fixture("\\*", false).await;
    let answer = call(&f, "mail.list", json!({}), &conversation())
        .await
        .unwrap();
    assert_eq!(answer["threads"].as_array().unwrap().len(), 3, "{answer}");
}

#[tokio::test]
async fn the_production_connector_refuses_a_loopback_mail_server() {
    let mailbox = imap_mailbox_standin(LOGIN, PASSWORD, inbox(), "\\*").await;
    let id = CredentialBindingId::new();
    let tools = MailTools::new(Arc::new(OneMailbox {
        id,
        port: 993,
        host: "127.0.0.1".to_string(),
        granted: true,
    }));
    let error = tools
        .invoke(
            "mail.list",
            &json!({"mailbox": id.0.to_string()}),
            &conversation(),
        )
        .await
        .unwrap_err();
    assert!(
        sentence(&error).contains("is not one this node may reach")
            && sentence(&error).contains("loopback"),
        "the guard did not refuse a loopback server: {error:?}"
    );
    assert_eq!(mailbox.connections(), 0);
}
