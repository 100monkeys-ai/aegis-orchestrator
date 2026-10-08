// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The CalDAV client (RFC 4791 on RFC 4918; AEGIS ADR-138 K1, K5, K6).
//!
//! Every URL the client reaches is resolved against the account's `server`
//! and must keep its origin (scheme, host and port): a principal, home set,
//! calendar or event a server names elsewhere is refused before any request
//! goes there ([`CalDavError::OutsideServer`]). A calendar is named by its
//! collection's `href` as the server gives it, and an event by the last
//! segment of its resource's path.
//!
//! - [`CalDavClient::discover`]: `PROPFIND` (Depth 0) on the principal for
//!   `current-user-principal` and `calendar-home-set`. The connect-time
//!   check is this request (K5).
//! - [`CalDavClient::calendars`]: `PROPFIND` (Depth 1) on the home set, the
//!   members whose resource type is a calendar.
//! - [`CalDavClient::events`]: `REPORT calendar-query` (Depth 1) with a
//!   `time-range` filter and `expand`, so a repeating event answers as its
//!   occurrences in the window.
//! - [`CalDavClient::event`]: `GET` of one event, with its `ETag`.

use super::xml::{self, DavResponse, Element, APPLE_ICAL, CALDAV, DAV};
use super::CalendarCheckFailure;
use super::{shown_reply, CalDavAuth, CalDavRequest, CalDavResponse, CalDavTransport};
use crate::domain::credential::CalendarSettings;
use chrono::{DateTime, Utc};
use url::Url;

/// Why a CalDAV request did not give what the client asked for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CalDavError {
    /// The account's settings name no usable server or principal.
    #[error("the calendar account's settings are not usable: {0}")]
    InvalidSettings(String),
    /// A URL the server named, or the client was given, leaves the server's
    /// origin.
    #[error("'{0}' is not on this calendar account's server")]
    OutsideServer(String),
    /// A calendar or event name that is not one path segment.
    #[error("'{0}' is not the name of an event")]
    InvalidEventId(String),
    /// Nothing answered: the request failed before the server replied.
    #[error("{0}")]
    Unreachable(String),
    /// The server answered with a status the request does not accept.
    #[error("the calendar server answered {status}: {reply}")]
    Refused { status: u16, reply: String },
    /// The server answered, but not what CalDAV answers.
    #[error("the calendar server's answer could not be read: {detail}")]
    Malformed { status: u16, detail: String },
    /// The principal's answer named no calendar home set.
    #[error("the calendar server's answer names no calendar-home-set")]
    NoHomeSet,
}

impl CalDavError {
    /// The server's status, when it answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            CalDavError::Refused { status, .. } | CalDavError::Malformed { status, .. } => {
                Some(*status)
            }
            CalDavError::NoHomeSet => Some(207),
            _ => None,
        }
    }

    /// The failure as the connect-time check answers it (K5): the status
    /// and the reply, the token redacted, control characters removed and
    /// cut.
    pub fn check_failure(&self, auth: &CalDavAuth) -> CalendarCheckFailure {
        let reply = match self {
            CalDavError::Refused { reply, .. } => reply.clone(),
            other => other.to_string(),
        };
        CalendarCheckFailure {
            status: self.status(),
            reply: shown_reply(&reply, auth),
        }
    }
}

/// What a principal's discovery found: the principal's own URL and its
/// calendar home set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub principal: Url,
    pub home_set: Url,
}

/// One calendar of a home set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarCollection {
    /// The collection's `href` as the server gave it: the calendar's id.
    pub href: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    /// Whether the account may write to it: its privileges hold `write`,
    /// `write-content` or `all`.
    pub writable: bool,
}

/// One event resource: its `href`, its id (the last segment of its path),
/// its `etag` and its iCalendar data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventResource {
    pub href: String,
    pub event_id: String,
    pub etag: Option<String>,
    pub data: String,
}

/// A CalDAV client for one account, over a transport.
pub struct CalDavClient<'a> {
    transport: &'a dyn CalDavTransport,
    auth: &'a CalDavAuth,
    server: Url,
    principal: Url,
}

impl<'a> CalDavClient<'a> {
    /// The client for `settings`: its server must be an absolute `http` or
    /// `https` URL, and its principal must resolve on the server's origin.
    pub fn new(
        transport: &'a dyn CalDavTransport,
        settings: &CalendarSettings,
        auth: &'a CalDavAuth,
    ) -> Result<Self, CalDavError> {
        let server = Url::parse(&settings.server)
            .map_err(|e| CalDavError::InvalidSettings(format!("server: {e}")))?;
        if !matches!(server.scheme(), "https" | "http") || server.host_str().is_none() {
            return Err(CalDavError::InvalidSettings(
                "server must be an http or https URL with a host".to_string(),
            ));
        }
        let principal = resolve_on(&server, &settings.principal)?;
        Ok(Self {
            transport,
            auth,
            server,
            principal,
        })
    }

    /// The account's principal URL.
    pub fn principal_url(&self) -> &Url {
        &self.principal
    }

    /// `reference` resolved against the server, refused unless it keeps
    /// the server's origin.
    pub fn resolve(&self, reference: &str) -> Result<Url, CalDavError> {
        resolve_on(&self.server, reference)
    }

    async fn send(
        &self,
        method: &'static str,
        url: Url,
        depth: Option<&'static str>,
        body: Option<String>,
    ) -> Result<CalDavResponse, CalDavError> {
        let mut headers = Vec::new();
        if let Some(depth) = depth {
            headers.push(("Depth", depth.to_string()));
        }
        if body.is_some() {
            headers.push(("Content-Type", "application/xml; charset=utf-8".to_string()));
        }
        self.transport
            .send(
                CalDavRequest {
                    method,
                    url,
                    headers,
                    body,
                },
                self.auth,
            )
            .await
            .map_err(CalDavError::Unreachable)
    }

    /// A multistatus answer's responses; any other status is refused.
    async fn multistatus(
        &self,
        method: &'static str,
        url: Url,
        depth: &'static str,
        body: String,
    ) -> Result<Vec<DavResponse>, CalDavError> {
        let response = self.send(method, url, Some(depth), Some(body)).await?;
        if response.status != 207 {
            return Err(CalDavError::Refused {
                status: response.status,
                reply: response.body,
            });
        }
        xml::parse_multistatus(&response.body).map_err(|detail| CalDavError::Malformed {
            status: response.status,
            detail,
        })
    }

    /// `PROPFIND` (Depth 0) on the principal for its own URL and its
    /// calendar home set (K5).
    pub async fn discover(&self) -> Result<Discovery, CalDavError> {
        let responses = self
            .multistatus(
                "PROPFIND",
                self.principal.clone(),
                "0",
                xml::principal_propfind(),
            )
            .await?;
        let mut principal = None;
        let mut home_set = None;
        for response in &responses {
            if principal.is_none() {
                principal = response
                    .prop(DAV, "current-user-principal")
                    .and_then(Element::href);
            }
            if home_set.is_none() {
                home_set = response
                    .prop(CALDAV, "calendar-home-set")
                    .and_then(Element::href)
                    .filter(|href| !href.is_empty());
            }
        }
        let home_set = home_set.ok_or(CalDavError::NoHomeSet)?;
        Ok(Discovery {
            principal: match principal {
                Some(href) if !href.is_empty() => self.resolve(&href)?,
                _ => self.principal.clone(),
            },
            home_set: self.resolve(&home_set)?,
        })
    }

    /// The calendars of the home set `home_set`: `PROPFIND` (Depth 1), the
    /// members whose resource type is a calendar, each with its name,
    /// description, colour and whether the account may write to it.
    pub async fn calendars(&self, home_set: &Url) -> Result<Vec<CalendarCollection>, CalDavError> {
        let home_set = self.resolve(home_set.as_str())?;
        let responses = self
            .multistatus("PROPFIND", home_set, "1", xml::calendars_propfind())
            .await?;
        let mut calendars = Vec::new();
        for response in responses {
            let is_calendar = response
                .prop(DAV, "resourcetype")
                .is_some_and(|kind| kind.has_descendant(CALDAV, "calendar"));
            if !is_calendar {
                continue;
            }
            self.resolve(&response.href)?;
            let writable = response
                .prop(DAV, "current-user-privilege-set")
                .is_some_and(|privileges| {
                    ["write", "write-content", "all"]
                        .iter()
                        .any(|name| privileges.has_descendant(DAV, name))
                });
            calendars.push(CalendarCollection {
                name: response
                    .prop(DAV, "displayname")
                    .and_then(Element::trimmed_text),
                description: response
                    .prop(CALDAV, "calendar-description")
                    .and_then(Element::trimmed_text),
                color: response
                    .prop(APPLE_ICAL, "calendar-color")
                    .and_then(Element::trimmed_text),
                writable,
                href: response.href,
            });
        }
        Ok(calendars)
    }

    /// The events of the calendar `calendar` overlapping `start` to `end`:
    /// `REPORT calendar-query` (Depth 1) with a `time-range` filter and
    /// `expand`, each with its `etag` and data. A response the server
    /// answers without data (a 404 inside the multistatus) is left out.
    pub async fn events(
        &self,
        calendar: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<EventResource>, CalDavError> {
        let url = self.resolve(calendar)?;
        let responses = self
            .multistatus("REPORT", url, "1", xml::calendar_query(start, end))
            .await?;
        let mut events = Vec::new();
        for response in responses {
            let Some(data) = response
                .prop(CALDAV, "calendar-data")
                .map(|d| d.text.clone())
                .filter(|d| !d.trim().is_empty())
            else {
                continue;
            };
            let url = self.resolve(&response.href)?;
            events.push(EventResource {
                event_id: last_segment(&url),
                etag: response
                    .prop(DAV, "getetag")
                    .and_then(Element::trimmed_text),
                href: response.href,
                data,
            });
        }
        Ok(events)
    }

    /// The event `event_id` of the calendar `calendar`: `GET`, answered 200
    /// with its data and its `ETag`.
    pub async fn event(
        &self,
        calendar: &str,
        event_id: &str,
    ) -> Result<EventResource, CalDavError> {
        if event_id.is_empty()
            || event_id == "."
            || event_id == ".."
            || event_id.contains('/')
            || event_id.chars().any(char::is_control)
        {
            return Err(CalDavError::InvalidEventId(shown(event_id)));
        }
        let mut url = self.resolve(calendar)?;
        url.path_segments_mut()
            .map_err(|_| CalDavError::OutsideServer(shown(calendar)))?
            .pop_if_empty()
            .push(event_id);
        let response = self.send("GET", url.clone(), None, None).await?;
        if response.status != 200 {
            return Err(CalDavError::Refused {
                status: response.status,
                reply: response.body,
            });
        }
        Ok(EventResource {
            href: url.path().to_string(),
            event_id: event_id.to_string(),
            etag: response.header("etag").map(str::to_string),
            data: response.body,
        })
    }
}

/// `reference` resolved against `server`, refused unless it keeps the
/// server's origin.
fn resolve_on(server: &Url, reference: &str) -> Result<Url, CalDavError> {
    let url = server
        .join(reference)
        .map_err(|_| CalDavError::OutsideServer(shown(reference)))?;
    if url.origin() != server.origin() {
        return Err(CalDavError::OutsideServer(shown(reference)));
    }
    Ok(url)
}

/// The last non-empty segment of `url`'s path, percent-decoded.
fn last_segment(url: &Url) -> String {
    let segment = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
        .unwrap_or_default();
    percent_decode(segment)
}

/// `segment` with every `%XX` escape decoded; an escape that is not two hex
/// digits is kept as written, and bytes that are not UTF-8 are replaced.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// At most the first 200 characters of `s`, control characters removed,
/// for an error.
fn shown(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(200).collect()
}
