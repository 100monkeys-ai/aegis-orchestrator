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
//! - `mail.label` takes `seen` (clause 26);
//! - a thread `mail.list` names is read whole in the same folder though
//!   the server's `HEADER References` search matches nothing, as Gmail's
//!   does: a reply linked to its thread by `References` alone is read
//!   (clause 39, proposed under the Update's Status tracking).
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
        tier: None,
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
// Clause 39: a thread is read whole when `HEADER References` matches nothing
// ---------------------------------------------------------------------------

/// The person's own reply in thread `<g1@x>`: `In-Reply-To` the later
/// message `<g2@x>`, the root in `References` alone, so only a
/// `HEADER References` search would find it by the root's id.
fn own_reply(uid: u32) -> StoredMessage {
    message(
        uid,
        &["\\Seen"],
        &[
            "Message-ID: <g3@x>",
            "In-Reply-To: <g2@x>",
            "References: <g1@x> <g2@x>",
            "From: owner@example.test",
            "To: Ann <ann@example.test>",
            "Subject: Re: Plans",
            "Date: Tue, 06 Oct 2026 16:55:19 +0000",
        ],
        "See you there.",
    )
}

/// Thread `<g1@x>` whole: Ann's message, her follow-up (`In-Reply-To` the
/// root) and the person's reply ([`own_reply`]), after an unrelated one.
fn whole_thread() -> Vec<StoredMessage> {
    vec![
        message(
            1,
            &["\\Seen"],
            &[
                "Message-ID: <u1@x>",
                "From: Bob <bob@example.test>",
                "Subject: Unrelated",
                "Date: Tue, 06 Oct 2026 08:00:00 +0000",
            ],
            "Not this thread.",
        ),
        message(
            2,
            &["\\Seen"],
            &[
                "Message-ID: <g1@x>",
                "From: Ann <ann@example.test>",
                "To: owner@example.test",
                "Subject: Plans",
                "Date: Tue, 06 Oct 2026 09:00:00 +0000",
            ],
            "Dinner on Friday?",
        ),
        message(
            3,
            &["\\Seen"],
            &[
                "Message-ID: <g2@x>",
                "In-Reply-To: <g1@x>",
                "References: <g1@x>",
                "From: Ann <ann@example.test>",
                "To: owner@example.test",
                "Subject: Re: Plans",
                "Date: Tue, 06 Oct 2026 10:00:00 +0000",
            ],
            "At seven.",
        ),
        own_reply(4),
    ]
}

/// The `message_id`s a read answered, oldest first.
fn read_ids(answer: &Value) -> Vec<String> {
    answer["messages"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter_map(|m| m["message_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_reply_listed_in_sent_is_read_there_when_the_references_search_matches_nothing() {
    let f = fixture(vec![
        StandInFolder::new(SENT, "\\Sent").holding(vec![own_reply(1)])
    ])
    .await;
    f.mailbox.references_search_matches_nothing();
    let mut wrong = Vec::new();
    match call(&f, "mail.list", json!({"folder": "sent"})).await {
        Ok(answer) => {
            let listed = answer["threads"]
                .as_array()
                .and_then(|t| t.iter().find(|t| t["thread_id"] == "<g1@x>"));
            if !listed.is_some_and(|t| t["message_count"] == 1) {
                wrong.push(format!(
                    "Sent did not list <g1@x> with one message: {answer}"
                ));
            }
        }
        Err(e) => wrong.push(format!("mail.list sent failed: {}", sentence(&e))),
    }
    match call(
        &f,
        "mail.read",
        json!({"folder": "sent", "thread_id": "<g1@x>"}),
    )
    .await
    {
        Ok(answer) => {
            if read_ids(&answer) != vec!["<g3@x>"] {
                wrong.push(format!(
                    "reading <g1@x> in Sent answered {:?}, not the reply <g3@x>",
                    read_ids(&answer)
                ));
            }
        }
        Err(e) => wrong.push(format!(
            "the thread Sent listed was refused in Sent: '{}'",
            sentence(&e)
        )),
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn a_threads_own_reply_is_in_its_read_when_the_references_search_matches_nothing() {
    let f = fixture(vec![
        StandInFolder::new(ALL, "\\All").holding(whole_thread())
    ])
    .await;
    f.mailbox.references_search_matches_nothing();
    let mut wrong = Vec::new();
    match call(&f, "mail.list", json!({"folder": "all"})).await {
        Ok(answer) => {
            let listed = answer["threads"]
                .as_array()
                .and_then(|t| t.iter().find(|t| t["thread_id"] == "<g1@x>"));
            if !listed.is_some_and(|t| t["message_count"] == 3) {
                wrong.push(format!(
                    "All Mail did not list <g1@x> with three messages: {answer}"
                ));
            }
        }
        Err(e) => wrong.push(format!("mail.list all failed: {}", sentence(&e))),
    }
    let from = f.mailbox.commands().len();
    match call(
        &f,
        "mail.read",
        json!({"folder": "all", "thread_id": "<g1@x>"}),
    )
    .await
    {
        Ok(answer) => {
            let ids = read_ids(&answer);
            if ids != vec!["<g1@x>", "<g2@x>", "<g3@x>"] {
                wrong.push(format!(
                    "reading <g1@x> in All Mail answered {ids:?}: the thread's own reply <g3@x> is missing"
                ));
            }
        }
        Err(e) => wrong.push(format!("mail.read all failed: {}", sentence(&e))),
    }
    wrong.extend(not_read_only(&sent_since(&f.mailbox, from), ALL));
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

// ---------------------------------------------------------------------------
// Clauses 30 and 31: an attachment's part number, and `mail.attachment`
// ---------------------------------------------------------------------------

mod attachments {
    use super::*;
    use aegis_orchestrator_core::application::file_operations_service::FileOperationsService;
    use aegis_orchestrator_core::application::tools::builtin_mail::{
        MailFiles, ATTACHMENT_TOO_LARGE, NO_PERSON,
    };
    use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
    use aegis_orchestrator_core::application::volume_manager::VolumeService;
    use aegis_orchestrator_core::domain::events::StorageEvent;
    use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
    use aegis_orchestrator_core::domain::iam::ZaruTier;
    use aegis_orchestrator_core::domain::repository::VolumeRepository;
    use aegis_orchestrator_core::domain::runtime::InstanceId;
    use aegis_orchestrator_core::domain::volume::{
        AccessMode, StorageClass, StorageTierLimits, Volume, VolumeBackend, VolumeId, VolumeMount,
        VolumeOwnership,
    };
    use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
    use aegis_orchestrator_core::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
    };
    use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
    use base64::Engine;
    use sha2::Digest;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    struct NoOpPublisher;

    #[async_trait]
    impl EventPublisher for NoOpPublisher {
        async fn publish_storage_event(&self, _event: StorageEvent) {}
    }

    /// Volumes as the storage layer makes them: host directories under
    /// `/hosts/<id>` in the scratch root the FSAL serves.
    struct HostVolumes {
        repo: Arc<InMemoryVolumeRepository>,
    }

    #[async_trait]
    impl VolumeService for HostVolumes {
        async fn create_volume(
            &self,
            name: String,
            tenant_id: TenantId,
            storage_class: StorageClass,
            size_limit_mb: u64,
            ownership: VolumeOwnership,
        ) -> anyhow::Result<VolumeId> {
            let id = VolumeId::new();
            let mut volume = Volume::new(
                name,
                tenant_id,
                storage_class,
                VolumeBackend::HostPath {
                    path: PathBuf::from(format!("/hosts/{id}")),
                },
                size_limit_mb * 1024 * 1024,
                ownership,
            )?;
            volume.id = id;
            volume.mark_available()?;
            self.repo.save(&volume).await?;
            Ok(id)
        }
        async fn get_volume(&self, id: VolumeId) -> anyhow::Result<Volume> {
            self.repo
                .find_by_id(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("not found"))
        }
        async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
            Ok(self.repo.find_by_tenant(tenant_id).await?)
        }
        async fn list_volumes_by_ownership(
            &self,
            ownership: &VolumeOwnership,
        ) -> anyhow::Result<Vec<Volume>> {
            Ok(self.repo.find_by_ownership(ownership).await?)
        }
        async fn attach_volume(
            &self,
            _vid: VolumeId,
            _iid: InstanceId,
            _m: PathBuf,
            _a: AccessMode,
        ) -> anyhow::Result<VolumeMount> {
            unimplemented!()
        }
        async fn detach_volume(&self, _vid: VolumeId, _iid: InstanceId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_volume(&self, _volume_id: VolumeId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn get_volume_usage(&self, _v: VolumeId) -> anyhow::Result<u64> {
            Ok(0)
        }
        async fn cleanup_expired_volumes(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn create_volumes_for_execution(
            &self,
            _eid: aegis_orchestrator_core::domain::execution::ExecutionId,
            _tid: TenantId,
            _vs: &[aegis_orchestrator_core::domain::agent::VolumeSpec],
            _m: &str,
        ) -> anyhow::Result<Vec<Volume>> {
            Ok(vec![])
        }
        async fn persist_external_volume(
            &self,
            _vid: VolumeId,
            _n: String,
            _t: TenantId,
            _p: String,
            _s: u64,
            _o: VolumeOwnership,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A PDF's first bytes, which content sniffing names `application/pdf`.
    const PDF: &[u8] =
        b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj << /Type /Catalog >> endobj\ntrailer << >>\n%%EOF\n";
    /// A PNG's signature and header chunk, sniffed as `image/png`.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00";

    fn b64(bytes: &[u8]) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        encoded
            .as_bytes()
            .chunks(76)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    /// Inbox uid 1, thread `<att@x>`: a `multipart/mixed` whose first part
    /// is a `multipart/alternative` holding the text body and a nested
    /// `multipart/mixed` (an HTML alternative and an unnamed PNG, part
    /// `1.2.2`), then a PDF (part `2`) and a quoted-printable Latin-1 text
    /// file (part `3`).
    fn nested() -> StoredMessage {
        let body = format!(
            "--MIX\r\n\
Content-Type: multipart/alternative; boundary=\"ALT\"\r\n\
\r\n\
--ALT\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
See the attached report.\r\n\
--ALT\r\n\
Content-Type: multipart/mixed; boundary=\"INNER\"\r\n\
\r\n\
--INNER\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>See the attached report.</p>\r\n\
--INNER\r\n\
Content-Type: image/png\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
{png}\r\n\
--INNER--\r\n\
--ALT--\r\n\
--MIX\r\n\
Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
{pdf}\r\n\
--MIX\r\n\
Content-Type: text/plain; charset=iso-8859-1\r\n\
Content-Disposition: attachment; filename=\"../notes\t.txt\"\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
caf=E9 au =\r\n\
lait\r\n\
--MIX--\r\n",
            png = b64(PNG),
            pdf = b64(PDF),
        );
        message(
            1,
            &[],
            &[
                "Message-ID: <att@x>",
                "From: Ann <ann@example.test>",
                "Subject: The report",
                "Date: Tue, 06 Oct 2026 09:00:00 +0000",
                "MIME-Version: 1.0",
                "Content-Type: multipart/mixed; boundary=\"MIX\"",
            ],
            &body,
        )
    }

    /// A message (uid `uid`) whose one attachment `name` is `bytes`, base64.
    fn one_attachment(uid: u32, id: &str, name: &str, kind: &str, bytes: &[u8]) -> StoredMessage {
        let body = format!(
            "--B\r\nContent-Type: text/plain\r\n\r\nAttached.\r\n--B\r\n\
Content-Type: {kind}; name=\"{name}\"\r\n\
Content-Disposition: attachment; filename=\"{name}\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\n{data}\r\n--B--\r\n",
            data = b64(bytes)
        );
        message(
            uid,
            &[],
            &[
                &format!("Message-ID: {id}"),
                "From: owner@example.test",
                "Subject: Attached",
                "Date: Tue, 06 Oct 2026 10:00:00 +0000",
                "Content-Type: multipart/mixed; boundary=\"B\"",
            ],
            &body,
        )
    }

    /// Sent uid 7: a CSV, attachment part `2`.
    const CSV: &[u8] = b"name,amount\nann,12\n";

    struct World {
        _root: tempfile::TempDir,
        root: PathBuf,
        mailbox: MailboxStandIn,
        tools: MailTools,
        volumes: Arc<InMemoryVolumeRepository>,
        id: CredentialBindingId,
    }

    async fn world(inbox: Vec<StoredMessage>) -> World {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let volumes = Arc::new(InMemoryVolumeRepository::new());
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(&root).unwrap()),
            volumes.clone(),
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoOpPublisher),
        ));
        let files = MailFiles {
            file_operations: Arc::new(FileOperationsService::new(
                fsal,
                Arc::new(InMemoryExecutionRepository::new()),
                Arc::new(InMemoryAgentRepository::new()),
            )),
            user_volumes: Arc::new(UserVolumeService::new(
                volumes.clone(),
                Arc::new(HostVolumes {
                    repo: volumes.clone(),
                }),
                Arc::new(EventBus::new(16)),
                StorageTierLimits::default(),
            )),
        };
        let folders = vec![
            StandInFolder::new(SENT, "\\Sent").holding(vec![one_attachment(
                7, "<s7@x>", "sums.csv", "text/csv", CSV,
            )]),
        ];
        let mailbox =
            imap_mailbox_standin_with_capabilities(LOGIN, PASSWORD, inbox, folders, "IMAP4rev1")
                .await;
        let id = CredentialBindingId::new();
        let source = Arc::new(OneMailbox {
            id,
            port: mailbox.port(),
        });
        World {
            _root: dir,
            root,
            tools: MailTools::with_connector(source, Arc::new(PlainConnector)).with_files(files),
            mailbox,
            volumes,
            id,
        }
    }

    fn person() -> MailActing {
        MailActing {
            tier: Some(ZaruTier::Free),
            ..conversation()
        }
    }

    async fn save(w: &World, args: Value) -> Result<Value, SealSessionError> {
        let mut args = args;
        args["mailbox"] = json!(w.id.0.to_string());
        w.tools.invoke("mail.attachment", &args, &person()).await
    }

    /// The person's volumes.
    async fn owned(w: &World) -> Vec<Volume> {
        w.volumes
            .find_by_owner(&TenantId::default(), USER)
            .await
            .unwrap()
    }

    /// The bytes a saved reference points at, read from the volume's host
    /// directory.
    fn saved_bytes(w: &World, answer: &Value) -> Option<Vec<u8>> {
        let volume = answer["volume_id"].as_str()?;
        let path = answer["path"].as_str()?;
        std::fs::read(w.root.join(format!("hosts/{volume}")).join(path)).ok()
    }

    /// Every file under the scratch root.
    fn files_under(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", sha2::Sha256::digest(bytes))
    }

    /// Clause 30: `mail.read` numbers each attachment as RFC 9051 numbers
    /// body parts, nested ones included.
    #[tokio::test]
    async fn mail_read_answers_each_attachment_with_its_rfc_9051_part_number() {
        let w = world(vec![nested()]).await;
        let mut args = json!({"thread_id": "<att@x>"});
        args["mailbox"] = json!(w.id.0.to_string());
        let answer = w
            .tools
            .invoke("mail.read", &args, &person())
            .await
            .expect("mail.read");
        let attachments: Vec<(Value, Value, Value)> = answer["messages"][0]["attachments"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|a| {
                (
                    a["part"].clone(),
                    a["filename"].clone(),
                    a["content_type"].clone(),
                )
            })
            .collect();
        assert_eq!(
            attachments,
            vec![
                (json!("1.2.2"), Value::Null, json!("image/png")),
                (json!("2"), json!("report.pdf"), json!("application/pdf")),
                (json!("3"), json!("../notes\t.txt"), json!("text/plain")),
            ],
            "the attachments mail.read answered: {answer}"
        );
        assert_eq!(
            answer["messages"][0]["body_text"],
            "See the attached report."
        );
    }

    /// Clause 31: a part is fetched read-only, decoded from base64 and
    /// written to the person's `chat-attachments` volume, provisioned on
    /// first use and reused after; its reference is answered with the bytes'
    /// type, size and SHA-256, and a PDF answers no `text`.
    #[tokio::test]
    async fn an_attachment_is_saved_to_the_persons_chat_attachments_volume_provisioned_on_first_use(
    ) {
        let w = world(vec![nested()]).await;
        let mut wrong = Vec::new();
        if !owned(&w).await.is_empty() {
            wrong.push("the person had a volume before the call".to_string());
        }
        let from = w.mailbox.commands().len();
        let pdf = save(&w, json!({"uid": 1, "part": "2"})).await;
        let pdf = match pdf {
            Ok(answer) => answer,
            Err(e) => panic!("mail.attachment failed: {}", sentence(&e)),
        };
        let volumes = owned(&w).await;
        match volumes.as_slice() {
            [v] if v.name == "chat-attachments"
                && v.ownership == VolumeOwnership::persistent(USER.to_string())
                && pdf["volume_id"] == v.id.to_string() => {}
            other => wrong.push(format!(
                "the person's volumes after the call: {:?}, the answer's {}",
                other.iter().map(|v| (&v.name, v.id)).collect::<Vec<_>>(),
                pdf["volume_id"]
            )),
        }
        let path = pdf["path"].as_str().unwrap_or_default().to_string();
        let segments: Vec<&str> = path.split('/').collect();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        if segments.len() != 4
            || segments[0] != "mail"
            || segments[1] != today
            || uuid::Uuid::parse_str(segments[2]).is_err()
            || segments[3] != "report.pdf"
        {
            wrong.push(format!(
                "the path is not mail/{today}/<uuid>/report.pdf: {path}"
            ));
        }
        for (key, expected) in [
            ("name", json!("report.pdf")),
            ("mime_type", json!("application/pdf")),
            ("size", json!(PDF.len())),
            ("sha256", json!(sha(PDF))),
        ] {
            if pdf[key] != expected {
                wrong.push(format!("{key} is {}, not {expected}", pdf[key]));
            }
        }
        if pdf.get("text").is_some() {
            wrong.push(format!("a PDF answered text: {}", pdf["text"]));
        }
        if saved_bytes(&w, &pdf).as_deref() != Some(PDF) {
            wrong.push("the saved file is not the PDF's decoded bytes".to_string());
        }
        let commands = sent_since(&w.mailbox, from);
        wrong.extend(not_read_only(&commands, "INBOX"));
        if !commands
            .iter()
            .any(|c| c.to_ascii_uppercase() == "UID FETCH 1 (UID BODY.PEEK[])")
        {
            wrong.push(format!(
                "the message was not fetched whole by uid: {commands:?}"
            ));
        }

        // A second save reuses the volume; an unnamed part is `attachment`.
        match save(&w, json!({"uid": 1, "part": "1.2.2"})).await {
            Ok(png) => {
                if png["volume_id"] != pdf["volume_id"] || owned(&w).await.len() != 1 {
                    wrong.push(format!("a second volume: {}", png["volume_id"]));
                }
                if png["name"] != "attachment" || png["mime_type"] != "image/png" {
                    wrong.push(format!("the unnamed PNG: {png}"));
                }
                if saved_bytes(&w, &png).as_deref() != Some(PNG) {
                    wrong.push("the saved file is not the PNG's decoded bytes".to_string());
                }
            }
            Err(e) => wrong.push(format!("the PNG: {}", sentence(&e))),
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Clause 31: a quoted-printable `text/*` part is decoded, saved as its
    /// bytes, named with path separators and control characters removed,
    /// and answered with its text in its charset.
    #[tokio::test]
    async fn a_short_text_attachment_is_decoded_from_quoted_printable_and_answers_its_text() {
        let w = world(vec![nested()]).await;
        let answer = match save(&w, json!({"uid": 1, "part": "3"})).await {
            Ok(answer) => answer,
            Err(e) => panic!("mail.attachment failed: {}", sentence(&e)),
        };
        let latin1 = b"caf\xe9 au lait";
        let mut wrong = Vec::new();
        if answer["text"] != "café au lait" {
            wrong.push(format!("text: {}", answer["text"]));
        }
        if answer["name"] != "..notes.txt" {
            wrong.push(format!("name: {}", answer["name"]));
        }
        if answer["size"] != json!(latin1.len()) || answer["sha256"] != json!(sha(latin1)) {
            wrong.push(format!("size and sha256: {answer}"));
        }
        if saved_bytes(&w, &answer).as_deref() != Some(&latin1[..]) {
            wrong.push("the saved file is not the decoded bytes".to_string());
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Clause 31 with clause 22: a message in another folder is saved by
    /// `folder`; its uid names nothing in the inbox.
    #[tokio::test]
    async fn an_attachment_of_a_message_in_another_folder_is_saved_by_folder() {
        let w = world(vec![nested()]).await;
        let mut wrong = Vec::new();
        let from = w.mailbox.commands().len();
        match save(&w, json!({"folder": "sent", "uid": 7, "part": "2"})).await {
            Ok(answer) => {
                if answer["name"] != "sums.csv"
                    || answer["text"] != "name,amount\nann,12\n"
                    || saved_bytes(&w, &answer).as_deref() != Some(CSV)
                {
                    wrong.push(format!("the CSV from Sent: {answer}"));
                }
            }
            Err(e) => wrong.push(format!("Sent: {}", sentence(&e))),
        }
        wrong.extend(not_read_only(&sent_since(&w.mailbox, from), SENT));
        match save(&w, json!({"uid": 7, "part": "2"})).await {
            Err(e) if sentence(&e) == "There is no attachment '2' on message 7 in this folder." => {
            }
            other => wrong.push(format!("uid 7 in the inbox: {other:?}")),
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Clause 31: a part the message does not carry as an attachment (none
    /// numbered so, or its text body) is refused with nothing provisioned or
    /// written.
    #[tokio::test]
    async fn a_part_that_is_no_attachment_is_refused_with_nothing_written() {
        let w = world(vec![nested()]).await;
        let mut wrong = Vec::new();
        for part in ["4", "1.1", "1.2.1"] {
            match save(&w, json!({"uid": 1, "part": part})).await {
                Err(e)
                    if sentence(&e)
                        == format!(
                            "There is no attachment '{part}' on message 1 in this folder."
                        ) => {}
                other => wrong.push(format!("part {part}: {other:?}")),
            }
        }
        if !owned(&w).await.is_empty() || !files_under(&w.root).is_empty() {
            wrong.push("a volume or a file was made for a missing part".to_string());
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Clause 31: a part larger than 20 MiB is refused with its sentence,
    /// nothing provisioned and nothing written, though the Free tier's
    /// files may be larger; a part of 20 MiB exactly, whose message is about
    /// 27 MiB in base64, is fetched whole and saved.
    #[tokio::test]
    async fn an_attachment_larger_than_20_mib_is_refused_with_nothing_written() {
        let limit = 20 * 1024 * 1024;
        let over = vec![b'x'; limit + 1];
        let at = vec![b'y'; limit];
        let w = world(vec![
            one_attachment(1, "<over@x>", "over.bin", "application/octet-stream", &over),
            one_attachment(2, "<at@x>", "at.bin", "application/octet-stream", &at),
        ])
        .await;
        let tier_max = StorageTierLimits::default().limits[&ZaruTier::Free].max_file_size_bytes;
        assert!(
            tier_max > over.len() as u64,
            "the Free tier allows {tier_max}"
        );
        let mut wrong = Vec::new();
        match save(&w, json!({"uid": 1, "part": "2"})).await {
            Err(e) if sentence(&e) == ATTACHMENT_TOO_LARGE => {}
            Err(e) => wrong.push(format!("20 MiB and a byte: {}", sentence(&e))),
            Ok(v) => wrong.push(format!("20 MiB and a byte was saved: {}", v["size"])),
        }
        if !owned(&w).await.is_empty() || !files_under(&w.root).is_empty() {
            wrong.push("a volume or a file was made for a refused part".to_string());
        }
        match save(&w, json!({"uid": 2, "part": "2"})).await {
            Ok(v)
                if v["size"] == json!(limit) && saved_bytes(&w, &v).as_deref() == Some(&at[..]) => {
            }
            Ok(v) => wrong.push(format!("20 MiB exactly: {v}")),
            Err(e) => wrong.push(format!("20 MiB exactly: {}", sentence(&e))),
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// A run with no person is refused as every mail tool refuses it, before
    /// any connection.
    #[tokio::test]
    async fn a_run_with_no_person_is_refused_before_any_connection() {
        let w = world(vec![nested()]).await;
        let acting = MailActing {
            user_id: None,
            ..person()
        };
        let args = json!({"mailbox": w.id.0.to_string(), "uid": 1, "part": "2"});
        match w.tools.invoke("mail.attachment", &args, &acting).await {
            Err(e) if sentence(&e) == NO_PERSON => {}
            other => panic!("no person: {other:?}"),
        }
        assert_eq!(w.mailbox.connections(), 0, "a connection was opened");
    }
}
