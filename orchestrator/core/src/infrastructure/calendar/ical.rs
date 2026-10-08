// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! iCalendar (RFC 5545), read and written in-house (AEGIS ADR-138 K2).
//!
//! - **Content lines** (§3.1): unfolded on reading (a line break followed by
//!   a space or a tab is removed), folded on writing at 75 octets without
//!   splitting a character, each continuation starting with one space.
//!   Parameters keep their order and their quoting.
//! - **Text** (§3.3.11): `\\`, `\;`, `\,` and `\n` (or `\N`) escaped and
//!   unescaped.
//! - **Components** are kept as a tree with every property in its order, so
//!   what is not read is written back unchanged; [`VEvent`] reads the event
//!   properties the calendar tools use.
//! - **Writing an event** (AEGIS ADR-138 K6, K6e): [`new_event_calendar`]
//!   builds a new `VCALENDAR` holding one `VEVENT` with its times in UTC (or
//!   `DATE` values all day); [`Component::set_property`],
//!   [`Component::remove_properties`], [`Component::bump_sequence`] and
//!   [`Component::set_partstat`] change an event read from its server and
//!   leave every other line as it was.
//! - **Times** are carried as written: a `TZID` parameter is kept with its
//!   local time, a UTC time (`Z`) and an all-day `DATE` are read as such. No
//!   time zone database and no recurrence engine: a server's `expand` gives
//!   repeating events as UTC occurrences.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};

/// The most octets of one physical line, its line break excluded (§3.1).
pub const FOLD_OCTETS: usize = 75;

/// Remove every line fold of `text` (§3.1): a CRLF, or a bare LF, followed
/// by one space or tab.
pub fn unfold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let after_break = match c {
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                true
            }
            '\n' => true,
            _ => false,
        };
        if after_break {
            if matches!(chars.peek(), Some(' ') | Some('\t')) {
                chars.next();
            } else {
                out.push_str("\r\n");
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Fold one content line at [`FOLD_OCTETS`] octets (§3.1), never inside a
/// character: the continuation lines start with one space. No line break is
/// added at the end.
pub fn fold(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + line.len() / FOLD_OCTETS * 3);
    let mut octets = 0;
    let mut limit = FOLD_OCTETS;
    for c in line.chars() {
        let width = c.len_utf8();
        if octets + width > limit {
            out.push_str("\r\n ");
            octets = 0;
            // A continuation's leading space counts toward its 75 octets.
            limit = FOLD_OCTETS - 1;
        }
        out.push(c);
        octets += width;
    }
    out
}

/// Escape a TEXT value (§3.3.11).
pub fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Unescape a TEXT value (§3.3.11). A backslash before any other character
/// is kept as written.
pub fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(e @ ('\\' | ';' | ',')) => out.push(e),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// One content line (§3.1): its name in upper case, its parameters in
/// order (each name in upper case with its values), and its value as
/// written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentLine {
    pub name: String,
    pub params: Vec<(String, Vec<String>)>,
    pub value: String,
}

impl ContentLine {
    /// A line with no parameters.
    pub fn new(name: &str, value: impl Into<String>) -> Self {
        Self {
            name: name.to_ascii_uppercase(),
            params: Vec::new(),
            value: value.into(),
        }
    }

    /// Read one unfolded line; `None` when it holds no `:` outside quotes
    /// or no name.
    pub fn parse(line: &str) -> Option<Self> {
        let mut in_quotes = false;
        let mut colon = None;
        for (i, c) in line.char_indices() {
            match c {
                '"' => in_quotes = !in_quotes,
                ':' if !in_quotes => {
                    colon = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let colon = colon?;
        let (head, value) = (&line[..colon], &line[colon + 1..]);
        let mut parts = split_unquoted(head, ';').into_iter();
        let name = parts.next()?.trim().to_ascii_uppercase();
        if name.is_empty() {
            return None;
        }
        let mut params = Vec::new();
        for part in parts {
            let (param, values) = part.split_once('=')?;
            let values = split_unquoted(values, ',')
                .into_iter()
                .map(|v| v.trim_matches('"').to_string())
                .collect();
            params.push((param.trim().to_ascii_uppercase(), values));
        }
        Some(Self {
            name,
            params,
            value: value.to_string(),
        })
    }

    /// The first value of the parameter `name`.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .and_then(|(_, values)| values.first())
            .map(String::as_str)
    }

    /// The line as written, unfolded; a parameter value holding `:`, `;` or
    /// `,` is quoted.
    pub fn to_line(&self) -> String {
        let mut line = self.name.clone();
        for (name, values) in &self.params {
            line.push(';');
            line.push_str(name);
            line.push('=');
            let values: Vec<String> = values
                .iter()
                .map(|v| {
                    if v.contains([':', ';', ',']) {
                        format!("\"{v}\"")
                    } else {
                        v.clone()
                    }
                })
                .collect();
            line.push_str(&values.join(","));
        }
        line.push(':');
        line.push_str(&self.value);
        line
    }
}

/// Split `s` at every `sep` outside double quotes.
fn split_unquoted(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c == sep && !in_quotes {
            parts.push(&s[start..i]);
            start = i + c.len_utf8();
        }
    }
    parts.push(&s[start..]);
    parts
}

/// One component (`VCALENDAR`, `VEVENT`, `VTIMEZONE`, `VALARM`, ...): its
/// name, its properties in order and the components inside it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Component {
    pub name: String,
    pub properties: Vec<ContentLine>,
    pub components: Vec<Component>,
}

impl Component {
    /// The first property `name`.
    pub fn property(&self, name: &str) -> Option<&ContentLine> {
        self.properties
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Every property `name`, in order.
    pub fn properties_named<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a ContentLine> + 'a {
        self.properties
            .iter()
            .filter(move |p| p.name.eq_ignore_ascii_case(name))
    }

    /// The `VEVENT` components directly inside this one.
    pub fn events(&self) -> impl Iterator<Item = VEvent<'_>> + '_ {
        self.components
            .iter()
            .filter(|c| c.name == "VEVENT")
            .map(VEvent)
    }
}

impl Component {
    /// Put `line` in place of the first property of its name, removing any
    /// further ones of that name; with none, add it after the others.
    pub fn set_property(&mut self, line: ContentLine) {
        match self.properties.iter().position(|p| p.name == line.name) {
            Some(first) => {
                let name = line.name.clone();
                self.properties[first] = line;
                let mut index = 0;
                self.properties.retain(|p| {
                    let keep = index <= first || p.name != name;
                    index += 1;
                    keep
                });
            }
            None => self.properties.push(line),
        }
    }

    /// Remove every property `name`.
    pub fn remove_properties(&mut self, name: &str) {
        self.properties
            .retain(|p| !p.name.eq_ignore_ascii_case(name));
    }

    /// Increment `SEQUENCE` (RFC 5545 §3.8.7.4): an absent or unreadable
    /// value counts as 0. Answers the new value.
    pub fn bump_sequence(&mut self) -> u64 {
        let next = self
            .property("SEQUENCE")
            .and_then(|p| p.value.trim().parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        self.set_property(ContentLine::new("SEQUENCE", next.to_string()));
        next
    }

    /// Set `PARTSTAT` to `partstat` on every `ATTENDEE` whose address is
    /// `address` ([`same_address`]), keeping its other parameters. Answers
    /// whether any did.
    pub fn set_partstat(&mut self, address: &str, partstat: &str) -> bool {
        let mut found = false;
        for property in self.properties.iter_mut().filter(|p| p.name == "ATTENDEE") {
            if !same_address(&Party::from_property(property).address, address) {
                continue;
            }
            found = true;
            match property
                .params
                .iter_mut()
                .find(|(name, _)| name == "PARTSTAT")
            {
                Some((_, values)) => *values = vec![partstat.to_string()],
                None => property
                    .params
                    .push(("PARTSTAT".to_string(), vec![partstat.to_string()])),
            }
        }
        found
    }
}

/// Whether two calendar addresses are the same: compared without case,
/// `mailto:` removed from either.
pub fn same_address(a: &str, b: &str) -> bool {
    fn bare(s: &str) -> &str {
        let s = s.trim();
        match s.get(..7) {
            Some(scheme) if scheme.eq_ignore_ascii_case("mailto:") => &s[7..],
            _ => s,
        }
    }
    bare(a).eq_ignore_ascii_case(bare(b))
}

/// The `PRODID` of every calendar object the tools write.
pub const PRODID: &str = "-//100monkeys.ai//AEGIS calendar tools//EN";

/// A time the tools write: an instant, written in UTC, or an all-day date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTime {
    At(DateTime<Utc>),
    Date(NaiveDate),
}

impl EventTime {
    /// The property `name` (`DTSTART`, `DTEND`) holding this time:
    /// `<name>:YYYYMMDDTHHMMSSZ`, or `<name>;VALUE=DATE:YYYYMMDD`.
    pub fn line(&self, name: &str) -> ContentLine {
        match self {
            EventTime::At(at) => ContentLine::new(name, utc_value(*at)),
            EventTime::Date(date) => ContentLine {
                name: name.to_ascii_uppercase(),
                params: vec![("VALUE".to_string(), vec!["DATE".to_string()])],
                value: date.format("%Y%m%d").to_string(),
            },
        }
    }
}

/// An instant as a UTC DATE-TIME value, `YYYYMMDDTHHMMSSZ`.
pub fn utc_value(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

/// `ORGANIZER:mailto:<address>`.
pub fn organizer_line(address: &str) -> ContentLine {
    ContentLine::new("ORGANIZER", format!("mailto:{address}"))
}

/// `ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:<address>`: an invited
/// attendee who has not answered.
pub fn attendee_line(address: &str) -> ContentLine {
    ContentLine {
        name: "ATTENDEE".to_string(),
        params: vec![
            ("PARTSTAT".to_string(), vec!["NEEDS-ACTION".to_string()]),
            ("RSVP".to_string(), vec!["TRUE".to_string()]),
        ],
        value: format!("mailto:{address}"),
    }
}

/// A new event as `calendar.create` writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub uid: String,
    pub stamp: DateTime<Utc>,
    pub start: EventTime,
    pub end: EventTime,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    /// The organiser's address, written only with attendees.
    pub organizer: String,
    pub attendees: Vec<String>,
}

/// A new `VCALENDAR` holding `event` as its one `VEVENT`: `UID`,
/// `DTSTAMP`, `DTSTART` and `DTEND` (UTC, or `DATE` all day), `SUMMARY`,
/// `DESCRIPTION` and `LOCATION` when given, `SEQUENCE:0`, and with
/// attendees the `ORGANIZER` and each `ATTENDEE` invited and unanswered.
pub fn new_event_calendar(event: &NewEvent) -> Component {
    let mut properties = vec![
        ContentLine::new("UID", event.uid.clone()),
        ContentLine::new("DTSTAMP", utc_value(event.stamp)),
        event.start.line("DTSTART"),
        event.end.line("DTEND"),
        ContentLine::new("SUMMARY", escape_text(&event.title)),
    ];
    if let Some(description) = &event.description {
        properties.push(ContentLine::new("DESCRIPTION", escape_text(description)));
    }
    if let Some(location) = &event.location {
        properties.push(ContentLine::new("LOCATION", escape_text(location)));
    }
    properties.push(ContentLine::new("SEQUENCE", "0"));
    if !event.attendees.is_empty() {
        properties.push(organizer_line(&event.organizer));
        properties.extend(event.attendees.iter().map(|a| attendee_line(a)));
    }
    Component {
        name: "VCALENDAR".to_string(),
        properties: vec![
            ContentLine::new("VERSION", "2.0"),
            ContentLine::new("PRODID", PRODID),
        ],
        components: vec![Component {
            name: "VEVENT".to_string(),
            properties,
            components: Vec::new(),
        }],
    }
}

/// Read an iCalendar object into its outermost component.
pub fn parse_calendar(text: &str) -> Result<Component, String> {
    let unfolded = unfold(text);
    let mut stack: Vec<Component> = Vec::new();
    let mut root: Option<Component> = None;
    for raw in unfolded.split("\r\n").flat_map(|l| l.split('\n')) {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let content = ContentLine::parse(line)
            .ok_or_else(|| format!("'{}' is not an iCalendar content line", shown(line)))?;
        match content.name.as_str() {
            "BEGIN" => stack.push(Component {
                name: content.value.trim().to_ascii_uppercase(),
                ..Component::default()
            }),
            "END" => {
                let component = stack
                    .pop()
                    .ok_or_else(|| format!("END:{} closes nothing", content.value))?;
                if !component.name.eq_ignore_ascii_case(content.value.trim()) {
                    return Err(format!(
                        "END:{} closes BEGIN:{}",
                        content.value, component.name
                    ));
                }
                match stack.last_mut() {
                    Some(parent) => parent.components.push(component),
                    None if root.is_none() => root = Some(component),
                    None => return Err("more than one outermost component".to_string()),
                }
            }
            _ => stack
                .last_mut()
                .ok_or_else(|| format!("'{}' is outside any component", content.name))?
                .properties
                .push(content),
        }
    }
    if let Some(open) = stack.last() {
        return Err(format!("BEGIN:{} is never closed", open.name));
    }
    root.ok_or_else(|| "no component".to_string())
}

/// Write `component` as iCalendar: every line folded, each ended by CRLF.
pub fn write_component(component: &Component) -> String {
    let mut out = String::new();
    write_into(component, &mut out);
    out
}

fn write_into(component: &Component, out: &mut String) {
    out.push_str(&fold(&format!("BEGIN:{}", component.name)));
    out.push_str("\r\n");
    for property in &component.properties {
        out.push_str(&fold(&property.to_line()));
        out.push_str("\r\n");
    }
    for inner in &component.components {
        write_into(inner, out);
    }
    out.push_str(&fold(&format!("END:{}", component.name)));
    out.push_str("\r\n");
}

/// At most the first 80 characters of `line`, for an error.
fn shown(line: &str) -> String {
    line.chars().filter(|c| !c.is_control()).take(80).collect()
}

/// A date or a date-time as the property wrote it: its value, the `TZID`
/// it was given in, and whether it is an all-day `DATE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcalTime {
    pub value: String,
    pub tzid: Option<String>,
    pub all_day: bool,
}

impl IcalTime {
    /// Read `DTSTART`, `DTEND`, `RECURRENCE-ID` or any other date-time
    /// property; `None` when its value is neither form.
    pub fn from_property(property: &ContentLine) -> Option<Self> {
        let value = property.value.trim().to_string();
        let all_day = property
            .param("VALUE")
            .is_some_and(|v| v.eq_ignore_ascii_case("DATE"))
            || (value.len() == 8 && !value.contains('T'));
        let time = Self {
            value,
            tzid: property.param("TZID").map(str::to_string),
            all_day,
        };
        if time.all_day {
            time.date()?;
        } else {
            time.local()?;
        }
        Some(time)
    }

    /// Whether the value is a UTC time (`...Z`).
    pub fn is_utc(&self) -> bool {
        !self.all_day && self.value.ends_with('Z')
    }

    /// An all-day value's date.
    pub fn date(&self) -> Option<NaiveDate> {
        NaiveDate::parse_from_str(&self.value, "%Y%m%d").ok()
    }

    /// The date and time as written, without its zone.
    pub fn local(&self) -> Option<NaiveDateTime> {
        NaiveDateTime::parse_from_str(self.value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()
    }

    /// A UTC value as an instant; `None` for an all-day, floating or
    /// `TZID` value.
    pub fn utc(&self) -> Option<DateTime<Utc>> {
        self.is_utc()
            .then(|| self.local())
            .flatten()
            .map(|t| Utc.from_utc_datetime(&t))
    }

    /// The value in RFC 3339 form: `YYYY-MM-DD` for an all-day value,
    /// `YYYY-MM-DDTHH:MM:SSZ` for a UTC one, and the local time without an
    /// offset for a floating or `TZID` one (its zone is [`Self::tzid`]).
    pub fn rfc3339(&self) -> Option<String> {
        if self.all_day {
            return self.date().map(|d| d.format("%Y-%m-%d").to_string());
        }
        let local = self.local()?.format("%Y-%m-%dT%H:%M:%S").to_string();
        Some(if self.is_utc() {
            format!("{local}Z")
        } else {
            local
        })
    }
}

/// A calendar user an event names (`ORGANIZER`, `ATTENDEE`): the address
/// without `mailto:`, the `CN`, and the `PARTSTAT` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Party {
    pub address: String,
    pub name: Option<String>,
    pub answer: Option<String>,
}

impl Party {
    pub fn from_property(property: &ContentLine) -> Self {
        let value = property.value.trim();
        let address = match value.get(..7) {
            Some(scheme) if scheme.eq_ignore_ascii_case("mailto:") => &value[7..],
            _ => value,
        };
        Self {
            address: address.to_string(),
            name: property.param("CN").map(str::to_string),
            answer: property.param("PARTSTAT").map(str::to_string),
        }
    }
}

/// A `VEVENT` read for the properties the calendar tools answer.
#[derive(Debug, Clone, Copy)]
pub struct VEvent<'a>(pub &'a Component);

impl<'a> VEvent<'a> {
    /// A TEXT property, unescaped.
    pub fn text(&self, name: &str) -> Option<String> {
        self.0.property(name).map(|p| unescape_text(&p.value))
    }

    pub fn uid(&self) -> Option<String> {
        self.0.property("UID").map(|p| p.value.trim().to_string())
    }

    pub fn summary(&self) -> Option<String> {
        self.text("SUMMARY")
    }

    pub fn location(&self) -> Option<String> {
        self.text("LOCATION")
    }

    pub fn description(&self) -> Option<String> {
        self.text("DESCRIPTION")
    }

    pub fn status(&self) -> Option<String> {
        self.0
            .property("STATUS")
            .map(|p| p.value.trim().to_ascii_uppercase())
    }

    pub fn start(&self) -> Option<IcalTime> {
        self.0.property("DTSTART").and_then(IcalTime::from_property)
    }

    pub fn end(&self) -> Option<IcalTime> {
        self.0.property("DTEND").and_then(IcalTime::from_property)
    }

    pub fn recurrence_id(&self) -> Option<IcalTime> {
        self.0
            .property("RECURRENCE-ID")
            .and_then(IcalTime::from_property)
    }

    /// Whether the event repeats: it carries a rule or dates of its own, or
    /// it is one occurrence of a repeating event.
    pub fn repeats(&self) -> bool {
        ["RRULE", "RDATE", "RECURRENCE-ID"]
            .iter()
            .any(|name| self.0.property(name).is_some())
    }

    pub fn organizer(&self) -> Option<Party> {
        self.0.property("ORGANIZER").map(Party::from_property)
    }

    pub fn attendees(&self) -> Vec<Party> {
        self.0
            .properties_named("ATTENDEE")
            .map(Party::from_property)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Example//EN\r\nBEGIN:VEVENT\r\nUID:abc-123@example.test\r\nDTSTART;TZID=Europe/Berlin:20261009T100000\r\nDTEND;TZID=Europe/Berlin:20261009T110000\r\nSUMMARY:Board review\\, Q4\\; budget\r\nLOCATION:Room 4\r\nDESCRIPTION:Line one\\nLine two with a long tail that runs past the seventy-f\r\n ive octet limit\r\nORGANIZER;CN=\"Doe, Jane\":mailto:jane@example.test\r\nATTENDEE;CN=Sam;PARTSTAT=ACCEPTED:MAILTO:sam@example.test\r\nATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:a@example.test\r\nRRULE:FREQ=WEEKLY\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn an_event_is_read_with_its_times_as_written_its_text_unescaped_and_its_parties() {
        let calendar = parse_calendar(EVENT).expect("parses");
        let event = calendar.events().next().expect("one event");
        assert_eq!(event.uid().as_deref(), Some("abc-123@example.test"));
        assert_eq!(event.summary().as_deref(), Some("Board review, Q4; budget"));
        assert_eq!(
            event.description().as_deref(),
            Some("Line one\nLine two with a long tail that runs past the seventy-five octet limit")
        );
        let start = event.start().expect("start");
        assert_eq!(start.tzid.as_deref(), Some("Europe/Berlin"));
        assert_eq!(start.rfc3339().as_deref(), Some("2026-10-09T10:00:00"));
        assert_eq!(start.utc(), None, "a TZID time is not read as UTC");
        assert!(event.repeats());
        assert_eq!(
            event.organizer(),
            Some(Party {
                address: "jane@example.test".to_string(),
                name: Some("Doe, Jane".to_string()),
                answer: None,
            })
        );
        let attendees = event.attendees();
        assert_eq!(attendees.len(), 2);
        assert_eq!(attendees[0].address, "sam@example.test");
        assert_eq!(attendees[0].answer.as_deref(), Some("ACCEPTED"));
        assert_eq!(attendees[1].answer.as_deref(), Some("NEEDS-ACTION"));
        assert_eq!(event.0.components[0].name, "VALARM");
    }

    #[test]
    fn utc_and_all_day_values_are_read_as_such() {
        let utc = IcalTime::from_property(&ContentLine::parse("DTSTART:20261008T090000Z").unwrap())
            .unwrap();
        assert_eq!(utc.rfc3339().as_deref(), Some("2026-10-08T09:00:00Z"));
        assert_eq!(
            utc.utc().map(|t| t.to_rfc3339()),
            Some("2026-10-08T09:00:00+00:00".to_string())
        );
        let day =
            IcalTime::from_property(&ContentLine::parse("DTSTART;VALUE=DATE:20261224").unwrap())
                .unwrap();
        assert!(day.all_day);
        assert_eq!(day.rfc3339().as_deref(), Some("2026-12-24"));
        assert!(
            IcalTime::from_property(&ContentLine::parse("DTSTART:tomorrow").unwrap()).is_none()
        );
    }

    #[test]
    fn folding_keeps_every_line_within_75_octets_and_unfolding_restores_it() {
        let line = format!(
            "DESCRIPTION:{}",
            "Grüße — ünïcödé text, folded across many lines; ".repeat(9)
        );
        let folded = fold(&line);
        for physical in folded.split("\r\n") {
            assert!(
                physical.len() <= FOLD_OCTETS,
                "a physical line is {} octets: {physical:?}",
                physical.len()
            );
        }
        assert!(folded.contains("\r\n "), "a long line was not folded");
        assert_eq!(unfold(&folded), line, "unfolding did not restore the line");
        assert_eq!(fold("SHORT:line"), "SHORT:line");
    }

    #[test]
    fn text_escaping_round_trips() {
        for value in [
            "plain",
            "comma, semicolon; backslash \\ newline\nend",
            "\\n is not a newline here, \\; nor this",
            "",
            "trailing backslash \\",
        ] {
            assert_eq!(unescape_text(&escape_text(value)), value, "{value:?}");
        }
        assert_eq!(
            escape_text("a,b;c\\d\ne"),
            "a\\,b\\;c\\\\d\\ne",
            "a comma, semicolon, backslash or newline is not escaped"
        );
        assert_eq!(unescape_text("x\\Ny"), "x\ny");
    }

    #[test]
    fn a_calendar_written_and_read_again_is_the_same_calendar() {
        let calendar = parse_calendar(EVENT).expect("parses");
        let written = write_component(&calendar);
        assert!(written.ends_with("END:VCALENDAR\r\n"));
        for physical in written.split("\r\n") {
            assert!(
                physical.len() <= FOLD_OCTETS,
                "a written line is longer than 75 octets: {physical:?}"
            );
        }
        assert_eq!(parse_calendar(&written).expect("reads back"), calendar);

        let mut built = Component {
            name: "VCALENDAR".to_string(),
            ..Component::default()
        };
        let summary = "Ünïcödé; long, with \\ and a newline\nsecond line ".repeat(4);
        built.components.push(Component {
            name: "VEVENT".to_string(),
            properties: vec![
                ContentLine::new("UID", "x@example.test"),
                ContentLine::new("SUMMARY", escape_text(&summary)),
                ContentLine {
                    name: "ATTENDEE".to_string(),
                    params: vec![("CN".to_string(), vec!["Doe, Jane: Esq".to_string()])],
                    value: "mailto:jane@example.test".to_string(),
                },
            ],
            components: Vec::new(),
        });
        let read = parse_calendar(&write_component(&built)).expect("reads back");
        assert_eq!(read, built);
        let event = read.events().next().unwrap();
        assert_eq!(event.summary().as_deref(), Some(summary.as_str()));
        assert_eq!(event.attendees()[0].name.as_deref(), Some("Doe, Jane: Esq"));
    }

    #[test]
    fn a_broken_object_is_refused_by_name() {
        assert!(
            parse_calendar("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nEND:VCALENDAR\r\n")
                .unwrap_err()
                .contains("closes BEGIN:VEVENT")
        );
        assert!(parse_calendar("BEGIN:VCALENDAR\r\n")
            .unwrap_err()
            .contains("never closed"));
        assert!(parse_calendar("no colon here").is_err());
    }

    #[test]
    fn a_new_event_is_written_in_utc_with_its_organiser_and_unanswered_attendees() {
        let stamp = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
        let event = NewEvent {
            uid: "u-1@example.test".to_string(),
            stamp,
            start: EventTime::At(Utc.with_ymd_and_hms(2026, 10, 9, 8, 0, 0).unwrap()),
            end: EventTime::At(Utc.with_ymd_and_hms(2026, 10, 9, 9, 0, 0).unwrap()),
            title: "Plan, review; go".to_string(),
            description: Some("Two\nlines".to_string()),
            location: None,
            organizer: "me@example.test".to_string(),
            attendees: vec!["ann@example.test".to_string()],
        };
        let written = write_component(&new_event_calendar(&event));
        let mut wrong = Vec::new();
        for line in [
            "PRODID:-//100monkeys.ai//AEGIS calendar tools//EN",
            "UID:u-1@example.test",
            "DTSTAMP:20261008T120000Z",
            "DTSTART:20261009T080000Z",
            "DTEND:20261009T090000Z",
            "SUMMARY:Plan\\, review\\; go",
            "SEQUENCE:0",
            "ORGANIZER:mailto:me@example.test",
            "ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:ann@example.test",
        ] {
            if !written.split("\r\n").any(|l| l == line) {
                wrong.push(format!("no line {line:?}"));
            }
        }
        if written.contains("LOCATION") {
            wrong.push("a LOCATION was written without one".to_string());
        }
        let alone = NewEvent {
            attendees: Vec::new(),
            start: EventTime::Date(NaiveDate::from_ymd_opt(2026, 10, 9).unwrap()),
            end: EventTime::Date(NaiveDate::from_ymd_opt(2026, 10, 10).unwrap()),
            ..event
        };
        let written = write_component(&new_event_calendar(&alone));
        if !written.contains("\r\nDTSTART;VALUE=DATE:20261009\r\n")
            || !written.contains("\r\nDTEND;VALUE=DATE:20261010\r\n")
        {
            wrong.push(format!(
                "an all-day event is not written as dates: {written}"
            ));
        }
        if written.contains("ORGANIZER") || written.contains("ATTENDEE") {
            wrong.push("an event without attendees names an organiser".to_string());
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn setting_a_property_changes_only_its_line_and_sequence_counts_up() {
        let mut calendar = parse_calendar(EVENT).expect("parses");
        let before = calendar.components[0].clone();
        let event = &mut calendar.components[0];
        event.set_property(ContentLine::new("SUMMARY", "New"));
        event.set_property(ContentLine::new("X-NEW", "1"));
        let mut wrong = Vec::new();
        if event.bump_sequence() != 1 || event.bump_sequence() != 2 {
            wrong.push("SEQUENCE did not count up from none".to_string());
        }
        let changed: Vec<String> = event
            .properties
            .iter()
            .filter(|p| !before.properties.contains(p))
            .map(ContentLine::to_line)
            .collect();
        if changed != ["SUMMARY:New", "X-NEW:1", "SEQUENCE:2"] {
            wrong.push(format!("the changed lines are {changed:?}"));
        }
        if event.properties.iter().position(|p| p.name == "SUMMARY")
            != before.properties.iter().position(|p| p.name == "SUMMARY")
        {
            wrong.push("SUMMARY moved".to_string());
        }
        if event.components != before.components {
            wrong.push("the alarm changed".to_string());
        }
        event.remove_properties("rrule");
        if event.property("RRULE").is_some() {
            wrong.push("RRULE was not removed".to_string());
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn partstat_is_set_on_the_attendee_of_that_address_only() {
        let mut calendar = parse_calendar(EVENT).expect("parses");
        let event = &mut calendar.components[0];
        let mut wrong = Vec::new();
        if !event.set_partstat("A@Example.Test", "ACCEPTED") {
            wrong.push("the attendee was not found without case".to_string());
        }
        if event.set_partstat("nobody@example.test", "ACCEPTED") {
            wrong.push("an address that attends nothing was found".to_string());
        }
        let lines: Vec<String> = event
            .properties_named("ATTENDEE")
            .map(ContentLine::to_line)
            .collect();
        if lines
            != [
                "ATTENDEE;CN=Sam;PARTSTAT=ACCEPTED:MAILTO:sam@example.test",
                "ATTENDEE;PARTSTAT=ACCEPTED;RSVP=TRUE:mailto:a@example.test",
            ]
        {
            wrong.push(format!("the attendees read {lines:?}"));
        }
        if !same_address("MAILTO:sam@example.test", "Sam@Example.test") {
            wrong.push("mailto: and case are not ignored".to_string());
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
