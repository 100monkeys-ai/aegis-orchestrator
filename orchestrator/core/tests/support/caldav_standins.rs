// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A loopback CalDAV stand-in (AEGIS ADR-138's tests): an HTTP/1.1 server on
//! 127.0.0.1 that answers a principal's `PROPFIND`, a home set's `PROPFIND`,
//! a calendar's `REPORT` and an event's `GET` as a CalDAV server does, and
//! records every request it receives. No request leaves the machine.
//!
//! A request whose `Authorization` is not `Bearer <accepted token>` is
//! answered `401`, its body repeating the header it was given, so a test can
//! show that a refusal's reply never carries the token.

#![allow(dead_code)]

use std::net::SocketAddr;
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

/// A running stand-in.
pub struct CalDavStandIn {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl CalDavStandIn {
    /// Start serving `config` on a loopback port.
    pub async fn start(config: StandInConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let config = Arc::new(config);
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (config, seen) = (config.clone(), seen.clone());
                tokio::spawn(async move {
                    serve(stream, &config, &seen).await;
                });
            }
        });
        Self { addr, requests }
    }

    /// The stand-in's origin, `http://127.0.0.1:<port>/`.
    pub fn origin(&self) -> url::Url {
        url::Url::parse(&format!("http://{}/", self.addr)).expect("origin")
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve(mut stream: TcpStream, config: &StandInConfig, seen: &Mutex<Vec<Recorded>>) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    seen.lock().unwrap().push(request.clone());
    let (status, headers, body) = answer(config, &request);
    let reason = match status {
        200 => "OK",
        207 => "Multi-Status",
        401 => "Unauthorized",
        404 => "Not Found",
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

fn answer(config: &StandInConfig, request: &Recorded) -> Answer {
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
        "GET" => {
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
        _ => (404, Vec::new(), "not found".to_string()),
    }
}
