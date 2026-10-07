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
//!
//! The mail tools' sessions (`EXAMINE`, `SELECT`, `UID SEARCH`, `UID
//! FETCH`, `UID STORE`) are [`session`], opened the same way over the same
//! connector.
//!
//! **What may be reached** is the production connector's [`guard`]: the
//! mail ports only, and public unicast addresses only, each host resolved
//! once and connected to at the addresses that were checked. Both endpoints
//! are admitted before either session opens, so a refusal opens no
//! connection.

pub mod guard;
pub mod imap;
pub mod session;
pub mod smtp;
pub mod tls;
mod wire;

use crate::domain::credential::{MailSecurity, MailboxSettings};
use crate::domain::secrets::SensitiveString;
use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

pub use tls::{RustlsMailConnector, SystemResolver, TcpDialer};

/// A byte stream a mail session runs over: TCP, or TLS over TCP.
pub trait MailStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> MailStream for T {}

/// A boxed [`MailStream`], so plaintext and TLS streams share one type.
pub type BoxedMailStream = Box<dyn MailStream>;

/// One endpoint a session connects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailTarget {
    pub protocol: MailProtocol,
    pub host: String,
    pub port: u16,
    pub security: MailSecurity,
}

/// An endpoint the connector admitted, with the addresses it checked. A
/// connector that resolves connects only to `addrs`.
#[derive(Debug, Clone)]
pub struct AdmittedTarget {
    pub target: MailTarget,
    pub addrs: Vec<SocketAddr>,
}

/// Why an endpoint was not admitted.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// The rule refuses the endpoint; `field` is the setting it names.
    #[error("{reason}")]
    NotAllowed { field: &'static str, reason: String },
    /// The host could not be resolved.
    #[error("{0}")]
    Unresolvable(String),
}

/// Opens the connections a mail session needs.
#[async_trait]
pub trait MailConnector: Send + Sync {
    /// Decide whether `target` may be reached, resolving its host when the
    /// connector resolves. Called for both endpoints before either session.
    async fn admit(&self, target: MailTarget) -> Result<AdmittedTarget, AdmissionError>;

    /// Connect to an admitted endpoint. For [`MailSecurity::Tls`] the
    /// returned stream is TLS from the first byte; for
    /// [`MailSecurity::Starttls`] it is plaintext until
    /// [`start_tls`](Self::start_tls).
    async fn connect(&self, admitted: &AdmittedTarget) -> std::io::Result<BoxedMailStream>;

    /// Upgrade a plaintext stream to TLS after the server accepted
    /// `STARTTLS`, verifying the server's certificate for `host`.
    async fn start_tls(
        &self,
        stream: BoxedMailStream,
        host: &str,
    ) -> std::io::Result<BoxedMailStream>;
}

/// Resolves a host name to the addresses the production connector checks.
#[async_trait]
pub trait MailResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
}

/// Opens a TCP connection to one checked address.
#[async_trait]
pub trait MailDialer: Send + Sync {
    async fn dial(&self, addr: SocketAddr) -> std::io::Result<BoxedMailStream>;
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

/// Why a mailbox check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckFailureKind {
    /// The server refused, failed or could not be reached.
    Unreachable,
    /// The [`guard`] refused the endpoint before any connection; `field`
    /// is the setting it names (`imap_host`, `smtp_host`, `imap_port`,
    /// `smtp_port`).
    HostNotAllowed { field: &'static str },
}

/// A mail session that did not complete: the protocol and the server's
/// reply (or what failed before the server could reply), with the password
/// redacted, control characters removed and at most 512 characters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{protocol} session failed: {reply}")]
pub struct MailboxCheckFailure {
    pub protocol: MailProtocol,
    pub reply: String,
    pub kind: CheckFailureKind,
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
    /// Admit one endpoint within the session timeout.
    async fn admit(&self, target: MailTarget) -> Result<AdmittedTarget, AdmissionError> {
        let protocol = target.protocol;
        tokio::time::timeout(self.timeout, self.connector.admit(target))
            .await
            .map_err(|_| {
                AdmissionError::Unresolvable(format!(
                    "{} could not be resolved within {} seconds",
                    guard::field(protocol, true),
                    self.timeout.as_secs()
                ))
            })?
    }

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
            kind: CheckFailureKind::Unreachable,
        };
        // Both endpoints are admitted before either session opens, so an
        // endpoint the guard refuses is never preceded by a connection.
        let imap_target = self
            .admit(MailTarget {
                protocol: MailProtocol::Imap,
                host: settings.imap_host.clone(),
                port: settings.imap_port,
                security: settings.imap_security.clone(),
            })
            .await
            .map_err(|e| wire::failure_of(MailProtocol::Imap, e, password))?;
        let smtp_target = self
            .admit(MailTarget {
                protocol: MailProtocol::Smtp,
                host: settings.smtp_host.clone(),
                port: settings.smtp_port,
                security: settings.smtp_security.clone(),
            })
            .await
            .map_err(|e| wire::failure_of(MailProtocol::Smtp, e, password))?;
        tokio::time::timeout(
            self.timeout,
            imap::check(self.connector.as_ref(), &imap_target, settings, password),
        )
        .await
        .map_err(|_| timed_out(MailProtocol::Imap))??;
        tokio::time::timeout(
            self.timeout,
            smtp::check(self.connector.as_ref(), &smtp_target, settings, password),
        )
        .await
        .map_err(|_| timed_out(MailProtocol::Smtp))??;
        Ok(())
    }
}
