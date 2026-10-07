// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Loopback stand-ins for an IMAP and an SMTP server, and a plaintext
//! [`MailConnector`], for the mailbox check of AEGIS ADR-125 D1.
//!
//! The stand-ins speak just enough of each protocol for the check
//! (greeting, CAPABILITY/EHLO, STARTTLS, LOGIN/AUTH, SELECT INBOX,
//! LOGOUT/QUIT) and record every command line they receive, so a test can
//! assert that no message was ever submitted. They listen on 127.0.0.1 and
//! never reach a real mail server.
//!
//! STARTTLS is answered with success and the session continues in
//! plaintext: the [`PlainConnector`] used with them makes `start_tls` the
//! identity, so the protocol logic around the upgrade is exercised without
//! a certificate. The TLS upgrade itself is the production connector's.
//!
//! Shared by `orchestrator/core/tests/mailbox_credential_tests.rs` and the
//! cli route tests through `#[path]`.
//!
//! [`imap_mailbox_standin`] is a second IMAP stand-in holding an in-memory
//! `INBOX`, for the mail tools (AEGIS ADR-125 D4): `EXAMINE`, `SELECT`,
//! `UID SEARCH` (ALL, SEEN, UNSEEN, FLAGGED, TEXT, FROM, SINCE, HEADER, OR,
//! UID, CHARSET), `UID FETCH` (UID, FLAGS, INTERNALDATE, `BODY[]`,
//! `BODY.PEEK[]`, `BODY.PEEK[HEADER.FIELDS (...)]`; a `BODY[]` without
//! `PEEK` sets `\Seen`, as a server does) and `UID STORE` (`+FLAGS`,
//! `-FLAGS`; a keyword its `PERMANENTFLAGS` does not keep is dropped, as a
//! server drops it). It records every command line, as the first does.
//!
//! [`imap_xoauth2_standin`] is that mailbox stand-in demanding SASL
//! `XOAUTH2` (AEGIS ADR-125's Update of 2026-10-07 clauses 8 and 10): it
//! refuses `LOGIN`, answers `AUTHENTICATE XOAUTH2` with `+`, and records the
//! decoded SASL string it receives; a wrong token gets Google's shape of
//! refusal, a `+` challenge holding a base64 JSON status, then, after the
//! client's line, a tagged `NO`. Its refusal repeats what it received, raw
//! and decoded, so a test can show the client redacts both.
//! [`smtp_xoauth2_standin`] does the same over `AUTH XOAUTH2`.
//! [`RedirectConnector`] records every endpoint it is asked to admit and
//! connects IMAP and SMTP to two stand-ins, so a session whose settings
//! name Google's hosts reaches loopback.

#![allow(dead_code)]

use aegis_orchestrator_core::infrastructure::mail::{
    AdmissionError, AdmittedTarget, BoxedMailStream, MailConnector, MailProtocol, MailTarget,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// The reply the IMAP stand-in gives to a LOGIN with the wrong password.
pub const IMAP_REFUSAL: &str = "NO [AUTHENTICATIONFAILED] Invalid credentials (imap stand-in)";
/// The reply the SMTP stand-in gives to an AUTH with the wrong password.
pub const SMTP_REFUSAL: &str = "535 5.7.8 Authentication credentials invalid (smtp stand-in)";

/// The JSON status Google's servers send, base64, as an XOAUTH2 challenge
/// when they refuse a token.
pub const XOAUTH2_CHALLENGE: &str =
    r#"{"status":"401","schemes":"Bearer","scope":"https://mail.google.com/"}"#;
/// The tagged refusal the XOAUTH2 IMAP stand-in ends a refused exchange
/// with, before the echo of what it received.
pub const XOAUTH2_IMAP_REFUSAL: &str = "NO [AUTHENTICATIONFAILED] Invalid credentials (Failure)";
/// The reply the XOAUTH2 SMTP stand-in ends a refused exchange with, before
/// the echo of what it received.
pub const XOAUTH2_SMTP_REFUSAL: &str = "535 5.7.8 Username and Password not accepted";

/// A running stand-in: its address and every command line it received.
#[derive(Clone)]
pub struct StandIn {
    pub addr: SocketAddr,
    pub commands: Arc<Mutex<Vec<String>>>,
    /// Every connection accepted, whether or not it sent a command.
    pub accepted: Arc<std::sync::atomic::AtomicUsize>,
    /// Each SASL `XOAUTH2` response received, decoded.
    pub sasl: Arc<Mutex<Vec<String>>>,
    /// Each line the client sent in answer to a refusal's challenge.
    pub after_challenge: Arc<Mutex<Vec<String>>>,
}

impl StandIn {
    fn new(
        addr: SocketAddr,
        commands: Arc<Mutex<Vec<String>>>,
        accepted: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            addr,
            commands,
            accepted,
            sasl: Arc::new(Mutex::new(Vec::new())),
            after_challenge: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub fn sasl(&self) -> Vec<String> {
        self.sasl.lock().unwrap().clone()
    }
    pub fn after_challenge(&self) -> Vec<String> {
        self.after_challenge.lock().unwrap().clone()
    }
    pub fn port(&self) -> u16 {
        self.addr.port()
    }
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
    pub fn connections(&self) -> usize {
        self.accepted.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Plaintext connector: TCP to the stand-in, STARTTLS is the identity.
pub struct PlainConnector;

#[async_trait]
impl MailConnector for PlainConnector {
    /// Admits every endpoint: the stand-ins are on loopback and on random
    /// ports, which the production rule refuses.
    async fn admit(&self, target: MailTarget) -> Result<AdmittedTarget, AdmissionError> {
        Ok(AdmittedTarget {
            target,
            addrs: Vec::new(),
        })
    }

    async fn connect(&self, admitted: &AdmittedTarget) -> std::io::Result<BoxedMailStream> {
        let stream =
            TcpStream::connect((admitted.target.host.as_str(), admitted.target.port)).await?;
        Ok(Box::new(stream))
    }

    async fn start_tls(
        &self,
        stream: BoxedMailStream,
        _host: &str,
    ) -> std::io::Result<BoxedMailStream> {
        Ok(stream)
    }
}

/// Split an IMAP argument list into atoms, quoted strings and literals.
/// Literals are announced as `{n}` at the end of a line; the caller has
/// already replaced each with its bytes, so this sees only atoms and quoted
/// strings.
fn imap_args(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = rest.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut s = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            s.push(n);
                        }
                    }
                    '"' => break,
                    other => s.push(other),
                }
            }
            out.push(s);
        } else {
            let mut s = String::new();
            while let Some(&c) = chars.peek() {
                if c == ' ' {
                    break;
                }
                s.push(c);
                chars.next();
            }
            out.push(s);
        }
    }
    out
}

/// Read one IMAP command, resolving `{n}` literals with a `+` continuation.
/// Literal bytes are spliced in as a quoted string so [`imap_args`] reads them.
async fn read_imap_command<R>(
    reader: &mut BufReader<R>,
    writer: &mut (impl AsyncWriteExt + Unpin),
) -> Option<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut command = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if let Some(open) = line.rfind('{') {
            if line.ends_with('}') {
                if let Ok(n) = line[open + 1..line.len() - 1].parse::<usize>() {
                    command.push_str(&line[..open]);
                    writer.write_all(b"+ go ahead\r\n").await.ok()?;
                    let mut buf = vec![0u8; n];
                    reader.read_exact(&mut buf).await.ok()?;
                    let text = String::from_utf8_lossy(&buf).to_string();
                    command.push('"');
                    command.push_str(&text.replace('\\', "\\\\").replace('"', "\\\""));
                    command.push('"');
                    continue;
                }
            }
        }
        command.push_str(&line);
        return Some(command);
    }
}

/// An IMAP stand-in accepting `user`/`password` on LOGIN.
pub async fn imap_standin(user: &str, password: &str) -> StandIn {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind imap");
    let addr = listener.local_addr().unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = commands.clone();
    let count = accepted.clone();
    let (user, password) = (user.to_string(), password.to_string());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);
            let seen = seen.clone();
            let (user, password) = (user.clone(), password.clone());
            tokio::spawn(async move {
                let _ = write.write_all(b"* OK IMAP4rev1 stand-in ready\r\n").await;
                let mut logged_in = false;
                while let Some(cmd) = read_imap_command(&mut reader, &mut write).await {
                    seen.lock().unwrap().push(cmd.clone());
                    let mut parts = cmd.splitn(3, ' ');
                    let tag = parts.next().unwrap_or("*").to_string();
                    let verb = parts.next().unwrap_or("").to_ascii_uppercase();
                    let rest = parts.next().unwrap_or("");
                    let reply = match verb.as_str() {
                        "CAPABILITY" => {
                            format!("* CAPABILITY IMAP4rev1 STARTTLS\r\n{tag} OK done\r\n")
                        }
                        "STARTTLS" => format!("{tag} OK begin TLS\r\n"),
                        "LOGIN" => {
                            let args = imap_args(rest);
                            if args.len() == 2 && args[0] == user && args[1] == password {
                                logged_in = true;
                                format!("{tag} OK LOGIN completed\r\n")
                            } else {
                                format!("{tag} {IMAP_REFUSAL}\r\n")
                            }
                        }
                        "SELECT" if logged_in => format!(
                            "* 0 EXISTS\r\n* 0 RECENT\r\n{tag} OK [READ-WRITE] SELECT completed\r\n"
                        ),
                        "SELECT" => format!("{tag} BAD not authenticated\r\n"),
                        "LOGOUT" => {
                            let _ = write
                                .write_all(format!("* BYE logging out\r\n{tag} OK\r\n").as_bytes())
                                .await;
                            break;
                        }
                        _ => format!("{tag} BAD unknown command\r\n"),
                    };
                    if write.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    StandIn::new(addr, commands, accepted)
}

/// An SMTP stand-in accepting `user`/`password` on AUTH PLAIN or LOGIN.
pub async fn smtp_standin(user: &str, password: &str) -> StandIn {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind smtp");
    let addr = listener.local_addr().unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = commands.clone();
    let count = accepted.clone();
    let (user, password) = (user.to_string(), password.to_string());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);
            let seen = seen.clone();
            let (user, password) = (user.clone(), password.clone());
            tokio::spawn(async move {
                let _ = write.write_all(b"220 smtp.stand-in ESMTP ready\r\n").await;
                let mut login_user: Option<String> = None;
                let mut in_login = false;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let line = line.trim_end_matches(['\r', '\n']).to_string();
                    seen.lock().unwrap().push(line.clone());
                    let decode = |s: &str| {
                        STANDARD
                            .decode(s.trim())
                            .ok()
                            .map(|b| String::from_utf8_lossy(&b).to_string())
                    };
                    let accepted = "235 2.7.0 Authentication successful\r\n".to_string();
                    let refused = format!("{SMTP_REFUSAL}\r\n");
                    let reply = if in_login {
                        match login_user.take() {
                            None => {
                                login_user = decode(&line);
                                "334 UGFzc3dvcmQ6\r\n".to_string()
                            }
                            Some(u) => {
                                in_login = false;
                                if u == user && decode(&line).as_deref() == Some(password.as_str())
                                {
                                    accepted
                                } else {
                                    refused
                                }
                            }
                        }
                    } else {
                        let upper = line.to_ascii_uppercase();
                        if upper.starts_with("EHLO") {
                            "250-smtp.stand-in\r\n250-STARTTLS\r\n250 AUTH PLAIN LOGIN\r\n"
                                .to_string()
                        } else if upper == "STARTTLS" {
                            "220 2.0.0 ready to start TLS\r\n".to_string()
                        } else if upper.starts_with("AUTH PLAIN ") {
                            let ok = decode(&line["AUTH PLAIN ".len()..])
                                .map(|s| {
                                    let mut p = s.split('\0');
                                    let _authz = p.next();
                                    p.next() == Some(user.as_str())
                                        && p.next() == Some(password.as_str())
                                })
                                .unwrap_or(false);
                            if ok {
                                accepted
                            } else {
                                refused
                            }
                        } else if upper == "AUTH LOGIN" {
                            in_login = true;
                            "334 VXNlcm5hbWU6\r\n".to_string()
                        } else if upper == "QUIT" {
                            let _ = write.write_all(b"221 2.0.0 bye\r\n").await;
                            break;
                        } else {
                            "502 5.5.2 command not implemented by the stand-in\r\n".to_string()
                        }
                    };
                    if write.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    StandIn::new(addr, commands, accepted)
}

/// The SASL `XOAUTH2` string a client sends as `user` with `token`.
pub fn xoauth2_string(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}

/// How the mailbox stand-in authenticates its client.
#[derive(Clone)]
enum StandInAuth {
    Login { user: String, password: String },
    XOAuth2 { user: String, token: String },
}

/// The answer to one `XOAUTH2` response line `raw`: `Ok(())` when it
/// carries `user` and `token`; otherwise the challenge has been sent, the
/// client's answer read and recorded, and the refusal's echo is returned.
async fn xoauth2_exchange<R, W>(
    reader: &mut BufReader<R>,
    write: &mut W,
    raw: &str,
    expected: &str,
    standin: &StandIn,
    challenge_prefix: &str,
) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let decoded = STANDARD
        .decode(raw.trim())
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();
    standin.sasl.lock().unwrap().push(decoded.clone());
    if decoded == expected {
        return Ok(());
    }
    let _ = write
        .write_all(
            format!(
                "{challenge_prefix}{}\r\n",
                STANDARD.encode(XOAUTH2_CHALLENGE)
            )
            .as_bytes(),
        )
        .await;
    let mut answer = String::new();
    let _ = reader.read_line(&mut answer).await;
    standin
        .after_challenge
        .lock()
        .unwrap()
        .push(answer.trim_end_matches(['\r', '\n']).to_string());
    Err(format!("for {} = {decoded}", raw.trim()))
}

/// An SMTP stand-in demanding `AUTH XOAUTH2` as `user` with `token`; `AUTH
/// PLAIN` and `AUTH LOGIN` are refused.
pub async fn smtp_xoauth2_standin(user: &str, token: &str) -> StandIn {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind smtp");
    let addr = listener.local_addr().unwrap();
    let standin = StandIn::new(
        addr,
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    let shared = standin.clone();
    let expected = xoauth2_string(user, token);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            shared
                .accepted
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);
            let standin = shared.clone();
            let expected = expected.clone();
            tokio::spawn(async move {
                let _ = write.write_all(b"220 smtp.stand-in ESMTP ready\r\n").await;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let line = line.trim_end_matches(['\r', '\n']).to_string();
                    let upper = line.to_ascii_uppercase();
                    let reply = if let Some(raw) = upper
                        .starts_with("AUTH XOAUTH2 ")
                        .then(|| line["AUTH XOAUTH2 ".len()..].to_string())
                    {
                        standin
                            .commands
                            .lock()
                            .unwrap()
                            .push("AUTH XOAUTH2".to_string());
                        match xoauth2_exchange(
                            &mut reader,
                            &mut write,
                            &raw,
                            &expected,
                            &standin,
                            "334 ",
                        )
                        .await
                        {
                            Ok(()) => "235 2.7.0 Accepted\r\n".to_string(),
                            Err(echo) => format!("{XOAUTH2_SMTP_REFUSAL} {echo}\r\n"),
                        }
                    } else {
                        standin.commands.lock().unwrap().push(line.clone());
                        if upper.starts_with("EHLO") {
                            "250-smtp.stand-in\r\n250 AUTH XOAUTH2 PLAIN LOGIN\r\n".to_string()
                        } else if upper.starts_with("AUTH ") {
                            format!("{SMTP_REFUSAL}\r\n")
                        } else if upper == "QUIT" {
                            let _ = write.write_all(b"221 2.0.0 bye\r\n").await;
                            break;
                        } else {
                            "502 5.5.2 command not implemented by the stand-in\r\n".to_string()
                        }
                    };
                    if write.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    standin
}

/// A connector that records every endpoint it is asked to admit and
/// connects IMAP to `imap` and SMTP to `smtp` in plaintext, whatever host
/// and port the settings name; `STARTTLS` is the identity.
pub struct RedirectConnector {
    pub imap: SocketAddr,
    pub smtp: SocketAddr,
    pub admitted: Arc<Mutex<Vec<MailTarget>>>,
}

impl RedirectConnector {
    pub fn new(imap: SocketAddr, smtp: SocketAddr) -> Self {
        Self {
            imap,
            smtp,
            admitted: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub fn admitted(&self) -> Vec<MailTarget> {
        self.admitted.lock().unwrap().clone()
    }
}

#[async_trait]
impl MailConnector for RedirectConnector {
    async fn admit(&self, target: MailTarget) -> Result<AdmittedTarget, AdmissionError> {
        self.admitted.lock().unwrap().push(target.clone());
        let addr = match target.protocol {
            MailProtocol::Imap => self.imap,
            MailProtocol::Smtp => self.smtp,
        };
        Ok(AdmittedTarget {
            target,
            addrs: vec![addr],
        })
    }

    async fn connect(&self, admitted: &AdmittedTarget) -> std::io::Result<BoxedMailStream> {
        let stream = TcpStream::connect(admitted.addrs[0]).await?;
        Ok(Box::new(stream))
    }

    async fn start_tls(
        &self,
        stream: BoxedMailStream,
        _host: &str,
    ) -> std::io::Result<BoxedMailStream> {
        Ok(stream)
    }
}

/// Whether any recorded SMTP command would submit a message.
pub fn smtp_submitted_a_message(commands: &[String]) -> bool {
    commands.iter().any(|c| {
        let u = c.to_ascii_uppercase();
        u.starts_with("MAIL FROM") || u.starts_with("RCPT TO") || u == "DATA"
    })
}

// ---------------------------------------------------------------------------
// An IMAP stand-in with a mailbox
// ---------------------------------------------------------------------------

/// The `UIDVALIDITY` the mailbox stand-in reports.
pub const UIDVALIDITY: u32 = 7;

/// One message of the stand-in's `INBOX`.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub uid: u32,
    pub flags: Vec<String>,
    /// As IMAP writes it: `06-Oct-2026 14:02:11 +0000`.
    pub internal_date: String,
    /// The whole message, CRLF line endings.
    pub raw: String,
}

impl StoredMessage {
    /// A message from header lines and a body, joined with CRLF.
    pub fn new(
        uid: u32,
        flags: &[&str],
        internal_date: &str,
        headers: &[&str],
        body: &str,
    ) -> Self {
        let mut raw = headers.join("\r\n");
        raw.push_str("\r\n\r\n");
        raw.push_str(&body.replace("\r\n", "\n").replace('\n', "\r\n"));
        Self {
            uid,
            flags: flags.iter().map(|f| f.to_string()).collect(),
            internal_date: internal_date.to_string(),
            raw,
        }
    }
}

/// A running mailbox stand-in: the stand-in and its messages.
#[derive(Clone)]
pub struct MailboxStandIn {
    pub standin: StandIn,
    pub messages: Arc<Mutex<Vec<StoredMessage>>>,
}

impl MailboxStandIn {
    pub fn port(&self) -> u16 {
        self.standin.port()
    }
    pub fn commands(&self) -> Vec<String> {
        self.standin.commands()
    }
    pub fn connections(&self) -> usize {
        self.standin.connections()
    }
    /// The flags message `uid` holds now.
    pub fn flags_of(&self, uid: u32) -> Vec<String> {
        self.messages
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.uid == uid)
            .map(|m| m.flags.clone())
            .unwrap_or_default()
    }
}

/// The header fields of a raw message, folded lines joined.
fn standin_headers(raw: &str) -> Vec<(String, String)> {
    let head = raw.split("\r\n\r\n").next().unwrap_or("");
    let mut out: Vec<(String, String)> = Vec::new();
    for line in head.split("\r\n") {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, v)) = out.last_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
        } else if let Some((n, v)) = line.split_once(':') {
            out.push((n.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn header_contains(m: &StoredMessage, name: &str, needle: &str) -> bool {
    standin_headers(&m.raw)
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case(name) && contains_ci(v, needle))
}

fn has_flag_ci(flags: &[String], flag: &str) -> bool {
    flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
}

/// `1-Oct-2026` or `06-Oct-2026 ...` as (year, month, day).
fn imap_date(s: &str) -> Option<(i32, u32, u32)> {
    let date = s.split_whitespace().next()?;
    let mut parts = date.split('-');
    let day: u32 = parts.next()?.trim().parse().ok()?;
    let month = match parts.next()?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i32 = parts.next()?.parse().ok()?;
    Some((year, month, day))
}

fn uid_set(set: &str) -> Vec<u32> {
    set.split(',')
        .filter_map(|u| u.trim().parse().ok())
        .collect()
}

/// One search key at `at` (and its arguments), for `m`.
fn search_key(tokens: &[String], at: &mut usize, m: &StoredMessage) -> bool {
    let Some(key) = tokens.get(*at).map(|k| k.to_ascii_uppercase()) else {
        return true;
    };
    *at += 1;
    let mut arg = || {
        let a = tokens.get(*at).cloned().unwrap_or_default();
        *at += 1;
        a
    };
    match key.as_str() {
        "ALL" => true,
        "SEEN" => has_flag_ci(&m.flags, "\\Seen"),
        "UNSEEN" => !has_flag_ci(&m.flags, "\\Seen"),
        "FLAGGED" => has_flag_ci(&m.flags, "\\Flagged"),
        "CHARSET" => {
            arg();
            true
        }
        "TEXT" => contains_ci(&m.raw, &arg()),
        "FROM" => header_contains(m, "From", &arg()),
        "SINCE" => {
            let since = imap_date(&arg());
            since.is_some() && imap_date(&m.internal_date) >= since
        }
        "HEADER" => {
            let name = arg();
            let needle = arg();
            header_contains(m, &name, &needle)
        }
        "UID" => uid_set(&arg()).contains(&m.uid),
        "OR" => {
            let a = search_key(tokens, at, m);
            let b = search_key(tokens, at, m);
            a || b
        }
        _ => false,
    }
}

/// The header block of `raw` holding only `fields`, as `HEADER.FIELDS`
/// answers it.
fn header_fields(raw: &str, fields: &[String]) -> String {
    let mut out = String::new();
    for (n, v) in standin_headers(raw) {
        if fields.iter().any(|f| f.eq_ignore_ascii_case(&n)) {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
    }
    out.push_str("\r\n");
    out
}

/// An IMAP stand-in accepting `user`/`password`, holding `messages` in
/// `INBOX` and reporting `PERMANENTFLAGS (<permanent_flags>)`.
pub async fn imap_mailbox_standin(
    user: &str,
    password: &str,
    messages: Vec<StoredMessage>,
    permanent_flags: &str,
) -> MailboxStandIn {
    mailbox_standin(
        StandInAuth::Login {
            user: user.to_string(),
            password: password.to_string(),
        },
        messages,
        permanent_flags,
    )
    .await
}

/// The mailbox stand-in demanding SASL `XOAUTH2` as `user` with `token`:
/// `LOGIN` is refused.
pub async fn imap_xoauth2_standin(
    user: &str,
    token: &str,
    messages: Vec<StoredMessage>,
    permanent_flags: &str,
) -> MailboxStandIn {
    mailbox_standin(
        StandInAuth::XOAuth2 {
            user: user.to_string(),
            token: token.to_string(),
        },
        messages,
        permanent_flags,
    )
    .await
}

async fn mailbox_standin(
    auth: StandInAuth,
    messages: Vec<StoredMessage>,
    permanent_flags: &str,
) -> MailboxStandIn {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind imap");
    let addr = listener.local_addr().unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let standin = StandIn::new(addr, commands.clone(), accepted.clone());
    let store = Arc::new(Mutex::new(messages));
    let (seen, count, mailbox) = (commands.clone(), accepted.clone(), store.clone());
    let permanent = permanent_flags.to_string();
    let shared = standin.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);
            let seen = seen.clone();
            let mailbox = mailbox.clone();
            let (auth, permanent, standin) = (auth.clone(), permanent.clone(), shared.clone());
            tokio::spawn(async move {
                let _ = write
                    .write_all(b"* OK IMAP4rev1 mailbox stand-in ready\r\n")
                    .await;
                let mut logged_in = false;
                while let Some(cmd) = read_imap_command(&mut reader, &mut write).await {
                    seen.lock().unwrap().push(cmd.clone());
                    let mut parts = cmd.splitn(3, ' ');
                    let tag = parts.next().unwrap_or("*").to_string();
                    let verb = parts.next().unwrap_or("").to_ascii_uppercase();
                    let rest = parts.next().unwrap_or("").to_string();
                    let mut reply: Vec<u8> = Vec::new();
                    match verb.as_str() {
                        "LOGIN" => {
                            let args = imap_args(&rest);
                            match &auth {
                                StandInAuth::Login { user, password }
                                    if args.len() == 2
                                        && &args[0] == user
                                        && &args[1] == password =>
                                {
                                    logged_in = true;
                                    reply.extend(format!("{tag} OK LOGIN completed\r\n").bytes());
                                }
                                StandInAuth::Login { .. } => {
                                    reply.extend(format!("{tag} {IMAP_REFUSAL}\r\n").bytes());
                                }
                                StandInAuth::XOAuth2 { .. } => reply.extend(
                                    format!("{tag} NO [ALERT] LOGIN is disabled: use XOAUTH2\r\n")
                                        .bytes(),
                                ),
                            }
                        }
                        "AUTHENTICATE" if rest.eq_ignore_ascii_case("XOAUTH2") => match &auth {
                            StandInAuth::XOAuth2 { user, token } => {
                                let _ = write.write_all(b"+ \r\n").await;
                                let mut raw = String::new();
                                if reader.read_line(&mut raw).await.unwrap_or(0) == 0 {
                                    break;
                                }
                                let expected = xoauth2_string(user, token);
                                match xoauth2_exchange(
                                    &mut reader,
                                    &mut write,
                                    &raw,
                                    &expected,
                                    &standin,
                                    "+ ",
                                )
                                .await
                                {
                                    Ok(()) => {
                                        logged_in = true;
                                        reply.extend(
                                            format!("{tag} OK AUTHENTICATE completed\r\n").bytes(),
                                        );
                                    }
                                    Err(echo) => reply.extend(
                                        format!("{tag} {XOAUTH2_IMAP_REFUSAL} {echo}\r\n").bytes(),
                                    ),
                                }
                            }
                            StandInAuth::Login { .. } => {
                                reply.extend(format!("{tag} NO XOAUTH2 is not offered\r\n").bytes())
                            }
                        },
                        "LOGOUT" => {
                            let _ = write
                                .write_all(format!("* BYE logging out\r\n{tag} OK\r\n").as_bytes())
                                .await;
                            break;
                        }
                        _ if !logged_in => {
                            reply.extend(format!("{tag} BAD not authenticated\r\n").bytes());
                        }
                        "SELECT" | "EXAMINE" => {
                            let n = mailbox.lock().unwrap().len();
                            let mode = if verb == "SELECT" {
                                "READ-WRITE"
                            } else {
                                "READ-ONLY"
                            };
                            reply.extend(
                                format!(
                                    "* {n} EXISTS\r\n* OK [UIDVALIDITY {UIDVALIDITY}] UIDs valid\r\n* OK [PERMANENTFLAGS ({permanent})] flags kept\r\n{tag} OK [{mode}] {verb} completed\r\n"
                                )
                                .bytes(),
                            );
                        }
                        "UID" => {
                            let (sub, args) = rest.split_once(' ').unwrap_or((rest.as_str(), ""));
                            match sub.to_ascii_uppercase().as_str() {
                                "SEARCH" => {
                                    let tokens = imap_args(args);
                                    let messages = mailbox.lock().unwrap().clone();
                                    let mut found = Vec::new();
                                    for m in &messages {
                                        let mut at = 0;
                                        let mut all = true;
                                        while at < tokens.len() {
                                            all &= search_key(&tokens, &mut at, m);
                                        }
                                        if all {
                                            found.push(m.uid.to_string());
                                        }
                                    }
                                    reply.extend(
                                        format!(
                                            "* SEARCH {}\r\n{tag} OK SEARCH completed\r\n",
                                            found.join(" ")
                                        )
                                        .bytes(),
                                    );
                                }
                                "FETCH" => {
                                    let (set, items) = args.split_once(' ').unwrap_or((args, ""));
                                    let wanted = uid_set(set);
                                    let upper = items.to_ascii_uppercase();
                                    let mut messages = mailbox.lock().unwrap();
                                    for (i, m) in messages.iter_mut().enumerate() {
                                        if !wanted.contains(&m.uid) {
                                            continue;
                                        }
                                        let whole_unpeeked = upper.contains("BODY[]")
                                            && !upper.contains("BODY.PEEK[]");
                                        if whole_unpeeked && !has_flag_ci(&m.flags, "\\Seen") {
                                            m.flags.push("\\Seen".to_string());
                                        }
                                        reply.extend(
                                            format!("* {} FETCH (UID {}", i + 1, m.uid).bytes(),
                                        );
                                        if upper.contains("FLAGS") {
                                            reply.extend(
                                                format!(" FLAGS ({})", m.flags.join(" ")).bytes(),
                                            );
                                        }
                                        if upper.contains("INTERNALDATE") {
                                            reply.extend(
                                                format!(" INTERNALDATE \"{}\"", m.internal_date)
                                                    .bytes(),
                                            );
                                        }
                                        if upper.contains("BODY.PEEK[]") || whole_unpeeked {
                                            reply.extend(
                                                format!(" BODY[] {{{}}}\r\n", m.raw.len()).bytes(),
                                            );
                                            reply.extend(m.raw.bytes());
                                        }
                                        if let Some(at) = upper.find("HEADER.FIELDS (") {
                                            let from = at + "HEADER.FIELDS (".len();
                                            let to = upper[from..]
                                                .find(')')
                                                .map(|t| from + t)
                                                .unwrap_or(upper.len());
                                            let fields: Vec<String> = items[from..to]
                                                .split_whitespace()
                                                .map(str::to_string)
                                                .collect();
                                            let block = header_fields(&m.raw, &fields);
                                            reply.extend(
                                                format!(
                                                    " BODY[HEADER.FIELDS ({})] {{{}}}\r\n",
                                                    fields.join(" "),
                                                    block.len()
                                                )
                                                .bytes(),
                                            );
                                            reply.extend(block.bytes());
                                        }
                                        reply.extend(b")\r\n");
                                    }
                                    reply.extend(format!("{tag} OK FETCH completed\r\n").bytes());
                                }
                                "STORE" => {
                                    let mut words = args.splitn(3, ' ');
                                    let set = uid_set(words.next().unwrap_or(""));
                                    let op = words.next().unwrap_or("").to_ascii_uppercase();
                                    let list = words.next().unwrap_or("");
                                    let flags: Vec<String> = list
                                        .trim()
                                        .trim_start_matches('(')
                                        .trim_end_matches(')')
                                        .split_whitespace()
                                        .map(str::to_string)
                                        .collect();
                                    let keeps_any =
                                        permanent.split_whitespace().any(|f| f == "\\*");
                                    let mut messages = mailbox.lock().unwrap();
                                    for (i, m) in messages.iter_mut().enumerate() {
                                        if !set.contains(&m.uid) {
                                            continue;
                                        }
                                        for f in &flags {
                                            let kept = f.starts_with('\\')
                                                || keeps_any
                                                || permanent
                                                    .split_whitespace()
                                                    .any(|p| p.eq_ignore_ascii_case(f));
                                            if op.starts_with('+') {
                                                if kept && !has_flag_ci(&m.flags, f) {
                                                    m.flags.push(f.clone());
                                                }
                                            } else if op.starts_with('-') {
                                                m.flags.retain(|x| !x.eq_ignore_ascii_case(f));
                                            }
                                        }
                                        reply.extend(
                                            format!(
                                                "* {} FETCH (UID {} FLAGS ({}))\r\n",
                                                i + 1,
                                                m.uid,
                                                m.flags.join(" ")
                                            )
                                            .bytes(),
                                        );
                                    }
                                    reply.extend(format!("{tag} OK STORE completed\r\n").bytes());
                                }
                                _ => reply
                                    .extend(format!("{tag} BAD unknown UID command\r\n").bytes()),
                            }
                        }
                        _ => reply.extend(format!("{tag} BAD unknown command\r\n").bytes()),
                    }
                    if write.write_all(&reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    MailboxStandIn {
        standin,
        messages: store,
    }
}
