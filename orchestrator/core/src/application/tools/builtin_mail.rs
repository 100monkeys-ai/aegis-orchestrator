// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The mail read tools: `mail.list`, `mail.read` and `mail.label` (AEGIS
//! ADR-125 D4; its Update of 2026-10-07 clauses 1, 2 and 7).
//!
//! They speak IMAP only, over a mailbox of the acting person (ADR-125's
//! Update of 2026-10-04 clause 3), inside the orchestrator: an `imap`
//! `mailbox` binding logs in with its password, and an OAuth binding
//! granted Google's mail scope authenticates with XOAUTH2 and the token
//! `access_token_for` answers (its Update of 2026-10-07 clauses 8 and 10);
//! neither secret reaches an agent. They work on `INBOX` only and by UID;
//! bodies are read with `BODY.PEEK`, so no tool marks a message read.
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
//! **Threads.** A thread's id is the root `Message-ID` of its messages (the
//! first `References` entry, else `In-Reply-To`, else the message's own); a
//! message with no `Message-ID` is its own thread `uid:<UIDVALIDITY>:<uid>`.
//! A thread's messages are found by `UID SEARCH` over `Message-ID`,
//! `References` and `In-Reply-To`: plain IMAP, no server extension.

use crate::application::credential_service::{ToolCallActor, ToolMailbox, ToolMailboxSource};
use crate::domain::agent::AgentId;
use crate::domain::credential::{CredentialBindingId, CredentialProvider};
use crate::domain::execution::{ContextChoice, ServerChoice};
use crate::domain::seal_session::{CallerAnswer, InternalFailure, SealSessionError};
use crate::domain::tenant::TenantId;
use crate::infrastructure::mail::session::{
    parse_headers, parse_message, Arg, Fetched, FolderStatus, Headers, ImapSession, StoreOp,
};
use crate::infrastructure::mail::{
    CheckFailureKind, MailConnector, MailboxCheckFailure, RustlsMailConnector,
};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

/// The folder every mail tool works on.
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

/// Whether `tool_name` is one of the mail tools this module serves.
pub fn is_mail_tool(tool_name: &str) -> bool {
    matches!(tool_name, "mail.list" | "mail.read" | "mail.label")
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
}

impl MailActing {
    /// The choice key a mailbox is chosen under: the provider `imap`.
    pub fn choice_key() -> &'static str {
        CredentialProvider::IMAP
    }
}

/// The mail tools: the mailbox source and the connector their sessions
/// open over.
pub struct MailTools {
    mailboxes: Arc<dyn ToolMailboxSource>,
    connector: Arc<dyn MailConnector>,
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
        }
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
        let run = run(self.connector.as_ref(), &mailbox, &request);
        match tokio::time::timeout(CALL_TIMEOUT, run).await {
            Ok(result) => result,
            Err(_) => Err(SealSessionError::UpstreamUnavailable(format!(
                "The mail server did not answer within {} seconds.",
                CALL_TIMEOUT.as_secs()
            ))),
        }
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
    },
    Read {
        thread_id: String,
    },
    Label {
        thread_id: String,
        add: Vec<String>,
        remove: Vec<String>,
        flagged: Option<bool>,
    },
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
        match tool_name {
            "mail.list" => {
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
                })
            }
            "mail.read" => Ok(Request::Read {
                thread_id: thread_id(args)?,
            }),
            "mail.label" => {
                let request = Request::Label {
                    thread_id: thread_id(args)?,
                    add: keywords(args, "add")?,
                    remove: keywords(args, "remove")?,
                    flagged: optional_bool(args, "flagged")?,
                };
                if let Request::Label {
                    add,
                    remove,
                    flagged: None,
                    ..
                } = &request
                {
                    if add.is_empty() && remove.is_empty() {
                        return Err(invalid(
                            "mail.label needs at least one of 'add', 'remove' or 'flagged'.",
                        ));
                    }
                }
                Ok(request)
            }
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
    let mut session = ImapSession::open(connector, &mailbox.settings, &mailbox.auth)
        .await
        .map_err(session_error)?;
    let answer = match request {
        Request::List { .. } => list(&mut session, mailbox, request).await,
        Request::Read { thread_id } => read(&mut session, mailbox, thread_id).await,
        Request::Label {
            thread_id,
            add,
            remove,
            flagged,
        } => label(&mut session, mailbox, thread_id, add, remove, *flagged).await,
    };
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
    } = request
    else {
        unreachable!("list is called with a list request")
    };
    let status = session.examine(FOLDER).await.map_err(session_error)?;
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
        "folder": FOLDER,
        "matched_messages": matched.len(),
        "truncated": truncated,
        "threads": threads,
    }))
}

/// The UIDs of the messages whose thread is `thread_id`, ascending, and the
/// fetch of each that `items` asked for.
async fn thread_messages(
    session: &mut ImapSession,
    status: &FolderStatus,
    thread_id: &str,
    items: &str,
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
        session
            .uid_search(&[
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
            ])
            .await
            .map_err(session_error)?
    };
    let fetched = session
        .uid_fetch(&candidates, items)
        .await
        .map_err(session_error)?;
    let in_thread: Vec<Fetched> = fetched
        .into_iter()
        .filter(|f| {
            let raw = f
                .header
                .as_deref()
                .or(f.full.as_deref())
                .unwrap_or_default();
            thread_root(&parse_headers(raw), status, f.uid) == thread_id
        })
        .collect();
    if in_thread.is_empty() {
        let shown: String = thread_id
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        let message = format!("There is no thread '{shown}' in this mailbox's inbox.");
        return Err(
            SealSessionError::NotFound(message.clone()).answered(CallerAnswer::NotFound(message))
        );
    }
    Ok(in_thread)
}

async fn read(
    session: &mut ImapSession,
    mailbox: &ToolMailbox,
    thread_id: &str,
) -> Result<Value, SealSessionError> {
    let status = session.examine(FOLDER).await.map_err(session_error)?;
    let messages = thread_messages(
        session,
        &status,
        thread_id,
        "UID FLAGS INTERNALDATE BODY.PEEK[]",
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
                    "filename": a.filename,
                    "content_type": a.content_type,
                    "size": a.size,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "mailbox": mailbox.binding_id.0.to_string(),
        "folder": FOLDER,
        "thread_id": thread_id,
        "subject": subject,
        "truncated": truncated,
        "messages": answered,
    }))
}

async fn label(
    session: &mut ImapSession,
    mailbox: &ToolMailbox,
    thread_id: &str,
    add: &[String],
    remove: &[String],
    flagged: Option<bool>,
) -> Result<Value, SealSessionError> {
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
    if let Some(flagged) = flagged {
        let op = if flagged {
            StoreOp::Add
        } else {
            StoreOp::Remove
        };
        session
            .uid_store(&uids, op, &["\\Flagged".to_string()])
            .await
            .map_err(session_error)?;
    }
    Ok(json!({
        "mailbox": mailbox.binding_id.0.to_string(),
        "folder": FOLDER,
        "thread_id": thread_id,
        "message_uids": uids,
        "added": add,
        "removed": remove,
        "flagged": flagged,
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
