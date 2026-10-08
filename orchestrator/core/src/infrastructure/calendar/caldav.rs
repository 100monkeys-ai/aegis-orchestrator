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
//! - [`CalDavClient::discover_account`]: the check of an account connected
//!   by password (K10): `PROPFIND` (Depth 0) on the server's URL for
//!   `current-user-principal`, `calendar-home-set` and
//!   `calendar-user-address-set`, then on the principal it names when the
//!   URL's own answer names no home set.
//! - [`CalDavClient::calendars`]: `PROPFIND` (Depth 1) on the home set, the
//!   members whose resource type is a calendar.
//! - [`CalDavClient::events`]: `REPORT calendar-query` (Depth 1) with a
//!   `time-range` filter and `expand`, so a repeating event answers as its
//!   occurrences in the window.
//! - [`CalDavClient::event`]: `GET` of one event, with its `ETag`.
//! - [`CalDavClient::put_new`]: `PUT` of a new event resource with
//!   `If-None-Match: *`, so an existing resource is never replaced.
//! - [`CalDavClient::put_existing`] and [`CalDavClient::delete`]: `PUT` and
//!   `DELETE` of an event with `If-Match` of the `ETag` it was read with, so
//!   a change made since that read is answered `412`
//!   ([`CalDavError::Changed`]) and nothing is written.
//!
//! A `404` or `405` on a calendar collection (its `REPORT`, or the `PUT` of
//! a new resource into it) is [`CalDavError::NoSuchCalendar`]: the
//! collection is not there, which is the caller's identifier at fault and
//! never the server's failure (AEGIS ADR-138 K6f). [`own_calendar`] picks
//! the account's own calendar among a home set's.

use super::xml::{self, DavResponse, Element, APPLE_ICAL, CALDAV, DAV};
use super::CalendarCheckFailure;
use super::{
    shown_reply, CalDavAuth, CalDavRequest, CalDavResponse, CalDavTransport, TransportFailure,
};
use crate::domain::credential::CalendarSettings;
use chrono::{DateTime, Utc};
use url::Url;

/// The content type of a WebDAV request body.
const XML_CONTENT_TYPE: &str = "application/xml; charset=utf-8";
/// The content type of an iCalendar resource written by `PUT`.
const CALENDAR_CONTENT_TYPE: &str = "text/calendar; charset=utf-8";

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
    /// The address rule refused the server before any connection: a port
    /// other than 443, or an address that is not public unicast (K10).
    #[error("{0}")]
    HostNotAllowed(String),
    /// The server answered with a status the request does not accept.
    #[error("the calendar server answered {status}: {reply}")]
    Refused { status: u16, reply: String },
    /// The server answered, but not what CalDAV answers.
    #[error("the calendar server's answer could not be read: {detail}")]
    Malformed { status: u16, detail: String },
    /// The principal's answer named no calendar home set.
    #[error("the calendar server's answer names no calendar-home-set")]
    NoHomeSet,
    /// A conditional write was answered `412`: the event changed since it
    /// was read, or a new resource's name is already taken.
    #[error("The event changed since it was read; read it again; nothing was changed.")]
    Changed,
    /// The calendar collection answered `404` or `405`: there is no
    /// calendar at that identifier on this account.
    #[error("The calendar '{0}' does not exist on this account.")]
    NoSuchCalendar(String),
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
            host_not_allowed: matches!(self, CalDavError::HostNotAllowed(_)),
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

/// What the check of an account connected by password found (K10, K10a):
/// its principal as a reference on the server (the `current-user-principal`
/// href, or the server's URL itself when its own answer names the home set
/// and no principal), and its first `mailto:` calendar user address, if
/// any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountDiscovery {
    pub principal: String,
    pub address: Option<String>,
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
        let content_type = body.as_ref().map(|_| XML_CONTENT_TYPE);
        self.send_with(method, url, headers, content_type, body)
            .await
    }

    /// Send one request with `headers`, and `body` as `content_type`.
    async fn send_with(
        &self,
        method: &'static str,
        url: Url,
        mut headers: Vec<(&'static str, String)>,
        content_type: Option<&'static str>,
        body: Option<String>,
    ) -> Result<CalDavResponse, CalDavError> {
        if let Some(content_type) = content_type {
            headers.push(("Content-Type", content_type.to_string()));
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
            .map_err(|failure| match failure {
                TransportFailure::NotAllowed(reason) => CalDavError::HostNotAllowed(reason),
                TransportFailure::Failed(reason) => CalDavError::Unreachable(reason),
            })
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

    /// The check of an account connected by password (K10): `PROPFIND`
    /// (Depth 0) on the principal URL this client was given (the server's
    /// URL) for `current-user-principal`, `calendar-home-set` and
    /// `calendar-user-address-set`. When that answer names no home set but
    /// names a principal elsewhere on the server, the same `PROPFIND` goes to
    /// that principal. No home set in the end is [`CalDavError::NoHomeSet`].
    pub async fn discover_account(&self) -> Result<AccountDiscovery, CalDavError> {
        let first = self.account_props(self.principal.clone()).await?;
        if first.home_set.is_some() {
            let principal = match first.principal {
                Some(href) => {
                    self.resolve(&href)?;
                    href
                }
                None => self.principal.to_string(),
            };
            return Ok(AccountDiscovery {
                principal,
                address: first.address,
            });
        }
        let Some(href) = first.principal else {
            return Err(CalDavError::NoHomeSet);
        };
        let named = self.resolve(&href)?;
        if named == self.principal {
            return Err(CalDavError::NoHomeSet);
        }
        let second = self.account_props(named).await?;
        if second.home_set.is_none() {
            return Err(CalDavError::NoHomeSet);
        }
        Ok(AccountDiscovery {
            principal: href,
            address: second.address.or(first.address),
        })
    }

    /// One account `PROPFIND` (Depth 0) on `url`: the principal, home set
    /// and first `mailto:` address its answer names.
    async fn account_props(&self, url: Url) -> Result<AccountProps, CalDavError> {
        let responses = self
            .multistatus("PROPFIND", url, "0", xml::account_propfind())
            .await?;
        let mut props = AccountProps::default();
        for response in &responses {
            if props.principal.is_none() {
                props.principal = response
                    .prop(DAV, "current-user-principal")
                    .and_then(Element::href)
                    .filter(|href| !href.is_empty());
            }
            if props.home_set.is_none() {
                props.home_set = response
                    .prop(CALDAV, "calendar-home-set")
                    .and_then(Element::href)
                    .filter(|href| !href.is_empty());
            }
            if props.address.is_none() {
                props.address =
                    response
                        .prop(CALDAV, "calendar-user-address-set")
                        .and_then(|set| {
                            set.children_named(DAV, "href")
                                .find_map(|href| mailto_address(&href.text))
                        });
            }
        }
        Ok(props)
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
            .await
            .map_err(|e| on_collection(e, calendar))?;
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

    /// The URL of the event `event_id` of the calendar `calendar`: the
    /// calendar resolved on the server's origin and the id appended as one
    /// path segment; an id that is not one segment is refused.
    pub fn event_url(&self, calendar: &str, event_id: &str) -> Result<Url, CalDavError> {
        if !is_event_id(event_id) {
            return Err(CalDavError::InvalidEventId(shown(event_id)));
        }
        let mut url = self.resolve(calendar)?;
        url.path_segments_mut()
            .map_err(|_| CalDavError::OutsideServer(shown(calendar)))?
            .pop_if_empty()
            .push(event_id);
        Ok(url)
    }

    /// The event `event_id` of the calendar `calendar`: `GET`, answered 200
    /// with its data and its `ETag`.
    pub async fn event(
        &self,
        calendar: &str,
        event_id: &str,
    ) -> Result<EventResource, CalDavError> {
        let url = self.event_url(calendar, event_id)?;
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

    /// A conditional write's answer: 200, 201 or 204 is written, with the
    /// `ETag` the server gave if any; 412 is [`CalDavError::Changed`];
    /// anything else is refused.
    fn written(response: CalDavResponse) -> Result<Option<String>, CalDavError> {
        match response.status {
            200 | 201 | 204 => Ok(response.header("etag").map(str::to_string)),
            412 => Err(CalDavError::Changed),
            status => Err(CalDavError::Refused {
                status,
                reply: response.body,
            }),
        }
    }

    /// `PUT` of the new event `event_id` into the calendar `calendar` with
    /// `If-None-Match: *`: a resource of that name is never replaced. Answers
    /// the new resource's `ETag` when the server gives one.
    pub async fn put_new(
        &self,
        calendar: &str,
        event_id: &str,
        ics: String,
    ) -> Result<Option<String>, CalDavError> {
        let url = self.event_url(calendar, event_id)?;
        let response = self
            .send_with(
                "PUT",
                url,
                vec![("If-None-Match", "*".to_string())],
                Some(CALENDAR_CONTENT_TYPE),
                Some(ics),
            )
            .await?;
        Self::written(response).map_err(|e| on_collection(e, calendar))
    }

    /// `PUT` of the event `event_id` of the calendar `calendar` with
    /// `If-Match: <etag>`: written only while the resource still has the
    /// `ETag` it was read with. Answers the new `ETag` when the server
    /// gives one.
    pub async fn put_existing(
        &self,
        calendar: &str,
        event_id: &str,
        ics: String,
        etag: &str,
    ) -> Result<Option<String>, CalDavError> {
        let url = self.event_url(calendar, event_id)?;
        let response = self
            .send_with(
                "PUT",
                url,
                vec![("If-Match", etag.to_string())],
                Some(CALENDAR_CONTENT_TYPE),
                Some(ics),
            )
            .await?;
        Self::written(response)
    }

    /// `DELETE` of the event `event_id` of the calendar `calendar` with
    /// `If-Match: <etag>`: removed only while the resource still has the
    /// `ETag` it was read with.
    pub async fn delete(
        &self,
        calendar: &str,
        event_id: &str,
        etag: &str,
    ) -> Result<(), CalDavError> {
        let url = self.event_url(calendar, event_id)?;
        let response = self
            .send_with(
                "DELETE",
                url,
                vec![("If-Match", etag.to_string())],
                None,
                None,
            )
            .await?;
        Self::written(response).map(|_| ())
    }
}

/// Whether `event_id` can name an event: one path segment, not `.` or
/// `..`, with no control character.
pub fn is_event_id(event_id: &str) -> bool {
    !(event_id.is_empty()
        || event_id == "."
        || event_id == ".."
        || event_id.contains('/')
        || event_id.chars().any(char::is_control))
}

/// A failure of a request on the collection `calendar` (its `REPORT`, or
/// the `PUT` of a new resource into it): a `404` or `405` is
/// [`CalDavError::NoSuchCalendar`], anything else as it was.
fn on_collection(error: CalDavError, calendar: &str) -> CalDavError {
    match error {
        CalDavError::Refused {
            status: 404 | 405, ..
        } => CalDavError::NoSuchCalendar(shown(calendar)),
        other => other,
    }
}

/// The account's own calendar among `calendars`: the one whose id ends with
/// `/<address>/events/` (Google's shape; the id percent-decoded and both
/// compared without case), or failing that the first. `None` when there
/// are no calendars.
pub fn own_calendar<'c>(
    calendars: &'c [CalendarCollection],
    address: &str,
) -> Option<&'c CalendarCollection> {
    let address = address.trim().to_lowercase();
    let suffix = format!("/{address}/events/");
    calendars
        .iter()
        .find(|c| !address.is_empty() && percent_decode(&c.href).to_lowercase().ends_with(&suffix))
        .or_else(|| calendars.first())
}

/// What one account `PROPFIND` answered.
#[derive(Debug, Default)]
struct AccountProps {
    principal: Option<String>,
    home_set: Option<String>,
    address: Option<String>,
}

/// The address of a `mailto:` calendar user address (the scheme compared
/// without case), or `None` for any other.
fn mailto_address(href: &str) -> Option<String> {
    let href = href.trim();
    let (scheme, rest) = href.split_once(':')?;
    let address = rest.trim();
    (scheme.eq_ignore_ascii_case("mailto") && !address.is_empty())
        .then(|| address.chars().filter(|c| !c.is_control()).collect())
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
