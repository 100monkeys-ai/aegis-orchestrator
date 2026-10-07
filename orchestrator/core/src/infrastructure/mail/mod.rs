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
//! **How a session authenticates** is a [`MailAuth`]: an SMTP-with-IMAP
//! mailbox logs in with its password (`LOGIN`, `AUTH PLAIN` or `AUTH
//! LOGIN`); a mailbox connected by OAuth authenticates with SASL `XOAUTH2`
//! (`AUTHENTICATE XOAUTH2`, `AUTH XOAUTH2`), the string
//! `user=<address>\x01auth=Bearer <token>\x01\x01` in base64 (AEGIS ADR-125's
//! Update of 2026-10-07 clauses 8 and 10). Google's mail hosts are fixed
//! here, keyed by the granted scope [`XOAUTH2_MAIL_SCOPE`], never by a
//! provider name ([`oauth_mailbox_settings`]).
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
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

pub use tls::{RustlsMailConnector, SystemResolver, TcpDialer};

/// How a mail session authenticates as the mailbox's `username`.
#[derive(Debug, Clone)]
pub enum MailAuth {
    /// The mailbox's password: `LOGIN`, `AUTH PLAIN` or `AUTH LOGIN`.
    Password(SensitiveString),
    /// An OAuth access token: SASL `XOAUTH2` (`AUTHENTICATE XOAUTH2`, `AUTH
    /// XOAUTH2`).
    XOAuth2(SensitiveString),
}

impl MailAuth {
    /// The SASL `XOAUTH2` initial response for `user`, in base64:
    /// `user=<user>\x01auth=Bearer <token>\x01\x01`. `None` for a password.
    pub fn xoauth2_response(&self, user: &str) -> Option<SensitiveString> {
        match self {
            MailAuth::Password(_) => None,
            MailAuth::XOAuth2(token) => Some(SensitiveString::new(STANDARD.encode(format!(
                "user={user}\x01auth=Bearer {}\x01\x01",
                token.expose()
            )))),
        }
    }

    /// Every form of the secret a server's reply could repeat, longest
    /// first: the password, or the token and the base64 response carrying
    /// it. A failure's text has each replaced by `[REDACTED]`.
    pub(crate) fn secret_forms(&self, user: &str) -> Vec<String> {
        let mut forms = match self {
            MailAuth::Password(password) => vec![password.expose().to_string()],
            MailAuth::XOAuth2(token) => vec![
                token.expose().to_string(),
                self.xoauth2_response(user)
                    .map(|r| r.expose().to_string())
                    .unwrap_or_default(),
            ],
        };
        forms.retain(|f| !f.is_empty());
        forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
        forms
    }
}

/// The scope Google grants for IMAP, POP and SMTP access ("The scope for
/// IMAP, POP, and SMTP access", Google's Gmail API scopes page): an OAuth
/// binding granted it is a mailbox reached over IMAP and SMTP with XOAUTH2
/// (AEGIS ADR-125's Update of 2026-10-07 clause 10, 8a).
pub const XOAUTH2_MAIL_SCOPE: &str = "https://mail.google.com/";

/// Whether an OAuth binding's granted scopes make it a mailbox: they hold
/// [`XOAUTH2_MAIL_SCOPE`] (clause 10, 8a). The one rule the callback and
/// [`oauth_mailbox_settings`] both read.
pub fn grants_mail_scope(granted_scopes: &[String]) -> bool {
    granted_scopes.iter().any(|s| s == XOAUTH2_MAIL_SCOPE)
}

/// The mailbox settings of an OAuth binding whose granted scopes hold
/// [`XOAUTH2_MAIL_SCOPE`]: Google's IMAP (`imap.gmail.com:993`, TLS) and
/// SMTP (`smtp.gmail.com:465`, TLS) servers, the account's `address` as the
/// address and the user both servers authenticate. `None` when the scope
/// was not granted. The rule is keyed by the scope, never by a provider
/// name (ADR-125's Update of 2026-10-04 clause 1; clause 10, 8a).
pub fn oauth_mailbox_settings(granted_scopes: &[String], address: &str) -> Option<MailboxSettings> {
    if !grants_mail_scope(granted_scopes) {
        return None;
    }
    Some(MailboxSettings {
        address: address.to_string(),
        display_name: None,
        imap_host: "imap.gmail.com".to_string(),
        imap_port: 993,
        imap_security: MailSecurity::Tls,
        smtp_host: "smtp.gmail.com".to_string(),
        smtp_port: 465,
        smtp_security: MailSecurity::Tls,
        username: address.to_string(),
    })
}

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

/// The check the credential service runs before storing a mailbox: an
/// `imap` mailbox with its password, or an OAuth mailbox with its token.
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

    /// The same two sessions authenticating with SASL `XOAUTH2` and the
    /// OAuth access token `token` as `settings.username` (AEGIS ADR-125's
    /// Update of 2026-10-07 clause 10, 8a).
    async fn check_xoauth2(
        &self,
        settings: &MailboxSettings,
        token: &SensitiveString,
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
        self.run(settings, &MailAuth::Password(password.clone()))
            .await
    }

    async fn check_xoauth2(
        &self,
        settings: &MailboxSettings,
        token: &SensitiveString,
    ) -> Result<(), MailboxCheckFailure> {
        self.run(settings, &MailAuth::XOAuth2(token.clone())).await
    }
}

impl SessionMailboxProbe {
    /// Both sessions, each endpoint admitted first, authenticating by `auth`.
    async fn run(
        &self,
        settings: &MailboxSettings,
        auth: &MailAuth,
    ) -> Result<(), MailboxCheckFailure> {
        let secrets = auth.secret_forms(&settings.username);
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
            .map_err(|e| wire::failure_of(MailProtocol::Imap, e, &secrets))?;
        let smtp_target = self
            .admit(MailTarget {
                protocol: MailProtocol::Smtp,
                host: settings.smtp_host.clone(),
                port: settings.smtp_port,
                security: settings.smtp_security.clone(),
            })
            .await
            .map_err(|e| wire::failure_of(MailProtocol::Smtp, e, &secrets))?;
        tokio::time::timeout(
            self.timeout,
            imap::check(self.connector.as_ref(), &imap_target, settings, auth),
        )
        .await
        .map_err(|_| timed_out(MailProtocol::Imap))??;
        tokio::time::timeout(
            self.timeout,
            smtp::check(self.connector.as_ref(), &smtp_target, settings, auth),
        )
        .await
        .map_err(|_| timed_out(MailProtocol::Smtp))??;
        Ok(())
    }
}
