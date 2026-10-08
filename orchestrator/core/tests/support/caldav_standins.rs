// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A loopback CalDAV stand-in (AEGIS ADR-138's tests): an HTTP/1.1 server on
//! 127.0.0.1 that answers a principal's `PROPFIND`, a home set's `PROPFIND`,
//! a calendar's `REPORT`, an event's `GET`, and an event's `PUT` and
//! `DELETE` with their conditions (`If-None-Match: *`, `If-Match`, answered
//! `412` when they do not hold), as a CalDAV server does, and records every
//! request it receives. No request leaves the machine.
//!
//! Its calendars are its state: a write changes them, gives the resource a
//! new `ETag`, and [`CalDavStandIn::event`] reads them back. Two switches:
//! [`CalDavStandIn::move_etag_after_get`] gives an event a new `ETag` right
//! after each `GET` of it (a change made by someone else between a read and
//! a write), and [`CalDavStandIn::refuse_writes_412`] answers every `PUT` and
//! `DELETE` `412`.
//!
//! A request whose `Authorization` is not `Bearer <accepted token>` is
//! answered `401`, its body repeating the header it was given, so a test can
//! show that a refusal's reply never carries the token.
//!
//! Three more setters serve a calendar account connected by password (AEGIS
//! ADR-138 K10): [`CalDavStandIn::accept_basic`] accepts one user name and
//! password by HTTP Basic as it accepts the token;
//! [`CalDavStandIn::serve_root`] answers a `PROPFIND` (Depth 0) on a server
//! root naming the principal and no home set, as a server's root does; and
//! [`CalDavStandIn::serve_address`] adds a `calendar-user-address-set` to the
//! principal's answer.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One request the stand-in received.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    /// The first header named `name`, compared without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One event resource of a stand-in calendar.
#[derive(Debug, Clone)]
pub struct StandInEvent {
    /// The resource's name within its calendar (`abc.ics`).
    pub name: String,
    pub etag: String,
    pub ics: String,
}

/// One calendar collection of the home set.
#[derive(Debug, Clone)]
pub struct StandInCalendar {
    /// The collection's path, ending in `/`.
    pub href: String,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<String>,
    pub writable: bool,
    pub events: Vec<StandInEvent>,
}

/// What the stand-in serves.
#[derive(Debug, Clone)]
pub struct StandInConfig {
    pub accepted_token: String,
    /// The principal's path, as the client's request line carries it.
    pub principal: String,
    /// The `calendar-home-set` href the principal names; `None`: its answer
    /// names none.
    pub home_set: Option<String>,
    pub calendars: Vec<StandInCalendar>,
}

/// The stand-in's state: what it serves and its two switches.
struct State {
    config: Mutex<StandInConfig>,
    move_etag_after_get: AtomicBool,
    refuse_writes_412: AtomicBool,
    next_etag: AtomicU64,
    /// The `Authorization` accepted besides the bearer: `Basic <base64>`.
    basic: Mutex<Option<String>>,
    /// A server root answered with the principal and no home set.
    root: Mutex<Option<String>>,
    /// The calendar user address the principal names.
    address: Mutex<Option<String>>,
}

impl State {
    fn new_etag(&self) -> String {
        format!("\"w{}\"", self.next_etag.fetch_add(1, Ordering::SeqCst))
    }
}

/// A running stand-in.
pub struct CalDavStandIn {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    state: Arc<State>,
}

impl CalDavStandIn {
    /// Start serving `config` on a loopback port.
    pub async fn start(config: StandInConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(State {
            config: Mutex::new(config),
            move_etag_after_get: AtomicBool::new(false),
            refuse_writes_412: AtomicBool::new(false),
            next_etag: AtomicU64::new(1),
            basic: Mutex::new(None),
            root: Mutex::new(None),
            address: Mutex::new(None),
        });
        let seen = requests.clone();
        let served = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (state, seen) = (served.clone(), seen.clone());
                tokio::spawn(async move {
                    serve(stream, &state, &seen).await;
                });
            }
        });
        Self {
            addr,
            requests,
            state,
        }
    }

    /// From now on, give an event a new `ETag` right after each `GET` of it.
    pub fn move_etag_after_get(&self, on: bool) {
        self.state.move_etag_after_get.store(on, Ordering::SeqCst);
    }

    /// From now on, answer every `PUT` and `DELETE` `412`.
    pub fn refuse_writes_412(&self, on: bool) {
        self.state.refuse_writes_412.store(on, Ordering::SeqCst);
    }

    /// From now on, accept `username` and `password` by HTTP Basic, as the
    /// token is accepted.
    pub fn accept_basic(&self, username: &str, password: &str) {
        use base64::Engine as _;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        *self.state.basic.lock().unwrap() = Some(format!("Basic {encoded}"));
    }

    /// From now on, answer a `PROPFIND` (Depth 0) on `path` with the
    /// principal as its `current-user-principal` and no home set.
    pub fn serve_root(&self, path: &str) {
        *self.state.root.lock().unwrap() = Some(path.to_string());
    }

    /// From now on, name `href` (`mailto:...`) as the principal's
    /// `calendar-user-address-set`.
    pub fn serve_address(&self, href: &str) {
        *self.state.address.lock().unwrap() = Some(href.to_string());
    }

    /// The event `name` of the calendar `href` as the stand-in now holds it.
    pub fn event(&self, href: &str, name: &str) -> Option<StandInEvent> {
        let config = self.state.config.lock().unwrap();
        config
            .calendars
            .iter()
            .find(|c| c.href == href)
            .and_then(|c| c.events.iter().find(|e| e.name == name).cloned())
    }

    /// Every event of the calendar `href` as the stand-in now holds them.
    pub fn events(&self, href: &str) -> Vec<StandInEvent> {
        let config = self.state.config.lock().unwrap();
        config
            .calendars
            .iter()
            .find(|c| c.href == href)
            .map(|c| c.events.clone())
            .unwrap_or_default()
    }

    /// Put `event` into the calendar `href` in place of the one of its
    /// name, or beside the others: a change made by someone else.
    pub fn set_event(&self, href: &str, event: StandInEvent) {
        let mut config = self.state.config.lock().unwrap();
        let calendar = config
            .calendars
            .iter_mut()
            .find(|c| c.href == href)
            .expect("the stand-in serves that calendar");
        match calendar.events.iter_mut().find(|e| e.name == event.name) {
            Some(existing) => *existing = event,
            None => calendar.events.push(event),
        }
    }

    /// The stand-in's origin, `http://127.0.0.1:<port>/`.
    /// (`reqwest`'s re-export of `url::Url`, so a crate including this file
    /// needs no `url` dependency of its own.)
    pub fn origin(&self) -> reqwest::Url {
        reqwest::Url::parse(&format!("http://{}/", self.addr)).expect("origin")
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve(mut stream: TcpStream, state: &State, seen: &Mutex<Vec<Recorded>>) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    seen.lock().unwrap().push(request.clone());
    let (status, headers, body) = answer(state, &request);
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        207 => "Multi-Status",
        401 => "Unauthorized",
        404 => "Not Found",
        412 => "Precondition Failed",
        _ => "Other",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Recorded {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

type Answer = (u16, Vec<(&'static str, String)>, String);

fn xml(status: u16, body: String) -> Answer {
    (
        status,
        vec![("Content-Type", "application/xml; charset=utf-8".to_string())],
        body,
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn answer(state: &State, request: &Recorded) -> Answer {
    let mut config = state.config.lock().unwrap();
    // The accepted Basic credentials are answered as the token is.
    let basic = state.basic.lock().unwrap().clone();
    let normalised;
    let request = match (&basic, request.header("authorization")) {
        (Some(accepted), Some(presented)) if presented == accepted.as_str() => {
            let mut r = request.clone();
            r.headers
                .retain(|(n, _)| !n.eq_ignore_ascii_case("authorization"));
            r.headers.push((
                "Authorization".to_string(),
                format!("Bearer {}", config.accepted_token),
            ));
            normalised = r;
            &normalised
        }
        _ => request,
    };
    let root = state.root.lock().unwrap().clone();
    if request.method == "PROPFIND"
        && request.header("depth") == Some("0")
        && root.as_deref() == Some(request.path.as_str())
        && request.path != config.principal
    {
        if let Some(refused) = unauthorized(&config, request) {
            return refused;
        }
        return xml(
            207,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
 <D:response>
  <D:href>{root}</D:href>
  <D:propstat>
   <D:prop><D:current-user-principal><D:href>{principal}</D:href></D:current-user-principal></D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
  <D:propstat>
   <D:prop><C:calendar-home-set/></D:prop>
   <D:status>HTTP/1.1 404 Not Found</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#,
                root = escape(&request.path),
                principal = escape(&config.principal),
            ),
        );
    }
    let address = state.address.lock().unwrap().clone();
    match request.method.as_str() {
        "PUT" | "DELETE" => return write(state, &mut config, request),
        "GET" => {
            let answer = read(&config, request);
            if answer.0 == 200 && state.move_etag_after_get.load(Ordering::SeqCst) {
                let etag = state.new_etag();
                if let Some(event) = event_at(&mut config, &request.path) {
                    event.etag = etag;
                }
            }
            return answer;
        }
        _ => {}
    }
    answer_read(&config, request, address.as_deref())
}

/// The event whose path is `path`, mutable.
fn event_at<'a>(config: &'a mut StandInConfig, path: &str) -> Option<&'a mut StandInEvent> {
    config.calendars.iter_mut().find_map(|c| {
        let href = c.href.clone();
        c.events
            .iter_mut()
            .find(|e| format!("{href}{}", e.name) == path)
    })
}

/// A request's `Authorization` refused, or `None` when it is the accepted
/// bearer.
fn unauthorized(config: &StandInConfig, request: &Recorded) -> Option<Answer> {
    let presented = request.header("authorization").unwrap_or("").to_string();
    (presented != format!("Bearer {}", config.accepted_token)).then(|| {
        (
            401,
            vec![("Content-Type", "text/plain".to_string())],
            format!("Unauthorized: the credentials presented ({presented}) are not valid for this calendar."),
        )
    })
}

/// `GET` of an event.
fn read(config: &StandInConfig, request: &Recorded) -> Answer {
    if let Some(refused) = unauthorized(config, request) {
        return refused;
    }
    let found = config.calendars.iter().find_map(|c| {
        c.events
            .iter()
            .find(|e| format!("{}{}", c.href, e.name) == request.path)
    });
    match found {
        Some(event) => (
            200,
            vec![
                ("Content-Type", "text/calendar; charset=utf-8".to_string()),
                ("ETag", event.etag.clone()),
            ],
            event.ics.clone(),
        ),
        None => (404, Vec::new(), "no such event".to_string()),
    }
}

/// `PUT` and `DELETE` of an event, with their conditions: `If-None-Match:
/// *` refuses an existing resource, `If-Match` one whose `ETag` differs or
/// that does not exist, each answered `412` with nothing changed.
fn write(state: &State, config: &mut StandInConfig, request: &Recorded) -> Answer {
    if let Some(refused) = unauthorized(config, request) {
        return refused;
    }
    if state.refuse_writes_412.load(Ordering::SeqCst) {
        return (412, Vec::new(), "precondition failed".to_string());
    }
    let Some((calendar, name)) = config.calendars.iter().enumerate().find_map(|(i, c)| {
        request
            .path
            .strip_prefix(&c.href)
            .filter(|rest| !rest.is_empty() && !rest.contains('/'))
            .map(|rest| (i, rest.to_string()))
    }) else {
        return (404, Vec::new(), "no such calendar".to_string());
    };
    let events = &mut config.calendars[calendar].events;
    let existing = events.iter().position(|e| e.name == name);
    if request.header("if-none-match") == Some("*") && existing.is_some() {
        return (412, Vec::new(), "the resource exists".to_string());
    }
    if let Some(wanted) = request.header("if-match") {
        match existing {
            Some(i) if events[i].etag == wanted => {}
            _ => return (412, Vec::new(), "the resource changed".to_string()),
        }
    }
    if request.method == "DELETE" {
        return match existing {
            Some(i) => {
                events.remove(i);
                (204, Vec::new(), String::new())
            }
            None => (404, Vec::new(), "no such event".to_string()),
        };
    }
    let etag = state.new_etag();
    let event = StandInEvent {
        name,
        etag: etag.clone(),
        ics: request.body.clone(),
    };
    let status = match existing {
        Some(i) => {
            events[i] = event;
            204
        }
        None => {
            events.push(event);
            201
        }
    };
    (status, vec![("ETag", etag)], String::new())
}

fn answer_read(config: &StandInConfig, request: &Recorded, address: Option<&str>) -> Answer {
    let presented = request.header("authorization").unwrap_or("").to_string();
    if presented != format!("Bearer {}", config.accepted_token) {
        return (
            401,
            vec![("Content-Type", "text/plain".to_string())],
            format!("Unauthorized: the credentials presented ({presented}) are not valid for this calendar."),
        );
    }
    let depth = request.header("depth").unwrap_or("");
    match request.method.as_str() {
        "PROPFIND" if request.path == config.principal && depth == "0" => {
            let home = match &config.home_set {
                Some(href) => format!(
                    "<C:calendar-home-set><D:href>{}</D:href></C:calendar-home-set>",
                    escape(href)
                ),
                None => String::new(),
            };
            let home = match address {
                Some(href) => format!(
                    "{home}<C:calendar-user-address-set><D:href>{}</D:href></C:calendar-user-address-set>",
                    escape(href)
                ),
                None => home,
            };
            xml(
                207,
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
 <D:response>
  <D:href>{principal}</D:href>
  <D:propstat>
   <D:prop><D:current-user-principal><D:href>{principal}</D:href></D:current-user-principal>{home}</D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#,
                    principal = escape(&config.principal),
                ),
            )
        }
        "PROPFIND" if config.home_set.as_deref() == Some(request.path.as_str()) && depth == "1" => {
            let mut body = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
 <d:response><d:href>{}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#,
                escape(&request.path)
            );
            for calendar in &config.calendars {
                let privileges = if calendar.writable {
                    "<d:privilege><d:read/></d:privilege><d:privilege><d:write/></d:privilege>"
                } else {
                    "<d:privilege><d:read/></d:privilege>"
                };
                let description = calendar
                    .description
                    .as_ref()
                    .map(|d| {
                        format!(
                            "<cal:calendar-description>{}</cal:calendar-description>",
                            escape(d)
                        )
                    })
                    .unwrap_or_default();
                let color = calendar
                    .color
                    .as_ref()
                    .map(|c| format!("<ic:calendar-color>{}</ic:calendar-color>", escape(c)))
                    .unwrap_or_default();
                body.push_str(&format!(
                    r#"
 <d:response><d:href>{href}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype><d:displayname>{name}</d:displayname>{description}{color}<d:current-user-privilege-set>{privileges}</d:current-user-privilege-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>{missing}</d:response>"#,
                    href = escape(&calendar.href),
                    name = escape(&calendar.name),
                    missing = if calendar.description.is_none() {
                        "<d:propstat><d:prop><cal:calendar-description/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>"
                    } else {
                        ""
                    },
                ));
            }
            body.push_str("\n</d:multistatus>");
            xml(207, body)
        }
        "REPORT" => match config.calendars.iter().find(|c| c.href == request.path) {
            Some(calendar) => {
                let mut body = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">"#
                    .to_string();
                for event in &calendar.events {
                    body.push_str(&format!(
                        r#"
 <D:response><D:href>{href}{name}</D:href><D:propstat><D:prop><D:getetag>{etag}</D:getetag><C:calendar-data>{data}</C:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"#,
                        href = escape(&calendar.href),
                        name = escape(&event.name),
                        etag = escape(&event.etag),
                        data = escape(&event.ics),
                    ));
                }
                body.push_str("\n</D:multistatus>");
                xml(207, body)
            }
            None => (404, Vec::new(), "no such calendar".to_string()),
        },
        _ => (404, Vec::new(), "not found".to_string()),
    }
}
