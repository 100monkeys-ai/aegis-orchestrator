// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The calendar tools (AEGIS ADR-138 K5a, K6, K6a, K6b, K6c, K6d, K6e, K7a):
//! `calendar.calendars`, `calendar.list` and `calendar.read` read;
//! `calendar.create`, `calendar.update`, `calendar.delete` and
//! `calendar.respond` write.
//!
//! They speak CalDAV only, over a calendar account of the acting person
//! (K1), inside the orchestrator: the OAuth access token `access_token_for`
//! answers goes as a bearer and never reaches an agent. The reads skip the
//! inner-loop judge and no gate holds them; the writes pass the judge and
//! wait at the approval gate (K6c, K7).
//!
//! **Writes** (K6, K6e). `calendar.create` puts a new resource
//! `<uuid>.ics` with `If-None-Match: *`, its times in UTC (dates all day),
//! and with attendees the account as `ORGANIZER` and each attendee invited
//! and unanswered. `calendar.update`, `calendar.delete` and
//! `calendar.respond` read the event (`GET`), refuse before any change a
//! repeating event (update, delete), an event another address organises
//! (update, delete) or one the account does not attend (respond), and write
//! with `If-Match` of the `ETag` they read: a `412` is answered "The event
//! changed since it was read; read it again; nothing was changed.". The
//! tools send no mail: invitations and updates to attendees are the
//! calendar server's (K6b).
//!
//! **The admission before the gate** (K7a). [`CalendarTools::admit`] checks
//! the account and the arguments, and for update, delete and respond reads
//! the event and answers the values the person reads on the card
//! (`current_title`, `current_start`; `title`, `start`, `end`, `attendees`;
//! `title`, `start`, `organizer`, `repeats`), which the dispatch writes over
//! the model's. A stored call run on its person's approval reads the event
//! again and writes with the `ETag` it then reads.
//!
//! **Who may use an account** (K5a, the mail tools' rules with "mailbox"
//! read as "calendar account"). The tool acts as the call's person (none
//! for a service account: refused). `account` must name the person's own
//! active calendar account, by id, or by its context name when accounts are
//! chosen. The choice for the key `caldav` (the execution record's
//! `contexts` when the execution has a record, else the call's
//! `_meta.contexts`) is a set of any number of accounts, which must, when
//! given, hold that binding; a choice of none refuses. With nothing chosen,
//! an execution with a record needs the binding granted to the calling
//! agent, its workflow or all agents; a call with no execution record (a
//! conversation, or an MCP client acting as the person) is admitted on the
//! ownership check alone.
//!
//! **Identifiers** (K6d). A calendar is named by its collection's `href` as
//! the server gives it (`calendar_id`), refused unless it resolves to the
//! account's server origin; an event by its resource's last path segment
//! (`event_id`).
//!
//! **Times.** `calendar.list` asks the server to expand repeating events
//! into their occurrences in the window, in UTC, and answers them so. A
//! value the server answers otherwise (a `TZID` or floating time) is
//! answered as written with its `tzid`, unconverted: no time-zone database
//! is carried. `calendar.read` answers `start` and `end` as written with
//! their `tzid`.

use crate::application::credential_service::{
    ToolCalendar, ToolCalendarSource, ToolCallActor, CALENDAR_CHOICE_KEY,
};
use crate::domain::agent::AgentId;
use crate::domain::credential::CredentialBindingId;
use crate::domain::execution::{ContextChoice, ServerChoice};
use crate::domain::seal_session::{CallerAnswer, InternalFailure, SealSessionError};
use crate::domain::tenant::TenantId;
use crate::infrastructure::calendar::caldav::EventResource;
use crate::infrastructure::calendar::caldav::{CalDavClient, CalDavError};
use crate::infrastructure::calendar::ical::{
    attendee_line, new_event_calendar, organizer_line, parse_calendar, same_address, utc_value,
    write_component, Component, ContentLine, EventTime, IcalTime, NewEvent, Party, VEvent,
};
use crate::infrastructure::calendar::{shown_reply, CalDavTransport, ReqwestTransport};
use crate::infrastructure::mail::message::is_address;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveTime, TimeZone, Utc};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// The widest window `calendar.list` reads, in days.
pub const MAX_WINDOW_DAYS: i64 = 92;
/// The window `calendar.list` reads when no `end` is given, in days.
pub const DEFAULT_WINDOW_DAYS: i64 = 7;
/// `calendar.list`'s default and largest `limit`.
pub const LIST_DEFAULT_LIMIT: u64 = 50;
pub const LIST_MAX_LIMIT: u64 = 100;
/// The most characters of a description `calendar.read` answers.
pub const DESCRIPTION_MAX_CHARS: usize = 32_000;
/// The longest one tool call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The refusal for a call with no person.
pub const NO_PERSON: &str =
    "This tool needs your own calendar account, and no person is recorded for this run.";
/// The refusal for a choice of none.
pub const NONE_CHOSEN: &str =
    "This tool needs your own calendar account, and none was chosen for this run.";
/// The refusal for one chosen account other than `account`.
pub const CHOSEN_DIFFERENT: &str =
    "This tool needs your own calendar account, and the one chosen for this run is a different one.";
/// The refusal for an account outside several chosen ones.
pub const NOT_AMONG_CHOSEN: &str =
    "This tool needs your own calendar account, and the one named is not among those chosen for this run.";
/// The refusal for an agent's run whose agent holds no grant.
pub const NOT_GRANTED: &str = "This tool needs your own calendar account, granted to this agent.";
/// The refusal for a window wider than [`MAX_WINDOW_DAYS`] (K6a).
pub const WINDOW_TOO_WIDE: &str = "'start' and 'end' must be at most 92 days apart.";
/// The refusal for an `end` not after `start` (K6a).
pub const END_NOT_AFTER_START: &str = "'end' must be after 'start'.";
/// The refusal for an `account` that is not a string.
pub const BAD_ACCOUNT: &str = "'account' must be the id of one of your calendar accounts.";
/// The refusal for a `limit` out of range.
pub const BAD_LIMIT: &str = "'limit' must be a whole number from 1 to 100.";
/// The refusal for an `event_id` that is not one path segment.
pub const BAD_EVENT_ID: &str = "'event_id' must be an event id calendar.list answered.";

/// The most attendees an event may have (K6a).
pub const MAX_ATTENDEES: usize = 50;
/// The most characters of a title (K6a).
pub const TITLE_MAX_CHARS: usize = 1_000;

/// The refusal for a title that is not one line of at most 1000
/// characters (K6a).
pub const BAD_TITLE: &str = "'title' must be one line of at most 1000 characters.";
/// The refusal for a description over 32000 characters (K6a).
pub const BAD_DESCRIPTION: &str = "'description' must be plain text of at most 32000 characters.";
/// The refusal for more than [`MAX_ATTENDEES`] attendees (K6a).
pub const TOO_MANY_ATTENDEES: &str = "An event has at most 50 attendees.";
/// The refusal for a repeating event to update or delete (K6a).
pub const REPEATS: &str = "This event repeats; changing or deleting a repeating event is not supported yet; nothing was changed.";
/// The refusal for an event another address organises (K6a).
pub const NOT_ORGANISER: &str = "You are not this event's organiser; answer it with calendar.respond instead; nothing was changed.";
/// The refusal for an event the account does not attend (K6a).
pub const NOT_ATTENDEE: &str = "You are not an attendee of this event; nothing was answered.";
/// The answer to a `412` on any write (K6a).
pub const EVENT_CHANGED: &str =
    "The event changed since it was read; read it again; nothing was changed.";
/// The refusal for an event its server gave no `ETag` (K6e (b)).
pub const NO_ETAG: &str = "The calendar server gave this event no etag; nothing was changed.";
/// The refusal for a date and a time given together (K6e (c)).
pub const MIXED_TIMES: &str = "'start' and 'end' must both be dates (YYYY-MM-DD) or both be times.";
/// The refusal for a `response` other than the three answers (K6e (i)).
pub const BAD_RESPONSE: &str = "'response' must be accepted, declined or tentative.";
/// The refusal for a `calendar.update` that changes nothing.
pub const NOTHING_TO_UPDATE: &str =
    "calendar.update needs at least one of title, start, end, description, location or attendees.";

/// The calendar tools that change an event; each waits for its person's
/// approval (K7).
pub const WRITE_TOOLS: &[&str] = &[
    "calendar.create",
    "calendar.update",
    "calendar.delete",
    "calendar.respond",
];

/// Whether `tool_name` is one of the calendar tools this module serves.
pub fn is_calendar_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "calendar.calendars" | "calendar.list" | "calendar.read"
    ) || WRITE_TOOLS.contains(&tool_name)
}

/// The refusal of `'<x>' is not an active calendar connection of yours.`
fn not_yours(raw: &str) -> SealSessionError {
    let shown: String = raw.chars().filter(|c| !c.is_control()).take(64).collect();
    binding_required(format!(
        "'{shown}' is not an active calendar connection of yours."
    ))
}

/// Who a calendar tool's call acts for, and what its run chose for
/// `caldav`.
#[derive(Debug, Clone)]
pub struct CalendarActing {
    pub tenant_id: TenantId,
    /// The acting person; `None` for a run with no person recorded.
    pub user_id: Option<String>,
    pub agent_id: AgentId,
    pub workflow_id: Option<uuid::Uuid>,
    /// The choice for the key `caldav`: nothing, none, or a set of accounts.
    pub choice: ServerChoice,
    /// Whether the call's execution has a record (an agent's run), as
    /// opposed to a conversation's session.
    pub has_execution_record: bool,
}

impl CalendarActing {
    /// The choice key a calendar account is chosen under.
    pub fn choice_key() -> &'static str {
        CALENDAR_CHOICE_KEY
    }
}

/// A gated calendar call as its admission found it: the account's binding
/// id, and the values read from the event to write over the model's.
#[derive(Debug, Clone)]
pub struct CalendarAdmitted {
    pub binding: CredentialBindingId,
    pub shown: Vec<(&'static str, Value)>,
}

/// The calendar tools: the account source and the transport their requests
/// go over.
pub struct CalendarTools {
    accounts: Arc<dyn ToolCalendarSource>,
    transport: Arc<dyn CalDavTransport>,
}

impl CalendarTools {
    /// The production tools: `https` only, no redirect followed.
    pub fn new(accounts: Arc<dyn ToolCalendarSource>) -> Self {
        Self::with_transport(accounts, Arc::new(ReqwestTransport::new()))
    }

    /// The tools over another transport (tests send every request to a
    /// loopback stand-in).
    pub fn with_transport(
        accounts: Arc<dyn ToolCalendarSource>,
        transport: Arc<dyn CalDavTransport>,
    ) -> Self {
        Self {
            accounts,
            transport,
        }
    }

    /// Run `tool_name` with `args` for `acting`.
    pub async fn invoke(
        &self,
        tool_name: &str,
        args: &Value,
        acting: &CalendarActing,
    ) -> Result<Value, SealSessionError> {
        let account = self.account_for(args, acting).await?;
        let request = Request::parse(tool_name, args, Utc::now())?;
        let run = run(self.transport.as_ref(), &account, &request);
        match tokio::time::timeout(CALL_TIMEOUT, run).await {
            Ok(result) => result,
            Err(_) => Err(timed_out()),
        }
    }

    /// Admit a gated calendar tool's call before the approval gate (K7a):
    /// the account (answered by id, a context name resolved), the arguments
    /// by K6a, the calendar's origin (K6d), and for update, delete and
    /// respond the event read and refused by K6a, with the values a person
    /// reads before answering. Nothing is written.
    pub async fn admit(
        &self,
        tool_name: &str,
        args: &Value,
        acting: &CalendarActing,
    ) -> Result<CalendarAdmitted, SealSessionError> {
        let account = self.account_for(args, acting).await?;
        let request = Request::parse(tool_name, args, Utc::now())?;
        let admit = admit(self.transport.as_ref(), &account, &request);
        let shown = match tokio::time::timeout(CALL_TIMEOUT, admit).await {
            Ok(shown) => shown?,
            Err(_) => return Err(timed_out()),
        };
        Ok(CalendarAdmitted {
            binding: account.binding_id,
            shown,
        })
    }

    /// The account the call may use, or its refusal.
    async fn account_for(
        &self,
        args: &Value,
        acting: &CalendarActing,
    ) -> Result<ToolCalendar, SealSessionError> {
        let Some(user_id) = acting.user_id.as_deref() else {
            return Err(binding_required(NO_PERSON.to_string()));
        };
        let raw = args
            .get("account")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(BAD_ACCOUNT))?;
        let id = match uuid::Uuid::parse_str(raw) {
            Ok(id) => CredentialBindingId(id),
            // A chosen account by its context name, resolved within the set.
            Err(_) => match &acting.choice {
                ServerChoice::Bindings(chosen) => self
                    .accounts
                    .calendar_contexts(&acting.tenant_id, user_id)
                    .await
                    .map_err(lookup_failed)?
                    .into_iter()
                    .find(|account| account.name == raw && chosen.contains(&account.id))
                    .map(|account| account.id)
                    .ok_or_else(|| not_yours(raw))?,
                _ => return Err(not_yours(raw)),
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
        let account = self
            .accounts
            .tool_calendar(&actor, &id)
            .await
            .map_err(lookup_failed)?
            .ok_or_else(|| not_yours(raw))?;
        match &acting.choice {
            ServerChoice::Bindings(chosen) if chosen.contains(&id) => {}
            ServerChoice::Bindings(chosen) if chosen.len() == 1 => {
                return Err(binding_required(CHOSEN_DIFFERENT.to_string()))
            }
            ServerChoice::Bindings(_) => {
                return Err(binding_required(NOT_AMONG_CHOSEN.to_string()))
            }
            ServerChoice::None => return Err(binding_required(NONE_CHOSEN.to_string())),
            ServerChoice::NotGiven if acting.has_execution_record && !account.granted => {
                return Err(binding_required(NOT_GRANTED.to_string()))
            }
            ServerChoice::NotGiven => {}
        }
        Ok(account)
    }
}

/// The answer for a call that is not served when the node has no calendar
/// tools configured.
pub fn not_configured() -> SealSessionError {
    SealSessionError::InternalError(
        "the calendar tools are not configured on this node".to_string(),
    )
    .answered(CallerAnswer::Internal(InternalFailure::Unavailable))
}

fn lookup_failed(e: anyhow::Error) -> SealSessionError {
    SealSessionError::InternalError(format!("calendar account lookup failed: {e}"))
        .answered(CallerAnswer::Internal(InternalFailure::Server))
}

fn timed_out() -> SealSessionError {
    SealSessionError::UpstreamUnavailable(format!(
        "The calendar server did not answer within {} seconds.",
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

/// A refusal about the event's own state: the call conflicts with it
/// (K6e (a)).
fn conflict(message: &str) -> SealSessionError {
    SealSessionError::InvalidArguments(message.to_string())
        .answered(CallerAnswer::Conflict(message.to_string()))
}

/// A client failure as the caller is told it: an identifier off the
/// server's origin with the client's own sentence (K6d), an event id that
/// is not one segment with [`BAD_EVENT_ID`], and anything else as the
/// server's failure, the token redacted, control characters removed and
/// cut.
fn caldav_error(error: CalDavError, account: &ToolCalendar) -> SealSessionError {
    match error {
        CalDavError::OutsideServer(_) => invalid(error.to_string()),
        CalDavError::InvalidEventId(_) => invalid(BAD_EVENT_ID),
        CalDavError::Changed => conflict(EVENT_CHANGED),
        other => SealSessionError::UpstreamUnavailable(format!(
            "The calendar server did not complete the request: {}",
            shown_reply(&other.to_string(), &account.auth)
        )),
    }
}

/// One call's arguments, checked.
#[derive(Debug, Clone, PartialEq)]
enum Request {
    Calendars,
    List {
        calendar_id: String,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        query: Option<String>,
        limit: usize,
    },
    Read {
        calendar_id: String,
        event_id: String,
    },
    Create(NewEventArgs),
    Update {
        calendar_id: String,
        event_id: String,
        changes: Changes,
    },
    Delete {
        calendar_id: String,
        event_id: String,
    },
    Respond {
        calendar_id: String,
        event_id: String,
        /// `accepted`, `declined` or `tentative`, as the call gave it.
        response: &'static str,
    },
}

/// `calendar.create`'s arguments, checked.
#[derive(Debug, Clone, PartialEq)]
struct NewEventArgs {
    calendar_id: String,
    title: String,
    start: EventTime,
    end: EventTime,
    description: Option<String>,
    location: Option<String>,
    attendees: Vec<String>,
}

/// What `calendar.update` changes; `None` leaves a property as it is.
#[derive(Debug, Clone, PartialEq, Default)]
struct Changes {
    title: Option<String>,
    start: Option<EventTime>,
    end: Option<EventTime>,
    description: Option<String>,
    location: Option<String>,
    attendees: Option<Vec<String>>,
}

impl Changes {
    fn is_empty(&self) -> bool {
        *self == Changes::default()
    }
}

/// A title: one line of at most [`TITLE_MAX_CHARS`] characters, not empty
/// (K6a, K6e (i)).
fn title_argument(args: &Value) -> Result<Option<String>, SealSessionError> {
    match args.get("title") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(title))
            if !title.trim().is_empty()
                && !title.contains(['\n', '\r'])
                && title.chars().count() <= TITLE_MAX_CHARS =>
        {
            Ok(Some(title.clone()))
        }
        Some(_) => Err(invalid(BAD_TITLE)),
    }
}

/// A description: plain text of at most [`DESCRIPTION_MAX_CHARS`]
/// characters (K6a).
fn description_argument(args: &Value) -> Result<Option<String>, SealSessionError> {
    match args.get("description") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(d)) if d.chars().count() <= DESCRIPTION_MAX_CHARS => Ok(Some(d.clone())),
        Some(_) => Err(invalid(BAD_DESCRIPTION)),
    }
}

/// A location: text.
fn location_argument(args: &Value) -> Result<Option<String>, SealSessionError> {
    match args.get("location") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(l)) => Ok(Some(l.clone())),
        Some(_) => Err(invalid("'location' must be text.")),
    }
}

/// The attendees: a list of at most [`MAX_ATTENDEES`] addresses, each by
/// the mail tools' address rule (K6a). `None` when absent.
fn attendees_argument(args: &Value) -> Result<Option<Vec<String>>, SealSessionError> {
    const NOT_A_LIST: &str = "'attendees' must be a list of email addresses.";
    let list = match args.get("attendees") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(list)) => list,
        Some(_) => return Err(invalid(NOT_A_LIST)),
    };
    if list.len() > MAX_ATTENDEES {
        return Err(invalid(TOO_MANY_ATTENDEES));
    }
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        let Some(address) = item.as_str() else {
            return Err(invalid(NOT_A_LIST));
        };
        if !is_address(address) {
            let shown: String = address
                .chars()
                .filter(|c| !c.is_control())
                .take(80)
                .collect();
            return Err(invalid(format!(
                "'{shown}' is not an email address this tool can invite."
            )));
        }
        out.push(address.to_string());
    }
    Ok(Some(out))
}

/// A time a write takes: an RFC 3339 time with an offset (written in UTC)
/// or a date `YYYY-MM-DD` (all day) (K6e (c)).
fn event_time_argument(args: &Value, name: &str) -> Result<Option<EventTime>, SealSessionError> {
    let refusal = || {
        invalid(format!(
            "'{name}' must be a time in RFC 3339 form with an offset, or a date written YYYY-MM-DD."
        ))
    };
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            if let Ok(at) = DateTime::parse_from_rfc3339(s) {
                return Ok(Some(EventTime::At(at.with_timezone(&Utc))));
            }
            if s.len() == 10 {
                if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                    return Ok(Some(EventTime::Date(date)));
                }
            }
            Err(refusal())
        }
        Some(_) => Err(refusal()),
    }
}

/// `end` after `start`, both of one kind (K6a, K6e (c)).
fn check_order(start: &EventTime, end: &EventTime) -> Result<(), SealSessionError> {
    match (start, end) {
        (EventTime::At(s), EventTime::At(e)) if e > s => Ok(()),
        (EventTime::Date(s), EventTime::Date(e)) if e > s => Ok(()),
        (EventTime::At(_), EventTime::At(_)) | (EventTime::Date(_), EventTime::Date(_)) => {
            Err(invalid(END_NOT_AFTER_START))
        }
        _ => Err(invalid(MIXED_TIMES)),
    }
}

fn required_string(args: &Value, name: &str, refusal: &str) -> Result<String, SealSessionError> {
    match args.get(name).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        _ => Err(invalid(refusal)),
    }
}

fn time_argument(args: &Value, name: &str) -> Result<Option<DateTime<Utc>>, SealSessionError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| Some(t.with_timezone(&Utc)))
            .ok_or_else(|| {
                invalid(format!(
                    "'{name}' must be a time in RFC 3339 form with an offset."
                ))
            }),
    }
}

impl Request {
    fn parse(tool_name: &str, args: &Value, now: DateTime<Utc>) -> Result<Self, SealSessionError> {
        const BAD_CALENDAR: &str =
            "'calendar_id' must be a calendar_id calendar.calendars answered.";
        match tool_name {
            "calendar.calendars" => Ok(Request::Calendars),
            "calendar.list" => {
                let calendar_id = required_string(args, "calendar_id", BAD_CALENDAR)?;
                let start = time_argument(args, "start")?.unwrap_or(now);
                let end = time_argument(args, "end")?
                    .unwrap_or(start + ChronoDuration::days(DEFAULT_WINDOW_DAYS));
                if end <= start {
                    return Err(invalid(END_NOT_AFTER_START));
                }
                if end - start > ChronoDuration::days(MAX_WINDOW_DAYS) {
                    return Err(invalid(WINDOW_TOO_WIDE));
                }
                let query = match args.get("query") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(q)) if q.trim().is_empty() => None,
                    Some(Value::String(q)) => Some(q.trim().to_string()),
                    Some(_) => return Err(invalid("'query' must be text.")),
                };
                let limit = match args.get("limit") {
                    None | Some(Value::Null) => LIST_DEFAULT_LIMIT,
                    Some(value) => value
                        .as_u64()
                        .filter(|n| (1..=LIST_MAX_LIMIT).contains(n))
                        .ok_or_else(|| invalid(BAD_LIMIT))?,
                };
                Ok(Request::List {
                    calendar_id,
                    start,
                    end,
                    query,
                    limit: limit as usize,
                })
            }
            "calendar.read" => Ok(Request::Read {
                calendar_id: required_string(args, "calendar_id", BAD_CALENDAR)?,
                event_id: required_string(args, "event_id", BAD_EVENT_ID)?,
            }),
            "calendar.create" => {
                let calendar_id = required_string(args, "calendar_id", BAD_CALENDAR)?;
                let title = title_argument(args)?.ok_or_else(|| invalid(BAD_TITLE))?;
                let start = event_time_argument(args, "start")?;
                let end = event_time_argument(args, "end")?;
                let (Some(start), Some(end)) = (start, end) else {
                    let missing = if start.is_none() { "start" } else { "end" };
                    return Err(invalid(format!(
                        "'{missing}' must be a time in RFC 3339 form with an offset, or a date written YYYY-MM-DD."
                    )));
                };
                check_order(&start, &end)?;
                Ok(Request::Create(NewEventArgs {
                    calendar_id,
                    title,
                    start,
                    end,
                    description: description_argument(args)?,
                    location: location_argument(args)?,
                    attendees: attendees_argument(args)?.unwrap_or_default(),
                }))
            }
            "calendar.update" => {
                let calendar_id = required_string(args, "calendar_id", BAD_CALENDAR)?;
                let event_id = required_string(args, "event_id", BAD_EVENT_ID)?;
                let changes = Changes {
                    title: title_argument(args)?,
                    start: event_time_argument(args, "start")?,
                    end: event_time_argument(args, "end")?,
                    description: description_argument(args)?,
                    location: location_argument(args)?,
                    attendees: attendees_argument(args)?,
                };
                if changes.is_empty() {
                    return Err(invalid(NOTHING_TO_UPDATE));
                }
                if let (Some(start), Some(end)) = (&changes.start, &changes.end) {
                    check_order(start, end)?;
                }
                Ok(Request::Update {
                    calendar_id,
                    event_id,
                    changes,
                })
            }
            "calendar.delete" => Ok(Request::Delete {
                calendar_id: required_string(args, "calendar_id", BAD_CALENDAR)?,
                event_id: required_string(args, "event_id", BAD_EVENT_ID)?,
            }),
            "calendar.respond" => {
                let calendar_id = required_string(args, "calendar_id", BAD_CALENDAR)?;
                let event_id = required_string(args, "event_id", BAD_EVENT_ID)?;
                let response = match args.get("response").and_then(Value::as_str) {
                    Some("accepted") => "accepted",
                    Some("declined") => "declined",
                    Some("tentative") => "tentative",
                    _ => return Err(invalid(BAD_RESPONSE)),
                };
                Ok(Request::Respond {
                    calendar_id,
                    event_id,
                    response,
                })
            }
            other => Err(invalid(format!("'{other}' is not a calendar tool."))),
        }
    }
}

async fn run(
    transport: &dyn CalDavTransport,
    account: &ToolCalendar,
    request: &Request,
) -> Result<Value, SealSessionError> {
    let client = CalDavClient::new(transport, &account.settings, &account.auth)
        .map_err(|e| caldav_error(e, account))?;
    let account_id = account.binding_id.0.to_string();
    match request {
        Request::Calendars => {
            let discovery = client
                .discover()
                .await
                .map_err(|e| caldav_error(e, account))?;
            let calendars = client
                .calendars(&discovery.home_set)
                .await
                .map_err(|e| caldav_error(e, account))?;
            let calendars: Vec<Value> = calendars
                .into_iter()
                .map(|calendar| {
                    let mut entry = Map::new();
                    entry.insert("calendar_id".into(), json!(calendar.href));
                    entry.insert("name".into(), json!(calendar.name));
                    entry.insert("description".into(), json!(calendar.description));
                    if let Some(color) = calendar.color {
                        entry.insert("color".into(), json!(color));
                    }
                    entry.insert("writable".into(), json!(calendar.writable));
                    Value::Object(entry)
                })
                .collect();
            Ok(json!({ "account": account_id, "calendars": calendars }))
        }
        Request::List {
            calendar_id,
            start,
            end,
            query,
            limit,
        } => {
            let resources = client
                .events(calendar_id, *start, *end)
                .await
                .map_err(|e| caldav_error(e, account))?;
            let needle = query.as_ref().map(|q| q.to_lowercase());
            let mut events: Vec<(DateTime<Utc>, Value)> = Vec::new();
            let mut unreadable = 0usize;
            for resource in &resources {
                let Ok(calendar) = parse_calendar(&resource.data) else {
                    unreadable += 1;
                    continue;
                };
                for event in calendar.events() {
                    if let Some(needle) = &needle {
                        let matched = [event.summary(), event.location(), event.description()]
                            .iter()
                            .flatten()
                            .any(|text| text.to_lowercase().contains(needle));
                        if !matched {
                            continue;
                        }
                    }
                    let order = event
                        .start()
                        .as_ref()
                        .and_then(sort_key)
                        .unwrap_or(DateTime::<Utc>::MAX_UTC);
                    events.push((order, event_fields(&resource.event_id, &event)));
                }
            }
            events.sort_by_key(|(order, _)| *order);
            let truncated = events.len() > *limit;
            let events: Vec<Value> = events
                .into_iter()
                .take(*limit)
                .map(|(_, value)| value)
                .collect();
            let mut answer = json!({
                "account": account_id,
                "calendar_id": calendar_id,
                "start": rfc3339_utc(*start),
                "end": rfc3339_utc(*end),
                "events": events,
                "truncated": truncated,
            });
            if unreadable > 0 {
                answer["unreadable"] = json!(unreadable);
            }
            Ok(answer)
        }
        Request::Read {
            calendar_id,
            event_id,
        } => {
            let resource = client
                .event(calendar_id, event_id)
                .await
                .map_err(|e| caldav_error(e, account))?;
            let calendar = parse_calendar(&resource.data).map_err(|detail| {
                SealSessionError::UpstreamUnavailable(format!(
                    "The calendar server did not complete the request: the event could not be read: {detail}"
                ))
            })?;
            let events: Vec<VEvent<'_>> = calendar.events().collect();
            let event = events
                .iter()
                .find(|event| event.recurrence_id().is_none())
                .or_else(|| events.first())
                .ok_or_else(|| {
                    SealSessionError::UpstreamUnavailable(
                        "The calendar server did not complete the request: the resource holds no event."
                            .to_string(),
                    )
                })?;
            let mut answer = event_fields(&resource.event_id, event);
            let description = event.description();
            let truncated = description
                .as_ref()
                .is_some_and(|d| d.chars().count() > DESCRIPTION_MAX_CHARS);
            answer["account"] = json!(account_id);
            answer["calendar_id"] = json!(calendar_id);
            answer["description"] = json!(
                description.map(|d| d.chars().take(DESCRIPTION_MAX_CHARS).collect::<String>())
            );
            answer["description_truncated"] = json!(truncated);
            answer["etag"] = json!(resource.etag);
            Ok(answer)
        }
        Request::Create(event) => {
            client
                .resolve(&event.calendar_id)
                .map_err(|e| caldav_error(e, account))?;
            let id = uuid::Uuid::new_v4();
            let event_id = format!("{id}.ics");
            let uid = match account.settings.address.rsplit_once('@') {
                Some((_, domain)) if !domain.is_empty() => {
                    format!("{id}@{}", domain.to_ascii_lowercase())
                }
                _ => id.to_string(),
            };
            let calendar = new_event_calendar(&NewEvent {
                uid: uid.clone(),
                stamp: Utc::now(),
                start: event.start,
                end: event.end,
                title: event.title.clone(),
                description: event.description.clone(),
                location: event.location.clone(),
                organizer: account.settings.address.clone(),
                attendees: event.attendees.clone(),
            });
            let etag = client
                .put_new(&event.calendar_id, &event_id, write_component(&calendar))
                .await
                .map_err(|e| caldav_error(e, account))?;
            Ok(json!({
                "account": account_id,
                "calendar_id": event.calendar_id,
                "event_id": event_id,
                "uid": uid,
                "etag": etag,
            }))
        }
        Request::Update {
            calendar_id,
            event_id,
            changes,
        } => {
            let read = read_for_change(&client, account, calendar_id, event_id).await?;
            refuse_change(&read, &account.settings.address)?;
            let calendar = updated(&read, changes, &account.settings.address, Utc::now())?;
            let etag = client
                .put_existing(
                    calendar_id,
                    event_id,
                    write_component(&calendar),
                    &read.etag,
                )
                .await
                .map_err(|e| caldav_error(e, account))?;
            Ok(json!({
                "account": account_id,
                "calendar_id": calendar_id,
                "event_id": event_id,
                "etag": etag,
            }))
        }
        Request::Delete {
            calendar_id,
            event_id,
        } => {
            let read = read_for_change(&client, account, calendar_id, event_id).await?;
            refuse_change(&read, &account.settings.address)?;
            client
                .delete(calendar_id, event_id, &read.etag)
                .await
                .map_err(|e| caldav_error(e, account))?;
            Ok(json!({
                "account": account_id,
                "calendar_id": calendar_id,
                "event_id": event_id,
                "deleted": true,
            }))
        }
        Request::Respond {
            calendar_id,
            event_id,
            response,
        } => {
            let read = read_for_change(&client, account, calendar_id, event_id).await?;
            let calendar = responded(&read, &account.settings.address, response, Utc::now())?;
            let etag = client
                .put_existing(
                    calendar_id,
                    event_id,
                    write_component(&calendar),
                    &read.etag,
                )
                .await
                .map_err(|e| caldav_error(e, account))?;
            Ok(json!({
                "account": account_id,
                "calendar_id": calendar_id,
                "event_id": event_id,
                "response": response,
                "etag": etag,
            }))
        }
    }
}

/// The admission's half of a gated call (K7a): the calendar's origin
/// checked, and for update, delete and respond the event read, refused by
/// K6a and answered as the values a person reads. Nothing is written.
async fn admit(
    transport: &dyn CalDavTransport,
    account: &ToolCalendar,
    request: &Request,
) -> Result<Vec<(&'static str, Value)>, SealSessionError> {
    let client = CalDavClient::new(transport, &account.settings, &account.auth)
        .map_err(|e| caldav_error(e, account))?;
    let address = &account.settings.address;
    match request {
        Request::Create(event) => {
            client
                .resolve(&event.calendar_id)
                .map_err(|e| caldav_error(e, account))?;
            Ok(Vec::new())
        }
        Request::Update {
            calendar_id,
            event_id,
            changes,
        } => {
            let read = read_for_change(&client, account, calendar_id, event_id).await?;
            refuse_change(&read, address)?;
            updated(&read, changes, address, Utc::now())?;
            let event = read.master();
            Ok(vec![
                ("current_title", json!(event.summary().map(|t| clean(&t)))),
                (
                    "current_start",
                    json!(event.start().as_ref().map(time_shown)),
                ),
            ])
        }
        Request::Delete {
            calendar_id,
            event_id,
        } => {
            let read = read_for_change(&client, account, calendar_id, event_id).await?;
            refuse_change(&read, address)?;
            let event = read.master();
            Ok(vec![
                ("title", json!(event.summary().map(|t| clean(&t)))),
                ("start", json!(event.start().as_ref().map(time_shown))),
                ("end", json!(event.end().as_ref().map(time_shown))),
                (
                    "attendees",
                    json!(event
                        .attendees()
                        .iter()
                        .map(|a| clean(&a.address))
                        .collect::<Vec<_>>()),
                ),
            ])
        }
        Request::Respond {
            calendar_id,
            event_id,
            response,
        } => {
            let read = read_for_change(&client, account, calendar_id, event_id).await?;
            responded(&read, address, response, Utc::now())?;
            let event = read.master();
            Ok(vec![
                ("title", json!(event.summary().map(|t| clean(&t)))),
                ("start", json!(event.start().as_ref().map(time_shown))),
                (
                    "organizer",
                    json!(event.organizer().as_ref().map(party_shown)),
                ),
                ("repeats", json!(read.repeats())),
            ])
        }
        Request::Calendars | Request::List { .. } | Request::Read { .. } => Ok(Vec::new()),
    }
}

/// An event read to be changed: its resource, its calendar object, and the
/// `ETag` the change is conditioned on.
struct ReadEvent {
    calendar: Component,
    etag: String,
}

impl ReadEvent {
    /// The index of the event the resource is about: its `VEVENT` without a
    /// `RECURRENCE-ID`, else its first.
    fn master_index(&self) -> usize {
        let events: Vec<usize> = self
            .calendar
            .components
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name == "VEVENT")
            .map(|(i, _)| i)
            .collect();
        events
            .iter()
            .copied()
            .find(|&i| {
                VEvent(&self.calendar.components[i])
                    .recurrence_id()
                    .is_none()
            })
            .or_else(|| events.first().copied())
            .expect("a read event holds a VEVENT")
    }

    fn master(&self) -> VEvent<'_> {
        VEvent(&self.calendar.components[self.master_index()])
    }

    /// Whether any event of the resource repeats.
    fn repeats(&self) -> bool {
        self.calendar.events().any(|e| e.repeats())
    }
}

/// `GET` of the event to change, read and holding an `ETag` (K6e (b)).
async fn read_for_change(
    client: &CalDavClient<'_>,
    account: &ToolCalendar,
    calendar_id: &str,
    event_id: &str,
) -> Result<ReadEvent, SealSessionError> {
    let EventResource { etag, data, .. } = client
        .event(calendar_id, event_id)
        .await
        .map_err(|e| caldav_error(e, account))?;
    let calendar = parse_calendar(&data).map_err(|detail| {
        SealSessionError::UpstreamUnavailable(format!(
            "The calendar server did not complete the request: the event could not be read: {detail}"
        ))
    })?;
    if calendar.events().next().is_none() {
        return Err(SealSessionError::UpstreamUnavailable(
            "The calendar server did not complete the request: the resource holds no event."
                .to_string(),
        ));
    }
    let etag = etag
        .filter(|e| !e.trim().is_empty())
        .ok_or_else(|| conflict(NO_ETAG))?;
    Ok(ReadEvent { calendar, etag })
}

/// K6a's refusals for `calendar.update` and `calendar.delete`: a repeating
/// event, and an event whose `ORGANIZER` is another address.
fn refuse_change(read: &ReadEvent, address: &str) -> Result<(), SealSessionError> {
    if read.repeats() {
        return Err(conflict(REPEATS));
    }
    if let Some(organizer) = read.master().organizer() {
        if !same_address(&organizer.address, address) {
            return Err(conflict(NOT_ORGANISER));
        }
    }
    Ok(())
}

/// The event's other end as a write compares with it: a UTC time as it is,
/// a date as a date, and a `TZID` or floating time read as UTC (no time
/// zone database is carried).
fn as_event_time(time: &IcalTime) -> Option<EventTime> {
    if time.all_day {
        return time.date().map(EventTime::Date);
    }
    time.utc()
        .or_else(|| time.local().map(|t| Utc.from_utc_datetime(&t)))
        .map(EventTime::At)
}

/// The calendar object `calendar.update` writes (K6, K6e (d)): only the
/// named properties changed, times in UTC with `TZID` dropped, a lone
/// `start` or `end` checked against the event's other end, `DURATION`
/// replaced by `DTEND` when `end` is given, the attendees replaced (those
/// kept keep their answer, an empty list removes them all), `SEQUENCE`
/// incremented and `DTSTAMP` refreshed.
fn updated(
    read: &ReadEvent,
    changes: &Changes,
    address: &str,
    now: DateTime<Utc>,
) -> Result<Component, SealSessionError> {
    let index = read.master_index();
    let mut calendar = read.calendar.clone();
    let current = VEvent(&read.calendar.components[index]);
    match (&changes.start, &changes.end) {
        (Some(start), None) => {
            if let Some(end) = current.end().as_ref().and_then(as_event_time) {
                check_order(start, &end)?;
            }
        }
        (None, Some(end)) => {
            if let Some(start) = current.start().as_ref().and_then(as_event_time) {
                check_order(&start, end)?;
            }
        }
        _ => {}
    }
    let event = &mut calendar.components[index];
    if let Some(title) = &changes.title {
        event.set_property(ContentLine::new("SUMMARY", escape(title)));
    }
    if let Some(description) = &changes.description {
        event.set_property(ContentLine::new("DESCRIPTION", escape(description)));
    }
    if let Some(location) = &changes.location {
        event.set_property(ContentLine::new("LOCATION", escape(location)));
    }
    if let Some(start) = &changes.start {
        event.set_property(start.line("DTSTART"));
    }
    if let Some(end) = &changes.end {
        event.remove_properties("DURATION");
        event.set_property(end.line("DTEND"));
    }
    if let Some(attendees) = &changes.attendees {
        let kept: Vec<ContentLine> = attendees
            .iter()
            .map(|wanted| {
                event
                    .properties_named("ATTENDEE")
                    .find(|line| same_address(&Party::from_property(line).address, wanted))
                    .cloned()
                    .unwrap_or_else(|| attendee_line(wanted))
            })
            .collect();
        let at = event
            .properties
            .iter()
            .position(|p| p.name == "ATTENDEE")
            .unwrap_or(event.properties.len());
        event.remove_properties("ATTENDEE");
        let at = at.min(event.properties.len());
        for (offset, line) in kept.into_iter().enumerate() {
            event.properties.insert(at + offset, line);
        }
        if !attendees.is_empty() && event.property("ORGANIZER").is_none() {
            event.properties.push(organizer_line(address));
        }
    }
    event.bump_sequence();
    event.set_property(ContentLine::new("DTSTAMP", utc_value(now)));
    Ok(calendar)
}

/// The calendar object `calendar.respond` writes (K6, K6e (h)): `PARTSTAT`
/// set on the account's `ATTENDEE` in every `VEVENT` it attends, `RSVP`
/// kept, and those events' `DTSTAMP` refreshed. Refused when the account
/// attends none (K6a).
fn responded(
    read: &ReadEvent,
    address: &str,
    response: &str,
    now: DateTime<Utc>,
) -> Result<Component, SealSessionError> {
    let partstat = response.to_ascii_uppercase();
    let mut calendar = read.calendar.clone();
    let mut attends = false;
    for event in calendar
        .components
        .iter_mut()
        .filter(|c| c.name == "VEVENT")
    {
        if event.set_partstat(address, &partstat) {
            attends = true;
            event.set_property(ContentLine::new("DTSTAMP", utc_value(now)));
        }
    }
    if !attends {
        return Err(conflict(NOT_ATTENDEE));
    }
    Ok(calendar)
}

fn escape(text: &str) -> String {
    crate::infrastructure::calendar::ical::escape_text(text)
}

/// `text` with its control characters removed, for a value a person reads.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// A time as a person reads it on the card (K6e (f)): RFC 3339 as written,
/// with its `TZID` after it when it has one.
fn time_shown(time: &IcalTime) -> String {
    let written = time.rfc3339().unwrap_or_else(|| time.value.clone());
    clean(&match &time.tzid {
        Some(tzid) => format!("{written} {tzid}"),
        None => written,
    })
}

/// A party as a person reads it on the card (K6e (f)): `Name <address>`,
/// or the address alone.
fn party_shown(party: &Party) -> String {
    clean(&match &party.name {
        Some(name) if !name.trim().is_empty() => format!("{name} <{}>", party.address),
        _ => party.address.clone(),
    })
}

/// An instant to order events by: a UTC value as it is, an all-day value
/// at midnight of its date and a floating or `TZID` value at its written
/// time, both read as UTC (no time-zone database is carried).
fn sort_key(time: &IcalTime) -> Option<DateTime<Utc>> {
    if let Some(utc) = time.utc() {
        return Some(utc);
    }
    if time.all_day {
        return time
            .date()
            .map(|d| Utc.from_utc_datetime(&d.and_time(NaiveTime::MIN)));
    }
    time.local().map(|t| Utc.from_utc_datetime(&t))
}

fn rfc3339_utc(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn party(party: &Party) -> Value {
    json!({ "address": party.address, "name": party.name })
}

/// An event as `calendar.list` answers it (K6): its id, its `UID`, title,
/// times (RFC 3339 as written: UTC when the server gave UTC, else the
/// written time with its `tzid`), whether it is all-day, location,
/// organiser, attendees with their answers, status, whether it repeats and
/// its `RECURRENCE-ID`.
fn event_fields(event_id: &str, event: &VEvent<'_>) -> Value {
    let start = event.start();
    let end = event.end();
    json!({
        "event_id": event_id,
        "uid": event.uid(),
        "title": event.summary(),
        "start": start.as_ref().and_then(IcalTime::rfc3339),
        "start_tzid": start.as_ref().and_then(|t| t.tzid.clone()),
        "end": end.as_ref().and_then(IcalTime::rfc3339),
        "end_tzid": end.as_ref().and_then(|t| t.tzid.clone()),
        "all_day": start.as_ref().is_some_and(|t| t.all_day),
        "location": event.location(),
        "organizer": event.organizer().as_ref().map(party),
        "attendees": event
            .attendees()
            .iter()
            .map(|a| json!({ "address": a.address, "name": a.name, "answer": a.answer }))
            .collect::<Vec<_>>(),
        "status": event.status(),
        "repeats": event.repeats(),
        "recurrence_id": event.recurrence_id().as_ref().and_then(IcalTime::rfc3339),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
    }

    fn refusal(result: Result<Request, SealSessionError>) -> String {
        match result {
            Err(SealSessionError::InvalidArguments(m)) => m,
            other => format!("not refused: {other:?}"),
        }
    }

    #[test]
    fn the_list_window_defaults_to_now_and_seven_days_on() {
        let request = Request::parse("calendar.list", &json!({"calendar_id": "/c/"}), now());
        assert_eq!(
            request.unwrap(),
            Request::List {
                calendar_id: "/c/".to_string(),
                start: now(),
                end: now() + ChronoDuration::days(7),
                query: None,
                limit: 50,
            }
        );
    }

    #[test]
    fn a_window_of_92_days_is_read_and_one_second_more_is_refused() {
        let args = |end: &str| json!({"calendar_id": "/c/", "start": "2026-10-01T00:00:00+02:00", "end": end});
        assert!(Request::parse("calendar.list", &args("2027-01-01T00:00:00+02:00"), now()).is_ok());
        assert_eq!(
            refusal(Request::parse(
                "calendar.list",
                &args("2027-01-01T00:00:01+02:00"),
                now()
            )),
            WINDOW_TOO_WIDE
        );
    }

    #[test]
    fn an_end_not_after_its_start_is_refused() {
        for end in ["2026-10-01T00:00:00Z", "2026-09-30T23:00:00Z"] {
            assert_eq!(
                refusal(Request::parse(
                    "calendar.list",
                    &json!({"calendar_id": "/c/", "start": "2026-10-01T00:00:00Z", "end": end}),
                    now()
                )),
                END_NOT_AFTER_START,
                "end {end}"
            );
        }
    }

    #[test]
    fn times_limits_and_ids_are_refused_with_their_sentences() {
        let cases = [
            (
                json!({"calendar_id": "/c/", "start": "2026-10-01"}),
                "'start' must be a time in RFC 3339 form with an offset.",
            ),
            (
                json!({"calendar_id": "/c/", "end": "2026-10-01T10:00:00"}),
                "'end' must be a time in RFC 3339 form with an offset.",
            ),
            (json!({"calendar_id": "/c/", "limit": 0}), BAD_LIMIT),
            (json!({"calendar_id": "/c/", "limit": 101}), BAD_LIMIT),
            (json!({"calendar_id": "/c/", "limit": 2.5}), BAD_LIMIT),
            (
                json!({}),
                "'calendar_id' must be a calendar_id calendar.calendars answered.",
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(
                refusal(Request::parse("calendar.list", &args, now())),
                expected,
                "{args}"
            );
        }
        assert_eq!(
            refusal(Request::parse(
                "calendar.read",
                &json!({"calendar_id": "/c/"}),
                now()
            )),
            BAD_EVENT_ID
        );
    }
}
