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

#![allow(dead_code)]

use aegis_orchestrator_core::infrastructure::mail::{
    AdmissionError, AdmittedTarget, BoxedMailStream, MailConnector, MailTarget,
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

/// A running stand-in: its address and every command line it received.
#[derive(Clone)]
pub struct StandIn {
    pub addr: SocketAddr,
    pub commands: Arc<Mutex<Vec<String>>>,
    /// Every connection accepted, whether or not it sent a command.
    pub accepted: Arc<std::sync::atomic::AtomicUsize>,
}

impl StandIn {
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
    StandIn {
        addr,
        commands,
        accepted,
    }
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
    StandIn {
        addr,
        commands,
        accepted,
    }
}

/// Whether any recorded SMTP command would submit a message.
pub fn smtp_submitted_a_message(commands: &[String]) -> bool {
    commands.iter().any(|c| {
        let u = c.to_ascii_uppercase();
        u.starts_with("MAIL FROM") || u.starts_with("RCPT TO") || u == "DATA"
    })
}
