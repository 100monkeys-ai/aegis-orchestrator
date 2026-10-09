// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An outgoing message (RFC 5322, RFC 2045, RFC 2047) for the outbound mail
//! tools (AEGIS ADR-125 D4, its Update of 2026-10-07 (3) clauses 13 and 14):
//! the text as `text/plain; charset=utf-8` in base64, with a `Message-ID`
//! the orchestrator mints as `<uuid@domain of the address>`. A message
//! carrying files (the Update of 2026-10-08 (5) clause 32) is
//! `multipart/mixed`: the text first, then each file in base64 with its
//! sniffed type and its name as an RFC 2231 `filename`. A forward (clause
//! 34) is `multipart/mixed` too: the note, then each forwarded message byte
//! for byte as a `message/rfc822` part, then the files. A message with no
//! file and nothing forwarded is written exactly as before.
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

/// A file a message carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    /// The file's name, written as an RFC 2231 `filename`.
    pub name: String,
    /// The type sniffed from the bytes; `application/octet-stream` when
    /// nothing is recognised.
    pub content_type: String,
    pub data: Vec<u8>,
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
    /// The files after the text and the forwarded messages, in order;
    /// none, with nothing forwarded, makes a `text/plain` message.
    pub attachments: Vec<OutgoingAttachment>,
    /// Whole messages forwarded after the text, each written byte for byte
    /// as a `message/rfc822` part, in order.
    pub forwarded: Vec<Vec<u8>>,
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

        let text = self.body.replace("\r\n", "\n").replace('\n', "\r\n");
        let mut out = head.join("\r\n");
        out.push_str("\r\n");
        if self.attachments.is_empty() && self.forwarded.is_empty() {
            out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
            out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
            push_base64(&mut out, text.as_bytes());
            return out.into_bytes();
        }
        // Base64 lines hold no `_`, so this boundary cannot occur in a part
        // the orchestrator encodes; a forwarded message is written as it is,
        // so a boundary one happens to hold is drawn again.
        let boundary = loop {
            let boundary = format!("=_aegis_{}", uuid::Uuid::new_v4().simple());
            let delimiter = format!("--{boundary}");
            if !self
                .forwarded
                .iter()
                .any(|m| contains(m, delimiter.as_bytes()))
            {
                break boundary;
            }
        };
        out.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n"
        ));
        if self.forwarded.iter().any(|m| !m.is_ascii()) {
            out.push_str("Content-Transfer-Encoding: 8bit\r\n");
        }
        out.push_str("\r\n");
        out.push_str(&format!("--{boundary}\r\n"));
        out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
        out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        push_base64(&mut out, text.as_bytes());
        let mut out = out.into_bytes();
        for message in &self.forwarded {
            let encoding = if message.is_ascii() { "7bit" } else { "8bit" };
            out.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Type: message/rfc822\r\nContent-Transfer-Encoding: {encoding}\r\n\r\n"
                )
                .as_bytes(),
            );
            out.extend_from_slice(message);
            // The CRLF before a boundary belongs to the boundary (RFC 2046),
            // so the part's content is the message's bytes exactly.
            out.extend_from_slice(b"\r\n");
        }
        for attachment in &self.attachments {
            let mut part = format!("--{boundary}\r\n");
            part.push_str(&format!(
                "Content-Type: {}\r\n",
                media_type(&attachment.content_type)
            ));
            part.push_str(&format!(
                "Content-Disposition: attachment;{}\r\n",
                rfc2231_filename(&attachment.name)
            ));
            part.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
            push_base64(&mut part, &attachment.data);
            out.extend_from_slice(part.as_bytes());
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }
}

/// Whether `needle` occurs in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `data` in base64, in lines of 76 characters, each ended by CRLF.
fn push_base64(out: &mut String, data: &[u8]) {
    let encoded = STANDARD.encode(data);
    for chunk in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push_str("\r\n");
    }
}

/// A media type as a header carries it: `type/subtype` of RFC 2045 token
/// characters, else `application/octet-stream`.
fn media_type(s: &str) -> &str {
    let token = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    };
    match s.split_once('/') {
        Some((kind, sub)) if token(kind) && token(sub) => s,
        _ => "application/octet-stream",
    }
}

/// The longest run of encoded characters one `filename*` segment carries.
const FILENAME_SEGMENT_CHARS: usize = 60;

/// `name` as the RFC 2231 `filename` parameter of a `Content-Disposition`,
/// with its leading space: UTF-8, every byte but an `attr-char`
/// percent-encoded, as one `filename*=` or, when long, as continuations
/// `filename*0*=`, `filename*1*=`, ... each folded onto a line of its own.
fn rfc2231_filename(name: &str) -> String {
    let attr_char = |b: u8| b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b);
    let mut pieces: Vec<String> = Vec::new();
    for b in name.bytes() {
        if attr_char(b) {
            pieces.push((b as char).to_string());
        } else {
            pieces.push(format!("%{b:02X}"));
        }
    }
    let mut segments: Vec<String> = Vec::new();
    let mut segment = String::new();
    for piece in pieces {
        if segment.len() + piece.len() > FILENAME_SEGMENT_CHARS {
            segments.push(std::mem::take(&mut segment));
        }
        segment.push_str(&piece);
    }
    segments.push(segment);
    if segments.len() == 1 {
        return format!(" filename*=utf-8''{}", segments[0]);
    }
    segments
        .iter()
        .enumerate()
        .map(|(i, segment)| {
            let charset = if i == 0 { "utf-8''" } else { "" };
            format!("\r\n filename*{i}*={charset}{segment}")
        })
        .collect::<Vec<_>>()
        .join(";")
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
            attachments: Vec::new(),
            forwarded: Vec::new(),
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

    fn message(attachments: Vec<OutgoingAttachment>) -> OutgoingMessage {
        OutgoingMessage {
            from_address: "owner@example.test".to_string(),
            from_name: Some("Mailbox Owner".to_string()),
            to: vec!["ann@example.test".to_string()],
            cc: vec!["bob@example.test".to_string()],
            subject: "Invoice".to_string(),
            body: "Paid.\nThanks.".to_string(),
            message_id: "<id@example.test>".to_string(),
            in_reply_to: Some("<parent@example.test>".to_string()),
            references: vec![
                "<root@example.test>".to_string(),
                "<parent@example.test>".to_string(),
            ],
            date: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            attachments,
            forwarded: Vec::new(),
        }
    }

    /// A header's value in `block` (a part's or the message's head),
    /// unfolded.
    fn header(block: &str, name: &str) -> Option<String> {
        let head = block.split("\r\n\r\n").next().unwrap_or_default();
        head.replace("\r\n ", " ").split("\r\n").find_map(|line| {
            let (n, v) = line.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }

    #[test]
    fn a_message_with_no_file_is_written_exactly_as_a_plain_text_message() {
        let raw = String::from_utf8(message(Vec::new()).render()).unwrap();
        let expected = "From: Mailbox Owner <owner@example.test>\r\n\
To: ann@example.test\r\n\
Cc: bob@example.test\r\n\
Subject: Invoice\r\n\
Date: Thu, 1 Jan 1970 00:00:00 +0000\r\n\
Message-ID: <id@example.test>\r\n\
In-Reply-To: <parent@example.test>\r\n\
References: <root@example.test> <parent@example.test>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
UGFpZC4NClRoYW5rcy4=\r\n";
        assert_eq!(raw, expected, "a message with no file changed its bytes");
    }

    #[test]
    fn a_message_with_two_files_is_multipart_mixed_with_the_text_first_then_each_file() {
        let pdf = b"%PDF-1.4 a small document".to_vec();
        let png: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
        let raw = String::from_utf8(
            message(vec![
                OutgoingAttachment {
                    name: "report.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    data: pdf.clone(),
                },
                OutgoingAttachment {
                    name: "été photo.png".to_string(),
                    content_type: "image/png".to_string(),
                    data: png.clone(),
                },
            ])
            .render(),
        )
        .unwrap();
        let mut wrong = Vec::new();
        if !raw.is_ascii() {
            wrong.push("the message is not ASCII".to_string());
        }
        let content_type = header(&raw, "Content-Type").unwrap_or_default();
        let boundary = content_type
            .strip_prefix("multipart/mixed; boundary=\"")
            .and_then(|b| b.strip_suffix('"'))
            .unwrap_or_default()
            .to_string();
        if boundary.is_empty() {
            wrong.push(format!(
                "the message is not multipart/mixed: {content_type:?}"
            ));
        }
        if header(&raw, "Message-ID").as_deref() != Some("<id@example.test>")
            || header(&raw, "In-Reply-To").as_deref() != Some("<parent@example.test>")
        {
            wrong.push("the message's own headers are not the call's".to_string());
        }
        let body = raw
            .split_once("\r\n\r\n")
            .map(|(_, b)| b)
            .unwrap_or_default();
        let closing = format!("--{boundary}--\r\n");
        if !body.ends_with(&closing) {
            wrong.push("the multipart body is not closed by its boundary".to_string());
        }
        let parts: Vec<&str> = body
            .trim_end_matches(&closing)
            .split(&format!("--{boundary}\r\n"))
            .skip(1)
            .collect();
        let decoded = |part: &str| {
            let b64 = part
                .split_once("\r\n\r\n")
                .map(|(_, b)| b)
                .unwrap_or_default();
            STANDARD.decode(b64.replace("\r\n", "")).unwrap_or_default()
        };
        match parts.as_slice() {
            [text, first, second] => {
                if header(text, "Content-Type").as_deref() != Some("text/plain; charset=utf-8")
                    || decoded(text) != b"Paid.\r\nThanks."
                {
                    wrong.push(format!("the first part is not the text: {text:?}"));
                }
                for (part, kind, disposition, data) in [
                    (
                        first,
                        "application/pdf",
                        "attachment; filename*=utf-8''report.pdf",
                        &pdf,
                    ),
                    (
                        second,
                        "image/png",
                        "attachment; filename*=utf-8''%C3%A9t%C3%A9%20photo.png",
                        &png,
                    ),
                ] {
                    if header(part, "Content-Type").as_deref() != Some(kind)
                        || header(part, "Content-Disposition").as_deref() != Some(disposition)
                        || header(part, "Content-Transfer-Encoding").as_deref() != Some("base64")
                    {
                        wrong.push(format!(
                            "a file's part is not {kind} named by RFC 2231: {:?}",
                            part.split("\r\n\r\n").next().unwrap_or_default()
                        ));
                    }
                    if &decoded(part) != data {
                        wrong.push(format!("the {kind} file did not survive its base64"));
                    }
                }
            }
            other => wrong.push(format!(
                "the message holds {} parts, not the text and two files",
                other.len()
            )),
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// The parts of a rendered `multipart/mixed` message, as bytes: each
    /// part's head and its content exactly as written between boundaries.
    fn byte_parts(raw: &[u8]) -> (String, Vec<(String, Vec<u8>)>) {
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a head");
        let head = String::from_utf8(raw[..split].to_vec()).unwrap();
        let boundary = header(&head, "Content-Type")
            .and_then(|c| {
                c.strip_prefix("multipart/mixed; boundary=\"")
                    .and_then(|b| b.strip_suffix('"'))
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let mut body = raw[split + 4..].to_vec();
        let closing = format!("\r\n--{boundary}--\r\n").into_bytes();
        assert!(body.ends_with(&closing), "the message is not closed");
        body.truncate(body.len() - closing.len());
        let opening = format!("--{boundary}\r\n").into_bytes();
        assert!(body.starts_with(&opening), "the message does not open");
        let body = &body[opening.len()..];
        let between = format!("\r\n--{boundary}\r\n").into_bytes();
        let mut parts = Vec::new();
        let mut rest = body;
        loop {
            let at = rest.windows(between.len()).position(|w| w == between);
            let part = &rest[..at.unwrap_or(rest.len())];
            let cut = part
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .unwrap_or(part.len());
            parts.push((
                String::from_utf8_lossy(&part[..cut]).to_string(),
                part.get(cut + 4..).unwrap_or_default().to_vec(),
            ));
            match at {
                Some(at) => rest = &rest[at + between.len()..],
                None => break,
            }
        }
        (head, parts)
    }

    /// A forward (clause 34) is the note, then each forwarded message byte
    /// for byte as a `message/rfc822` part (7bit, or 8bit for one holding a
    /// byte that is not ASCII, even one that is not UTF-8), then the files;
    /// a message holding an 8bit part says so at the top, and a line that
    /// looks like the start of a boundary is carried as it is.
    #[test]
    fn a_forward_is_the_note_then_each_message_byte_for_byte_then_the_files() {
        let first =
            b"Message-ID: <f1@x>\r\nSubject: Plans\r\n\r\n.a dot line\r\n--=_aegis_x\r\nAnn\r\n"
                .to_vec();
        let second = b"Message-ID: <f2@x>\r\nSubject: Caf\xe9\r\n\r\nLatin-1: caf\xe9".to_vec();
        let pdf = b"%PDF-1.4 a small document".to_vec();
        let mut forward = message(vec![OutgoingAttachment {
            name: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            data: pdf.clone(),
        }]);
        forward.in_reply_to = None;
        forward.references = Vec::new();
        forward.forwarded = vec![first.clone(), second.clone()];
        let raw = forward.render();
        let (head, parts) = byte_parts(&raw);
        let mut wrong = Vec::new();
        if header(&head, "Content-Transfer-Encoding").as_deref() != Some("8bit")
            || header(&head, "In-Reply-To").is_some()
        {
            wrong.push(format!("the forward's head is {head:?}"));
        }
        match parts.as_slice() {
            [(note, note_body), (a, a_body), (b, b_body), (file, file_body)] => {
                if header(note, "Content-Type").as_deref() != Some("text/plain; charset=utf-8")
                    || STANDARD
                        .decode(String::from_utf8_lossy(note_body).replace("\r\n", ""))
                        .unwrap_or_default()
                        != b"Paid.\r\nThanks."
                {
                    wrong.push(format!("the first part is not the note: {note:?}"));
                }
                for (part, content, original, encoding) in
                    [(a, a_body, &first, "7bit"), (b, b_body, &second, "8bit")]
                {
                    if header(part, "Content-Type").as_deref() != Some("message/rfc822")
                        || header(part, "Content-Transfer-Encoding").as_deref() != Some(encoding)
                        || content != original
                    {
                        wrong.push(format!(
                            "a forwarded part is not its message byte for byte: {part:?} {:?}",
                            String::from_utf8_lossy(content)
                        ));
                    }
                }
                if header(file, "Content-Type").as_deref() != Some("application/pdf")
                    || STANDARD
                        .decode(String::from_utf8_lossy(file_body).replace("\r\n", ""))
                        .unwrap_or_default()
                        != pdf
                {
                    wrong.push(format!("the last part is not the file: {file:?}"));
                }
            }
            other => wrong.push(format!(
                "the forward holds {} parts, not the note, two messages and the file",
                other.len()
            )),
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn a_files_name_is_percent_encoded_and_a_long_one_continued_so_no_name_writes_a_header() {
        assert_eq!(
            rfc2231_filename("a\"b\r\nBcc: x@y.pdf"),
            " filename*=utf-8''a%22b%0D%0ABcc%3A%20x%40y.pdf"
        );
        let long = format!("{}.txt", "n".repeat(130));
        let written = rfc2231_filename(&long);
        let segments: Vec<&str> = written.split(";\r\n ").collect();
        assert!(
            written.starts_with("\r\n filename*0*=utf-8''nnn")
                && segments.len() == 3
                && segments[1].starts_with("filename*1*=nnn")
                && segments[2].starts_with("filename*2*=")
                && written.ends_with(".txt"),
            "a long name is not continued: {written:?}"
        );
        let rejoined: String = segments
            .iter()
            .map(|s| s.trim_start_matches("\r\n ").split_once('=').unwrap().1)
            .collect::<String>()
            .trim_start_matches("utf-8''")
            .to_string();
        assert_eq!(
            rejoined, long,
            "the continued name does not rejoin to the name"
        );
        assert_eq!(
            media_type("text/plain\r\nBcc: x@y"),
            "application/octet-stream"
        );
        assert_eq!(media_type("image/png"), "image/png");
    }
}
