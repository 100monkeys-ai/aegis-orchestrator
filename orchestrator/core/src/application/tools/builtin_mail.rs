// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The mail tools: `mail.list`, `mail.read` and `mail.label` (AEGIS
//! ADR-125 D4; its Update of 2026-10-07 clauses 1, 2 and 7), and the
//! outbound `mail.draft`, `mail.send` and `mail.reply` (its Update of
//! 2026-10-07 (3), clauses 11 to 15), `mail.delete` (its Update of
//! 2026-10-08 (4), clauses 17 to 20), and `mail.archive` (its Update of
//! 2026-10-08 (5), clauses 27 to 29).
//!
//! They speak IMAP only, over a mailbox of the acting person (ADR-125's
//! Update of 2026-10-04 clause 3), inside the orchestrator: an `imap`
//! `mailbox` binding logs in with its password, and an OAuth binding
//! granted Google's mail scope authenticates with XOAUTH2 and the token
//! `access_token_for` answers (its Update of 2026-10-07 clauses 8 and 10);
//! neither secret reaches an agent. They work by UID; bodies are read with
//! `BODY.PEEK`, so no tool marks a message read.
//!
//! **Folders** (its Update of 2026-10-08 (5), clauses 22 to 25).
//! `mail.list` and `mail.read` take `folder`: `inbox` (the default),
//! `sent`, `drafts`, `trash`, `archive` or `all`, each located by its RFC
//! 6154 attribute or the name rule (`locate`) and opened by `EXAMINE`;
//! one folder per call, since UIDs are per folder. Every other tool that
//! names a thread works on `INBOX` and refuses another `folder` before any
//! connection. `mail.label` sets and clears `\Seen` by `seen` (clause 26).
//!
//! **Who may use a mailbox** (clause 7, and the Update of 2026-10-07 (2)).
//! The tool acts as the call's person (none for a service account:
//! refused). The `mailbox` argument must name the person's own active
//! `mailbox` binding, by id, or by its context name when mailboxes are
//! chosen. The choice for the key `imap` (the execution record's `contexts`
//! when the execution has a record, else the call's `_meta.contexts`) is a
//! set of any number of mailboxes, which must, when given, hold that
//! binding; a choice of none refuses. With nothing chosen, an execution with a record needs
//! the binding granted to the calling agent, its workflow or all agents; a
//! call with no execution record (a conversation, or an MCP client acting
//! as the person) is admitted on the ownership check alone, since its agent
//! id is the session's own and no grant can name it.
//!
//! **The outbound tools.** A message is given in full: `to` (1 to 50
//! addresses), an optional `cc` (at most 50 recipients in all), `subject`
//! (one line) and a plain-text `body`; no `bcc`, and no draft id, since
//! the approval summary is built from the call's arguments. The
//! orchestrator mints its `Message-ID` and writes it to the result.
//! `mail.draft` appends it to the Drafts folder with `\Draft \Seen`.
//! `mail.send` submits it over SMTP, then appends it to the Sent folder
//! with `\Seen` unless that folder already holds its `Message-ID` (a server
//! that files its own copy); a failure to append after a send is reported
//! in the result, never as an error, so no retry sends twice. `mail.reply`
//! answers a thread's newest `INBOX` message, threading by `In-Reply-To`
//! and `References`. `mail.send` and `mail.reply` are gated by the
//! approval gate; their mailbox is admitted before it ([`MailTools::admit_mailbox`]).
//!
//! **Deleting.** `mail.delete` moves a thread's `INBOX` messages to the
//! Trash folder (the folder `LIST` marks `\Trash`, else one named `Trash`
//! or `Deleted`), by `UID MOVE` where the server can move, else by `UID
//! COPY`, `UID STORE +FLAGS (\Deleted)` and `UID EXPUNGE` of those UIDs
//! only; a server that can do neither is refused before any change. It
//! never expunges Trash and never sends a plain `EXPUNGE`. It is gated;
//! before the gate its admission reads the thread's subject and senders
//! into the call's `subject` and `from`, so the person reads what is
//! deleted.
//!
//! **Archiving.** `mail.archive` moves a thread's `INBOX` messages to the
//! Archive folder (the folder `LIST` marks `\Archive`, else one named
//! `Archive`, else the folder `LIST` marks `\All`, where a server that
//! shows the inbox as a label archives by a move out of `INBOX`) by the same
//! move as `mail.delete` and with the same refusals, its own sentences
//! saying nothing was archived. It is matched by the RFC 6154 attribute,
//! never by a provider's folder name or extension. It is gated, and its
//! admission reads the thread's subject and senders as `mail.delete`'s does.
//!
//! **Attachments** (its Update of 2026-10-08 (5), clauses 30 and 31).
//! `mail.read` answers each attachment's `part`, its section number as RFC
//! 9051 numbers a message's body parts. `mail.attachment` fetches one
//! message by uid from a folder, decodes that part and saves it to the
//! acting person's own `chat-attachments` volume (provisioned as an upload
//! provisions it) at `mail/<UTC date>/<uuid>/<name>`, answering the file's
//! reference and, for a short text part, its text. A part larger than the
//! person's tier allows a file to be, or than 20 MiB, is refused with
//! nothing written.
//!
//! **Threads.** A thread's id is the root `Message-ID` of its messages (the
//! first `References` entry, else `In-Reply-To`, else the message's own); a
//! message with no `Message-ID` is its own thread `uid:<UIDVALIDITY>:<uid>`.
//! A thread's messages are found by `UID SEARCH` over `Message-ID`,
//! `References` and `In-Reply-To`: plain IMAP, no server extension.

use crate::application::credential_service::{ToolCallActor, ToolMailbox, ToolMailboxSource};
use crate::application::file_operations_service::{FileOperationsError, FileOperationsService};
use crate::application::user_volume_service::UserVolumeService;
use crate::domain::agent::AgentId;
use crate::domain::credential::{CredentialBindingId, CredentialProvider};
use crate::domain::execution::{ContextChoice, ServerChoice};
use crate::domain::iam::ZaruTier;
use crate::domain::seal_session::{CallerAnswer, InternalFailure, SealSessionError};
use crate::domain::tenant::TenantId;
use crate::infrastructure::mail::message::{is_address, mint_message_id, OutgoingMessage};
use crate::infrastructure::mail::session::{
    attachment_part, decode_text, parse_headers, parse_message, Arg, Fetched, FolderStatus,
    Headers, ImapSession, ListedFolder, StoreOp,
};
use crate::infrastructure::mail::submission;
use crate::infrastructure::mail::{
    CheckFailureKind, MailConnector, MailboxCheckFailure, RustlsMailConnector,
};
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

/// The inbox: the folder every mail tool but the two read tools works on,
/// and theirs by default.
pub const FOLDER: &str = "INBOX";
/// The newest matches `mail.list` groups into threads.
pub const LIST_WINDOW: usize = 200;
/// `mail.list`'s default and largest `limit`.
pub const LIST_DEFAULT_LIMIT: u64 = 20;
pub const LIST_MAX_LIMIT: u64 = 50;
/// The most messages of one thread `mail.read` answers (the newest).
pub const READ_MAX_MESSAGES: usize = 50;
/// The most characters of one message's body `mail.read` answers.
pub const BODY_MAX_CHARS: usize = 32_000;
/// The largest attachment `mail.attachment` saves, whatever the person's
/// tier allows (the Update of 2026-10-08 (5) clause 31).
pub const ATTACHMENT_MAX_BYTES: usize = 20 * 1024 * 1024;
/// The largest message `mail.attachment` fetches whole: room for an
/// attachment of [`ATTACHMENT_MAX_BYTES`] in base64 (about 27.4 MiB with its
/// line breaks) beside the rest of the message.
pub const ATTACHMENT_MESSAGE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// The most characters of a `text/*` attachment `mail.attachment` answers
/// as `text`; a longer one is answered by its file alone.
pub const ATTACHMENT_TEXT_MAX_CHARS: usize = 32_000;
/// The longest a label may be.
pub const KEYWORD_MAX_CHARS: usize = 64;
/// The longest one tool call's session may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The header fields `mail.list` and `mail.label` fetch.
const LIST_FIELDS: &str =
    "BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES)]";

/// The refusal for a call with no person.
pub const NO_PERSON: &str =
    "This tool needs your own mailbox, and no person is recorded for this run.";
/// The refusal for a chosen binding other than the `mailbox` argument.
pub const CHOSEN_DIFFERENT: &str =
    "This tool needs your own mailbox, and the one chosen for this run is a different one.";
/// The refusal for a mailbox outside several chosen ones (the Update of
/// 2026-10-07 (2) clause 1).
pub const NOT_AMONG_CHOSEN: &str =
    "This tool needs your own mailbox, and the one named is not among those chosen for this run.";
/// The refusal for a choice of none.
pub const NONE_CHOSEN: &str = "This tool needs your own mailbox, and none was chosen for this run.";
/// The refusal for an agent's run whose agent holds no grant.
pub const NOT_GRANTED: &str = "This tool needs your own mailbox, granted to this agent.";
/// The refusal of `mail.draft` for a mailbox with no Drafts folder.
pub const NO_DRAFTS: &str = "This mailbox has no Drafts folder; nothing was saved.";
/// The refusal of `mail.delete` for a mailbox with no Trash folder.
pub const NO_TRASH: &str = "This mailbox has no Trash folder; nothing was deleted.";
/// The refusal of `mail.delete` for a server that advertises neither
/// `MOVE` nor `UIDPLUS` (nor `IMAP4rev2`, which holds both).
pub const NO_SAFE_MOVE: &str =
    "This mailbox's server can neither move messages nor expunge only chosen ones; nothing was deleted.";
/// The refusal of `mail.archive` for a mailbox with no Archive folder (the
/// Update of 2026-10-08 (5) clause 28).
pub const NO_ARCHIVE: &str = "This mailbox has no Archive folder; nothing was archived.";
/// The refusal of `mail.archive` for a server that advertises neither
/// `MOVE` nor `UIDPLUS` (nor `IMAP4rev2`) (its clause 27).
pub const NO_SAFE_ARCHIVE: &str =
    "This mailbox's server can neither move messages nor expunge only chosen ones; nothing was archived.";
/// What a send's result says when the mailbox has no Sent folder.
pub const NO_SENT: &str = "This mailbox has no Sent folder.";
/// The refusal of a `folder` that is not one of the six (the Update of
/// 2026-10-08 (5) clause 22).
pub const UNKNOWN_FOLDER: &str =
    "'folder' must be one of inbox, sent, drafts, trash, archive or all.";
/// The refusal of a `folder` other than the inbox on a tool that works on
/// the inbox only (its clause 25).
pub const INBOX_ONLY: &str =
    "This tool works on threads in the inbox only; leave out 'folder' or set it to inbox.";

/// The refusal of `mail.attachment` for a part larger than the person's
/// files may be, or than [`ATTACHMENT_MAX_BYTES`] (its clause 31).
pub const ATTACHMENT_TOO_LARGE: &str =
    "This attachment is larger than your files allow; nothing was saved.";
/// What a saved attachment is named when its part names no file.
pub const ATTACHMENT_DEFAULT_NAME: &str = "attachment";

/// The most recipients one message has, `to` and `cc` together.
pub const MAX_RECIPIENTS: usize = 50;
/// The longest a subject may be, in characters.
pub const SUBJECT_MAX_CHARS: usize = 998;
/// The longest an outbound body may be, in characters.
pub const SEND_BODY_MAX_CHARS: usize = 100_000;

/// The refusal for a message with more than [`MAX_RECIPIENTS`] recipients.
pub const TOO_MANY_RECIPIENTS: &str = "A message has at most 50 recipients.";
/// The refusal for a subject that is not one line of at most
/// [`SUBJECT_MAX_CHARS`] characters.
pub const BAD_SUBJECT: &str = "'subject' must be one line of at most 998 characters.";
/// The refusal for a body that is not plain text of at most
/// [`SEND_BODY_MAX_CHARS`] characters.
pub const BAD_BODY: &str = "'body' must be plain text of at most 100000 characters.";

/// Whether `tool_name` is one of the mail tools this module serves.
pub fn is_mail_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "mail.list"
            | "mail.read"
            | "mail.label"
            | "mail.draft"
            | "mail.send"
            | "mail.reply"
            | "mail.delete"
            | "mail.archive"
            | "mail.attachment"
    )
}

/// What [`MailTools::admit_mailbox`] admitted: the binding the call names,
/// and the arguments the admission read for the person (`subject` and
/// `from` for `mail.delete` and `mail.archive`; none for another tool),
/// which the caller writes into the call before the gate.
#[derive(Debug, Clone)]
pub struct Admitted {
    pub binding: CredentialBindingId,
    pub shown: Vec<(&'static str, Value)>,
}

/// Who a mail tool's call acts for, and what its run chose for `imap`.
#[derive(Debug, Clone)]
pub struct MailActing {
    pub tenant_id: TenantId,
    /// The acting person; `None` for a run with no person recorded.
    pub user_id: Option<String>,
    pub agent_id: AgentId,
    pub workflow_id: Option<uuid::Uuid>,
    /// The choice for the key `imap`: nothing, none, or a set of mailboxes.
    pub choice: ServerChoice,
    /// Whether the call's execution has a record (an agent's run), as
    /// opposed to a conversation's session.
    pub has_execution_record: bool,
    /// The acting person's tier, which bounds a file `mail.attachment`
    /// saves; `None` where the caller is no consumer, read as the Free
    /// tier's limits.
    pub tier: Option<ZaruTier>,
}

impl MailActing {
    /// The choice key a mailbox is chosen under: the provider `imap`.
    pub fn choice_key() -> &'static str {
        CredentialProvider::IMAP
    }
}

/// The file services `mail.attachment` saves through: the person's
/// `chat-attachments` volume found or provisioned, and the file written
/// under their tier's limit.
#[derive(Clone)]
pub struct MailFiles {
    pub file_operations: Arc<FileOperationsService>,
    pub user_volumes: Arc<UserVolumeService>,
}

/// The mail tools: the mailbox source, the connector their sessions open
/// over, and the file services an attachment is saved through.
pub struct MailTools {
    mailboxes: Arc<dyn ToolMailboxSource>,
    connector: Arc<dyn MailConnector>,
    files: Option<MailFiles>,
}

impl MailTools {
    /// The production tools: TLS by rustls and the address guard.
    pub fn new(mailboxes: Arc<dyn ToolMailboxSource>) -> Self {
        Self::with_connector(mailboxes, Arc::new(RustlsMailConnector::new()))
    }

    /// The tools over another connector (tests use a plaintext one against
    /// loopback stand-ins).
    pub fn with_connector(
        mailboxes: Arc<dyn ToolMailboxSource>,
        connector: Arc<dyn MailConnector>,
    ) -> Self {
        Self {
            mailboxes,
            connector,
            files: None,
        }
    }

    /// The tools with the file services `mail.attachment` saves through;
    /// without them it is refused as not configured.
    pub fn with_files(mut self, files: MailFiles) -> Self {
        self.files = Some(files);
        self
    }

    /// Run `tool_name` with `args` for `acting`.
    pub async fn invoke(
        &self,
        tool_name: &str,
        args: &Value,
        acting: &MailActing,
    ) -> Result<Value, SealSessionError> {
        let mailbox = self.mailbox_for(args, acting).await?;
        let request = Request::parse(tool_name, args)?;
        if let Request::Attachment { folder, uid, part } = &request {
            let Some(files) = self.files.as_ref() else {
                return Err(not_configured());
            };
            let save = save_attachment(
                self.connector.as_ref(),
                files,
                &mailbox,
                acting,
                *folder,
                *uid,
                part,
            );
            return match tokio::time::timeout(CALL_TIMEOUT, save).await {
                Ok(result) => result,
                Err(_) => Err(timed_out()),
            };
        }
        let run = run(self.connector.as_ref(), &mailbox, &request);
        match tokio::time::timeout(CALL_TIMEOUT, run).await {
            Ok(result) => result,
            Err(_) => Err(timed_out()),
        }
    }

    /// Admit a call before the approval gate (the Update of 2026-10-07 (3)
    /// clause 11): its arguments are well formed and its `mailbox`, by id or
    /// by context name, is one the call may use, by the same resolution the
    /// call itself makes. Answers the binding's id, which the caller writes
    /// back into `mailbox` so the stored call and any standing choice name
    /// the binding by id; a refusal is the call's own sentence.
    ///
    /// For `mail.delete` (the Update of 2026-10-08 (4) clause 20) it also
    /// reads the thread, read-only, and answers the arguments the person
    /// reads before answering: `subject` and `from`, which the caller
    /// writes into the call over any value the model gave. A thread not in
    /// `INBOX`, a mailbox with no Trash folder, or a server that cannot move
    /// safely is refused here, so no person is asked about it. `mail.archive`
    /// is admitted the same way, its Archive folder in place of Trash (the
    /// Update of 2026-10-08 (5) clause 29).
    pub async fn admit_mailbox(
        &self,
        tool_name: &str,
        args: &Value,
        acting: &MailActing,
    ) -> Result<Admitted, SealSessionError> {
        let mailbox = self.mailbox_for(args, acting).await?;
        let request = Request::parse(tool_name, args)?;
        let shown = match &request {
            Request::Move { thread_id, to } => {
                let read = shown_for_move(self.connector.as_ref(), &mailbox, thread_id, *to);
                match tokio::time::timeout(CALL_TIMEOUT, read).await {
                    Ok(shown) => shown?,
                    Err(_) => return Err(timed_out()),
                }
            }
            _ => Vec::new(),
        };
        Ok(Admitted {
            binding: mailbox.binding_id,
            shown,
        })
    }

    /// The mailbox the call may use, or its refusal.
    async fn mailbox_for(
        &self,
        args: &Value,
        acting: &MailActing,
    ) -> Result<ToolMailbox, SealSessionError> {
        let Some(user_id) = acting.user_id.as_deref() else {
            return Err(binding_required(NO_PERSON.to_string()));
        };
        let raw = args.get("mailbox").and_then(Value::as_str).ok_or_else(|| {
            SealSessionError::InvalidArguments(
                "'mailbox' must be the id of one of your mailbox connections.".to_string(),
            )
        })?;
        let not_yours = || {
            let shown: String = raw.chars().filter(|c| !c.is_control()).take(64).collect();
            binding_required(format!(
                "'{shown}' is not an active mailbox connection of yours."
            ))
        };
        let id = match uuid::Uuid::parse_str(raw) {
            Ok(id) => CredentialBindingId(id),
            // The Update of 2026-10-07 (2) clause 2: a chosen mailbox by its
            // context name, resolved within the set.
            Err(_) => match &acting.choice {
                ServerChoice::Bindings(chosen) => self
                    .mailboxes
                    .mailbox_contexts(&acting.tenant_id, user_id)
                    .await
                    .map_err(|e| {
                        SealSessionError::InternalError(format!("mailbox lookup failed: {e}"))
                            .answered(CallerAnswer::Internal(InternalFailure::Server))
                    })?
                    .into_iter()
                    .find(|mailbox| mailbox.name == raw && chosen.contains(&mailbox.id))
                    .map(|mailbox| mailbox.id)
                    .ok_or_else(not_yours)?,
                _ => return Err(not_yours()),
            },
        };
        let actor = ToolCallActor {
            tenant_id: &acting.tenant_id,
            user_id,
            agent_id: acting.agent_id,
            workflow_id: acting.workflow_id,
            context: match &acting.choice {
                ServerChoice::NotGiven => ContextChoice::NotGiven,
                ServerChoice::Bindings(chosen) if chosen.contains(&id) => {
                    ContextChoice::Binding(id)
                }
                ServerChoice::None | ServerChoice::Bindings(_) => ContextChoice::None,
            },
        };
        let mailbox = self
            .mailboxes
            .tool_mailbox(&actor, &id)
            .await
            .map_err(|e| {
                SealSessionError::InternalError(format!("mailbox lookup failed: {e}"))
                    .answered(CallerAnswer::Internal(InternalFailure::Server))
            })?
            .ok_or_else(not_yours)?;
        match &acting.choice {
            ServerChoice::Bindings(chosen) if chosen.contains(&id) => {}
            ServerChoice::Bindings(chosen) if chosen.len() == 1 => {
                return Err(binding_required(CHOSEN_DIFFERENT.to_string()))
            }
            ServerChoice::Bindings(_) => {
                return Err(binding_required(NOT_AMONG_CHOSEN.to_string()))
            }
            ServerChoice::None => return Err(binding_required(NONE_CHOSEN.to_string())),
            ServerChoice::NotGiven if acting.has_execution_record && !mailbox.granted => {
                return Err(binding_required(NOT_GRANTED.to_string()))
            }
            ServerChoice::NotGiven => {}
        }
        Ok(mailbox)
    }
}

/// The answer for a call that is not served when the node has no mail
/// tools configured.
pub fn not_configured() -> SealSessionError {
    SealSessionError::InternalError("the mail tools are not configured on this node".to_string())
        .answered(CallerAnswer::Internal(InternalFailure::Unavailable))
}

fn timed_out() -> SealSessionError {
    SealSessionError::UpstreamUnavailable(format!(
        "The mail server did not answer within {} seconds.",
        CALL_TIMEOUT.as_secs()
    ))
}

fn binding_required(message: String) -> SealSessionError {
    SealSessionError::NotFound(message.clone())
        .answered(CallerAnswer::CredentialBindingRequired { message })
}

fn invalid(message: impl Into<String>) -> SealSessionError {
    SealSessionError::InvalidArguments(message.into())
}

fn session_error(failure: MailboxCheckFailure) -> SealSessionError {
    match failure.kind {
        CheckFailureKind::HostNotAllowed { .. } => invalid(format!(
            "This mailbox's mail server is not one this node may reach: {}",
            failure.reply
        )),
        CheckFailureKind::Unreachable => SealSessionError::UpstreamUnavailable(format!(
            "The mail server did not complete the request: {}",
            failure.reply
        )),
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Request {
    List {
        query: Option<String>,
        from: Option<String>,
        unread_only: bool,
        flagged_only: bool,
        since: Option<chrono::NaiveDate>,
        limit: usize,
        folder: FolderKind,
    },
    Read {
        thread_id: String,
        folder: FolderKind,
    },
    Label {
        thread_id: String,
        add: Vec<String>,
        remove: Vec<String>,
        flagged: Option<bool>,
        seen: Option<bool>,
    },
    Draft {
        message: Outbound,
        thread_id: Option<String>,
    },
    Send {
        message: Outbound,
    },
    Reply {
        message: Outbound,
        thread_id: String,
    },
    /// `mail.delete` (to Trash) and `mail.archive` (to Archive).
    Move {
        thread_id: String,
        to: MoveTo,
    },
    /// `mail.attachment`: one part of one message, saved to the person's
    /// files.
    Attachment {
        folder: FolderKind,
        uid: u32,
        part: String,
    },
}

/// Where a thread's `INBOX` messages move: Trash (`mail.delete`) or the
/// Archive folder (`mail.archive`), each with its own refusals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveTo {
    Trash,
    Archive,
}

impl MoveTo {
    /// The folder kind located by `locate`.
    fn kind(self) -> FolderKind {
        match self {
            MoveTo::Trash => FolderKind::Trash,
            MoveTo::Archive => FolderKind::Archive,
        }
    }

    /// The refusal for a mailbox without the folder.
    fn no_folder(self) -> &'static str {
        match self {
            MoveTo::Trash => NO_TRASH,
            MoveTo::Archive => NO_ARCHIVE,
        }
    }

    /// The refusal for a server that can neither move nor expunge chosen
    /// messages.
    fn no_safe_move(self) -> &'static str {
        match self {
            MoveTo::Trash => NO_SAFE_MOVE,
            MoveTo::Archive => NO_SAFE_ARCHIVE,
        }
    }

    /// The result's key naming the folder the thread moved to.
    fn result_key(self) -> &'static str {
        match self {
            MoveTo::Trash => "trash_folder",
            MoveTo::Archive => "archive_folder",
        }
    }
}

/// Which folder a read tool works on (the Update of 2026-10-08 (5) clause
/// 22): the `folder` argument's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKind {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Archive,
    All,
}

impl FolderKind {
    /// The `folder` argument: absent or null is the inbox; anything but
    /// one of the six lowercase values is refused.
    fn parse(args: &Value) -> Result<Self, SealSessionError> {
        match args.get("folder") {
            None | Some(Value::Null) => Ok(FolderKind::Inbox),
            Some(Value::String(s)) => match s.as_str() {
                "inbox" => Ok(FolderKind::Inbox),
                "sent" => Ok(FolderKind::Sent),
                "drafts" => Ok(FolderKind::Drafts),
                "trash" => Ok(FolderKind::Trash),
                "archive" => Ok(FolderKind::Archive),
                "all" => Ok(FolderKind::All),
                _ => Err(invalid(UNKNOWN_FOLDER)),
            },
            Some(_) => Err(invalid(UNKNOWN_FOLDER)),
        }
    }

    /// The argument's value, which a result carries as `folder_kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            FolderKind::Inbox => "inbox",
            FolderKind::Sent => "sent",
            FolderKind::Drafts => "drafts",
            FolderKind::Trash => "trash",
            FolderKind::Archive => "archive",
            FolderKind::All => "all",
        }
    }

    /// The folder as a person names it.
    fn title(self) -> &'static str {
        match self {
            FolderKind::Inbox => "Inbox",
            FolderKind::Sent => "Sent",
            FolderKind::Drafts => "Drafts",
            FolderKind::Trash => "Trash",
            FolderKind::Archive => "Archive",
            FolderKind::All => "All Mail",
        }
    }

    /// Where a thread was looked for, in the not-found sentence (clause
    /// 24): `inbox`, as before, or `<title> folder`.
    fn place(self) -> String {
        match self {
            FolderKind::Inbox => "inbox".to_string(),
            other => format!("{} folder", other.title()),
        }
    }
}

/// A tool that works on the inbox only refuses any other `folder` (the
/// Update of 2026-10-08 (5) clause 25): absent, null and `inbox` pass.
fn inbox_only(args: &Value) -> Result<(), SealSessionError> {
    match args.get("folder") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(s)) if s == "inbox" => Ok(()),
        Some(_) => Err(invalid(INBOX_ONLY)),
    }
}

/// What an outbound call gives: the recipients, subject and body.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outbound {
    to: Vec<String>,
    cc: Vec<String>,
    subject: String,
    body: String,
}

impl Outbound {
    /// Parse an outbound call's message. `to` and `subject` are required
    /// unless `drafting`; the body always is.
    fn parse(args: &Value, drafting: bool) -> Result<Self, SealSessionError> {
        let to = addresses(args, "to")?;
        let cc = addresses(args, "cc")?;
        if to.is_empty() && !drafting {
            return Err(invalid("'to' must list at least one email address."));
        }
        if to.len() + cc.len() > MAX_RECIPIENTS {
            return Err(invalid(TOO_MANY_RECIPIENTS));
        }
        let subject = match args.get("subject") {
            None | Some(Value::Null) if drafting => String::new(),
            Some(Value::String(s))
                if !s.contains(['\r', '\n']) && s.chars().count() <= SUBJECT_MAX_CHARS =>
            {
                s.clone()
            }
            _ => return Err(invalid(BAD_SUBJECT)),
        };
        let body = match args.get("body") {
            Some(Value::String(b)) if b.chars().count() <= SEND_BODY_MAX_CHARS => b.clone(),
            _ => return Err(invalid(BAD_BODY)),
        };
        Ok(Self {
            to,
            cc,
            subject,
            body,
        })
    }
}

/// The addresses of the list argument `name`: absent is none.
fn addresses(args: &Value, name: &str) -> Result<Vec<String>, SealSessionError> {
    let list = match args.get(name) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(list)) => list,
        Some(_) => {
            return Err(invalid(format!(
                "'{name}' must be a list of email addresses."
            )))
        }
    };
    let mut out = Vec::new();
    for item in list {
        let Some(address) = item.as_str() else {
            return Err(invalid(format!(
                "'{name}' must be a list of email addresses."
            )));
        };
        if !is_address(address) {
            let shown: String = address
                .chars()
                .filter(|c| !c.is_control())
                .take(80)
                .collect();
            return Err(invalid(format!(
                "'{shown}' is not an email address this tool can send to."
            )));
        }
        out.push(address.to_string());
    }
    Ok(out)
}

fn optional_string(args: &Value, name: &str) -> Result<Option<String>, SealSessionError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(_) => Err(invalid(format!("'{name}' must be a string."))),
    }
}

fn optional_bool(args: &Value, name: &str) -> Result<Option<bool>, SealSessionError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid(format!("'{name}' must be true or false."))),
    }
}

fn thread_id(args: &Value) -> Result<String, SealSessionError> {
    optional_string(args, "thread_id")?
        .ok_or_else(|| invalid("'thread_id' must be a thread id that mail.list answered."))
}

fn keywords(args: &Value, name: &str) -> Result<Vec<String>, SealSessionError> {
    let list = match args.get(name) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(list)) => list,
        Some(_) => return Err(invalid(format!("'{name}' must be a list of labels."))),
    };
    let mut out = Vec::new();
    for item in list {
        let Some(keyword) = item.as_str() else {
            return Err(invalid(format!("'{name}' must be a list of labels.")));
        };
        if !is_keyword(keyword) {
            let shown: String = keyword
                .chars()
                .filter(|c| !c.is_control())
                .take(80)
                .collect();
            return Err(invalid(format!(
                "'{shown}' is not a label this tool can set: a label is 1 to {KEYWORD_MAX_CHARS} letters, digits or punctuation, without spaces, parentheses, braces, quotes, '%', '*', ']' or a leading backslash."
            )));
        }
        if !out.iter().any(|k: &String| k.eq_ignore_ascii_case(keyword)) {
            out.push(keyword.to_string());
        }
    }
    Ok(out)
}

/// An IMAP flag keyword (RFC 9051 `atom`), not a system flag.
fn is_keyword(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= KEYWORD_MAX_CHARS
        && !s.starts_with('\\')
        && s.bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !b"(){%*\"\\]".contains(&b))
}

impl Request {
    fn parse(tool_name: &str, args: &Value) -> Result<Self, SealSessionError> {
        if matches!(
            tool_name,
            "mail.label" | "mail.delete" | "mail.archive" | "mail.reply" | "mail.draft"
        ) {
            inbox_only(args)?;
        }
        match tool_name {
            "mail.list" => {
                let folder = FolderKind::parse(args)?;
                let since = match optional_string(args, "since")? {
                    None => None,
                    Some(s) => Some(
                        chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                            .map_err(|_| invalid("'since' must be a date written YYYY-MM-DD."))?,
                    ),
                };
                let limit = match args.get("limit") {
                    None | Some(Value::Null) => LIST_DEFAULT_LIMIT,
                    Some(v) => match v.as_u64() {
                        Some(n) if (1..=LIST_MAX_LIMIT).contains(&n) => n,
                        _ => {
                            return Err(invalid(format!(
                                "'limit' must be a whole number from 1 to {LIST_MAX_LIMIT}."
                            )))
                        }
                    },
                };
                Ok(Request::List {
                    query: optional_string(args, "query")?,
                    from: optional_string(args, "from")?,
                    unread_only: optional_bool(args, "unread_only")?.unwrap_or(false),
                    flagged_only: optional_bool(args, "flagged_only")?.unwrap_or(false),
                    since,
                    limit: limit as usize,
                    folder,
                })
            }
            "mail.read" => Ok(Request::Read {
                folder: FolderKind::parse(args)?,
                thread_id: thread_id(args)?,
            }),
            "mail.label" => {
                let request = Request::Label {
                    thread_id: thread_id(args)?,
                    add: keywords(args, "add")?,
                    remove: keywords(args, "remove")?,
                    flagged: optional_bool(args, "flagged")?,
                    seen: optional_bool(args, "seen")?,
                };
                if let Request::Label {
                    add,
                    remove,
                    flagged: None,
                    seen: None,
                    ..
                } = &request
                {
                    if add.is_empty() && remove.is_empty() {
                        return Err(invalid(
                            "mail.label needs at least one of 'add', 'remove', 'flagged' or 'seen'.",
                        ));
                    }
                }
                Ok(request)
            }
            "mail.draft" => Ok(Request::Draft {
                message: Outbound::parse(args, true)?,
                thread_id: optional_string(args, "thread_id")?,
            }),
            "mail.send" => Ok(Request::Send {
                message: Outbound::parse(args, false)?,
            }),
            "mail.reply" => Ok(Request::Reply {
                thread_id: thread_id(args)?,
                message: Outbound::parse(args, false)?,
            }),
            "mail.delete" => Ok(Request::Move {
                thread_id: thread_id(args)?,
                to: MoveTo::Trash,
            }),
            "mail.archive" => Ok(Request::Move {
                thread_id: thread_id(args)?,
                to: MoveTo::Archive,
            }),
            "mail.attachment" => Ok(Request::Attachment {
                folder: FolderKind::parse(args)?,
                uid: match args.get("uid").and_then(Value::as_u64) {
                    Some(uid) if (1..=u64::from(u32::MAX)).contains(&uid) => uid as u32,
                    _ => {
                        return Err(invalid(
                            "'uid' must be the uid of a message mail.read answered.",
                        ))
                    }
                },
                part: match args.get("part").and_then(Value::as_str) {
                    Some(part) if is_part_number(part) => part.to_string(),
                    _ => {
                        return Err(invalid(
                            "'part' must be an attachment's part number as mail.read answered it, such as 2 or 1.2.",
                        ))
                    }
                },
            }),
            other => Err(invalid(format!("'{other}' is not a mail tool."))),
        }
    }
}

// ---------------------------------------------------------------------------
// Running a request
// ---------------------------------------------------------------------------

async fn run(
    connector: &dyn MailConnector,
    mailbox: &ToolMailbox,
    request: &Request,
) -> Result<Value, SealSessionError> {
    match request {
        Request::Draft { message, thread_id } => {
            return draft(connector, mailbox, message, thread_id.as_deref()).await
        }
        Request::Send { message } => return send(connector, mailbox, message, None).await,
        Request::Reply { message, thread_id } => {
            return send(connector, mailbox, message, Some(thread_id.as_str())).await
        }
        Request::Move { thread_id, to } => {
            return move_thread(connector, mailbox, thread_id, *to).await
        }
        Request::Attachment { .. } => {
            unreachable!("an attachment is saved by MailTools::invoke, with its file services")
        }
        Request::List { .. } | Request::Read { .. } | Request::Label { .. } => {}
    }
    let mut session = ImapSession::open(connector, &mailbox.settings, &mailbox.auth)
        .await
        .map_err(session_error)?;
    let answer = match request {
        Request::List { .. } => list(&mut session, mailbox, request).await,
        Request::Read { thread_id, folder } => {
            read(&mut session, mailbox, thread_id, *folder).await
        }
        Request::Label {
            thread_id,
            add,
            remove,
            flagged,
            seen,
        } => {
            let marks = Marks {
                add,
                remove,
                flagged: *flagged,
                seen: *seen,
            };
            label(&mut session, mailbox, thread_id, &marks).await
        }
        Request::Draft { .. }
        | Request::Send { .. }
        | Request::Reply { .. }
        | Request::Move { .. }
        | Request::Attachment { .. } => {
            unreachable!("an outbound or move request is run before the session opens")
        }
    };
    session.logout().await;
    answer
}

// ---------------------------------------------------------------------------
// The outbound tools (the Update of 2026-10-07 (3), clauses 13 to 15)
// ---------------------------------------------------------------------------

/// How a reply threads: the parent's `Message-ID` and the `References` it
/// carries on.
#[derive(Debug, Clone, Default)]
struct Threading {
    in_reply_to: Option<String>,
    references: Vec<String>,
}

/// The threading of a reply to `thread_id`'s newest `INBOX` message:
/// `In-Reply-To` its `Message-ID`, `References` its `References` (or its
/// `In-Reply-To`) followed by its `Message-ID`.
async fn threading_of(
    session: &mut ImapSession,
    thread_id: &str,
) -> Result<Threading, SealSessionError> {
    let status = session.examine(FOLDER).await.map_err(session_error)?;
    let messages = thread_messages(
        session,
        &status,
        thread_id,
        "UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID IN-REPLY-TO REFERENCES)]",
        FolderKind::Inbox,
    )
    .await?;
    let newest = messages.last().expect("a thread has a message");
    let headers = parse_headers(newest.header.as_deref().unwrap_or_default());
    let parent = headers.ids("Message-ID").into_iter().next();
    let mut references = headers.ids("References");
    if references.is_empty() {
        references = headers.ids("In-Reply-To");
    }
    if let Some(parent) = &parent {
        if !references.contains(parent) {
            references.push(parent.clone());
        }
    }
    Ok(Threading {
        in_reply_to: parent,
        references,
    })
}

/// The folder `LIST` marks with `attribute` (RFC 6154), else the first of
/// `names`, in their order, that names a folder (case-insensitive) at the
/// top level or directly under `INBOX`.
fn special_folder(folders: &[ListedFolder], attribute: &str, names: &[&str]) -> Option<String> {
    if let Some(marked) = folders.iter().find(|f| f.has_attribute(attribute)) {
        return Some(marked.name.clone());
    }
    names.iter().find_map(|name| {
        folders
            .iter()
            .find(|f| {
                f.name.eq_ignore_ascii_case(name)
                    || f.delimiter
                        .as_deref()
                        .is_some_and(|d| f.name.eq_ignore_ascii_case(&format!("{FOLDER}{d}{name}")))
            })
            .map(|f| f.name.clone())
    })
}

/// The folder `LIST` marks with `attribute`, by that attribute alone.
fn marked_folder(folders: &[ListedFolder], attribute: &str) -> Option<String> {
    folders
        .iter()
        .find(|f| f.has_attribute(attribute))
        .map(|f| f.name.clone())
}

/// The folder of `kind` among `folders`, by one rule for every tool (the
/// Update of 2026-10-07 (3) clause 15, the Update of 2026-10-08 (4) clause
/// 18 and the Update of 2026-10-08 (5) clauses 22 and 28): Sent, Drafts and
/// Trash by their attribute, else by name (Trash, else Deleted); Archive by
/// `\Archive`, else by the name Archive, else the `\All` folder; all mail
/// by `\All` only. Never by a provider's folder name.
fn locate(folders: &[ListedFolder], kind: FolderKind) -> Option<String> {
    match kind {
        FolderKind::Inbox => Some(FOLDER.to_string()),
        FolderKind::Sent => special_folder(folders, "\\Sent", &["Sent"]),
        FolderKind::Drafts => special_folder(folders, "\\Drafts", &["Drafts"]),
        FolderKind::Trash => special_folder(folders, "\\Trash", &["Trash", "Deleted"]),
        FolderKind::Archive => special_folder(folders, "\\Archive", &["Archive"])
            .or_else(|| marked_folder(folders, "\\All")),
        FolderKind::All => marked_folder(folders, "\\All"),
    }
}

/// Open the folder of `kind` read-only by `EXAMINE`: the inbox directly,
/// another folder after `LIST` locates it; a mailbox without it is refused
/// with nothing opened. Answers the folder's name as `LIST` wrote it and
/// its status.
async fn examine_kind(
    session: &mut ImapSession,
    kind: FolderKind,
) -> Result<(String, FolderStatus), SealSessionError> {
    let name = match kind {
        FolderKind::Inbox => FOLDER.to_string(),
        other => {
            let folders = session.list_folders().await.map_err(session_error)?;
            locate(&folders, other).ok_or_else(|| {
                invalid(format!(
                    "This mailbox has no {} folder; nothing was read.",
                    other.title()
                ))
            })?
        }
    };
    let status = session.examine(&name).await.map_err(session_error)?;
    Ok((name, status))
}

/// The message `given` from `mailbox`, with `message_id` and `threading`.
fn compose(
    mailbox: &ToolMailbox,
    given: &Outbound,
    message_id: &str,
    threading: &Threading,
) -> OutgoingMessage {
    OutgoingMessage {
        from_address: mailbox.settings.address.clone(),
        from_name: mailbox.settings.display_name.clone(),
        to: given.to.clone(),
        cc: given.cc.clone(),
        subject: given.subject.clone(),
        body: given.body.clone(),
        message_id: message_id.to_string(),
        in_reply_to: threading.in_reply_to.clone(),
        references: threading.references.clone(),
        date: chrono::Utc::now(),
    }
}

/// `mail.draft`: the message appended to the Drafts folder with `\Draft
/// \Seen`; refused when the mailbox has no Drafts folder.
async fn draft(
    connector: &dyn MailConnector,
    mailbox: &ToolMailbox,
    given: &Outbound,
    thread_id: Option<&str>,
) -> Result<Value, SealSessionError> {
    let mut session = ImapSession::open(connector, &mailbox.settings, &mailbox.auth)
        .await
        .map_err(session_error)?;
    let answer = async {
        let threading = match thread_id {
            Some(thread_id) => threading_of(&mut session, thread_id).await?,
            None => Threading::default(),
        };
        let folders = session.list_folders().await.map_err(session_error)?;
        let Some(drafts) = locate(&folders, FolderKind::Drafts) else {
            return Err(invalid(NO_DRAFTS));
        };
        let message_id = mint_message_id(&mailbox.settings.address);
        let raw = compose(mailbox, given, &message_id, &threading).render();
        session
            .append(&drafts, &["\\Draft", "\\Seen"], &raw)
            .await
            .map_err(session_error)?;
        Ok(json!({
            "mailbox": mailbox.binding_id.0.to_string(),
            "folder": drafts,
            "message_id": message_id,
            "to": given.to,
            "cc": given.cc,
            "subject": given.subject,
            "thread_id": thread_id,
            "in_reply_to": threading.in_reply_to,
            "saved": true,
        }))
    }
    .await;
    session.logout().await;
    answer
}

/// Where a sent message's copy went.
enum SentCopy {
    /// In `folder`: appended, or already filed there by the server.
    Saved { folder: String },
    /// The mailbox has no Sent folder.
    NoFolder,
    /// Saving failed with the server's reply.
    Failed { reply: String },
}

/// `mail.send` and `mail.reply`: the message submitted over SMTP, then its
/// copy saved to the Sent folder. A failure to save after the send is in
/// the result, never an error.
async fn send(
    connector: &dyn MailConnector,
    mailbox: &ToolMailbox,
    given: &Outbound,
    thread_id: Option<&str>,
) -> Result<Value, SealSessionError> {
    let threading = match thread_id {
        Some(thread_id) => {
            let mut session = ImapSession::open(connector, &mailbox.settings, &mailbox.auth)
                .await
                .map_err(session_error)?;
            let threading = threading_of(&mut session, thread_id).await;
            session.logout().await;
            threading?
        }
        None => Threading::default(),
    };
    let message_id = mint_message_id(&mailbox.settings.address);
    let raw = compose(mailbox, given, &message_id, &threading).render();
    let recipients: Vec<String> = given.to.iter().chain(given.cc.iter()).cloned().collect();
    submission::submit(
        connector,
        &mailbox.settings,
        &mailbox.auth,
        &mailbox.settings.address,
        &recipients,
        &raw,
    )
    .await
    .map_err(session_error)?;
    let (saved, folder, reply) = match save_to_sent(connector, mailbox, &message_id, &raw).await {
        SentCopy::Saved { folder } => (true, Some(folder), None),
        SentCopy::NoFolder => (false, None, Some(NO_SENT.to_string())),
        SentCopy::Failed { reply } => (false, None, Some(reply)),
    };
    let mut answer = json!({
        "mailbox": mailbox.binding_id.0.to_string(),
        "message_id": message_id,
        "to": given.to,
        "cc": given.cc,
        "subject": given.subject,
        "saved_to_sent": saved,
        "sent_folder": folder,
        "sent_folder_reply": reply,
    });
    if let Some(thread_id) = thread_id {
        answer["thread_id"] = json!(thread_id);
        answer["in_reply_to"] = json!(threading.in_reply_to);
    }
    Ok(answer)
}

/// Save a sent message's copy: find the Sent folder, search it for the
/// message's `Message-ID` and append the message with `\Seen` only when it
/// is not there (a server that files its own copy is matched by what it
/// does, never by its name).
async fn save_to_sent(
    connector: &dyn MailConnector,
    mailbox: &ToolMailbox,
    message_id: &str,
    raw: &[u8],
) -> SentCopy {
    let mut session = match ImapSession::open(connector, &mailbox.settings, &mailbox.auth).await {
        Ok(session) => session,
        Err(failure) => {
            return SentCopy::Failed {
                reply: failure.reply,
            }
        }
    };
    let copy: Result<SentCopy, MailboxCheckFailure> = async {
        let folders = session.list_folders().await?;
        let Some(sent) = locate(&folders, FolderKind::Sent) else {
            return Ok(SentCopy::NoFolder);
        };
        session.examine(&sent).await?;
        let filed = session
            .uid_search(&[
                Arg::atom("HEADER"),
                Arg::atom("Message-ID"),
                Arg::string(message_id.to_string()),
            ])
            .await?;
        if filed.is_empty() {
            session.append(&sent, &["\\Seen"], raw).await?;
        }
        Ok(SentCopy::Saved { folder: sent })
    }
    .await;
    session.logout().await;
    copy.unwrap_or_else(|failure| SentCopy::Failed {
        reply: failure.reply,
    })
}

// ---------------------------------------------------------------------------
// Deleting and archiving (the Update of 2026-10-08 (4), clauses 17 to 20,
// and the Update of 2026-10-08 (5), clauses 27 to 29)
// ---------------------------------------------------------------------------

/// How a thread reaches Trash or the Archive folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveHow {
    /// `UID MOVE` (RFC 6851).
    Move,
    /// `UID COPY`, `UID STORE +FLAGS (\Deleted)`, `UID EXPUNGE` (RFC 4315).
    CopyThenExpunge,
}

/// The folder `to` names and how to reach it, or the refusal: the server's
/// `CAPABILITY` after authentication (`IMAP4rev2` holds both `MOVE` and
/// `UID EXPUNGE`) and the folders `LIST` answers.
async fn move_plan(
    session: &mut ImapSession,
    to: MoveTo,
) -> Result<(String, MoveHow), SealSessionError> {
    let capabilities = session.capabilities().await.map_err(session_error)?;
    let has = |name: &str| capabilities.iter().any(|c| c == name || c == "IMAP4REV2");
    let how = if has("MOVE") {
        MoveHow::Move
    } else if has("UIDPLUS") {
        MoveHow::CopyThenExpunge
    } else {
        return Err(invalid(to.no_safe_move()));
    };
    let folders = session.list_folders().await.map_err(session_error)?;
    let folder = locate(&folders, to.kind()).ok_or_else(|| invalid(to.no_folder()))?;
    Ok((folder, how))
}

/// Text a person reads, control characters removed.
fn shown_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// The admission's read for `mail.delete` and `mail.archive` before the
/// gate: the plan's refusals, the thread's presence in `INBOX`, and what the
/// person reads: `subject`, the oldest message's subject, and `from`, the
/// distinct senders, oldest first. Read-only: `EXAMINE`, headers by
/// `BODY.PEEK`.
async fn shown_for_move(
    connector: &dyn MailConnector,
    mailbox: &ToolMailbox,
    thread_id: &str,
    to: MoveTo,
) -> Result<Vec<(&'static str, Value)>, SealSessionError> {
    let mut session = ImapSession::open(connector, &mailbox.settings, &mailbox.auth)
        .await
        .map_err(session_error)?;
    let shown = async {
        move_plan(&mut session, to).await?;
        let status = session.examine(FOLDER).await.map_err(session_error)?;
        let messages = thread_messages(
            &mut session,
            &status,
            thread_id,
            "UID BODY.PEEK[HEADER.FIELDS (FROM SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES)]",
            FolderKind::Inbox,
        )
        .await?;
        let mut subject = String::new();
        let mut from: Vec<String> = Vec::new();
        for message in &messages {
            let headers = parse_headers(message.header.as_deref().unwrap_or_default());
            if subject.is_empty() {
                subject = shown_text(&headers.get("Subject").unwrap_or_default());
            }
            if let Some(sender) = headers.get("From").map(|f| shown_text(&f)) {
                if !from.contains(&sender) {
                    from.push(sender);
                }
            }
        }
        Ok(vec![("subject", json!(subject)), ("from", json!(from))])
    }
    .await;
    session.logout().await;
    shown
}

/// `mail.delete` and `mail.archive`: the thread's `INBOX` messages at the
/// run moved to Trash or the Archive folder, by `UID MOVE`, else by `UID
/// COPY`, `UID STORE +FLAGS (\Deleted)` and `UID EXPUNGE` of exactly those
/// UIDs. The folder moved to is never selected and a plain `EXPUNGE` is
/// never sent.
async fn move_thread(
    connector: &dyn MailConnector,
    mailbox: &ToolMailbox,
    thread_id: &str,
    to: MoveTo,
) -> Result<Value, SealSessionError> {
    let mut session = ImapSession::open(connector, &mailbox.settings, &mailbox.auth)
        .await
        .map_err(session_error)?;
    let answer = async {
        let (folder, how) = move_plan(&mut session, to).await?;
        let status = session.select(FOLDER).await.map_err(session_error)?;
        let messages = thread_messages(
            &mut session,
            &status,
            thread_id,
            "UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID IN-REPLY-TO REFERENCES)]",
            FolderKind::Inbox,
        )
        .await?;
        let uids: Vec<u32> = messages.iter().map(|f| f.uid).collect();
        match how {
            MoveHow::Move => session
                .uid_move(&uids, &folder)
                .await
                .map_err(session_error)?,
            MoveHow::CopyThenExpunge => {
                session
                    .uid_copy(&uids, &folder)
                    .await
                    .map_err(session_error)?;
                session
                    .uid_store(&uids, StoreOp::Add, &["\\Deleted".to_string()])
                    .await
                    .map_err(session_error)?;
                session.uid_expunge(&uids).await.map_err(session_error)?;
            }
        }
        let mut answer = json!({
            "mailbox": mailbox.binding_id.0.to_string(),
            "folder": FOLDER,
            "folder_kind": FolderKind::Inbox.as_str(),
            "thread_id": thread_id,
            "moved": uids.len(),
            "message_uids": uids,
        });
        answer[to.result_key()] = json!(folder);
        Ok(answer)
    }
    .await;
    session.logout().await;
    answer
}

/// The root `Message-ID` of a message: its thread's id.
fn thread_root(headers: &Headers, status: &FolderStatus, uid: u32) -> String {
    headers
        .ids("References")
        .into_iter()
        .next()
        .or_else(|| headers.ids("In-Reply-To").into_iter().next())
        .or_else(|| headers.ids("Message-ID").into_iter().next())
        .unwrap_or_else(|| format!("uid:{}:{uid}", status.uidvalidity.unwrap_or(0)))
}

fn keywords_of(flags: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = flags
        .iter()
        .filter(|f| !f.starts_with('\\'))
        .cloned()
        .collect();
    set.into_iter().collect()
}

fn has_flag(flags: &[String], flag: &str) -> bool {
    flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
}

/// A message's date as RFC 3339 in UTC: its `Date` header, else its
/// `INTERNALDATE`.
fn date_of(headers: &Headers, fetched: &Fetched) -> Option<String> {
    let from_header = headers
        .raw("Date")
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(d.trim()).ok());
    let from_internal = || {
        fetched
            .internal_date
            .as_deref()
            .and_then(|d| chrono::DateTime::parse_from_str(d.trim(), "%d-%b-%Y %H:%M:%S %z").ok())
    };
    from_header.or_else(from_internal).map(|d| {
        d.with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

async fn list(
    session: &mut ImapSession,
    mailbox: &ToolMailbox,
    request: &Request,
) -> Result<Value, SealSessionError> {
    let Request::List {
        query,
        from,
        unread_only,
        flagged_only,
        since,
        limit,
        folder,
    } = request
    else {
        unreachable!("list is called with a list request")
    };
    let (name, status) = examine_kind(session, *folder).await?;
    let mut criteria = Vec::new();
    if let Some(query) = query {
        criteria.push(Arg::atom("TEXT"));
        criteria.push(Arg::string(query.clone()));
    }
    if let Some(from) = from {
        criteria.push(Arg::atom("FROM"));
        criteria.push(Arg::string(from.clone()));
    }
    if *unread_only {
        criteria.push(Arg::atom("UNSEEN"));
    }
    if *flagged_only {
        criteria.push(Arg::atom("FLAGGED"));
    }
    if let Some(since) = since {
        criteria.push(Arg::atom("SINCE"));
        criteria.push(Arg::atom(since.format("%-d-%b-%Y").to_string()));
    }
    if criteria.is_empty() {
        criteria.push(Arg::atom("ALL"));
    }
    let matched = session.uid_search(&criteria).await.map_err(session_error)?;
    let window: Vec<u32> = matched
        .iter()
        .rev()
        .take(LIST_WINDOW)
        .rev()
        .copied()
        .collect();
    let fetched = session
        .uid_fetch(&window, &format!("UID FLAGS INTERNALDATE {LIST_FIELDS}"))
        .await
        .map_err(session_error)?;

    // Group by root, messages in ascending UID order.
    let mut order: Vec<String> = Vec::new();
    let mut threads: HashMap<String, Vec<(Fetched, Headers)>> = HashMap::new();
    for message in fetched {
        let headers = parse_headers(message.header.as_deref().unwrap_or_default());
        let root = thread_root(&headers, &status, message.uid);
        if !threads.contains_key(&root) {
            order.push(root.clone());
        }
        threads.entry(root).or_default().push((message, headers));
    }
    let mut summaries: Vec<(u32, Value)> = order
        .into_iter()
        .map(|root| {
            let messages = &threads[&root];
            let (latest, latest_headers) = messages
                .iter()
                .max_by_key(|(f, _)| f.uid)
                .expect("a thread has a message");
            let mut participants: Vec<String> = Vec::new();
            let mut keywords: BTreeSet<String> = BTreeSet::new();
            for (f, h) in messages {
                if let Some(from) = h.get("From") {
                    if !participants.contains(&from) {
                        participants.push(from);
                    }
                }
                keywords.extend(keywords_of(&f.flags));
            }
            let unread = messages
                .iter()
                .filter(|(f, _)| !has_flag(&f.flags, "\\Seen"))
                .count();
            let flagged = messages
                .iter()
                .any(|(f, _)| has_flag(&f.flags, "\\Flagged"));
            (
                latest.uid,
                json!({
                    "thread_id": root,
                    "subject": latest_headers.get("Subject").unwrap_or_default(),
                    "participants": participants,
                    "latest_from": latest_headers.get("From"),
                    "latest_date": date_of(latest_headers, latest),
                    "message_count": messages.len(),
                    "unread_count": unread,
                    "flagged": flagged,
                    "keywords": keywords.into_iter().collect::<Vec<_>>(),
                }),
            )
        })
        .collect();
    summaries.sort_by_key(|s| std::cmp::Reverse(s.0));
    let truncated = matched.len() > window.len() || summaries.len() > *limit;
    let threads: Vec<Value> = summaries.into_iter().take(*limit).map(|(_, v)| v).collect();
    Ok(json!({
        "mailbox": mailbox.binding_id.0.to_string(),
        "folder": name,
        "folder_kind": folder.as_str(),
        "matched_messages": matched.len(),
        "truncated": truncated,
        "threads": threads,
    }))
}

/// The header fields that place a message in its thread.
const THREAD_FIELDS: &str = "BODY.PEEK[HEADER.FIELDS (MESSAGE-ID IN-REPLY-TO REFERENCES)]";

/// The UIDs of the messages whose thread is `thread_id` in the open folder
/// (of `kind`), ascending, and the fetch of each that `items` asked for. A
/// `uid:` id names a message only in the folder whose `UIDVALIDITY` it
/// carries; a thread with no message here is refused with the folder named.
///
/// The candidates are the union of a `HEADER` search for the id and the
/// folder's [`LIST_WINDOW`] newest messages, the window `mail.list` groups
/// (AEGIS ADR-125's Update of 2026-10-08 (5), clause 39): a server whose
/// `HEADER References` search does not match (Gmail's) still yields every
/// message `mail.list` counted in the thread, a reply linked to it by
/// `References` alone among them. The union is one `UID SEARCH`, the
/// window a sequence set; the candidates' threading headers are fetched,
/// and only the messages whose [`thread_root`] is the id are fetched with
/// `items`.
async fn thread_messages(
    session: &mut ImapSession,
    status: &FolderStatus,
    thread_id: &str,
    items: &str,
    kind: FolderKind,
) -> Result<Vec<Fetched>, SealSessionError> {
    let candidates = if let Some(rest) = thread_id.strip_prefix("uid:") {
        let mut parts = rest.splitn(2, ':');
        let validity: Option<u32> = parts.next().and_then(|v| v.parse().ok());
        let uid: Option<u32> = parts.next().and_then(|u| u.parse().ok());
        match (validity, uid) {
            (Some(v), Some(uid)) if Some(v) == status.uidvalidity.or(Some(0)) => session
                .uid_search(&[Arg::atom("UID"), Arg::atom(uid.to_string())])
                .await
                .map_err(session_error)?,
            _ => Vec::new(),
        }
    } else {
        let id = Arg::string(thread_id.to_string());
        let mut criteria = Vec::new();
        if status.exists > 0 {
            let first = status.exists.saturating_sub(LIST_WINDOW as u32 - 1).max(1);
            criteria.push(Arg::atom("OR"));
            criteria.push(Arg::atom(format!("{first}:{}", status.exists)));
        }
        criteria.extend([
            Arg::atom("OR"),
            Arg::atom("OR"),
            Arg::atom("HEADER"),
            Arg::atom("Message-ID"),
            id.clone(),
            Arg::atom("HEADER"),
            Arg::atom("References"),
            id.clone(),
            Arg::atom("HEADER"),
            Arg::atom("In-Reply-To"),
            id,
        ]);
        let found = session.uid_search(&criteria).await.map_err(session_error)?;
        session
            .uid_fetch(&found, &format!("UID {THREAD_FIELDS}"))
            .await
            .map_err(session_error)?
            .into_iter()
            .filter(|f| in_thread_of(f, status, thread_id))
            .map(|f| f.uid)
            .collect()
    };
    let in_thread: Vec<Fetched> = session
        .uid_fetch(&candidates, items)
        .await
        .map_err(session_error)?
        .into_iter()
        .filter(|f| in_thread_of(f, status, thread_id))
        .collect();
    if in_thread.is_empty() {
        let shown: String = thread_id
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        let message = format!(
            "There is no thread '{shown}' in this mailbox's {}.",
            kind.place()
        );
        return Err(
            SealSessionError::NotFound(message.clone()).answered(CallerAnswer::NotFound(message))
        );
    }
    Ok(in_thread)
}

/// Whether the fetched message `f` belongs to thread `thread_id`.
fn in_thread_of(f: &Fetched, status: &FolderStatus, thread_id: &str) -> bool {
    let raw = f
        .header
        .as_deref()
        .or(f.full.as_deref())
        .unwrap_or_default();
    thread_root(&parse_headers(raw), status, f.uid) == thread_id
}

async fn read(
    session: &mut ImapSession,
    mailbox: &ToolMailbox,
    thread_id: &str,
    folder: FolderKind,
) -> Result<Value, SealSessionError> {
    let (name, status) = examine_kind(session, folder).await?;
    let messages = thread_messages(
        session,
        &status,
        thread_id,
        "UID FLAGS INTERNALDATE BODY.PEEK[]",
        folder,
    )
    .await?;
    let truncated = messages.len() > READ_MAX_MESSAGES;
    let skip = messages.len().saturating_sub(READ_MAX_MESSAGES);
    let mut subject = String::new();
    let answered: Vec<Value> = messages
        .iter()
        .skip(skip)
        .map(|f| {
            let message = parse_message(f.full.as_deref().unwrap_or_default());
            let h = &message.headers;
            if subject.is_empty() {
                subject = h.get("Subject").unwrap_or_default();
            }
            let body_truncated = message.text.chars().count() > BODY_MAX_CHARS;
            let body: String = message.text.chars().take(BODY_MAX_CHARS).collect();
            json!({
                "uid": f.uid,
                "message_id": h.ids("Message-ID").into_iter().next(),
                "date": date_of(h, f),
                "from": h.get("From"),
                "to": h.addresses("To"),
                "cc": h.addresses("Cc"),
                "subject": h.get("Subject").unwrap_or_default(),
                "in_reply_to": h.ids("In-Reply-To").into_iter().next(),
                "seen": has_flag(&f.flags, "\\Seen"),
                "flagged": has_flag(&f.flags, "\\Flagged"),
                "keywords": keywords_of(&f.flags),
                "body_text": body,
                "body_truncated": body_truncated,
                "attachments": message.attachments.iter().map(|a| json!({
                    "part": a.part,
                    "filename": a.filename,
                    "content_type": a.content_type,
                    "size": a.size,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "mailbox": mailbox.binding_id.0.to_string(),
        "folder": name,
        "folder_kind": folder.as_str(),
        "thread_id": thread_id,
        "subject": subject,
        "truncated": truncated,
        "messages": answered,
    }))
}

// ---------------------------------------------------------------------------
// Attachments (the Update of 2026-10-08 (5), clause 31)
// ---------------------------------------------------------------------------

/// A part number as RFC 9051 writes one: numbers from 1, joined by dots.
fn is_part_number(part: &str) -> bool {
    part.len() <= 64
        && part
            .split('.')
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && !n.starts_with('0'))
}

/// The refusal for a part no attachment of the message carries.
fn no_attachment(part: &str, uid: u32) -> SealSessionError {
    let message = format!("There is no attachment '{part}' on message {uid} in this folder.");
    SealSessionError::NotFound(message.clone()).answered(CallerAnswer::NotFound(message))
}

/// A saved attachment's name: its part's file name with path separators
/// and control characters removed, `attachment` when nothing is left.
pub fn attachment_name(filename: Option<&str>) -> String {
    let cleaned: String = filename
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .take(200)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        ATTACHMENT_DEFAULT_NAME.to_string()
    } else {
        cleaned.to_string()
    }
}

/// Fetch message `uid` of the folder of `folder` read-only, decode its
/// attachment `part`, and write it to the acting person's `chat-attachments`
/// volume. The size is refused before the volume is found or anything is
/// written.
async fn save_attachment(
    connector: &dyn MailConnector,
    files: &MailFiles,
    mailbox: &ToolMailbox,
    acting: &MailActing,
    folder: FolderKind,
    uid: u32,
    part: &str,
) -> Result<Value, SealSessionError> {
    let Some(user_id) = acting.user_id.as_deref() else {
        return Err(binding_required(NO_PERSON.to_string()));
    };
    let mut session = ImapSession::open(connector, &mailbox.settings, &mailbox.auth)
        .await
        .map_err(session_error)?;
    session.read_literals_up_to(ATTACHMENT_MESSAGE_MAX_BYTES);
    let fetched = async {
        examine_kind(&mut session, folder).await?;
        session
            .uid_fetch(&[uid], "UID BODY.PEEK[]")
            .await
            .map_err(session_error)
    }
    .await;
    session.logout().await;
    let raw = fetched?
        .into_iter()
        .find(|f| f.uid == uid)
        .and_then(|f| f.full)
        .ok_or_else(|| no_attachment(part, uid))?;
    let found = attachment_part(&raw, part).ok_or_else(|| no_attachment(part, uid))?;

    let tier = acting.tier.clone().unwrap_or(ZaruTier::Free);
    let tier_max = crate::domain::volume::StorageTierLimits::default()
        .limits
        .get(&tier)
        .map(|l| l.max_file_size_bytes)
        .unwrap_or(0);
    let size = found.bytes.len();
    if size > ATTACHMENT_MAX_BYTES || size as u64 > tier_max {
        return Err(invalid(ATTACHMENT_TOO_LARGE));
    }

    let volume_id = files
        .user_volumes
        .find_or_provision_chat_attachments(&acting.tenant_id, user_id, &tier)
        .await
        .map_err(|e| {
            SealSessionError::InternalError(format!("the attachments volume failed: {e}"))
                .answered(CallerAnswer::Internal(InternalFailure::Server))
        })?;
    let name = attachment_name(found.attachment.filename.as_deref());
    let path = format!(
        "mail/{}/{}/{}",
        chrono::Utc::now().format("%Y-%m-%d"),
        uuid::Uuid::new_v4(),
        name
    );
    files
        .file_operations
        .write_file_for_tier(
            &volume_id,
            &acting.tenant_id,
            user_id,
            &path,
            &found.bytes,
            &tier,
        )
        .await
        .map_err(|e| match e {
            FileOperationsError::FileTooLarge => invalid(ATTACHMENT_TOO_LARGE),
            other => {
                SealSessionError::InternalError(format!("saving the attachment failed: {other}"))
                    .answered(CallerAnswer::Internal(InternalFailure::Server))
            }
        })?;

    let mime_type = infer::get(&found.bytes)
        .map(|k| k.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let sha256 = format!("{:x}", sha2::Sha256::digest(&found.bytes));
    let mut answer = json!({
        "volume_id": volume_id.to_string(),
        "path": path,
        "name": name,
        "mime_type": mime_type,
        "size": size,
        "sha256": sha256,
    });
    if found.attachment.content_type.starts_with("text/") {
        let text = decode_text(&found.bytes, found.charset.as_deref());
        if text.chars().count() <= ATTACHMENT_TEXT_MAX_CHARS {
            answer["text"] = Value::String(text);
        }
    }
    Ok(answer)
}

/// What `mail.label` changes on every message of a thread: keywords added
/// and removed, and `\Flagged` and `\Seen` set or cleared.
struct Marks<'a> {
    add: &'a [String],
    remove: &'a [String],
    flagged: Option<bool>,
    seen: Option<bool>,
}

async fn label(
    session: &mut ImapSession,
    mailbox: &ToolMailbox,
    thread_id: &str,
    marks: &Marks<'_>,
) -> Result<Value, SealSessionError> {
    let Marks {
        add,
        remove,
        flagged,
        seen,
    } = *marks;
    let status = session.select(FOLDER).await.map_err(session_error)?;
    // A label the server will not keep is refused before anything is stored.
    if let Some(unkept) = add.iter().find(|k| !status.keeps_keyword(k)) {
        return Err(invalid(format!(
            "This mailbox's server does not keep the label '{unkept}'; nothing was changed. Flagging still works."
        )));
    }
    let messages = thread_messages(
        session,
        &status,
        thread_id,
        "UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID IN-REPLY-TO REFERENCES)]",
        FolderKind::Inbox,
    )
    .await?;
    let uids: Vec<u32> = messages.iter().map(|f| f.uid).collect();
    session
        .uid_store(&uids, StoreOp::Add, add)
        .await
        .map_err(session_error)?;
    session
        .uid_store(&uids, StoreOp::Remove, remove)
        .await
        .map_err(session_error)?;
    for (flag, set) in [("\\Flagged", flagged), ("\\Seen", seen)] {
        if let Some(set) = set {
            let op = if set { StoreOp::Add } else { StoreOp::Remove };
            session
                .uid_store(&uids, op, &[flag.to_string()])
                .await
                .map_err(session_error)?;
        }
    }
    Ok(json!({
        "mailbox": mailbox.binding_id.0.to_string(),
        "folder": FOLDER,
        "folder_kind": FolderKind::Inbox.as_str(),
        "thread_id": thread_id,
        "message_uids": uids,
        "added": add,
        "removed": remove,
        "flagged": flagged,
        "seen": seen,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyword_is_an_atom_and_never_a_system_flag() {
        assert!(is_keyword("zaru/triaged"));
        assert!(is_keyword("$Label1"));
        assert!(!is_keyword("\\Seen"));
        assert!(!is_keyword("two words"));
        assert!(!is_keyword("a(b"));
        assert!(!is_keyword(""));
        assert!(!is_keyword(&"x".repeat(65)));
    }

    #[test]
    fn a_label_call_with_nothing_to_change_is_refused() {
        let error = Request::parse("mail.label", &json!({"thread_id": "<a@x>"})).unwrap_err();
        assert!(error.to_string().contains("at least one of"), "{error}");
    }
}
