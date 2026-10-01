// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Mailbox session checks (AEGIS ADR-125 D1)
//!
//! Before an SMTP-with-IMAP mailbox binding is stored, the orchestrator opens
//! one session of each kind with the settings the user supplied:
//!
//! - **IMAP:** greeting, `STARTTLS` when the security is `starttls`, `LOGIN`,
//!   `SELECT INBOX`, `LOGOUT` ([`imap`]).
//! - **SMTP:** greeting, `EHLO`, `STARTTLS` and a second `EHLO` when the
//!   security is `starttls`, `AUTH PLAIN` (or `AUTH LOGIN`), `QUIT`
//!   ([`smtp`]). No `MAIL`, `RCPT` or `DATA` is ever sent: no message leaves.
//!
//! A refusal by either server is a [`MailboxCheckFailure`] carrying the
//! server's own reply, which the route answers as `422 mailbox_unreachable`.
//! The password is written to the wire only inside the login command and is
//! never logged, never put in an error, and redacted from any reply that
//! repeats it.
//!
//! The sessions run over a [`MailConnector`]: production uses
//! [`RustlsMailConnector`] (rustls with the Mozilla roots of `webpki-roots`),
//! and tests use a plaintext connector against loopback stand-ins.

pub mod imap;
pub mod smtp;
pub mod tls;
mod wire;

use crate::domain::credential::{MailSecurity, MailboxSettings};
use crate::domain::secrets::SensitiveString;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

pub use tls::RustlsMailConnector;

/// A byte stream a mail session runs over: TCP, or TLS over TCP.
pub trait MailStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> MailStream for T {}

/// A boxed [`MailStream`], so plaintext and TLS streams share one type.
pub type BoxedMailStream = Box<dyn MailStream>;

/// Opens the connections a mail session needs.
#[async_trait]
pub trait MailConnector: Send + Sync {
    /// Connect to `host:port`. For [`MailSecurity::Tls`] the returned stream
    /// is TLS from the first byte; for [`MailSecurity::Starttls`] it is
    /// plaintext until [`start_tls`](Self::start_tls).
    async fn connect(
        &self,
        host: &str,
        port: u16,
        security: MailSecurity,
    ) -> std::io::Result<BoxedMailStream>;

    /// Upgrade a plaintext stream to TLS after the server accepted
    /// `STARTTLS`, verifying the server's certificate for `host`.
    async fn start_tls(
        &self,
        stream: BoxedMailStream,
        host: &str,
    ) -> std::io::Result<BoxedMailStream>;
}

/// Which session refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailProtocol {
    Imap,
    Smtp,
}

impl std::fmt::Display for MailProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MailProtocol::Imap => write!(f, "imap"),
            MailProtocol::Smtp => write!(f, "smtp"),
        }
    }
}

/// A mail session that did not complete: the protocol and the server's
/// reply (or what failed before the server could reply), with the password
/// redacted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{protocol} session failed: {reply}")]
pub struct MailboxCheckFailure {
    pub protocol: MailProtocol,
    pub reply: String,
}

/// The check the credential service runs before storing an `imap` mailbox.
#[async_trait]
pub trait MailboxProbe: Send + Sync {
    /// Open an IMAP session and then an SMTP session with `settings` and
    /// `password`; `Ok` only when both authenticate (and IMAP selects the
    /// inbox).
    async fn check(
        &self,
        settings: &MailboxSettings,
        password: &SensitiveString,
    ) -> Result<(), MailboxCheckFailure>;
}

/// The longest either session may take, connection included.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(30);

/// [`MailboxProbe`] that runs the two real sessions over a [`MailConnector`].
pub struct SessionMailboxProbe {
    connector: Arc<dyn MailConnector>,
    timeout: Duration,
}

impl SessionMailboxProbe {
    pub fn new(connector: Arc<dyn MailConnector>) -> Self {
        Self {
            connector,
            timeout: SESSION_TIMEOUT,
        }
    }

    /// The production probe: TLS by rustls, verified against the Mozilla
    /// root store.
    pub fn tls() -> Self {
        Self::new(Arc::new(RustlsMailConnector::new()))
    }
}

#[async_trait]
impl MailboxProbe for SessionMailboxProbe {
    async fn check(
        &self,
        settings: &MailboxSettings,
        password: &SensitiveString,
    ) -> Result<(), MailboxCheckFailure> {
        let timed_out = |protocol| MailboxCheckFailure {
            protocol,
            reply: format!("no answer within {} seconds", self.timeout.as_secs()),
        };
        tokio::time::timeout(
            self.timeout,
            imap::check(self.connector.as_ref(), settings, password),
        )
        .await
        .map_err(|_| timed_out(MailProtocol::Imap))??;
        tokio::time::timeout(
            self.timeout,
            smtp::check(self.connector.as_ref(), settings, password),
        )
        .await
        .map_err(|_| timed_out(MailProtocol::Smtp))??;
        Ok(())
    }
}
