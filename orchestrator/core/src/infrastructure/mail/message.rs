// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An outgoing message (RFC 5322, RFC 2045, RFC 2047) for the outbound mail
//! tools (AEGIS ADR-125 D4, its Update of 2026-10-07 (3) clauses 13 and 14):
//! plain text only, `text/plain; charset=utf-8` in base64, with a
//! `Message-ID` the orchestrator mints as `<uuid@domain of the address>`.
//!
//! Every header value is checked or encoded here, so no argument can add a
//! header: an address is plain ASCII with no whitespace or special
//! character ([`is_address`]); a subject and a display name are written as
//! RFC 2047 encoded words when they are not printable ASCII.

use base64::{engine::general_purpose::STANDARD, Engine as _};

/// The longest an address may be (RFC 5321's path limit).
pub const ADDRESS_MAX_CHARS: usize = 254;

/// Whether `s` is an address the tools send to and from: ASCII, exactly one
/// `@` with something on each side, no whitespace, control character or
/// `<>(),;:"[]\`, at most [`ADDRESS_MAX_CHARS`] characters.
pub fn is_address(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && s.len() <= ADDRESS_MAX_CHARS
        && s.bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !b"<>(),;:\"[]\\".contains(&b))
}

/// A fresh `Message-ID` for a message from `address`: `<uuid@domain>`, the
/// domain lowercased.
pub fn mint_message_id(address: &str) -> String {
    let domain = address
        .rsplit_once('@')
        .map(|(_, d)| d)
        .unwrap_or("localhost")
        .to_ascii_lowercase();
    format!("<{}@{domain}>", uuid::Uuid::new_v4())
}

/// The message the tools write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub from_address: String,
    pub from_name: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    /// Plain text; line endings become CRLF.
    pub body: String,
    pub message_id: String,
    pub in_reply_to: Option<String>,
    /// The `References` ids, in order.
    pub references: Vec<String>,
    pub date: chrono::DateTime<chrono::Utc>,
}

impl OutgoingMessage {
    /// The message as RFC 5322 bytes, CRLF line endings.
    pub fn render(&self) -> Vec<u8> {
        let mut head: Vec<String> = Vec::new();
        let from = match &self.from_name {
            Some(name) if !name.trim().is_empty() => {
                format!("{} <{}>", phrase(name.trim()), self.from_address)
            }
            _ => self.from_address.clone(),
        };
        head.push(format!("From: {from}"));
        if !self.to.is_empty() {
            head.push(format!("To: {}", self.to.join(", ")));
        }
        if !self.cc.is_empty() {
            head.push(format!("Cc: {}", self.cc.join(", ")));
        }
        head.push(format!("Subject: {}", unstructured(&self.subject)));
        head.push(format!("Date: {}", self.date.to_rfc2822()));
        head.push(format!("Message-ID: {}", self.message_id));
        if let Some(parent) = &self.in_reply_to {
            head.push(format!("In-Reply-To: {parent}"));
        }
        if !self.references.is_empty() {
            head.push(format!("References: {}", self.references.join(" ")));
        }
        head.push("MIME-Version: 1.0".to_string());
        head.push("Content-Type: text/plain; charset=utf-8".to_string());
        head.push("Content-Transfer-Encoding: base64".to_string());

        let text = self.body.replace("\r\n", "\n").replace('\n', "\r\n");
        let encoded = STANDARD.encode(text.as_bytes());
        let mut out = head.join("\r\n");
        out.push_str("\r\n\r\n");
        for chunk in encoded.as_bytes().chunks(76) {
            out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
            out.push_str("\r\n");
        }
        out.into_bytes()
    }
}

/// Whether `s` can be written in a header as it is: printable ASCII and
/// spaces only.
fn printable(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// An unstructured header value (a subject): as it is when printable
/// ASCII, else RFC 2047 encoded words.
fn unstructured(s: &str) -> String {
    if printable(s) {
        s.to_string()
    } else {
        encoded_words(s)
    }
}

/// A display name: as it is when it is words of RFC 5322 `atext`, a quoted
/// string when other printable ASCII, else RFC 2047 encoded words.
fn phrase(s: &str) -> String {
    let atext = |c: char| c.is_ascii_alphanumeric() || " !#$%&'*+-/=?^_`{|}~".contains(c);
    if s.chars().all(atext) {
        s.to_string()
    } else if printable(s) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        encoded_words(s)
    }
}

/// `s` as `=?UTF-8?B?...?=` words of at most 75 characters each, split on
/// character boundaries and joined by folding whitespace.
fn encoded_words(s: &str) -> String {
    // 75 - len("=?UTF-8?B??=") = 63 base64 characters: 45 bytes of text.
    const MAX_BYTES: usize = 45;
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in s.chars() {
        if chunk.len() + c.len_utf8() > MAX_BYTES {
            words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(chunk.as_bytes())));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() || words.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(chunk.as_bytes())));
    }
    words.join("\r\n ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_plain_ascii_with_one_at_and_nothing_special() {
        assert!(is_address("ann@example.test"));
        assert!(is_address("a.b+tag@sub.example.test"));
        assert!(!is_address("ann"));
        assert!(!is_address("@example.test"));
        assert!(!is_address("ann@"));
        assert!(!is_address("ann@b@c"));
        assert!(!is_address("Ann <ann@example.test>"));
        assert!(!is_address("ann@example.test\r\nBcc: x@y"));
        assert!(!is_address("änn@example.test"));
        assert!(!is_address(&format!("{}@x.test", "a".repeat(250))));
    }

    #[test]
    fn a_minted_message_id_is_a_uuid_at_the_addresss_domain() {
        let id = mint_message_id("Owner@Example.TEST");
        let inner = id
            .strip_prefix('<')
            .and_then(|i| i.strip_suffix('>'))
            .unwrap();
        let (local, domain) = inner.split_once('@').unwrap();
        assert_eq!(domain, "example.test");
        assert!(uuid::Uuid::parse_str(local).is_ok(), "{id}");
        assert_ne!(id, mint_message_id("owner@example.test"));
    }

    #[test]
    fn a_non_ascii_subject_is_encoded_words_and_the_body_is_base64() {
        let message = OutgoingMessage {
            from_address: "owner@example.test".to_string(),
            from_name: Some("Jéshua".to_string()),
            to: vec!["son@example.test".to_string()],
            cc: Vec::new(),
            subject: "Je t'aime, mon fils ❤".to_string(),
            body: "Line one\nLine two".to_string(),
            message_id: "<id@example.test>".to_string(),
            in_reply_to: None,
            references: Vec::new(),
            date: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        };
        let raw = String::from_utf8(message.render()).unwrap();
        assert!(raw.is_ascii(), "{raw}");
        assert!(raw.contains("Subject: =?UTF-8?B?"), "{raw}");
        assert!(raw.contains("From: =?UTF-8?B?"), "{raw}");
        let body = raw.split_once("\r\n\r\n").unwrap().1.replace("\r\n", "");
        assert_eq!(
            String::from_utf8(STANDARD.decode(body).unwrap()).unwrap(),
            "Line one\r\nLine two"
        );
    }
}
