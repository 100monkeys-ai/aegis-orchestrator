// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! WebDAV and CalDAV XML (RFC 4918, RFC 4791): the request bodies the client
//! sends and a reader of the multistatus answers it gets, over `quick-xml`
//! (AEGIS ADR-138 K2).
//!
//! The reader builds a small element tree with every name resolved to its
//! namespace, so a server's choice of prefixes does not matter. A document
//! type declaration is read past and never expanded: no entity a server
//! defines is resolved and nothing is fetched; the five predefined entities
//! and character references are.

use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

/// The WebDAV namespace.
pub const DAV: &str = "DAV:";
/// The CalDAV namespace.
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
/// The namespace of the `calendar-color` property most servers answer.
pub const APPLE_ICAL: &str = "http://apple.com/ns/ical/";

/// The `PROPFIND` body asking a principal for its own URL and its calendar
/// home set (RFC 5397, RFC 4791 §6.2.1).
pub fn principal_propfind() -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="{DAV}" xmlns:c="{CALDAV}">
  <d:prop>
    <d:current-user-principal/>
    <c:calendar-home-set/>
  </d:prop>
</d:propfind>
"#
    )
}

/// The `PROPFIND` body a calendar account connected by password is
/// discovered with (AEGIS ADR-138 K10): the principal, its calendar home
/// set, and its calendar user addresses (RFC 6638).
pub fn account_propfind() -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="{DAV}" xmlns:c="{CALDAV}">
  <d:prop>
    <d:current-user-principal/>
    <c:calendar-home-set/>
    <c:calendar-user-address-set/>
  </d:prop>
</d:propfind>
"#
    )
}

/// The `PROPFIND` body asking a home set's members what each is: its
/// resource type, name, description, colour, and what the account may do
/// to it.
pub fn calendars_propfind() -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="{DAV}" xmlns:c="{CALDAV}" xmlns:a="{APPLE_ICAL}">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <c:calendar-description/>
    <a:calendar-color/>
    <d:current-user-privilege-set/>
  </d:prop>
</d:propfind>
"#
    )
}

/// A time as a CalDAV `time-range` and `expand` write it: UTC, basic form.
pub fn utc_stamp(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

/// The `REPORT calendar-query` body for the events overlapping `start` to
/// `end` (RFC 4791 §7.8), each with its `etag` and its data with repeating
/// events expanded by the server into their occurrences in that window
/// (§9.6.5), in UTC.
pub fn calendar_query(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let (start, end) = (utc_stamp(start), utc_stamp(end));
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="{DAV}" xmlns:c="{CALDAV}">
  <d:prop>
    <d:getetag/>
    <c:calendar-data>
      <c:expand start="{start}" end="{end}"/>
    </c:calendar-data>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="{start}" end="{end}"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>
"#
    )
}

/// One element of an XML document: its namespace (empty when it has none),
/// its local name, the text directly inside it, and its child elements.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    pub namespace: String,
    pub name: String,
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    /// Whether this element is `name` in `namespace`.
    pub fn is(&self, namespace: &str, name: &str) -> bool {
        self.namespace == namespace && self.name == name
    }

    /// The first child that is `name` in `namespace`.
    pub fn child(&self, namespace: &str, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.is(namespace, name))
    }

    /// Every child that is `name` in `namespace`.
    pub fn children_named<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.is(namespace, name))
    }

    /// Whether any element below this one is `name` in `namespace`.
    pub fn has_descendant(&self, namespace: &str, name: &str) -> bool {
        self.children
            .iter()
            .any(|c| c.is(namespace, name) || c.has_descendant(namespace, name))
    }

    /// The text of the first `DAV:` `href` below this element, trimmed.
    pub fn href(&self) -> Option<String> {
        for child in &self.children {
            if child.is(DAV, "href") {
                return Some(child.text.trim().to_string());
            }
            if let Some(href) = child.href() {
                return Some(href);
            }
        }
        None
    }

    /// This element's own text, trimmed; `None` when it is empty.
    pub fn trimmed_text(&self) -> Option<String> {
        let text = self.text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }
}

/// Read `body` into its root element.
pub fn parse(body: &str) -> Result<Element, String> {
    let mut reader = NsReader::from_str(body);
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    let not_xml = |e: &dyn std::fmt::Display| format!("the answer is not well-formed XML: {e}");
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(|e| not_xml(&e))?;
        let namespace = match namespace {
            ResolveResult::Bound(ns) => String::from_utf8_lossy(ns.as_ref()).into_owned(),
            _ => String::new(),
        };
        match event {
            Event::Start(start) => stack.push(Element {
                namespace,
                name: String::from_utf8_lossy(start.local_name().as_ref()).into_owned(),
                ..Element::default()
            }),
            Event::Empty(empty) => {
                let element = Element {
                    namespace,
                    name: String::from_utf8_lossy(empty.local_name().as_ref()).into_owned(),
                    ..Element::default()
                };
                attach(&mut stack, &mut root, element)?;
            }
            Event::End(_) => {
                let element = stack
                    .pop()
                    .ok_or_else(|| "the answer closes an element it never opened".to_string())?;
                attach(&mut stack, &mut root, element)?;
            }
            Event::Text(text) => {
                let text = text.decode().map_err(|e| not_xml(&e))?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&text);
                }
            }
            Event::CData(data) => {
                let data = data.decode().map_err(|e| not_xml(&e))?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&data);
                }
            }
            Event::GeneralRef(reference) => {
                let name = reference.decode().map_err(|e| not_xml(&e))?;
                let resolved = quick_xml::escape::unescape(&format!("&{name};"))
                    .map_err(|_| {
                        format!("the answer names an entity it does not define: &{name};")
                    })?
                    .into_owned();
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&resolved);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("the answer ends inside an element".to_string());
    }
    root.ok_or_else(|| "the answer holds no element".to_string())
}

fn attach(
    stack: &mut [Element],
    root: &mut Option<Element>,
    element: Element,
) -> Result<(), String> {
    match stack.last_mut() {
        Some(parent) => parent.children.push(element),
        None if root.is_none() => *root = Some(element),
        None => return Err("the answer holds more than one root element".to_string()),
    }
    Ok(())
}

/// One `response` of a multistatus: its `href` as the server wrote it, its
/// own status when it carries one, and the properties of every `propstat`
/// whose status is a success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavResponse {
    pub href: String,
    pub status: Option<u16>,
    pub props: Vec<Element>,
}

impl DavResponse {
    /// The property `name` in `namespace`, when the server answered it with
    /// a success.
    pub fn prop(&self, namespace: &str, name: &str) -> Option<&Element> {
        self.props.iter().find(|p| p.is(namespace, name))
    }
}

/// The status code of an HTTP status line (`HTTP/1.1 200 OK`).
pub fn status_code(line: &str) -> Option<u16> {
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Read a `207 Multi-Status` body into its responses (RFC 4918 §13).
pub fn parse_multistatus(body: &str) -> Result<Vec<DavResponse>, String> {
    let root = parse(body)?;
    if !root.is(DAV, "multistatus") {
        return Err(format!(
            "the answer is not a multistatus but <{}> in '{}'",
            root.name, root.namespace
        ));
    }
    let mut responses = Vec::new();
    for response in root.children_named(DAV, "response") {
        let href = response
            .child(DAV, "href")
            .map(|h| h.text.trim().to_string())
            .ok_or_else(|| "a response of the multistatus names no href".to_string())?;
        let status = response
            .child(DAV, "status")
            .and_then(|s| status_code(&s.text));
        let mut props = Vec::new();
        for propstat in response.children_named(DAV, "propstat") {
            let ok = propstat
                .child(DAV, "status")
                .and_then(|s| status_code(&s.text))
                .is_some_and(|code| (200..300).contains(&code));
            if !ok {
                continue;
            }
            if let Some(prop) = propstat.child(DAV, "prop") {
                props.extend(prop.children.iter().cloned());
            }
        }
        responses.push(DavResponse {
            href,
            status,
            props,
        });
    }
    Ok(responses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const MULTISTATUS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/caldav/v2/a%40example.test/user</D:href>
    <D:propstat>
      <D:prop>
        <D:current-user-principal><D:href>/caldav/v2/a%40example.test/user</D:href></D:current-user-principal>
        <cal:calendar-home-set><D:href>/caldav/v2/a%40example.test/</D:href></cal:calendar-home-set>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
    <D:propstat>
      <D:prop><D:displayname/></D:prop>
      <D:status>HTTP/1.1 404 Not Found</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

    #[test]
    fn a_multistatus_answers_its_hrefs_and_only_the_successful_properties() {
        let responses = parse_multistatus(MULTISTATUS).expect("parses");
        assert_eq!(responses.len(), 1);
        let r = &responses[0];
        assert_eq!(r.href, "/caldav/v2/a%40example.test/user");
        assert_eq!(
            r.prop(CALDAV, "calendar-home-set").and_then(Element::href),
            Some("/caldav/v2/a%40example.test/".to_string())
        );
        assert_eq!(
            r.prop(DAV, "current-user-principal")
                .and_then(Element::href),
            Some("/caldav/v2/a%40example.test/user".to_string())
        );
        assert!(
            r.prop(DAV, "displayname").is_none(),
            "a property answered 404 was read as answered"
        );
    }

    #[test]
    fn names_are_read_by_namespace_whatever_the_prefix() {
        let body = r#"<multistatus xmlns="DAV:"><response><href>/x/</href><propstat><prop><resourcetype><collection/><C:calendar xmlns:C="urn:ietf:params:xml:ns:caldav"/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;
        let responses = parse_multistatus(body).expect("parses");
        let kind = responses[0]
            .prop(DAV, "resourcetype")
            .expect("resourcetype");
        assert!(kind.has_descendant(CALDAV, "calendar"));
        assert!(kind.has_descendant(DAV, "collection"));
        assert!(!kind.has_descendant(DAV, "calendar"));
    }

    #[test]
    fn entities_and_cdata_are_text_and_a_defined_entity_is_never_expanded() {
        let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/e/a&amp;b.ics</d:href><d:propstat><d:prop><c:calendar-data><![CDATA[BEGIN:VCALENDAR
END:VCALENDAR
]]></c:calendar-data><d:getetag>&quot;1&#x32;&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        let r = &parse_multistatus(body).expect("parses")[0];
        assert_eq!(r.href, "/e/a&b.ics");
        assert_eq!(
            r.prop(DAV, "getetag").map(|e| e.text.clone()),
            Some("\"12\"".to_string())
        );
        assert_eq!(
            r.prop(CALDAV, "calendar-data").map(|e| e.text.clone()),
            Some("BEGIN:VCALENDAR\nEND:VCALENDAR\n".to_string())
        );
        let hostile = r#"<?xml version="1.0"?><!DOCTYPE d [<!ENTITY x SYSTEM "file:///etc/passwd">]><d:multistatus xmlns:d="DAV:"><d:response><d:href>&x;</d:href></d:response></d:multistatus>"#;
        let refused = parse_multistatus(hostile).expect_err("a defined entity is refused");
        assert!(refused.contains("&x;"), "{refused}");
    }

    #[test]
    fn what_is_not_a_multistatus_is_refused_by_name() {
        assert!(parse_multistatus("<html><body>Sign in</body></html>")
            .unwrap_err()
            .contains("not a multistatus"));
        assert!(parse_multistatus("not xml at all <").is_err());
        assert!(parse_multistatus("").is_err());
    }

    #[test]
    fn the_query_names_its_window_in_utc_for_the_filter_and_the_expansion() {
        let start = Utc.with_ymd_and_hms(2026, 10, 8, 9, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 10, 15, 9, 0, 0).unwrap();
        let body = calendar_query(start, end);
        let root = parse(&body).expect("the body is XML");
        assert!(root.is(CALDAV, "calendar-query"));
        let expand = root
            .child(DAV, "prop")
            .and_then(|p| p.child(CALDAV, "calendar-data"))
            .and_then(|d| d.child(CALDAV, "expand"));
        assert!(expand.is_some(), "no expand: {body}");
        assert!(body.contains(r#"<c:expand start="20261008T090000Z" end="20261015T090000Z"/>"#));
        assert!(body.contains(r#"<c:time-range start="20261008T090000Z" end="20261015T090000Z"/>"#));
        assert!(root.has_descendant(CALDAV, "time-range"));
        let propfind = parse(&principal_propfind()).expect("XML");
        let prop = propfind.child(DAV, "prop").expect("prop");
        assert!(prop.child(DAV, "current-user-principal").is_some());
        assert!(prop.child(CALDAV, "calendar-home-set").is_some());
        let listing = parse(&calendars_propfind()).expect("XML");
        let prop = listing.child(DAV, "prop").expect("prop");
        for (ns, name) in [
            (DAV, "resourcetype"),
            (DAV, "displayname"),
            (CALDAV, "calendar-description"),
            (APPLE_ICAL, "calendar-color"),
            (DAV, "current-user-privilege-set"),
        ] {
            assert!(prop.child(ns, name).is_some(), "{name} is not asked for");
        }
    }
}
