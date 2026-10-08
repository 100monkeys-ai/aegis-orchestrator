// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The mailbox beyond the inbox (AEGIS ADR-125's Update of 2026-10-08 (5),
//! clauses 22 to 26), against the loopback IMAP stand-in, whose folders
//! each report a `UIDVALIDITY` of their own:
//!
//! - `mail.list` and `mail.read` take `folder`, one of `inbox`, `sent`,
//!   `drafts`, `trash`, `archive` and `all`; each folder is located by its
//!   RFC 6154 attribute or the name rule, opened by `EXAMINE`, read with
//!   `BODY.PEEK`, and nothing is stored (clause 22);
//! - an unknown `folder` is refused before any connection, and a folder
//!   the mailbox lacks after `LIST` with nothing opened (clause 22);
//! - every result carries `folder` (as `LIST` wrote it) and `folder_kind`
//!   (clause 23);
//! - one thread id across folders, one folder per call; a `uid:` id is
//!   found only in its own folder; the not-found sentence names the folder,
//!   the inbox's byte for byte as before (clause 24);
//! - `mail.label`, `mail.delete`, `mail.reply` and `mail.draft` refuse a
//!   `folder` other than `inbox` before any connection (clause 25);
//! - `mail.label` takes `seen` (clause 26).
//!
//! The gated tools' refusal before the gate, with no approval row, is
//! tested in `mail_tools_tests.rs`.

#[path = "support/mail_standins.rs"]
mod mail_standins;

use aegis_orchestrator_core::application::credential_service::{
    ToolCallActor, ToolMailbox, ToolMailboxSource,
};
use aegis_orchestrator_core::application::tools::builtin_mail::{MailActing, MailTools};
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
use mail_standins::{
    folder_uidvalidity, imap_mailbox_standin_with_capabilities, MailboxStandIn, PlainConnector,
    StandInFolder, StoredMessage, UIDVALIDITY,
};
use serde_json::{json, Value};
use std::sync::Arc;

const USER: &str = "mail-owner";
const LOGIN: &str = "owner@example.test";
const PASSWORD: &str = "Mk7-folders-password";

/// Clause 22's refusal of a `folder` that is not one of the six.
const UNKNOWN_FOLDER: &str = "'folder' must be one of inbox, sent, drafts, trash, archive or all.";
/// Clause 25's refusal of a `folder` on a tool that works on the inbox.
const INBOX_ONLY: &str =
    "This tool works on threads in the inbox only; leave out 'folder' or set it to inbox.";

const SENT: &str = "[Gmail]/Sent Mail";
const DRAFTS: &str = "[Gmail]/Drafts";
const TRASH: &str = "[Gmail]/Trash";
const ARCHIVE: &str = "Archives";
const ALL: &str = "[Gmail]/All Mail";

/// Answers one mailbox, to its owner only.
struct OneMailbox {
    id: CredentialBindingId,
    port: u16,
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
                imap_host: "127.0.0.1".to_string(),
                imap_port: self.port,
                imap_security: MailSecurity::Tls,
                smtp_host: "127.0.0.1".to_string(),
                smtp_port: 465,
                smtp_security: MailSecurity::Tls,
                username: LOGIN.to_string(),
            },
            auth: MailAuth::Password(SensitiveString::new(PASSWORD)),
            granted: true,
        }))
    }
}

fn message(uid: u32, flags: &[&str], headers: &[&str], body: &str) -> StoredMessage {
    StoredMessage::new(uid, flags, "06-Oct-2026 09:00:00 +0000", headers, body)
}

/// The inbox: thread `<t1@x>` (uid 1, unread), whose reply is in Sent, and
/// thread `<i2@x>` (uids 2 and 3, read).
fn inbox() -> Vec<StoredMessage> {
    vec![
        message(
            1,
            &[],
            &[
                "Message-ID: <t1@x>",
                "From: Ann <ann@example.test>",
                "To: owner@example.test",
                "Subject: Quote",
                "Date: Tue, 06 Oct 2026 09:00:00 +0000",
            ],
            "Can you send a quote?",
        ),
        message(
            2,
            &["\\Seen"],
            &[
                "Message-ID: <i2@x>",
                "From: Bob <bob@example.test>",
                "Subject: Lunch",
                "Date: Tue, 06 Oct 2026 10:00:00 +0000",
            ],
            "Lunch?",
        ),
        message(
            3,
            &["\\Seen"],
            &[
                "Message-ID: <i3@x>",
                "In-Reply-To: <i2@x>",
                "References: <i2@x>",
                "From: Bob <bob@example.test>",
                "Subject: Re: Lunch",
                "Date: Tue, 06 Oct 2026 11:00:00 +0000",
            ],
            "Noon?",
        ),
    ]
}

/// Sent: the reply in thread `<t1@x>` (uid 1, unread so a stored `\Seen`
/// would show) and a message with no `Message-ID` (uid 2).
fn sent_messages() -> Vec<StoredMessage> {
    vec![
        message(
            1,
            &[],
            &[
                "Message-ID: <t2@x>",
                "In-Reply-To: <t1@x>",
                "References: <t1@x>",
                "From: owner@example.test",
                "To: Ann <ann@example.test>",
                "Subject: Re: Quote",
                "Date: Tue, 06 Oct 2026 12:00:00 +0000",
            ],
            "Here is the quote.",
        ),
        message(
            2,
            &[],
            &[
                "From: owner@example.test",
                "To: Carol <carol@example.test>",
                "Subject: No id, sent",
                "Date: Tue, 06 Oct 2026 13:00:00 +0000",
            ],
            "Sent without an id.",
        ),
    ]
}

/// One unread message with the subject `subject`, its own thread `id`.
fn single(id: &str, subject: &str) -> Vec<StoredMessage> {
    vec![message(
        1,
        &[],
        &[
            &format!("Message-ID: {id}"),
            "From: owner@example.test",
            &format!("Subject: {subject}"),
            "Date: Tue, 06 Oct 2026 14:00:00 +0000",
        ],
        "Body.",
    )]
}

/// The kinds beside the inbox, each with the folder the full mailbox marks
/// for it, a thread it alone holds and that thread's subject.
const KINDS: [(&str, &str, &str, &str); 5] = [
    ("sent", SENT, "<t1@x>", "Re: Quote"),
    ("drafts", DRAFTS, "<d1@x>", "A draft"),
    ("trash", TRASH, "<x1@x>", "Binned"),
    ("archive", ARCHIVE, "<r1@x>", "Filed away"),
    ("all", ALL, "<l1@x>", "Everything"),
];

/// Every special folder, each marked by its RFC 6154 attribute under a
/// name the name rule would not find, beside decoys the attribute must
/// win over.
fn marked_folders() -> Vec<StandInFolder> {
    vec![
        StandInFolder::new("Sent", ""),
        StandInFolder::new(SENT, "\\Sent").holding(sent_messages()),
        StandInFolder::new(DRAFTS, "\\Drafts").holding(single("<d1@x>", "A draft")),
        StandInFolder::new(TRASH, "\\Trash").holding(single("<x1@x>", "Binned")),
        StandInFolder::new("Archive", ""),
        StandInFolder::new(ARCHIVE, "\\Archive").holding(single("<r1@x>", "Filed away")),
        StandInFolder::new(ALL, "\\All").holding(single("<l1@x>", "Everything")),
    ]
}

struct Fixture {
    mailbox: MailboxStandIn,
    tools: MailTools,
    id: CredentialBindingId,
}

async fn fixture_with(folders: Vec<StandInFolder>, capabilities: &str) -> Fixture {
    let mailbox =
        imap_mailbox_standin_with_capabilities(LOGIN, PASSWORD, inbox(), folders, capabilities)
            .await;
    let id = CredentialBindingId::new();
    let source = Arc::new(OneMailbox {
        id,
        port: mailbox.port(),
    });
    Fixture {
        tools: MailTools::with_connector(source, Arc::new(PlainConnector)),
        mailbox,
        id,
    }
}

async fn fixture(folders: Vec<StandInFolder>) -> Fixture {
    fixture_with(folders, "IMAP4rev1").await
}

fn conversation() -> MailActing {
    MailActing {
        tenant_id: TenantId::default(),
        user_id: Some(USER.to_string()),
        agent_id: AgentId::new(),
        workflow_id: None,
        choice: ServerChoice::NotGiven,
        has_execution_record: false,
    }
}

async fn call(f: &Fixture, tool: &str, mut args: Value) -> Result<Value, SealSessionError> {
    args["mailbox"] = json!(f.id.0.to_string());
    f.tools.invoke(tool, &args, &conversation()).await
}

/// The sentence a refusal tells its caller.
fn sentence(error: &SealSessionError) -> String {
    match error {
        SealSessionError::Answered {
            answer: CallerAnswer::CredentialBindingRequired { message },
            ..
        }
        | SealSessionError::Answered {
            answer: CallerAnswer::NotFound(message),
            ..
        } => message.clone(),
        SealSessionError::InvalidArguments(message)
        | SealSessionError::UpstreamUnavailable(message) => message.clone(),
        other => format!("{other:?}"),
    }
}

/// The commands sent since `from`, tags dropped.
fn sent_since(mailbox: &MailboxStandIn, from: usize) -> Vec<String> {
    mailbox
        .commands()
        .into_iter()
        .skip(from)
        .map(|c| {
            c.split_once(' ')
                .map(|(_, rest)| rest.to_string())
                .unwrap_or(c)
        })
        .collect()
}

/// What in `commands` breaks reading `folder` read-only: anything but an
/// `EXAMINE` of it, a `SELECT`, a `STORE`, or a body fetched without
/// `PEEK`.
fn not_read_only(commands: &[String], folder: &str) -> Vec<String> {
    let mut wrong = Vec::new();
    if !commands
        .iter()
        .any(|c| c.starts_with("EXAMINE ") && c.contains(folder))
    {
        wrong.push(format!("{folder} was not opened by EXAMINE: {commands:?}"));
    }
    for c in commands {
        let upper = c.to_ascii_uppercase();
        if upper.starts_with("SELECT ") || upper.contains("STORE") {
            wrong.push(format!("{folder}: '{c}' was sent"));
        }
        if upper.contains("FETCH") && upper.contains("BODY[") && !upper.contains("BODY.PEEK[") {
            wrong.push(format!("{folder}: a body was fetched without PEEK: '{c}'"));
        }
    }
    wrong
}

// ---------------------------------------------------------------------------
// Clauses 22 and 23: each folder, read-only, named in every result
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_folder_is_found_by_its_attribute_opened_by_examine_and_read_with_peek() {
    let f = fixture(marked_folders()).await;
    let mut wrong = Vec::new();
    for (kind, folder, thread, subject) in KINDS {
        let from = f.mailbox.commands().len();
        match call(&f, "mail.list", json!({"folder": kind})).await {
            Ok(answer) => {
                if answer["folder"] != folder || answer["folder_kind"] != kind {
                    wrong.push(format!(
                        "mail.list {kind}: folder {} and folder_kind {}, not {folder} and {kind}",
                        answer["folder"], answer["folder_kind"]
                    ));
                }
                if !answer["threads"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|t| t["thread_id"] == thread))
                {
                    wrong.push(format!("mail.list {kind} did not list {thread}: {answer}"));
                }
            }
            Err(e) => wrong.push(format!("mail.list {kind} failed: {}", sentence(&e))),
        }
        match call(
            &f,
            "mail.read",
            json!({"folder": kind, "thread_id": thread}),
        )
        .await
        {
            Ok(answer) => {
                if answer["folder"] != folder || answer["folder_kind"] != kind {
                    wrong.push(format!(
                        "mail.read {kind}: folder {} and folder_kind {}, not {folder} and {kind}",
                        answer["folder"], answer["folder_kind"]
                    ));
                }
                if answer["messages"][0]["subject"] != subject {
                    wrong.push(format!(
                        "mail.read {kind} did not read '{subject}': {answer}"
                    ));
                }
            }
            Err(e) => wrong.push(format!("mail.read {kind} failed: {}", sentence(&e))),
        }
        let commands = sent_since(&f.mailbox, from);
        wrong.extend(not_read_only(&commands, folder));
        if !commands.iter().any(|c| c.starts_with("LIST ")) {
            wrong.push(format!("{kind}: no LIST was sent: {commands:?}"));
        }
        if f.mailbox
            .folder_messages(folder)
            .iter()
            .any(|m| m.flags.iter().any(|x| x == "\\Seen"))
        {
            wrong.push(format!("{kind}: a message of {folder} was marked read"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn the_inbox_is_the_default_and_named_inbox_in_every_result_with_no_list_sent() {
    let f = fixture(marked_folders()).await;
    let mut wrong = Vec::new();
    for folder in [None, Some(json!("inbox")), Some(Value::Null)] {
        let mut args = json!({});
        let mut read = json!({"thread_id": "<t1@x>"});
        if let Some(folder) = &folder {
            args["folder"] = folder.clone();
            read["folder"] = folder.clone();
        }
        let from = f.mailbox.commands().len();
        for (tool, args) in [("mail.list", args), ("mail.read", read)] {
            match call(&f, tool, args).await {
                Ok(answer) => {
                    if answer["folder"] != "INBOX" || answer["folder_kind"] != "inbox" {
                        wrong.push(format!(
                            "{tool} with folder {folder:?}: folder {} and folder_kind {}",
                            answer["folder"], answer["folder_kind"]
                        ));
                    }
                }
                Err(e) => wrong.push(format!("{tool} {folder:?} failed: {}", sentence(&e))),
            }
        }
        let commands = sent_since(&f.mailbox, from);
        wrong.extend(not_read_only(&commands, "INBOX"));
        if commands.iter().any(|c| c.starts_with("LIST ")) {
            wrong.push(format!("LIST was sent for the inbox: {commands:?}"));
        }
    }
    if f.mailbox
        .folder_messages("INBOX")
        .iter()
        .any(|m| m.uid == 1 && m.flags.iter().any(|x| x == "\\Seen"))
    {
        wrong.push("the inbox's unread message was marked read".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn each_folder_is_found_by_its_name_when_no_folder_carries_its_attribute() {
    // Sent at the top level, Drafts under INBOX, Trash as "Deleted",
    // Archive by its name; the name rule is case-insensitive.
    let cases: [(&str, Vec<StandInFolder>, &str); 5] = [
        ("sent", vec![StandInFolder::new("sent", "")], "sent"),
        (
            "drafts",
            vec![StandInFolder::new("INBOX/Drafts", "")],
            "INBOX/Drafts",
        ),
        ("trash", vec![StandInFolder::new("Deleted", "")], "Deleted"),
        (
            "archive",
            vec![StandInFolder::new("ARCHIVE", "")],
            "ARCHIVE",
        ),
        // With no \Archive and no Archive by name, the \All folder.
        (
            "archive",
            vec![StandInFolder::new("[Gmail]/All Mail", "\\All")],
            "[Gmail]/All Mail",
        ),
    ];
    let mut wrong = Vec::new();
    for (kind, folders, expected) in cases {
        let f = fixture(folders).await;
        match call(&f, "mail.list", json!({"folder": kind})).await {
            Ok(answer) => {
                if answer["folder"] != expected || answer["folder_kind"] != kind {
                    wrong.push(format!(
                        "{kind}: folder {} and folder_kind {}, not {expected}",
                        answer["folder"], answer["folder_kind"]
                    ));
                }
            }
            Err(e) => wrong.push(format!("{kind} ({expected}) failed: {}", sentence(&e))),
        }
        wrong.extend(not_read_only(&sent_since(&f.mailbox, 0), expected));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// Clause 22: the two refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_folder_that_is_not_one_of_the_six_is_refused_before_any_connection() {
    let f = fixture(marked_folders()).await;
    let mut wrong = Vec::new();
    for folder in [
        json!("Sent"),
        json!("spam"),
        json!(""),
        json!("[Gmail]/Sent Mail"),
        json!(3),
        json!(["sent"]),
    ] {
        for (tool, args) in [
            ("mail.list", json!({"folder": folder})),
            (
                "mail.read",
                json!({"folder": folder, "thread_id": "<t1@x>"}),
            ),
        ] {
            match call(&f, tool, args).await {
                Err(e) if sentence(&e) == UNKNOWN_FOLDER => {}
                Err(e) => wrong.push(format!("{tool} {folder}: refused with '{}'", sentence(&e))),
                Ok(answer) => wrong.push(format!("{tool} {folder} answered {answer}")),
            }
        }
    }
    if f.mailbox.connections() != 0 {
        wrong.push(format!(
            "{} connections were opened for refused calls",
            f.mailbox.connections()
        ));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn a_folder_the_mailbox_lacks_is_refused_after_list_with_nothing_opened() {
    // A folder named "All Mail" with no attribute is not the `all` folder,
    // and one named "Sent Messages" is not Sent by the name rule.
    let f = fixture(vec![
        StandInFolder::new("All Mail", ""),
        StandInFolder::new("Sent Messages", ""),
    ])
    .await;
    let mut wrong = Vec::new();
    for (kind, name) in [
        ("sent", "Sent"),
        ("drafts", "Drafts"),
        ("trash", "Trash"),
        ("archive", "Archive"),
        ("all", "All Mail"),
    ] {
        let expected = format!("This mailbox has no {name} folder; nothing was read.");
        for (tool, args) in [
            ("mail.list", json!({"folder": kind})),
            ("mail.read", json!({"folder": kind, "thread_id": "<t1@x>"})),
        ] {
            let from = f.mailbox.commands().len();
            match call(&f, tool, args).await {
                Err(e) if sentence(&e) == expected => {}
                Err(e) => wrong.push(format!("{tool} {kind}: refused with '{}'", sentence(&e))),
                Ok(answer) => wrong.push(format!("{tool} {kind} answered {answer}")),
            }
            let commands = sent_since(&f.mailbox, from);
            if !commands.iter().any(|c| c.starts_with("LIST ")) {
                wrong.push(format!("{tool} {kind}: no LIST was sent: {commands:?}"));
            }
            if commands
                .iter()
                .any(|c| c.starts_with("EXAMINE ") || c.starts_with("SELECT "))
            {
                wrong.push(format!("{tool} {kind}: a folder was opened: {commands:?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// Clause 24: one thread id across folders, one folder per call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_thread_in_the_inbox_and_in_sent_has_one_id_and_is_read_one_folder_per_call() {
    let f = fixture(marked_folders()).await;
    let mut wrong = Vec::new();
    for (folder, expected) in [("inbox", "Quote"), ("sent", "Re: Quote")] {
        match call(&f, "mail.list", json!({"folder": folder})).await {
            Ok(answer) => {
                let listed = answer["threads"]
                    .as_array()
                    .and_then(|t| t.iter().find(|t| t["thread_id"] == "<t1@x>"));
                match listed {
                    Some(t) if t["message_count"] == 1 && t["subject"] == expected => {}
                    other => wrong.push(format!("{folder} listed <t1@x> as {other:?}")),
                }
            }
            Err(e) => wrong.push(format!("mail.list {folder} failed: {}", sentence(&e))),
        }
        match call(
            &f,
            "mail.read",
            json!({"folder": folder, "thread_id": "<t1@x>"}),
        )
        .await
        {
            Ok(answer) => {
                let subjects: Vec<&str> = answer["messages"]
                    .as_array()
                    .map(|m| m.iter().filter_map(|m| m["subject"].as_str()).collect())
                    .unwrap_or_default();
                if subjects != vec![expected] || answer["thread_id"] != "<t1@x>" {
                    wrong.push(format!(
                        "reading <t1@x> in {folder} answered {subjects:?}, not [{expected}]"
                    ));
                }
            }
            Err(e) => wrong.push(format!("mail.read {folder} failed: {}", sentence(&e))),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn a_uid_thread_id_is_found_only_in_the_folder_whose_uidvalidity_it_carries() {
    let f = fixture(marked_folders()).await;
    let validity = folder_uidvalidity(SENT);
    assert_ne!(
        validity, UIDVALIDITY,
        "the stand-in's Sent shares INBOX's UIDVALIDITY"
    );
    let id = format!("uid:{validity}:2");
    let mut wrong = Vec::new();
    match call(&f, "mail.list", json!({"folder": "sent"})).await {
        Ok(answer) => {
            if !answer["threads"]
                .as_array()
                .is_some_and(|t| t.iter().any(|t| t["thread_id"] == id.as_str()))
            {
                wrong.push(format!(
                    "Sent's message with no id is not listed as {id}: {answer}"
                ));
            }
        }
        Err(e) => wrong.push(format!("mail.list sent failed: {}", sentence(&e))),
    }
    match call(&f, "mail.read", json!({"folder": "sent", "thread_id": id})).await {
        Ok(answer) if answer["messages"][0]["subject"] == "No id, sent" => {}
        Ok(answer) => wrong.push(format!("reading {id} in Sent answered {answer}")),
        Err(e) => wrong.push(format!("reading {id} in Sent failed: {}", sentence(&e))),
    }
    // The same id in the inbox, and an inbox `uid:` id in Sent, name no
    // thread there, though both folders hold a message with that UID.
    let inbox_id = format!("uid:{UIDVALIDITY}:2");
    for (folder, thread, place) in [
        ("inbox", id.as_str(), "inbox"),
        ("sent", inbox_id.as_str(), "Sent folder"),
    ] {
        let expected = format!("There is no thread '{thread}' in this mailbox's {place}.");
        match call(
            &f,
            "mail.read",
            json!({"folder": folder, "thread_id": thread}),
        )
        .await
        {
            Err(e) if sentence(&e) == expected => {}
            Err(e) => wrong.push(format!("{thread} in {folder}: '{}'", sentence(&e))),
            Ok(answer) => wrong.push(format!("{thread} was found in {folder}: {answer}")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn a_thread_not_in_the_named_folder_is_refused_with_that_folders_sentence() {
    let f = fixture(marked_folders()).await;
    let mut wrong = Vec::new();
    for (kind, place) in [
        ("inbox", "inbox"),
        ("sent", "Sent folder"),
        ("drafts", "Drafts folder"),
        ("trash", "Trash folder"),
        ("archive", "Archive folder"),
        ("all", "All Mail folder"),
    ] {
        let expected = format!("There is no thread '<nope@x>' in this mailbox's {place}.");
        match call(
            &f,
            "mail.read",
            json!({"folder": kind, "thread_id": "<nope@x>"}),
        )
        .await
        {
            Err(e) if sentence(&e) == expected => {}
            Err(e) => wrong.push(format!("{kind}: '{}', not '{expected}'", sentence(&e))),
            Ok(answer) => wrong.push(format!("{kind} answered {answer}")),
        }
    }
    // The inbox's sentence of before, byte for byte, with no folder named.
    match call(&f, "mail.read", json!({"thread_id": "<nope@x>"})).await {
        Err(e) if sentence(&e) == "There is no thread '<nope@x>' in this mailbox's inbox." => {}
        other => wrong.push(format!("with no folder: {other:?}")),
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// Clause 25: the writes stay in the inbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_four_inbox_tools_refuse_a_folder_other_than_inbox_before_any_connection() {
    let f = fixture_with(marked_folders(), "IMAP4rev1 MOVE UIDPLUS").await;
    let reply = json!({"to": ["ann@example.test"], "subject": "Re: Quote", "body": "Done."});
    let calls = [
        (
            "mail.label",
            json!({"thread_id": "<t1@x>", "flagged": true}),
        ),
        ("mail.delete", json!({"thread_id": "<t1@x>"})),
        ("mail.reply", {
            let mut r = reply.clone();
            r["thread_id"] = json!("<t1@x>");
            r
        }),
        ("mail.draft", json!({"body": "A draft."})),
    ];
    let mut wrong = Vec::new();
    for (tool, args) in &calls {
        for folder in [
            json!("sent"),
            json!("trash"),
            json!("all"),
            json!("spam"),
            json!(1),
        ] {
            let mut args = args.clone();
            args["folder"] = folder.clone();
            match call(&f, tool, args).await {
                Err(e) if sentence(&e) == INBOX_ONLY => {}
                Err(e) => wrong.push(format!("{tool} {folder}: refused with '{}'", sentence(&e))),
                Ok(answer) => wrong.push(format!("{tool} {folder} answered {answer}")),
            }
        }
    }
    if f.mailbox.connections() != 0 {
        wrong.push(format!(
            "{} connections were opened for refused calls",
            f.mailbox.connections()
        ));
    }
    if f.mailbox.folder_messages("INBOX").len() != 3 {
        wrong.push("the inbox changed".to_string());
    }
    // `inbox` itself is the folder these tools work on.
    match call(
        &f,
        "mail.label",
        json!({"thread_id": "<t1@x>", "flagged": true, "folder": "inbox"}),
    )
    .await
    {
        Ok(answer) if answer["folder"] == "INBOX" && answer["folder_kind"] == "inbox" => {}
        other => wrong.push(format!("mail.label with folder inbox: {other:?}")),
    }
    match call(
        &f,
        "mail.delete",
        json!({"thread_id": "<i2@x>", "folder": "inbox"}),
    )
    .await
    {
        Ok(answer) if answer["folder"] == "INBOX" && answer["folder_kind"] == "inbox" => {}
        other => wrong.push(format!("mail.delete with folder inbox: {other:?}")),
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// Clause 26: read and unread
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mail_label_seen_sets_and_clears_seen_on_every_message_of_the_thread() {
    let f = fixture(marked_folders()).await;
    let mut wrong = Vec::new();
    let seen_on = |uid: u32| {
        f.mailbox
            .folder_messages("INBOX")
            .iter()
            .any(|m| m.uid == uid && m.flags.iter().any(|x| x == "\\Seen"))
    };
    let from = f.mailbox.commands().len();
    match call(
        &f,
        "mail.label",
        json!({"thread_id": "<i2@x>", "seen": false}),
    )
    .await
    {
        Ok(answer) => {
            if answer["seen"] != false || answer["folder_kind"] != "inbox" {
                wrong.push(format!("seen false answered {answer}"));
            }
        }
        Err(e) => wrong.push(format!("seen false failed: {}", sentence(&e))),
    }
    if !sent_since(&f.mailbox, from)
        .iter()
        .any(|c| c == "UID STORE 2,3 -FLAGS (\\Seen)")
    {
        wrong.push(format!(
            "no UID STORE 2,3 -FLAGS (\\Seen): {:?}",
            sent_since(&f.mailbox, from)
        ));
    }
    if seen_on(2) || seen_on(3) {
        wrong.push("the thread is still read".to_string());
    }
    let from = f.mailbox.commands().len();
    match call(
        &f,
        "mail.label",
        json!({"thread_id": "<i2@x>", "seen": true}),
    )
    .await
    {
        Ok(answer) => {
            if answer["seen"] != true {
                wrong.push(format!("seen true answered {answer}"));
            }
        }
        Err(e) => wrong.push(format!("seen true failed: {}", sentence(&e))),
    }
    if !sent_since(&f.mailbox, from)
        .iter()
        .any(|c| c == "UID STORE 2,3 +FLAGS (\\Seen)")
    {
        wrong.push(format!(
            "no UID STORE 2,3 +FLAGS (\\Seen): {:?}",
            sent_since(&f.mailbox, from)
        ));
    }
    if !(seen_on(2) && seen_on(3)) {
        wrong.push("the thread is not read".to_string());
    }
    // A label call with none of add, remove, flagged and seen is refused.
    match call(&f, "mail.label", json!({"thread_id": "<i2@x>"})).await {
        Err(e)
            if sentence(&e)
                == "mail.label needs at least one of 'add', 'remove', 'flagged' or 'seen'." => {}
        other => wrong.push(format!("a label call with nothing to change: {other:?}")),
    }
    match call(
        &f,
        "mail.label",
        json!({"thread_id": "<i2@x>", "seen": "yes"}),
    )
    .await
    {
        Err(e) if sentence(&e) == "'seen' must be true or false." => {}
        other => wrong.push(format!("seen \"yes\": {other:?}")),
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
