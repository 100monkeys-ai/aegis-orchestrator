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
//! - An OAuth mailbox authenticates with SASL `XOAUTH2` and the token its
//!   source answers, never `LOGIN`; a refused token answers the session
//!   failure with the decoded challenge, and neither the token nor its
//!   base64 response reaches the error (AEGIS ADR-125's Update of
//!   2026-10-07 clauses 8 and 10).
//!
//! - The outbound tools (its Update of 2026-10-07 (3), clauses 13 to 15):
//!   `mail.draft` appends to Drafts with `\Draft \Seen`; `mail.send`
//!   submits over SMTP (`AUTH XOAUTH2` for an OAuth mailbox, else `AUTH
//!   PLAIN`, or `AUTH LOGIN` when only LOGIN is offered) with a
//!   `Message-ID` the orchestrator minted, then appends to Sent unless the
//!   Sent folder already holds it; `mail.reply` threads by `In-Reply-To`
//!   and `References`; a refused recipient sends nothing; malformed
//!   arguments are refused before any connection.
//!
//! - `mail.delete` (its Update of 2026-10-08 (4), clauses 17 to 19) moves a
//!   thread's `INBOX` messages to Trash (`\Trash`, else `Trash` or
//!   `Deleted`) by `UID MOVE`, else by `UID COPY`, `UID STORE +FLAGS
//!   (\Deleted)` and `UID EXPUNGE` of exactly those UIDs; it refuses a
//!   mailbox with no Trash folder and a server that can do neither, with
//!   nothing changed; it never sends a plain `EXPUNGE` and never selects
//!   Trash.
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
use aegis_orchestrator_core::domain::execution::ServerChoice;
use aegis_orchestrator_core::domain::seal_session::{CallerAnswer, SealSessionError};
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::mail::MailAuth;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use mail_standins::{
    imap_mailbox_standin, imap_mailbox_standin_with_capabilities,
    imap_mailbox_standin_with_folders, imap_xoauth2_standin, imap_xoauth2_standin_with_folders,
    smtp_submission_standin, xoauth2_string, MailboxStandIn, PlainConnector, SmtpSubmission,
    StandInFolder, StoredMessage, SubmitAuth, RCPT_REFUSAL, XOAUTH2_CHALLENGE,
    XOAUTH2_IMAP_REFUSAL,
};
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
            auth: MailAuth::Password(SensitiveString::new(PASSWORD)),
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
        choice: ServerChoice::NotGiven,
        has_execution_record: false,
        tier: None,
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
        json!([{"part": "2", "filename": "receipt.pdf", "content_type": "application/pdf", "size": 3}]),
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
        choice: ServerChoice::Bindings(vec![CredentialBindingId::new()]),
        ..conversation()
    };
    let error = call(&f, "mail.list", json!({}), &chosen_other)
        .await
        .unwrap_err();
    assert_eq!(sentence(&error), CHOSEN_DIFFERENT, "{error:?}");
    let chosen_none = MailActing {
        choice: ServerChoice::None,
        ..agent_run()
    };
    let error = call(&f, "mail.list", json!({}), &chosen_none)
        .await
        .unwrap_err();
    assert_eq!(sentence(&error), NONE_CHOSEN, "{error:?}");
    assert_eq!(f.mailbox.connections(), 0);

    // The chosen mailbox itself is admitted, no grant needed.
    let chosen = MailActing {
        choice: ServerChoice::Bindings(vec![f.id]),
        ..agent_run()
    };
    let f = fixture("\\*", false).await;
    let chosen = MailActing {
        choice: ServerChoice::Bindings(vec![f.id]),
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

// ---------------------------------------------------------------------------
// An OAuth mailbox: SASL XOAUTH2 (ADR-125's Update of 2026-10-07 clauses 8, 10)
// ---------------------------------------------------------------------------

const OAUTH_ADDRESS: &str = "jeshua@workspace.example.test";
const TOKEN: &str = "ya29.Mk7-xoauth2-access-token";

/// Answers one OAuth mailbox to its owner, authenticating by XOAUTH2 with
/// `token`, and records each binding it was asked for.
struct OAuthMailbox {
    id: CredentialBindingId,
    port: u16,
    token: String,
    asked: std::sync::Mutex<Vec<CredentialBindingId>>,
}

#[async_trait]
impl ToolMailboxSource for OAuthMailbox {
    async fn tool_mailbox(
        &self,
        actor: &ToolCallActor<'_>,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<Option<ToolMailbox>> {
        self.asked.lock().unwrap().push(*binding_id);
        if *binding_id != self.id || actor.user_id != USER {
            return Ok(None);
        }
        Ok(Some(ToolMailbox {
            binding_id: self.id,
            settings: MailboxSettings {
                address: OAUTH_ADDRESS.to_string(),
                display_name: None,
                imap_host: "127.0.0.1".to_string(),
                imap_port: self.port,
                imap_security: MailSecurity::Tls,
                smtp_host: "127.0.0.1".to_string(),
                smtp_port: 465,
                smtp_security: MailSecurity::Tls,
                username: OAUTH_ADDRESS.to_string(),
            },
            auth: MailAuth::XOAuth2(SensitiveString::new(self.token.clone())),
            granted: false,
        }))
    }
}

/// The XOAUTH2 stand-in holding the inbox, and tools whose source answers
/// `token` for it.
async fn oauth_fixture(
    token: &str,
) -> (
    MailboxStandIn,
    MailTools,
    CredentialBindingId,
    Arc<OAuthMailbox>,
) {
    let mailbox = imap_xoauth2_standin(OAUTH_ADDRESS, TOKEN, inbox(), "\\*").await;
    let id = CredentialBindingId::new();
    let source = Arc::new(OAuthMailbox {
        id,
        port: mailbox.port(),
        token: token.to_string(),
        asked: std::sync::Mutex::new(Vec::new()),
    });
    let tools = MailTools::with_connector(source.clone(), Arc::new(PlainConnector));
    (mailbox, tools, id, source)
}

#[tokio::test]
async fn an_oauth_mailbox_authenticates_with_xoauth2_and_never_logs_in() {
    let (mailbox, tools, id, source) = oauth_fixture(TOKEN).await;
    let answer = tools
        .invoke(
            "mail.list",
            &json!({"mailbox": id.0.to_string()}),
            &conversation(),
        )
        .await;
    let commands = mailbox.commands();
    assert!(
        !commands.iter().any(|c| c
            .split_whitespace()
            .nth(1)
            .is_some_and(|v| v.eq_ignore_ascii_case("LOGIN"))),
        "a LOGIN was sent to an OAuth mailbox: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c == "A1 AUTHENTICATE XOAUTH2"),
        "no AUTHENTICATE XOAUTH2 was sent: {commands:?}"
    );
    assert_eq!(
        mailbox.standin.sasl(),
        vec![xoauth2_string(OAUTH_ADDRESS, TOKEN)],
        "the stand-in did not receive exactly the SASL string"
    );
    let answer = answer.expect("mail.list over XOAUTH2");
    assert_eq!(answer["threads"].as_array().map(Vec::len), Some(3));
    assert_eq!(source.asked.lock().unwrap().clone(), vec![id]);
}

#[tokio::test]
async fn a_refused_token_answers_the_session_failure_with_the_decoded_challenge_and_never_the_token(
) {
    let stale = "ya29.Mk7-refused-access-token";
    let (mailbox, tools, id, _) = oauth_fixture(stale).await;
    let error = tools
        .invoke(
            "mail.list",
            &json!({"mailbox": id.0.to_string()}),
            &conversation(),
        )
        .await
        .expect_err("the stand-in refuses the token");
    let said = sentence(&error);
    let base64_response = STANDARD.encode(xoauth2_string(OAUTH_ADDRESS, stale));
    assert_eq!(
        mailbox.standin.after_challenge(),
        vec![String::new()],
        "the client did not answer the challenge with an empty line"
    );
    assert!(!said.contains(stale), "the token reached the error: {said}");
    assert!(
        !said.contains(&base64_response),
        "the base64 response reached the error: {said}"
    );
    assert_eq!(
        said,
        format!(
            "The mail server did not complete the request: {XOAUTH2_IMAP_REFUSAL} for [REDACTED] = user={OAUTH_ADDRESS}auth=Bearer [REDACTED] {XOAUTH2_CHALLENGE}"
        ),
        "the refusal is not the session failure with the decoded challenge"
    );
}

// ---------------------------------------------------------------------------
// The outbound tools (ADR-125's Update of 2026-10-07 (3), clauses 13 to 15)
// ---------------------------------------------------------------------------

const ANN: &str = "ann@example.test";
const ACCOUNTS: &str = "accounts@example.test";

/// Answers one mailbox to its owner: its IMAP and SMTP stand-ins' ports,
/// its address, and how it authenticates.
struct SendMailbox {
    id: CredentialBindingId,
    imap_port: u16,
    smtp_port: u16,
    address: String,
    auth: MailAuth,
}

#[async_trait]
impl ToolMailboxSource for SendMailbox {
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
                address: self.address.clone(),
                display_name: Some("Mailbox Owner".to_string()),
                imap_host: "127.0.0.1".to_string(),
                imap_port: self.imap_port,
                imap_security: MailSecurity::Tls,
                smtp_host: "127.0.0.1".to_string(),
                smtp_port: self.smtp_port,
                smtp_security: MailSecurity::Starttls,
                username: self.address.clone(),
            },
            auth: self.auth.clone(),
            granted: false,
        }))
    }
}

/// Sent and Drafts, marked as RFC 6154 marks them.
fn sent_and_drafts() -> Vec<StandInFolder> {
    vec![
        StandInFolder::new("Sent Items", "\\Sent"),
        StandInFolder::new("Drafts", "\\Drafts"),
    ]
}

struct Outbound {
    mailbox: MailboxStandIn,
    smtp: SmtpSubmission,
    tools: MailTools,
    id: CredentialBindingId,
}

/// A password mailbox with `folders`, its SMTP stand-in offering `offered`.
async fn outbound(
    offered: fn(String, String) -> SubmitAuth,
    folders: Vec<StandInFolder>,
    refused: Option<&str>,
) -> Outbound {
    let mailbox = imap_mailbox_standin_with_folders(LOGIN, PASSWORD, inbox(), folders).await;
    let smtp =
        smtp_submission_standin(offered(LOGIN.to_string(), PASSWORD.to_string()), refused).await;
    outbound_over(
        mailbox,
        smtp,
        LOGIN,
        MailAuth::Password(SensitiveString::new(PASSWORD)),
    )
}

fn outbound_over(
    mailbox: MailboxStandIn,
    smtp: SmtpSubmission,
    address: &str,
    auth: MailAuth,
) -> Outbound {
    let id = CredentialBindingId::new();
    let source = Arc::new(SendMailbox {
        id,
        imap_port: mailbox.port(),
        smtp_port: smtp.port(),
        address: address.to_string(),
        auth,
    });
    Outbound {
        tools: MailTools::with_connector(source, Arc::new(PlainConnector)),
        mailbox,
        smtp,
        id,
    }
}

fn plain(user: String, password: String) -> SubmitAuth {
    SubmitAuth::Plain { user, password }
}

fn login_only(user: String, password: String) -> SubmitAuth {
    SubmitAuth::LoginOnly { user, password }
}

async fn send(o: &Outbound, tool: &str, mut args: Value) -> Result<Value, SealSessionError> {
    args["mailbox"] = json!(o.id.0.to_string());
    o.tools.invoke(tool, &args, &conversation()).await
}

/// A header's value in a raw message, folded lines joined.
fn header_of(raw: &str, name: &str) -> Option<String> {
    let head = raw.split("\r\n\r\n").next().unwrap_or("");
    let mut found: Option<String> = None;
    for line in head.split("\r\n") {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(v) = found.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if found.is_some() {
            break;
        }
        if let Some((n, v)) = line.split_once(':') {
            if n.trim().eq_ignore_ascii_case(name) {
                found = Some(v.trim().to_string());
            }
        }
    }
    found
}

/// The decoded text of a message whose body is base64.
fn body_of(raw: &str) -> String {
    let body: String = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or("")
        .split_whitespace()
        .collect();
    String::from_utf8(STANDARD.decode(body).unwrap_or_default()).unwrap_or_default()
}

/// A `Message-ID` the orchestrator minted for `address`: `<uuid@domain>`.
fn minted_for(message_id: &str, address: &str) -> bool {
    let domain = address.rsplit('@').next().unwrap_or_default();
    message_id
        .strip_prefix('<')
        .and_then(|m| m.strip_suffix('>'))
        .and_then(|m| m.split_once('@'))
        .is_some_and(|(local, d)| {
            d.eq_ignore_ascii_case(domain) && uuid::Uuid::parse_str(local).is_ok()
        })
}

#[tokio::test]
async fn mail_draft_appends_to_drafts_with_draft_and_seen() {
    let o = outbound(plain, sent_and_drafts(), None).await;
    let answer = send(
        &o,
        "mail.draft",
        json!({"to": [ANN], "subject": "Quarterly numbers", "body": "Draft text."}),
    )
    .await;
    let mut wrong = Vec::new();
    match &answer {
        Ok(_) => {}
        Err(e) => wrong.push(format!("mail.draft did not save a draft: {}", sentence(e))),
    }
    let drafts = o.mailbox.folder_messages("Drafts");
    if drafts.len() != 1 {
        wrong.push(format!("Drafts holds {} messages, not 1", drafts.len()));
    }
    if let (Some(draft), Ok(answer)) = (drafts.first(), &answer) {
        if draft.flags != vec!["\\Draft".to_string(), "\\Seen".to_string()] {
            wrong.push(format!("the draft's flags are {:?}", draft.flags));
        }
        let id = answer["message_id"].as_str().unwrap_or_default();
        if header_of(&draft.raw, "Message-ID").as_deref() != Some(id) || !minted_for(id, LOGIN) {
            wrong.push(format!(
                "the draft's Message-ID is not the minted {id}: {}",
                draft.raw
            ));
        }
        if header_of(&draft.raw, "Subject").as_deref() != Some("Quarterly numbers")
            || header_of(&draft.raw, "To").as_deref() != Some(ANN)
            || body_of(&draft.raw) != "Draft text."
        {
            wrong.push(format!("the draft is not the message given: {}", draft.raw));
        }
        if answer["folder"] != "Drafts" {
            wrong.push(format!("the answer does not name Drafts: {answer}"));
        }
    }
    if o.smtp.standin.connections() != 0 {
        wrong.push("a draft opened an SMTP session".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn mail_draft_without_a_drafts_folder_saves_nothing() {
    let o = outbound(plain, vec![StandInFolder::new("Sent", "\\Sent")], None).await;
    let error = send(&o, "mail.draft", json!({"body": "Draft text."}))
        .await
        .expect_err("a mailbox without Drafts refuses");
    assert_eq!(
        sentence(&error),
        "This mailbox has no Drafts folder; nothing was saved."
    );
}

#[tokio::test]
async fn mail_send_submits_with_auth_plain_then_appends_to_sent_with_its_message_id() {
    let o = outbound(plain, sent_and_drafts(), None).await;
    let answer = send(
        &o,
        "mail.send",
        json!({"to": [ANN], "cc": [ACCOUNTS], "subject": "Invoice paid", "body": "Paid today.\n.\nThanks."}),
    )
    .await;
    let mut wrong = Vec::new();
    let answer = match answer {
        Ok(answer) => answer,
        Err(e) => panic!("mail.send did not send: {}", sentence(&e)),
    };
    let id = answer["message_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if !minted_for(&id, LOGIN) {
        wrong.push(format!(
            "the answer's message_id is not one the orchestrator minted: {answer}"
        ));
    }
    if o.smtp.mechanisms() != vec!["PLAIN".to_string()] {
        wrong.push(format!(
            "authenticated by {:?}, not AUTH PLAIN",
            o.smtp.mechanisms()
        ));
    }
    let submitted = o.smtp.submitted();
    match submitted.as_slice() {
        [message] => {
            if message.from != LOGIN
                || message.recipients != vec![ANN.to_string(), ACCOUNTS.to_string()]
            {
                wrong.push(format!(
                    "the envelope is {} -> {:?}",
                    message.from, message.recipients
                ));
            }
            if header_of(&message.data, "Message-ID").as_deref() != Some(id.as_str()) {
                wrong.push(format!(
                    "the message's Message-ID is not {id}: {}",
                    message.data
                ));
            }
            if body_of(&message.data) != "Paid today.\r\n.\r\nThanks." {
                wrong.push(format!("the body arrived as {:?}", body_of(&message.data)));
            }
            if header_of(&message.data, "Cc").as_deref() != Some(ACCOUNTS)
                || header_of(&message.data, "From").as_deref()
                    != Some(&*format!("Mailbox Owner <{LOGIN}>"))
            {
                wrong.push(format!("the headers are not the call's: {}", message.data));
            }
        }
        other => wrong.push(format!("{} messages were submitted, not 1", other.len())),
    }
    let sent = o.mailbox.folder_messages("Sent Items");
    match sent.as_slice() {
        [copy] => {
            if copy.flags != vec!["\\Seen".to_string()]
                || header_of(&copy.raw, "Message-ID").as_deref() != Some(id.as_str())
            {
                wrong.push(format!("the Sent copy is {:?} {}", copy.flags, copy.raw));
            }
        }
        other => wrong.push(format!("Sent holds {} messages, not 1", other.len())),
    }
    if answer["saved_to_sent"] != true || answer["sent_folder"] != "Sent Items" {
        wrong.push(format!(
            "the answer does not say the copy was saved: {answer}"
        ));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn mail_send_uses_auth_login_when_only_login_is_offered() {
    let o = outbound(login_only, sent_and_drafts(), None).await;
    let answer = send(
        &o,
        "mail.send",
        json!({"to": [ANN], "subject": "Hello", "body": "Hi."}),
    )
    .await;
    assert!(answer.is_ok(), "mail.send did not send: {answer:?}");
    assert_eq!(o.smtp.mechanisms(), vec!["LOGIN".to_string()]);
    assert_eq!(o.smtp.submitted().len(), 1);
}

#[tokio::test]
async fn mail_send_on_an_oauth_mailbox_uses_xoauth2_and_no_token_reaches_the_result() {
    let mailbox =
        imap_xoauth2_standin_with_folders(OAUTH_ADDRESS, TOKEN, inbox(), sent_and_drafts()).await;
    let smtp = smtp_submission_standin(
        SubmitAuth::XOAuth2 {
            user: OAUTH_ADDRESS.to_string(),
            token: TOKEN.to_string(),
        },
        None,
    )
    .await;
    let o = outbound_over(
        mailbox,
        smtp,
        OAUTH_ADDRESS,
        MailAuth::XOAuth2(SensitiveString::new(TOKEN)),
    );
    let answer = send(
        &o,
        "mail.send",
        json!({"to": [ANN], "subject": "Hello", "body": "Hi."}),
    )
    .await;
    let mut wrong = Vec::new();
    let text = format!("{answer:?}");
    match &answer {
        Ok(answer)
            if minted_for(
                answer["message_id"].as_str().unwrap_or_default(),
                OAUTH_ADDRESS,
            ) => {}
        other => wrong.push(format!("mail.send did not send over XOAUTH2: {other:?}")),
    }
    if o.smtp.mechanisms() != vec!["XOAUTH2".to_string()] {
        wrong.push(format!(
            "authenticated by {:?}, not AUTH XOAUTH2",
            o.smtp.mechanisms()
        ));
    }
    if o.smtp.standin.sasl() != vec![xoauth2_string(OAUTH_ADDRESS, TOKEN)] {
        wrong.push(format!(
            "the SMTP stand-in received {:?}",
            o.smtp.standin.sasl()
        ));
    }
    let base64_response = STANDARD.encode(xoauth2_string(OAUTH_ADDRESS, TOKEN));
    if text.contains(TOKEN) || text.contains(&base64_response) {
        wrong.push(format!("the token reached the result: {text}"));
    }
    if o.mailbox.folder_messages("Sent Items").len() != 1 {
        wrong.push("the sent message was not appended to Sent".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn mail_send_appends_nothing_when_the_sent_folder_already_holds_its_message() {
    let mailbox =
        imap_mailbox_standin_with_folders(LOGIN, PASSWORD, inbox(), sent_and_drafts()).await;
    let smtp = mail_standins::smtp_submission_standin_filing(
        plain(LOGIN.to_string(), PASSWORD.to_string()),
        &mailbox,
        "Sent Items",
    )
    .await;
    let o = outbound_over(
        mailbox,
        smtp,
        LOGIN,
        MailAuth::Password(SensitiveString::new(PASSWORD)),
    );
    let answer = send(
        &o,
        "mail.send",
        json!({"to": [ANN], "subject": "Hello", "body": "Hi."}),
    )
    .await;
    assert!(answer.is_ok(), "mail.send did not send: {answer:?}");
    let appended: Vec<String> = o
        .mailbox
        .commands()
        .into_iter()
        .filter(|c| {
            c.split_whitespace()
                .nth(1)
                .is_some_and(|v| v.eq_ignore_ascii_case("APPEND"))
        })
        .collect();
    assert!(
        appended.is_empty() && o.mailbox.folder_messages("Sent Items").len() == 1,
        "a copy the server had filed was appended again: {} in Sent, {appended:?}",
        o.mailbox.folder_messages("Sent Items").len()
    );
    assert_eq!(answer.unwrap()["saved_to_sent"], true);
}

#[tokio::test]
async fn mail_send_without_a_sent_folder_sends_and_says_so() {
    let o = outbound(plain, vec![StandInFolder::new("Drafts", "\\Drafts")], None).await;
    let answer = send(
        &o,
        "mail.send",
        json!({"to": [ANN], "subject": "Hello", "body": "Hi."}),
    )
    .await
    .unwrap_or_else(|e| panic!("mail.send did not send: {}", sentence(&e)));
    assert_eq!(o.smtp.submitted().len(), 1);
    assert_eq!(answer["saved_to_sent"], false, "{answer}");
    assert!(answer["sent_folder"].is_null(), "{answer}");
}

#[tokio::test]
async fn a_sent_folder_named_sent_is_found_without_its_attribute() {
    let o = outbound(plain, vec![StandInFolder::new("sent", "")], None).await;
    let answer = send(
        &o,
        "mail.send",
        json!({"to": [ANN], "subject": "Hello", "body": "Hi."}),
    )
    .await
    .unwrap_or_else(|e| panic!("mail.send did not send: {}", sentence(&e)));
    assert_eq!(answer["sent_folder"], "sent", "{answer}");
    assert_eq!(o.mailbox.folder_messages("sent").len(), 1);
}

#[tokio::test]
async fn mail_reply_carries_in_reply_to_and_references_from_the_threads_newest_message() {
    let o = outbound(plain, sent_and_drafts(), None).await;
    let answer = send(
        &o,
        "mail.reply",
        json!({"thread_id": "<a1@x>", "to": [ANN], "subject": "Re: Invoice", "body": "Received."}),
    )
    .await;
    let mut wrong = Vec::new();
    if let Err(e) = &answer {
        wrong.push(format!("mail.reply did not send: {}", sentence(e)));
    }
    match o.smtp.submitted().as_slice() {
        [message] => {
            if header_of(&message.data, "In-Reply-To").as_deref() != Some("<a2@x>") {
                wrong.push(format!(
                    "In-Reply-To is {:?}, not the newest message <a2@x>",
                    header_of(&message.data, "In-Reply-To")
                ));
            }
            if header_of(&message.data, "References").as_deref() != Some("<a1@x> <a2@x>") {
                wrong.push(format!(
                    "References is {:?}, not <a1@x> <a2@x>",
                    header_of(&message.data, "References")
                ));
            }
        }
        other => wrong.push(format!("{} messages were submitted, not 1", other.len())),
    }
    if let Ok(answer) = &answer {
        if answer["in_reply_to"] != "<a2@x>" || answer["thread_id"] != "<a1@x>" {
            wrong.push(format!("the answer does not name the thread: {answer}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn a_refused_recipient_sends_nothing() {
    let o = outbound(plain, sent_and_drafts(), Some("nobody@example.test")).await;
    let error = send(
        &o,
        "mail.send",
        json!({"to": [ANN, "nobody@example.test"], "subject": "Hello", "body": "Hi."}),
    )
    .await
    .expect_err("a refused recipient fails the send");
    let said = sentence(&error);
    let commands = o.smtp.commands();
    let mut wrong = Vec::new();
    if !said.contains("nobody@example.test") || !said.contains(RCPT_REFUSAL) {
        wrong.push(format!("the refusal does not name the address: {said}"));
    }
    if !o.smtp.submitted().is_empty() || commands.iter().any(|c| c.eq_ignore_ascii_case("DATA")) {
        wrong.push(format!("a message was submitted: {commands:?}"));
    }
    if !commands.iter().any(|c| c.eq_ignore_ascii_case("RSET")) {
        wrong.push(format!("the envelope was not reset: {commands:?}"));
    }
    if !o.mailbox.folder_messages("Sent Items").is_empty() {
        wrong.push("a refused send was appended to Sent".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn malformed_outbound_arguments_are_refused_before_any_connection() {
    let o = outbound(plain, sent_and_drafts(), None).await;
    let many: Vec<String> = (0..51).map(|i| format!("r{i}@example.test")).collect();
    let mut wrong = Vec::new();
    for (case, tool, args, expected) in [
        (
            "an address with a line break",
            "mail.send",
            json!({"to": ["ann@example.test\r\nBcc: x@y.test"], "subject": "s", "body": "b"}),
            "'ann@example.testBcc: x@y.test' is not an email address this tool can send to.",
        ),
        (
            "an address without @",
            "mail.draft",
            json!({"to": ["ann"], "body": "b"}),
            "'ann' is not an email address this tool can send to.",
        ),
        (
            "51 recipients",
            "mail.send",
            json!({"to": many, "subject": "s", "body": "b"}),
            "A message has at most 50 recipients.",
        ),
        (
            "a subject with a line break",
            "mail.reply",
            json!({"thread_id": "<a1@x>", "to": [ANN], "subject": "a\nBcc: x@y.test", "body": "b"}),
            "'subject' must be one line of at most 998 characters.",
        ),
        (
            "a body over 100000 characters",
            "mail.send",
            json!({"to": [ANN], "subject": "s", "body": "x".repeat(100_001)}),
            "'body' must be plain text of at most 100000 characters.",
        ),
    ] {
        match send(&o, tool, args).await {
            Ok(answer) => wrong.push(format!("{case}: {tool} was not refused: {answer}")),
            Err(e) if sentence(&e) != expected => {
                wrong.push(format!("{case}: refused with {:?}", sentence(&e)))
            }
            Err(_) => {}
        }
    }
    if o.smtp.standin.connections() != 0 || o.mailbox.connections() != 0 {
        wrong.push("a malformed call opened a session".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// mail.delete (AEGIS ADR-125's Update of 2026-10-08 (4), clauses 17 to 19)
// ---------------------------------------------------------------------------

/// The refusal for a mailbox with no Trash folder, as the clause gives it.
const NO_TRASH_SENTENCE: &str = "This mailbox has no Trash folder; nothing was deleted.";
/// The refusal for a server with neither `MOVE` nor `UIDPLUS`.
const NO_SAFE_MOVE_SENTENCE: &str =
    "This mailbox's server can neither move messages nor expunge only chosen ones; nothing was deleted.";

/// A fixture over a mailbox whose `CAPABILITY` answers `capabilities`, with
/// `folders` beside `INBOX` holding `messages`.
async fn delete_fixture(
    capabilities: &str,
    messages: Vec<StoredMessage>,
    folders: Vec<StandInFolder>,
) -> Fixture {
    let mailbox =
        imap_mailbox_standin_with_capabilities(LOGIN, PASSWORD, messages, folders, capabilities)
            .await;
    let id = CredentialBindingId::new();
    let source = Arc::new(OneMailbox {
        id,
        port: mailbox.port(),
        host: "127.0.0.1".to_string(),
        granted: false,
    });
    Fixture {
        tools: MailTools::with_connector(source, Arc::new(PlainConnector)),
        mailbox,
        id,
    }
}

/// The verb of a recorded command (`UID MOVE` counts as `UID MOVE`).
fn verb_of(command: &str) -> String {
    let mut words = command.split_whitespace().skip(1);
    let first = words.next().unwrap_or("").to_ascii_uppercase();
    if first == "UID" {
        format!("UID {}", words.next().unwrap_or("").to_ascii_uppercase())
    } else {
        first
    }
}

/// The commands that change a mailbox: a move, copy, store or expunge.
fn changing(commands: &[String]) -> Vec<String> {
    commands
        .iter()
        .filter(|c| {
            matches!(
                verb_of(c).as_str(),
                "UID MOVE" | "UID COPY" | "UID STORE" | "UID EXPUNGE" | "EXPUNGE"
            )
        })
        .cloned()
        .collect()
}

/// What `commands` did that the tool never does: a plain `EXPUNGE`, or
/// opening a Trash folder.
fn trash_touched(commands: &[String], trash: &str) -> Vec<String> {
    commands
        .iter()
        .filter(|c| {
            let verb = verb_of(c);
            verb == "EXPUNGE"
                || (matches!(verb.as_str(), "SELECT" | "EXAMINE") && c.contains(trash))
        })
        .cloned()
        .collect()
}

fn uids_of(messages: &[StoredMessage]) -> Vec<u32> {
    messages.iter().map(|m| m.uid).collect()
}

#[tokio::test]
async fn mail_delete_moves_exactly_the_threads_inbox_messages_to_trash_by_uid_move() {
    let f = delete_fixture(
        "IMAP4rev1 MOVE UIDPLUS",
        inbox(),
        vec![
            StandInFolder::new("Trash", ""),
            StandInFolder::new("[Gmail]/Trash", "\\Trash"),
        ],
    )
    .await;
    let mut wrong = Vec::new();
    match call(
        &f,
        "mail.delete",
        json!({"thread_id": "<a1@x>"}),
        &conversation(),
    )
    .await
    {
        Ok(result) => {
            if result["moved"] != 2
                || result["message_uids"] != json!([1, 3])
                || result["trash_folder"] != "[Gmail]/Trash"
                || result["folder"] != "INBOX"
                || result["thread_id"] != "<a1@x>"
            {
                wrong.push(format!("the result is {result}"));
            }
        }
        Err(e) => wrong.push(format!("mail.delete failed: {}", sentence(&e))),
    }
    if uids_of(&f.mailbox.folder_messages("INBOX")) != vec![2, 4] {
        wrong.push(format!(
            "INBOX holds {:?}, not the other messages 2 and 4",
            uids_of(&f.mailbox.folder_messages("INBOX"))
        ));
    }
    let moved = f.mailbox.folder_messages("[Gmail]/Trash");
    if moved.len() != 2 || !moved.iter().all(|m| m.raw.contains("Invoice")) {
        wrong.push(format!(
            "the \\Trash folder holds {} messages, not the thread's two",
            moved.len()
        ));
    }
    if !f.mailbox.folder_messages("Trash").is_empty() {
        wrong.push("the folder named Trash was used over the one marked \\Trash".to_string());
    }
    let commands = f.mailbox.commands();
    let changed = changing(&commands);
    if changed.len() != 1 || !changed[0].contains("UID MOVE 1,3 ") {
        wrong.push(format!(
            "the changing commands were {changed:?}, not one UID MOVE of 1,3"
        ));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn mail_delete_without_move_copies_stores_deleted_and_expunges_only_those_uids() {
    let mut messages = inbox();
    // An unrelated message another client already marked deleted: a plain
    // EXPUNGE would remove it, UID EXPUNGE of the thread's uids does not.
    messages[1].flags.push("\\Deleted".to_string());
    let f = delete_fixture(
        "IMAP4rev1 UIDPLUS",
        messages,
        vec![StandInFolder::new("Trash", "\\Trash")],
    )
    .await;
    let mut wrong = Vec::new();
    match call(
        &f,
        "mail.delete",
        json!({"thread_id": "<a1@x>"}),
        &conversation(),
    )
    .await
    {
        Ok(result) => {
            if result["moved"] != 2 || result["message_uids"] != json!([1, 3]) {
                wrong.push(format!("the result is {result}"));
            }
        }
        Err(e) => wrong.push(format!("mail.delete failed: {}", sentence(&e))),
    }
    let changed: Vec<String> = changing(&f.mailbox.commands())
        .iter()
        .map(|c| {
            c.split_once(' ')
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_default()
        })
        .collect();
    let expected = vec![
        "UID COPY 1,3 \"Trash\"".to_string(),
        "UID STORE 1,3 +FLAGS (\\Deleted)".to_string(),
        "UID EXPUNGE 1,3".to_string(),
    ];
    if changed != expected {
        wrong.push(format!(
            "the changing commands were {changed:?}, not {expected:?}"
        ));
    }
    if uids_of(&f.mailbox.folder_messages("INBOX")) != vec![2, 4] {
        wrong.push(format!(
            "INBOX holds {:?}: the unrelated deleted message 2 must survive",
            uids_of(&f.mailbox.folder_messages("INBOX"))
        ));
    }
    if f.mailbox.folder_messages("Trash").len() != 2 {
        wrong.push("Trash does not hold the thread's two messages".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn mail_delete_without_a_trash_folder_refuses_and_changes_nothing() {
    let f = delete_fixture(
        "IMAP4rev1 MOVE UIDPLUS",
        inbox(),
        vec![StandInFolder::new("Sent", "\\Sent")],
    )
    .await;
    let mut wrong = Vec::new();
    match call(
        &f,
        "mail.delete",
        json!({"thread_id": "<a1@x>"}),
        &conversation(),
    )
    .await
    {
        Err(e) if sentence(&e) == NO_TRASH_SENTENCE => {}
        other => wrong.push(format!("not refused for its missing Trash: {other:?}")),
    }
    let changed = changing(&f.mailbox.commands());
    if !changed.is_empty() {
        wrong.push(format!("the refused call sent {changed:?}"));
    }
    if uids_of(&f.mailbox.folder_messages("INBOX")) != vec![1, 2, 3, 4] {
        wrong.push("INBOX changed".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn mail_delete_finds_a_trash_folder_named_deleted_or_under_inbox() {
    let mut wrong = Vec::new();
    for (folder, expected) in [
        ("Deleted", "Deleted"),
        ("INBOX/Trash", "INBOX/Trash"),
        ("trash", "trash"),
    ] {
        let f = delete_fixture(
            "IMAP4rev1 MOVE",
            inbox(),
            vec![
                StandInFolder::new("Archive", ""),
                StandInFolder::new(folder, ""),
            ],
        )
        .await;
        match call(
            &f,
            "mail.delete",
            json!({"thread_id": "<b1@x>"}),
            &conversation(),
        )
        .await
        {
            Ok(result) if result["trash_folder"] == expected && result["moved"] == 1 => {}
            Ok(result) => wrong.push(format!("with {folder}: {result}")),
            Err(e) => wrong.push(format!("with {folder}: {}", sentence(&e))),
        }
        if f.mailbox.folder_messages(folder).len() != 1 {
            wrong.push(format!("{folder} does not hold the moved message"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn mail_delete_on_a_server_with_neither_move_nor_uidplus_refuses_and_changes_nothing() {
    let f = delete_fixture(
        "IMAP4rev1",
        inbox(),
        vec![StandInFolder::new("Trash", "\\Trash")],
    )
    .await;
    let mut wrong = Vec::new();
    match call(
        &f,
        "mail.delete",
        json!({"thread_id": "<a1@x>"}),
        &conversation(),
    )
    .await
    {
        Err(e) if sentence(&e) == NO_SAFE_MOVE_SENTENCE => {}
        other => wrong.push(format!("not refused for its server: {other:?}")),
    }
    let changed = changing(&f.mailbox.commands());
    if !changed.is_empty() {
        wrong.push(format!("the refused call sent {changed:?}"));
    }
    if uids_of(&f.mailbox.folder_messages("INBOX")) != vec![1, 2, 3, 4]
        || !f.mailbox.folder_messages("Trash").is_empty()
    {
        wrong.push("a folder changed".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn mail_delete_never_expunges_trash_nor_sends_a_plain_expunge() {
    let mut wrong = Vec::new();
    for capabilities in ["IMAP4rev1 MOVE", "IMAP4rev1 UIDPLUS", "IMAP4rev2"] {
        // Trash already holds a message marked deleted: expunging Trash, or
        // a plain EXPUNGE there, would remove it.
        let kept = StoredMessage::new(
            1,
            &["\\Seen", "\\Deleted"],
            "01-Oct-2026 09:00:00 +0000",
            &["Message-ID: <old@x>", "Subject: Old"],
            "Old.",
        );
        let f = delete_fixture(
            capabilities,
            inbox(),
            vec![StandInFolder::new("Trash", "\\Trash").holding(vec![kept])],
        )
        .await;
        if let Err(e) = call(
            &f,
            "mail.delete",
            json!({"thread_id": "<a1@x>"}),
            &conversation(),
        )
        .await
        {
            wrong.push(format!("with {capabilities}: {}", sentence(&e)));
        }
        let touched = trash_touched(&f.mailbox.commands(), "Trash");
        if !touched.is_empty() {
            wrong.push(format!("with {capabilities}: sent {touched:?}"));
        }
        let trash = f.mailbox.folder_messages("Trash");
        if trash.len() != 3 || !trash.iter().any(|m| m.raw.contains("<old@x>")) {
            wrong.push(format!(
                "with {capabilities}: Trash holds {} messages, the old one {}",
                trash.len(),
                if trash.iter().any(|m| m.raw.contains("<old@x>")) {
                    "kept"
                } else {
                    "gone"
                }
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
