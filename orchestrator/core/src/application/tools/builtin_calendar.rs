// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The calendar read tools: `calendar.calendars`, `calendar.list` and
//! `calendar.read` (AEGIS ADR-138 K5a, K6's read half, K6a's read refusals,
//! K6c, K6d).
//!
//! They speak CalDAV only, over a calendar account of the acting person
//! (K1), inside the orchestrator: the OAuth access token `access_token_for`
//! answers goes as a bearer and never reaches an agent. They read and never
//! write, so they skip the inner-loop judge (K6c) and no gate holds them.
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
use crate::infrastructure::calendar::caldav::{CalDavClient, CalDavError};
use crate::infrastructure::calendar::ical::{parse_calendar, IcalTime, Party, VEvent};
use crate::infrastructure::calendar::{shown_reply, CalDavTransport, ReqwestTransport};
use chrono::{DateTime, Duration as ChronoDuration, NaiveTime, TimeZone, Utc};
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

/// Whether `tool_name` is one of the calendar tools this module serves.
pub fn is_calendar_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "calendar.calendars" | "calendar.list" | "calendar.read"
    )
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

/// A client failure as the caller is told it: an identifier off the
/// server's origin with the client's own sentence (K6d), an event id that
/// is not one segment with [`BAD_EVENT_ID`], and anything else as the
/// server's failure, the token redacted, control characters removed and
/// cut.
fn caldav_error(error: CalDavError, account: &ToolCalendar) -> SealSessionError {
    match error {
        CalDavError::OutsideServer(_) => invalid(error.to_string()),
        CalDavError::InvalidEventId(_) => invalid(BAD_EVENT_ID),
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
    }
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
