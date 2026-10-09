// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An IMAP session for the mail tools (AEGIS ADR-125 D4, its Update of
//! 2026-10-07 clauses 1, 2 and 7, and its Update of 2026-10-07 (3) clause
//! 15): `EXAMINE` or `SELECT`, `UID SEARCH`, `UID FETCH`, `UID STORE`,
//! `LIST` and `APPEND` over the same wire, TLS and guard as the mailbox
//! check; and, for `mail.delete` (its Update of 2026-10-08 (4) clause 19),
//! `CAPABILITY`, `UID MOVE`, `UID COPY` and `UID EXPUNGE`. A plain
//! `EXPUNGE`, which removes every message any client marked `\Deleted`, is
//! never sent.
//!
//! The session is opened as the check opens it ([`super::imap`]): the
//! endpoint is admitted by the connector (the production connector applies
//! the [`super::guard`]), then greeting, `STARTTLS` when asked, and `LOGIN`
//! with a password or `AUTHENTICATE XOAUTH2` with an OAuth token
//! ([`MailAuth`]). A failure is a [`MailboxCheckFailure`]: the server's
//! reply, the secret redacted in every form, control characters removed, at
//! most 512 characters.
//!
//! Message bodies are always fetched with `BODY.PEEK`, so no session of
//! this module sets `\Seen`. The module also reads what a fetch answers: a
//! message's headers (folded lines joined, encoded words decoded) and its
//! `text/plain` body and attachments ([`parse_message`]).

use super::imap::{self, strip_tag, trailing_literal};
use super::wire::{failure_of, Wire};
use super::{MailAuth, MailConnector, MailProtocol, MailTarget, MailboxCheckFailure};
use crate::domain::credential::MailboxSettings;
use base64::Engine as _;

/// The largest literal a session reads: one message, attachments included.
pub const MAX_LITERAL: u64 = 20 * 1024 * 1024;

/// What `EXAMINE` or `SELECT` reported about a folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderStatus {
    pub exists: u32,
    pub uidvalidity: Option<u32>,
    /// The flags the server keeps across sessions; `\*` means it keeps any
    /// keyword a client sets.
    pub permanent_flags: Vec<String>,
}

impl FolderStatus {
    /// Whether the server keeps `keyword` when a client sets it.
    pub fn keeps_keyword(&self, keyword: &str) -> bool {
        self.permanent_flags
            .iter()
            .any(|f| f == "\\*" || f.eq_ignore_ascii_case(keyword))
    }
}

/// One argument of a command: an atom written as is, a string sent as a
/// quoted string or, when it holds a byte a quoted string cannot, a
/// literal, or bytes always sent as a literal (a message `APPEND` stores).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Atom(String),
    Str(String),
    Literal(Vec<u8>),
}

/// One folder as `LIST` answered it: its name as the server writes it, its
/// attributes (`\Sent`, `\Drafts`, `\HasNoChildren`, ...) and its hierarchy
/// delimiter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedFolder {
    pub name: String,
    pub attributes: Vec<String>,
    pub delimiter: Option<String>,
}

impl ListedFolder {
    /// Whether `LIST` marked the folder with `attribute` (RFC 6154).
    pub fn has_attribute(&self, attribute: &str) -> bool {
        self.attributes
            .iter()
            .any(|a| a.eq_ignore_ascii_case(attribute))
    }
}

impl Arg {
    pub fn atom(s: impl Into<String>) -> Self {
        Arg::Atom(s.into())
    }
    pub fn string(s: impl Into<String>) -> Self {
        Arg::Str(s.into())
    }
}

/// One message as `UID FETCH` answered it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    pub uid: u32,
    pub flags: Vec<String>,
    pub internal_date: Option<String>,
    /// The bytes of a `BODY[HEADER...]` section, if fetched.
    pub header: Option<Vec<u8>>,
    /// The bytes of `BODY[]`, if fetched.
    pub full: Option<Vec<u8>>,
}

/// How `UID STORE` changes flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    Add,
    Remove,
}

/// An authenticated IMAP session.
pub struct ImapSession {
    wire: Wire,
    next_tag: u32,
    /// The largest literal this session reads ([`MAX_LITERAL`] unless
    /// raised by [`ImapSession::read_literals_up_to`]).
    max_literal: u64,
}

impl ImapSession {
    /// Admit the mailbox's IMAP endpoint and open an authenticated session.
    pub async fn open(
        connector: &dyn MailConnector,
        settings: &MailboxSettings,
        auth: &MailAuth,
    ) -> Result<ImapSession, MailboxCheckFailure> {
        let admitted = connector
            .admit(MailTarget {
                protocol: MailProtocol::Imap,
                host: settings.imap_host.clone(),
                port: settings.imap_port,
                security: settings.imap_security.clone(),
            })
            .await
            .map_err(|e| {
                failure_of(
                    MailProtocol::Imap,
                    e,
                    &auth.secret_forms(&settings.username),
                )
            })?;
        let wire = imap::open(connector, &admitted, settings, auth).await?;
        Ok(Self {
            wire,
            next_tag: 1,
            max_literal: MAX_LITERAL,
        })
    }

    /// Read literals of up to `max` bytes from here on: a fetch of one
    /// whole message whose attachment is itself bounded (`mail.attachment`)
    /// reads a message larger than [`MAX_LITERAL`], since an attachment
    /// encoded in base64 takes about a third more bytes than it holds.
    pub fn read_literals_up_to(&mut self, max: u64) {
        self.max_literal = max;
    }

    /// `EXAMINE <folder>`: open the folder read-only.
    pub async fn examine(&mut self, folder: &str) -> Result<FolderStatus, MailboxCheckFailure> {
        self.open_folder("EXAMINE", folder).await
    }

    /// `SELECT <folder>`: open the folder read-write.
    pub async fn select(&mut self, folder: &str) -> Result<FolderStatus, MailboxCheckFailure> {
        self.open_folder("SELECT", folder).await
    }

    async fn open_folder(
        &mut self,
        verb: &str,
        folder: &str,
    ) -> Result<FolderStatus, MailboxCheckFailure> {
        let responses = self
            .command(&[Arg::atom(verb), Arg::string(folder)])
            .await?;
        let mut status = FolderStatus::default();
        for response in &responses {
            let text = response.text();
            let upper = text.to_ascii_uppercase();
            if let Some(n) = upper
                .strip_prefix("* ")
                .and_then(|r| r.strip_suffix(" EXISTS"))
            {
                status.exists = n.trim().parse().unwrap_or(0);
            } else if let Some(rest) = code_of(&text, "UIDVALIDITY") {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                status.uidvalidity = digits.parse().ok();
            } else if let Some(rest) = code_of(&text, "PERMANENTFLAGS") {
                if let (Some(open), Some(close)) = (rest.find('('), rest.find(')')) {
                    status.permanent_flags = rest[open + 1..close]
                        .split_whitespace()
                        .map(str::to_string)
                        .collect();
                }
            }
        }
        Ok(status)
    }

    /// `UID SEARCH <criteria>`: the matching UIDs, ascending. `CHARSET
    /// UTF-8` is named when any string argument is not ASCII.
    pub async fn uid_search(&mut self, criteria: &[Arg]) -> Result<Vec<u32>, MailboxCheckFailure> {
        let mut args = vec![Arg::atom("UID"), Arg::atom("SEARCH")];
        if criteria
            .iter()
            .any(|a| matches!(a, Arg::Str(s) if !s.is_ascii()))
        {
            args.push(Arg::atom("CHARSET"));
            args.push(Arg::atom("UTF-8"));
        }
        args.extend_from_slice(criteria);
        let responses = self.command(&args).await?;
        let mut uids: Vec<u32> = responses
            .iter()
            .filter_map(|r| {
                let text = r.text();
                let upper = text.to_ascii_uppercase();
                upper.strip_prefix("* SEARCH").map(|rest| {
                    rest.split_whitespace()
                        .filter_map(|n| n.parse().ok())
                        .collect::<Vec<u32>>()
                })
            })
            .flatten()
            .collect();
        uids.sort_unstable();
        uids.dedup();
        Ok(uids)
    }

    /// `UID FETCH <uids> (<items>)`: each message the server answered for,
    /// by ascending UID. `items` is the parenthesised item list's inside,
    /// for example `UID FLAGS BODY.PEEK[]`.
    pub async fn uid_fetch(
        &mut self,
        uids: &[u32],
        items: &str,
    ) -> Result<Vec<Fetched>, MailboxCheckFailure> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let responses = self
            .command(&[
                Arg::atom("UID"),
                Arg::atom("FETCH"),
                Arg::atom(uid_set(uids)),
                Arg::atom(format!("({items})")),
            ])
            .await?;
        let mut out: Vec<Fetched> = responses.iter().filter_map(parse_fetch).collect();
        out.sort_by_key(|f| f.uid);
        out.dedup_by_key(|f| f.uid);
        Ok(out)
    }

    /// `UID STORE <uids> +FLAGS (<flags>)` or `-FLAGS (<flags>)`.
    pub async fn uid_store(
        &mut self,
        uids: &[u32],
        op: StoreOp,
        flags: &[String],
    ) -> Result<(), MailboxCheckFailure> {
        if uids.is_empty() || flags.is_empty() {
            return Ok(());
        }
        let verb = match op {
            StoreOp::Add => "+FLAGS",
            StoreOp::Remove => "-FLAGS",
        };
        self.command(&[
            Arg::atom("UID"),
            Arg::atom("STORE"),
            Arg::atom(uid_set(uids)),
            Arg::atom(verb),
            Arg::atom(format!("({})", flags.join(" "))),
        ])
        .await?;
        Ok(())
    }

    /// `CAPABILITY`: what the server says it can do, after authentication
    /// (a server may name more once a client has logged in). Upper-cased.
    pub async fn capabilities(&mut self) -> Result<Vec<String>, MailboxCheckFailure> {
        let responses = self.command(&[Arg::atom("CAPABILITY")]).await?;
        Ok(responses
            .iter()
            .filter_map(|r| {
                let text = r.text();
                let upper = text.to_ascii_uppercase();
                upper.strip_prefix("* CAPABILITY").map(|rest| {
                    rest.split_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
            })
            .flatten()
            .collect())
    }

    /// `UID MOVE <uids> <folder>` (RFC 6851).
    pub async fn uid_move(
        &mut self,
        uids: &[u32],
        folder: &str,
    ) -> Result<(), MailboxCheckFailure> {
        self.uid_to_folder("MOVE", uids, folder).await
    }

    /// `UID COPY <uids> <folder>`.
    pub async fn uid_copy(
        &mut self,
        uids: &[u32],
        folder: &str,
    ) -> Result<(), MailboxCheckFailure> {
        self.uid_to_folder("COPY", uids, folder).await
    }

    async fn uid_to_folder(
        &mut self,
        verb: &str,
        uids: &[u32],
        folder: &str,
    ) -> Result<(), MailboxCheckFailure> {
        if uids.is_empty() {
            return Ok(());
        }
        self.command(&[
            Arg::atom("UID"),
            Arg::atom(verb),
            Arg::atom(uid_set(uids)),
            Arg::string(folder),
        ])
        .await?;
        Ok(())
    }

    /// `UID EXPUNGE <uids>` (RFC 4315): removes only those of `uids` that
    /// carry `\Deleted`, never another message.
    pub async fn uid_expunge(&mut self, uids: &[u32]) -> Result<(), MailboxCheckFailure> {
        if uids.is_empty() {
            return Ok(());
        }
        self.command(&[
            Arg::atom("UID"),
            Arg::atom("EXPUNGE"),
            Arg::atom(uid_set(uids)),
        ])
        .await?;
        Ok(())
    }

    /// `LIST "" "*"`: every folder of the mailbox.
    pub async fn list_folders(&mut self) -> Result<Vec<ListedFolder>, MailboxCheckFailure> {
        let responses = self
            .command(&[Arg::atom("LIST"), Arg::string(""), Arg::string("*")])
            .await?;
        Ok(responses.iter().filter_map(parse_list).collect())
    }

    /// `APPEND <folder> (<flags>) {n}`: store `message` in `folder` with
    /// `flags`, the message sent as a literal after the server's `+`.
    pub async fn append(
        &mut self,
        folder: &str,
        flags: &[&str],
        message: &[u8],
    ) -> Result<(), MailboxCheckFailure> {
        self.command(&[
            Arg::atom("APPEND"),
            Arg::string(folder),
            Arg::atom(format!("({})", flags.join(" "))),
            Arg::Literal(message.to_vec()),
        ])
        .await?;
        Ok(())
    }

    /// `LOGOUT`; a server that answers it badly changes nothing.
    pub async fn logout(mut self) {
        let tag = self.tag();
        if self
            .wire
            .send(format!("{tag} LOGOUT\r\n").as_bytes())
            .await
            .is_ok()
        {
            let _ = imap::tagged(&mut self.wire, &tag).await;
        }
    }

    fn tag(&mut self) -> String {
        let tag = format!("B{}", self.next_tag);
        self.next_tag += 1;
        tag
    }

    /// Send one command and read its untagged responses up to its tagged
    /// `OK`; a `NO`, `BAD` or `* BYE` is a failure carrying the reply.
    async fn command(&mut self, args: &[Arg]) -> Result<Vec<Response>, MailboxCheckFailure> {
        let tag = self.tag();
        let mut pending = tag.clone().into_bytes();
        for arg in args {
            pending.push(b' ');
            match arg {
                Arg::Atom(a) => pending.extend_from_slice(a.as_bytes()),
                Arg::Str(s) if quotable(s) => {
                    pending.push(b'"');
                    for b in s.bytes() {
                        if b == b'"' || b == b'\\' {
                            pending.push(b'\\');
                        }
                        pending.push(b);
                    }
                    pending.push(b'"');
                }
                Arg::Str(s) => self.literal(&mut pending, &tag, s.as_bytes()).await?,
                Arg::Literal(bytes) => self.literal(&mut pending, &tag, bytes).await?,
            }
        }
        pending.extend_from_slice(b"\r\n");
        self.wire.send(&pending).await?;

        let mut responses = Vec::new();
        loop {
            let response = self.read_response().await?;
            let first = response.first_line();
            if first.starts_with(&format!("{tag} ")) {
                let status = strip_tag(first, &tag);
                if status.len() >= 2 && status[..2].eq_ignore_ascii_case("OK") {
                    return Ok(responses);
                }
                return Err(self.wire.fail(status));
            }
            if first.starts_with("* BYE") {
                return Err(self.wire.fail(first));
            }
            responses.push(response);
        }
    }

    /// Announce `bytes` as a synchronising literal at the end of `pending`,
    /// send what is pending, wait for the server's `+` and leave `bytes`
    /// pending.
    async fn literal(
        &mut self,
        pending: &mut Vec<u8>,
        tag: &str,
        bytes: &[u8],
    ) -> Result<(), MailboxCheckFailure> {
        pending.extend_from_slice(format!("{{{}}}\r\n", bytes.len()).as_bytes());
        self.wire.send(pending).await?;
        pending.clear();
        let next = self.wire.line().await?;
        if !next.starts_with('+') {
            return Err(self.wire.fail(strip_tag(&next, tag)));
        }
        pending.extend_from_slice(bytes);
        Ok(())
    }

    /// One response: a line, and each literal it announces with the rest
    /// of the line after it.
    async fn read_response(&mut self) -> Result<Response, MailboxCheckFailure> {
        let mut segments = Vec::new();
        loop {
            let line = self.wire.line().await?;
            match trailing_literal(&line) {
                Some(n) => {
                    let open = line.rfind('{').unwrap_or(line.len());
                    segments.push(Segment::Text(line[..open].to_string()));
                    let bytes = self.wire.literal(n, self.max_literal).await?;
                    segments.push(Segment::Literal(bytes));
                }
                None => {
                    segments.push(Segment::Text(line));
                    return Ok(Response { segments });
                }
            }
        }
    }
}

fn quotable(s: &str) -> bool {
    s.bytes()
        .all(|b| (0x01..=0x7f).contains(&b) && b != b'\r' && b != b'\n')
}

/// `1,2,5`: the UIDs as a set.
fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The text after `[<code> ` in an untagged `OK` response.
fn code_of(text: &str, code: &str) -> Option<String> {
    let upper = text.to_ascii_uppercase();
    let at = upper.find(&format!("[{code} "))?;
    Some(text[at + code.len() + 2..].to_string())
}

#[derive(Debug, Clone)]
enum Segment {
    Text(String),
    Literal(Vec<u8>),
}

/// One untagged or tagged response, with its literals.
#[derive(Debug, Clone)]
struct Response {
    segments: Vec<Segment>,
}

impl Response {
    fn first_line(&self) -> &str {
        match self.segments.first() {
            Some(Segment::Text(t)) => t,
            _ => "",
        }
    }

    /// The response's text, literals left out.
    fn text(&self) -> String {
        self.segments
            .iter()
            .filter_map(|s| match s {
                Segment::Text(t) => Some(t.as_str()),
                Segment::Literal(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

// ---------------------------------------------------------------------------
// Reading a FETCH response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Atom(String),
    Quoted(String),
    Literal(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Atom(String),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Item>),
}

fn tokens(response: &Response) -> Vec<Token> {
    let mut out = Vec::new();
    for segment in &response.segments {
        match segment {
            Segment::Literal(b) => out.push(Token::Literal(b.clone())),
            Segment::Text(t) => lex(t, &mut out),
        }
    }
    out
}

fn lex(text: &str, out: &mut Vec<Token>) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            ' ' => i += 1,
            '(' => {
                out.push(Token::Open);
                i += 1;
            }
            ')' => {
                out.push(Token::Close);
                i += 1;
            }
            '"' => {
                i += 1;
                let mut s = String::new();
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                i += 1;
                out.push(Token::Quoted(s));
            }
            _ => {
                // An atom; a `[...]` section (which may hold spaces and a
                // parenthesised list) is part of it.
                let mut s = String::new();
                let mut depth = 0usize;
                while i < chars.len() {
                    let c = chars[i];
                    if depth == 0 && (c == ' ' || c == '(' || c == ')' || c == '"') {
                        break;
                    }
                    if c == '[' {
                        depth += 1;
                    } else if c == ']' {
                        depth = depth.saturating_sub(1);
                    }
                    s.push(c);
                    i += 1;
                }
                out.push(Token::Atom(s));
            }
        }
    }
}

fn parse_item(tokens: &[Token], at: &mut usize) -> Option<Item> {
    let token = tokens.get(*at)?.clone();
    *at += 1;
    Some(match token {
        Token::Open => {
            let mut list = Vec::new();
            while let Some(t) = tokens.get(*at) {
                if *t == Token::Close {
                    *at += 1;
                    break;
                }
                list.push(parse_item(tokens, at)?);
            }
            Item::List(list)
        }
        Token::Close => return None,
        Token::Atom(a) => Item::Atom(a),
        Token::Quoted(s) => Item::Str(s),
        Token::Literal(b) => Item::Bytes(b),
    })
}

/// A `* LIST (<attributes>) <delimiter> <name>` response, or `None` for any
/// other.
fn parse_list(response: &Response) -> Option<ListedFolder> {
    let tokens = tokens(response);
    match (tokens.first(), tokens.get(1)) {
        (Some(Token::Atom(star)), Some(Token::Atom(list)))
            if star == "*" && list.eq_ignore_ascii_case("LIST") => {}
        _ => return None,
    }
    let mut at = 2;
    let Item::List(attributes) = parse_item(&tokens, &mut at)? else {
        return None;
    };
    let delimiter = match parse_item(&tokens, &mut at)? {
        Item::Str(d) => Some(d),
        _ => None,
    };
    let name = match parse_item(&tokens, &mut at)? {
        Item::Str(n) | Item::Atom(n) => n,
        Item::Bytes(b) => String::from_utf8_lossy(&b).to_string(),
        Item::List(_) => return None,
    };
    Some(ListedFolder {
        name,
        attributes: attributes
            .into_iter()
            .filter_map(|a| match a {
                Item::Atom(a) => Some(a),
                _ => None,
            })
            .collect(),
        delimiter,
    })
}

/// A `* <n> FETCH (...)` response, or `None` for any other.
fn parse_fetch(response: &Response) -> Option<Fetched> {
    let tokens = tokens(response);
    // `*`, the sequence number, `FETCH`, then the list.
    match (tokens.first(), tokens.get(2)) {
        (Some(Token::Atom(star)), Some(Token::Atom(fetch)))
            if star == "*" && fetch.eq_ignore_ascii_case("FETCH") => {}
        _ => return None,
    }
    let mut at = 3;
    let Item::List(items) = parse_item(&tokens, &mut at)? else {
        return None;
    };
    let mut fetched = Fetched::default();
    let mut uid = None;
    let mut pairs = items.into_iter();
    while let (Some(name), Some(value)) = (pairs.next(), pairs.next()) {
        let Item::Atom(name) = name else { continue };
        let upper = name.to_ascii_uppercase();
        let bytes = |v: Item| match v {
            Item::Bytes(b) => Some(b),
            Item::Str(s) => Some(s.into_bytes()),
            _ => None,
        };
        if upper == "UID" {
            if let Item::Atom(n) = value {
                uid = n.parse().ok();
            }
        } else if upper == "FLAGS" {
            if let Item::List(flags) = value {
                fetched.flags = flags
                    .into_iter()
                    .filter_map(|f| match f {
                        Item::Atom(a) => Some(a),
                        _ => None,
                    })
                    .collect();
            }
        } else if upper == "INTERNALDATE" {
            if let Item::Str(s) = value {
                fetched.internal_date = Some(s);
            }
        } else if upper.starts_with("BODY[HEADER") || upper.starts_with("RFC822.HEADER") {
            fetched.header = bytes(value);
        } else if upper == "BODY[]" || upper == "RFC822" {
            fetched.full = bytes(value);
        }
    }
    fetched.uid = uid?;
    Some(fetched)
}

// ---------------------------------------------------------------------------
// Reading a message
// ---------------------------------------------------------------------------

/// A message's headers, folded lines joined, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
    /// The first header of `name`, raw (encoded words not decoded).
    pub fn raw(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The first header of `name`, encoded words decoded.
    pub fn get(&self, name: &str) -> Option<String> {
        self.raw(name).map(decode_words)
    }

    /// The `<id>`s a `Message-ID`, `In-Reply-To` or `References` header holds.
    pub fn ids(&self, name: &str) -> Vec<String> {
        let Some(value) = self.raw(name) else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        let mut rest = value;
        while let Some(open) = rest.find('<') {
            match rest[open..].find('>') {
                Some(close) => {
                    ids.push(rest[open..open + close + 1].to_string());
                    rest = &rest[open + close + 1..];
                }
                None => break,
            }
        }
        ids
    }

    /// The addresses of a `To` or `Cc` header, each as written, decoded.
    pub fn addresses(&self, name: &str) -> Vec<String> {
        let Some(value) = self.get(name) else {
            return Vec::new();
        };
        split_addresses(&value)
    }
}

/// Split an address list at the commas outside quotes and angle brackets.
fn split_addresses(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut angle) = (false, false);
    for c in value.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' if !quoted && !angle => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_string());
                }
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// Split a message's header block from its body.
fn split_head(raw: &[u8]) -> (&[u8], &[u8]) {
    for (i, w) in raw.windows(4).enumerate() {
        if w == b"\r\n\r\n" {
            return (&raw[..i], &raw[i + 4..]);
        }
    }
    for (i, w) in raw.windows(2).enumerate() {
        if w == b"\n\n" {
            return (&raw[..i], &raw[i + 2..]);
        }
    }
    (raw, &[])
}

/// Read a header block (or a whole message's headers).
pub fn parse_headers(raw: &[u8]) -> Headers {
    let (head, _) = split_head(raw);
    let text = String::from_utf8_lossy(head);
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Headers(headers)
}

/// An attachment of a message: its section number as RFC 9051 numbers a
/// message's body parts (`1` for a message that is not multipart, `2`, `1.2`
/// within one; AEGIS ADR-125's Update of 2026-10-08 (5) clause 30), what it
/// is called, its type and its size in bytes once decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub part: String,
    pub filename: Option<String>,
    pub content_type: String,
    pub size: usize,
}

/// A message read whole: its headers, its `text/plain` body (empty when it
/// has none) and its attachments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub headers: Headers,
    pub text: String,
    pub attachments: Vec<Attachment>,
}

/// Read a whole message (`BODY[]`).
pub fn parse_message(raw: &[u8]) -> Message {
    let headers = parse_headers(raw);
    let mut walk = Walk::default();
    walk_part(raw, "", 0, &mut walk);
    Message {
        headers,
        text: walk.text.unwrap_or_default(),
        attachments: walk.attachments,
    }
}

/// One attachment of a message, decoded: what [`parse_message`] answers
/// for it, its bytes once its transfer encoding (base64, quoted-printable)
/// is undone, and the `charset` its `Content-Type` names, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentPart {
    pub attachment: Attachment,
    pub bytes: Vec<u8>,
    pub charset: Option<String>,
}

/// The attachment numbered `part` of a whole message (`BODY[]`), found by
/// the walk that numbers [`parse_message`]'s attachments, so a number
/// `mail.read` answered names the same part here; `None` when no
/// attachment has that number (the message's own text body is not one).
pub fn attachment_part(raw: &[u8], part: &str) -> Option<AttachmentPart> {
    let mut walk = Walk {
        want: Some(part.to_string()),
        ..Walk::default()
    };
    walk_part(raw, "", 0, &mut walk);
    walk.found
}

/// Bytes in `charset` as text, as a message's text body is read.
pub fn decode_text(bytes: &[u8], charset: Option<&str>) -> String {
    decode_charset(bytes, charset)
}

/// A `Content-Type` or `Content-Disposition` value: the value and its
/// parameters, names lower-cased.
fn parse_params(value: &str) -> (String, Vec<(String, String)>) {
    let mut parts = value.split(';');
    let main = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let params = parts
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((
                k.trim().to_ascii_lowercase(),
                v.trim().trim_matches('"').to_string(),
            ))
        })
        .collect();
    (main, params)
}

fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// What a walk over a message's parts gathers: its first `text/plain` body,
/// its attachments, and, when `want` names a part, that part decoded.
#[derive(Default)]
struct Walk {
    text: Option<String>,
    attachments: Vec<Attachment>,
    want: Option<String>,
    found: Option<AttachmentPart>,
}

/// Walk the part `raw`, numbered `section` (empty for the message itself):
/// a multipart's children are numbered `1`, `2`, ... under it (`1.2` for the
/// second child of its first), and a message that is not multipart is its
/// own part `1` (RFC 9051 6.4.5).
fn walk_part(raw: &[u8], section: &str, depth: usize, walk: &mut Walk) {
    let headers = parse_headers(raw);
    let (_, body) = split_head(raw);
    let (content_type, params) = headers
        .raw("Content-Type")
        .map(parse_params)
        .unwrap_or_else(|| ("text/plain".to_string(), Vec::new()));
    if content_type.starts_with("multipart/") && depth < 8 {
        if let Some(boundary) = param(&params, "boundary") {
            for (i, part) in split_multipart(body, boundary).into_iter().enumerate() {
                let child = if section.is_empty() {
                    (i + 1).to_string()
                } else {
                    format!("{section}.{}", i + 1)
                };
                walk_part(part, &child, depth + 1, walk);
            }
            return;
        }
    }
    let encoding = headers
        .raw("Content-Transfer-Encoding")
        .unwrap_or("7bit")
        .trim()
        .to_ascii_lowercase();
    let decoded = decode_transfer(body, &encoding);
    let (disposition, dparams) = headers
        .raw("Content-Disposition")
        .map(parse_params)
        .unwrap_or_default();
    let filename = param(&dparams, "filename")
        .or_else(|| param(&params, "name"))
        .map(decode_words);
    let is_attachment = disposition == "attachment" || filename.is_some();
    if content_type == "text/plain" && !is_attachment && walk.text.is_none() {
        walk.text = Some(decode_charset(&decoded, param(&params, "charset")));
    } else if !content_type.starts_with("multipart/") && (is_attachment || depth > 0) {
        if content_type == "text/html" && !is_attachment {
            // The alternative of a text body: not an attachment.
            return;
        }
        let attachment = Attachment {
            part: if section.is_empty() {
                "1".to_string()
            } else {
                section.to_string()
            },
            filename,
            content_type,
            size: decoded.len(),
        };
        if walk.found.is_none() && walk.want.as_deref() == Some(attachment.part.as_str()) {
            walk.found = Some(AttachmentPart {
                attachment: attachment.clone(),
                bytes: decoded,
                charset: param(&params, "charset").map(str::to_string),
            });
        }
        walk.attachments.push(attachment);
    }
}

/// The parts of a multipart body between its `--boundary` lines.
fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delimiter = format!("--{boundary}");
    let d = delimiter.as_bytes();
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + d.len() <= body.len() {
        let at_line_start = i == 0 || body[i - 1] == b'\n';
        if at_line_start && &body[i..i + d.len()] == d {
            let mut end = i + d.len();
            let closing = body.get(end..end + 2) == Some(b"--");
            while end < body.len() && body[end] != b'\n' {
                end += 1;
            }
            starts.push((i, (end + 1).min(body.len())));
            if closing {
                break;
            }
            i = end;
        }
        i += 1;
    }
    starts
        .windows(2)
        .map(|w| {
            let (from, to) = (w[0].1, w[1].0);
            let mut part = &body[from..to];
            if part.ends_with(b"\r\n") {
                part = &part[..part.len() - 2];
            } else if part.ends_with(b"\n") {
                part = &part[..part.len() - 1];
            }
            part
        })
        .collect()
}

fn decode_transfer(body: &[u8], encoding: &str) -> Vec<u8> {
    match encoding {
        "base64" => {
            let clean: Vec<u8> = body
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            base64::engine::general_purpose::STANDARD
                .decode(&clean)
                .unwrap_or_default()
        }
        "quoted-printable" => decode_quoted_printable(body, false),
        _ => body.to_vec(),
    }
}

/// Quoted-printable (RFC 2045 6.7); `underscore_is_space` for the `Q` form
/// of an encoded word (RFC 2047 4.2).
fn decode_quoted_printable(body: &[u8], underscore_is_space: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        let b = body[i];
        if b == b'=' {
            // A soft line break.
            if body.get(i + 1) == Some(&b'\r') && body.get(i + 2) == Some(&b'\n') {
                i += 3;
                continue;
            }
            if body.get(i + 1) == Some(&b'\n') {
                i += 2;
                continue;
            }
            let hex = body.get(i + 1..i + 3).and_then(|h| {
                std::str::from_utf8(h)
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            });
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        if underscore_is_space && b == b'_' {
            out.push(b' ');
        } else {
            out.push(b);
        }
        i += 1;
    }
    out
}

/// Bytes in `charset` as text: UTF-8 and ASCII directly, any other label
/// `encoding_rs` knows through it, and an unknown label read as UTF-8 with
/// replacement characters.
fn decode_charset(bytes: &[u8], charset: Option<&str>) -> String {
    let label = charset.unwrap_or("utf-8").trim().to_ascii_lowercase();
    if label == "utf-8" || label == "us-ascii" || label == "ascii" || label == "utf8" {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    match encoding_rs::Encoding::for_label(label.as_bytes()) {
        Some(encoding) => encoding.decode(bytes).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Decode the encoded words (`=?charset?B?...?=`, `=?charset?Q?...?=`) of a
/// header value; the space between two adjacent encoded words is dropped.
pub fn decode_words(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    let mut last_was_word = false;
    while let Some(start) = rest.find("=?") {
        let (before, after) = rest.split_at(start);
        let word = parse_encoded_word(after);
        match word {
            Some((decoded, used)) => {
                if !(last_was_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&decoded);
                rest = &after[used..];
                last_was_word = true;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                rest = &after[2..];
                last_was_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

/// One encoded word at the start of `s`: its text and the bytes it used.
fn parse_encoded_word(s: &str) -> Option<(String, usize)> {
    let inner = s.strip_prefix("=?")?;
    let (charset, rest) = inner.split_once('?')?;
    let (kind, rest) = rest.split_once('?')?;
    let end = rest.find("?=")?;
    let payload = &rest[..end];
    if payload.contains(' ') {
        return None;
    }
    let bytes = match kind.to_ascii_uppercase().as_str() {
        "B" => base64::engine::general_purpose::STANDARD
            .decode(payload)
            .ok()?,
        "Q" => decode_quoted_printable(payload.as_bytes(), true),
        _ => return None,
    };
    let used = 2 + charset.len() + 1 + kind.len() + 1 + end + 2;
    // RFC 2231's language suffix (`utf-8*en`) is not part of the label.
    let label = charset.split('*').next().unwrap_or(charset);
    Some((decode_charset(&bytes, Some(label)), used))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fetch_with_a_section_and_a_literal_is_read() {
        let response = Response {
            segments: vec![
                Segment::Text(
                    "* 3 FETCH (UID 42 FLAGS (\\Seen zaru/lead) INTERNALDATE \"06-Oct-2026 14:02:11 +0000\" BODY[HEADER.FIELDS (DATE FROM)] ".to_string(),
                ),
                Segment::Literal(b"From: a@b\r\n\r\n".to_vec()),
                Segment::Text(")".to_string()),
            ],
        };
        let f = parse_fetch(&response).expect("a FETCH");
        assert_eq!(f.uid, 42);
        assert_eq!(f.flags, vec!["\\Seen", "zaru/lead"]);
        assert_eq!(
            f.internal_date.as_deref(),
            Some("06-Oct-2026 14:02:11 +0000")
        );
        assert_eq!(f.header.as_deref(), Some(&b"From: a@b\r\n\r\n"[..]));
    }

    #[test]
    fn a_list_response_is_read_with_its_attributes_delimiter_and_name() {
        let response = Response {
            segments: vec![Segment::Text(
                r#"* LIST (\HasNoChildren \Sent) "/" "[Gmail]/Sent Mail""#.to_string(),
            )],
        };
        assert_eq!(
            parse_list(&response),
            Some(ListedFolder {
                name: "[Gmail]/Sent Mail".to_string(),
                attributes: vec!["\\HasNoChildren".to_string(), "\\Sent".to_string()],
                delimiter: Some("/".to_string()),
            })
        );
        let atom = Response {
            segments: vec![Segment::Text("* LIST () NIL INBOX".to_string())],
        };
        assert_eq!(parse_list(&atom).map(|f| f.name), Some("INBOX".to_string()));
    }

    #[test]
    fn the_uidvalidity_code_is_read() {
        assert_eq!(
            code_of("* OK [UIDVALIDITY 7] UIDs valid", "UIDVALIDITY").as_deref(),
            Some("7] UIDs valid")
        );
    }

    #[test]
    fn encoded_words_and_folded_headers_are_read() {
        let h = parse_headers(
            b"Subject: =?UTF-8?B?w6ljaG8=?= =?ISO-8859-1?Q?_caf=E9?=\r\nReferences: <a@x>\r\n <b@x>\r\n\r\nbody",
        );
        assert_eq!(h.get("subject").as_deref(), Some("écho café"));
        assert_eq!(h.ids("References"), vec!["<a@x>", "<b@x>"]);
    }

    #[test]
    fn the_plain_part_of_a_multipart_message_is_its_text_and_the_rest_attachments() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"XX\"\r\n\r\n--XX\r\nContent-Type: text/plain; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=E9 =\r\nau lait\r\n--XX\r\nContent-Type: application/pdf; name=\"a.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nAAEC\r\n--XX--\r\n";
        let m = parse_message(raw);
        assert_eq!(m.text, "café au lait");
        assert_eq!(
            m.attachments,
            vec![Attachment {
                part: "2".to_string(),
                filename: Some("a.pdf".to_string()),
                content_type: "application/pdf".to_string(),
                size: 3,
            }]
        );
    }

    #[test]
    fn parts_are_numbered_as_rfc_9051_numbers_them_and_one_is_found_decoded() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"M\"\r\n\r\n--M\r\nContent-Type: multipart/alternative; boundary=\"A\"\r\n\r\n--A\r\nContent-Type: text/plain\r\n\r\nhi\r\n--A\r\nContent-Type: multipart/mixed; boundary=\"I\"\r\n\r\n--I\r\nContent-Type: text/html\r\n\r\n<p>hi</p>\r\n--I\r\nContent-Type: image/gif\r\nContent-Transfer-Encoding: base64\r\n\r\nR0lG\r\n--I--\r\n--A--\r\n--M\r\nContent-Type: text/plain; charset=iso-8859-1; name=\"n.txt\"\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=E9\r\n--M--\r\n";
        let parts: Vec<String> = parse_message(raw)
            .attachments
            .into_iter()
            .map(|a| a.part)
            .collect();
        assert_eq!(parts, vec!["1.2.2", "2"]);
        let gif = attachment_part(raw, "1.2.2").expect("part 1.2.2");
        assert_eq!(gif.bytes, b"GIF");
        let text = attachment_part(raw, "2").expect("part 2");
        assert_eq!(text.bytes, b"caf\xe9");
        assert_eq!(text.charset.as_deref(), Some("iso-8859-1"));
        assert!(
            attachment_part(raw, "1.1").is_none(),
            "the text body is no attachment"
        );
        let single = b"Content-Type: application/pdf; name=\"a.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nAAEC\r\n";
        assert_eq!(parse_message(single).attachments[0].part, "1");
    }
}
