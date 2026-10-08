// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Calendar accounts over CalDAV (AEGIS ADR-138 K1, K2, K4, K5)
//!
//! A calendar account is reached over CalDAV (RFC 4791) on WebDAV (RFC
//! 4918), with iCalendar bodies (RFC 5545), from inside the orchestrator and
//! never from an agent container. Nothing here is a client of any provider's
//! own calendar API, and no provider name is matched: a binding is a calendar
//! account because its granted scopes hold [`GOOGLE_CALDAV_SCOPE`]
//! ([`grants_calendar_scope`]), and Google's CalDAV root is fixed by that
//! scope ([`oauth_calendar_settings`]), as the mail scope fixes Google's mail
//! hosts.
//!
//! - [`xml`] writes the request bodies and reads WebDAV multistatus answers
//!   with `quick-xml` (no document type is expanded, so no external entity is
//!   ever fetched).
//! - [`ical`] reads and writes iCalendar: line unfolding and folding at 75
//!   octets, text escaping, `VEVENT` components, a `TZID` carried through as
//!   written. No recurrence engine: the server expands repeating events.
//! - [`caldav`] is the client: principal and home-set discovery, the
//!   calendars of a home set, a `calendar-query` with `time-range` and
//!   `expand`, and one event by `GET` with its `etag`.
//!
//! **The connect-time check** ([`CalendarProbe`], K5): before a calendar
//! account's token is stored, a `PROPFIND` (Depth 0) asks its principal for
//! `current-user-principal` and `calendar-home-set` with the token as a
//! bearer. A refusal, or an answer naming no home set, is a
//! [`CalendarCheckFailure`] carrying the server's status and reply, cut to
//! [`REPLY_MAX_CHARS`] characters with control characters removed and the
//! token redacted ([`shown_reply`]).
//!
//! Requests go over a [`CalDavTransport`]: production uses
//! [`ReqwestTransport`], which reaches only `https` URLs on port 443 and
//! follows no redirect; tests send every request to a loopback stand-in with
//! [`ReqwestTransport::to_origin`].
//!
//! **The address rule** (AEGIS ADR-138 K10, K10a; the mail check's rule of
//! ADR-125 D1). A calendar account connected by password names its own
//! server, so every request is admitted first: the server's host is resolved
//! once through a [`MailResolver`] (an IP literal is read as written), and
//! the request is refused, before any connection, when any address is not
//! public unicast ([`forbidden_address`]) or the port is not 443
//! ([`TransportFailure::NotAllowed`]). The request then goes only to the
//! addresses that were checked: no second resolution, no rebinding window.

pub mod caldav;
pub mod ical;
pub mod xml;

use crate::domain::credential::CalendarSettings;
use crate::domain::secrets::SensitiveString;
use crate::infrastructure::mail::guard::forbidden_address;
use crate::infrastructure::mail::{MailResolver, SystemResolver};
use async_trait::async_trait;
use base64::Engine as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// The scope a Google account grants for its calendars: an OAuth binding
/// granted it is a calendar account reached over CalDAV (AEGIS ADR-138 K3,
/// K4).
pub const GOOGLE_CALDAV_SCOPE: &str = "https://www.googleapis.com/auth/calendar";

/// The CalDAV root of an account granted [`GOOGLE_CALDAV_SCOPE`] (K4). It
/// belongs to the scope that made the binding a calendar account, not to a
/// provider name.
pub const GOOGLE_CALDAV_SERVER: &str = "https://apidata.googleusercontent.com/caldav/v2/";

/// The most characters of a server's reply a refusal carries.
pub const REPLY_MAX_CHARS: usize = 512;

/// The most bytes of one answer the client reads.
pub const RESPONSE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// The longest one request may take, connection included.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether an OAuth binding's granted scopes make it a calendar account: they
/// hold [`GOOGLE_CALDAV_SCOPE`] (K4). The one rule the callback and
/// [`oauth_calendar_settings`] both read.
pub fn grants_calendar_scope(granted_scopes: &[String]) -> bool {
    granted_scopes.iter().any(|s| s == GOOGLE_CALDAV_SCOPE)
}

/// The calendar settings of an OAuth binding whose granted scopes hold
/// [`GOOGLE_CALDAV_SCOPE`]: [`GOOGLE_CALDAV_SERVER`], the principal
/// `<address>/user` resolved against it, and the account's `address`.
/// `None` when the scope was not granted. Keyed by the scope, never by a
/// provider name (K4).
pub fn oauth_calendar_settings(
    granted_scopes: &[String],
    address: &str,
) -> Option<CalendarSettings> {
    if !grants_calendar_scope(granted_scopes) {
        return None;
    }
    Some(CalendarSettings {
        server: GOOGLE_CALDAV_SERVER.to_string(),
        principal: format!("{address}/user"),
        address: address.to_string(),
    })
}

/// How a CalDAV request authenticates: with an OAuth access token as a
/// bearer (K4), or with a user name and password by HTTP Basic (K10).
#[derive(Clone)]
pub enum CalDavAuth {
    Bearer(SensitiveString),
    Basic {
        username: String,
        password: SensitiveString,
    },
}

impl CalDavAuth {
    /// The `Authorization` header's value.
    fn header_value(&self) -> String {
        match self {
            CalDavAuth::Bearer(token) => format!("Bearer {}", token.expose()),
            CalDavAuth::Basic { .. } => format!("Basic {}", self.basic_credentials()),
        }
    }

    /// The Basic credentials, `username:password` in base64; empty for a
    /// bearer.
    fn basic_credentials(&self) -> String {
        match self {
            CalDavAuth::Bearer(_) => String::new(),
            CalDavAuth::Basic { username, password } => base64::engine::general_purpose::STANDARD
                .encode(format!("{username}:{}", password.expose())),
        }
    }

    /// Every secret this authentication carries, so no reply repeats one:
    /// the token, or the password and the encoded credentials.
    fn secrets(&self) -> Vec<String> {
        match self {
            CalDavAuth::Bearer(token) => vec![token.expose().to_string()],
            CalDavAuth::Basic { password, .. } => {
                vec![password.expose().to_string(), self.basic_credentials()]
            }
        }
    }
}

impl std::fmt::Debug for CalDavAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalDavAuth::Bearer(_) => f.write_str("Bearer([REDACTED])"),
            CalDavAuth::Basic { .. } => f.write_str("Basic([REDACTED])"),
        }
    }
}

/// One WebDAV or CalDAV request: its method, URL, headers (`Depth`,
/// `Content-Type`) and body. The `Authorization` header is the transport's,
/// from the [`CalDavAuth`] it is handed.
#[derive(Debug, Clone)]
pub struct CalDavRequest {
    pub method: &'static str,
    pub url: url::Url,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<String>,
}

/// A server's answer: its status, its headers (names in lower case) and its
/// body as text.
#[derive(Debug, Clone)]
pub struct CalDavResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl CalDavResponse {
    /// The first header named `name`, compared without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Why a request got no answer, in words that never carry the credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportFailure {
    /// The address rule refused the server before any connection: a port
    /// other than 443, or an address that is not public unicast.
    NotAllowed(String),
    /// The request failed before the server answered.
    Failed(String),
}

impl std::fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportFailure::NotAllowed(reason) | TransportFailure::Failed(reason) => {
                f.write_str(reason)
            }
        }
    }
}

/// Sends CalDAV requests. `Err` is what stopped a request before the server
/// answered.
#[async_trait]
pub trait CalDavTransport: Send + Sync {
    async fn send(
        &self,
        request: CalDavRequest,
        auth: &CalDavAuth,
    ) -> Result<CalDavResponse, TransportFailure>;
}

/// The one port a calendar server is reached on.
pub const CALDAV_PORT: u16 = 443;

/// The address [`ReqwestTransport::to_origin`] answers for every name: a
/// public unicast address (TEST-NET-1), so the address rule admits a test's
/// host without a lookup, and the request then goes to the stand-in.
const STAND_IN_PUBLIC_ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

/// A [`MailResolver`] answering one fixed address for every name.
struct FixedAddressResolver(IpAddr);

#[async_trait]
impl MailResolver for FixedAddressResolver {
    async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new(self.0, port)])
    }
}

/// The host a request names and the addresses it was admitted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    /// The host as the URL names it (a domain, or an IP literal's text).
    pub host: String,
    /// The addresses the request may go to, each checked.
    pub addrs: Vec<SocketAddr>,
}

/// Admit `url` by the address rule: an `https` URL on port 443 whose host,
/// resolved once by `resolver` (an IP literal is read as written), resolves
/// only to public unicast addresses. A refusal of the port or an address is
/// [`TransportFailure::NotAllowed`]; any other is
/// [`TransportFailure::Failed`]. Nothing is dialled.
pub async fn admit(
    url: &url::Url,
    resolver: &dyn MailResolver,
) -> Result<Admitted, TransportFailure> {
    if url.scheme() != "https" {
        return Err(TransportFailure::Failed(format!(
            "the calendar server must be reached over https, not {}",
            url.scheme()
        )));
    }
    let port = url.port_or_known_default().unwrap_or(CALDAV_PORT);
    if port != CALDAV_PORT {
        return Err(TransportFailure::NotAllowed(format!(
            "the calendar server must be reached on port {CALDAV_PORT}, not {port}"
        )));
    }
    let (host, literal) = match url.host() {
        Some(url::Host::Domain(domain)) => (domain.to_string(), None),
        Some(url::Host::Ipv4(ip)) => (ip.to_string(), Some(IpAddr::V4(ip))),
        Some(url::Host::Ipv6(ip)) => (ip.to_string(), Some(IpAddr::V6(ip))),
        None => {
            return Err(TransportFailure::Failed(
                "the calendar server's URL names no host".to_string(),
            ))
        }
    };
    let addrs = match literal {
        Some(ip) => vec![SocketAddr::new(ip, port)],
        None => resolver.resolve(&host, port).await.map_err(|e| {
            TransportFailure::Failed(format!(
                "the calendar server {host} could not be resolved: {e}"
            ))
        })?,
    };
    if addrs.is_empty() {
        return Err(TransportFailure::Failed(format!(
            "the calendar server {host} resolves to no address"
        )));
    }
    for addr in &addrs {
        if let Some(class) = forbidden_address(addr.ip()) {
            return Err(TransportFailure::NotAllowed(format!(
                "the calendar server {host} resolves to {}, {class}; calendar accounts are reached only on public servers",
                addr.ip()
            )));
        }
    }
    Ok(Admitted { host, addrs })
}

/// The production transport: `reqwest` over rustls, `https` on port 443
/// only, every request admitted by the address rule ([`admit`]) and sent
/// only to the addresses it checked, no redirect followed,
/// [`REQUEST_TIMEOUT`] per request and [`RESPONSE_MAX_BYTES`] per answer.
pub struct ReqwestTransport {
    /// The client of a stand-in transport; production builds one per
    /// request, pinned to the admitted addresses.
    client: reqwest::Client,
    /// Resolves a server's host for the address rule.
    resolver: Arc<dyn MailResolver>,
    /// Where every request goes instead of its URL's own scheme, host and
    /// port; `None` in production.
    origin: Option<url::Url>,
}

impl ReqwestTransport {
    /// The production transport, over the system resolver.
    pub fn new() -> Self {
        Self::with_resolver(Arc::new(SystemResolver))
    }

    /// The production transport over another resolver; the rule is the
    /// same. Tests answer a host's addresses with it.
    pub fn with_resolver(resolver: Arc<dyn MailResolver>) -> Self {
        Self {
            client: Self::builder()
                .build()
                .expect("the CalDAV client must build"),
            resolver,
            origin: None,
        }
    }

    /// A transport that sends every request to `origin` (its scheme, host
    /// and port), keeping the path and query: tests point it at a loopback
    /// stand-in, so the client's URLs stay the ones production uses. Each
    /// request is first admitted by the address rule, every name answering
    /// a public address.
    pub fn to_origin(origin: url::Url) -> Self {
        Self::to_origin_with_resolver(
            origin,
            Arc::new(FixedAddressResolver(IpAddr::V4(STAND_IN_PUBLIC_ADDRESS))),
        )
    }

    /// [`ReqwestTransport::to_origin`] with the address rule's names
    /// answered by `resolver`: a request it refuses never reaches the
    /// stand-in.
    pub fn to_origin_with_resolver(origin: url::Url, resolver: Arc<dyn MailResolver>) -> Self {
        Self {
            client: Self::builder()
                .build()
                .expect("the CalDAV client must build"),
            resolver,
            origin: Some(origin),
        }
    }

    fn builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
    }

    /// The client and URL a request is sent with: admitted by the address
    /// rule, then, in production, a client that reaches the host only at
    /// the admitted addresses.
    async fn route(&self, url: &url::Url) -> Result<(reqwest::Client, url::Url), TransportFailure> {
        let admitted = admit(url, self.resolver.as_ref()).await?;
        if self.origin.is_some() {
            let target = self.target(url).map_err(TransportFailure::Failed)?;
            return Ok((self.client.clone(), target));
        }
        let client = Self::builder()
            .resolve_to_addrs(&admitted.host, &admitted.addrs)
            .build()
            .map_err(|e| {
                TransportFailure::Failed(format!(
                    "the calendar client could not be built: {}",
                    e.without_url()
                ))
            })?;
        Ok((client, url.clone()))
    }

    /// The URL a request is sent to.
    fn target(&self, url: &url::Url) -> Result<url::Url, String> {
        let Some(origin) = &self.origin else {
            return Ok(url.clone());
        };
        let mut target = url.clone();
        target
            .set_scheme(origin.scheme())
            .map_err(|_| "the stand-in's scheme cannot be used".to_string())?;
        target
            .set_host(origin.host_str())
            .map_err(|e| format!("the stand-in's host cannot be used: {e}"))?;
        target
            .set_port(origin.port())
            .map_err(|_| "the stand-in's port cannot be used".to_string())?;
        Ok(target)
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CalDavTransport for ReqwestTransport {
    async fn send(
        &self,
        request: CalDavRequest,
        auth: &CalDavAuth,
    ) -> Result<CalDavResponse, TransportFailure> {
        let (client, target) = self.route(&request.url).await?;
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| {
            TransportFailure::Failed(format!("'{}' is not an HTTP method", request.method))
        })?;
        let mut builder = client
            .request(method, target)
            .header(reqwest::header::AUTHORIZATION, auth.header_value());
        for (name, value) in &request.headers {
            builder = builder.header(*name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let mut response = builder.send().await.map_err(|e| {
            TransportFailure::Failed(format!(
                "the calendar server could not be reached: {}",
                e.without_url()
            ))
        })?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            TransportFailure::Failed(format!(
                "the calendar server's answer could not be read: {}",
                e.without_url()
            ))
        })? {
            if body.len() + chunk.len() > RESPONSE_MAX_BYTES {
                return Err(TransportFailure::Failed(format!(
                    "the calendar server's answer is longer than {RESPONSE_MAX_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(CalDavResponse {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }
}

/// A server's reply as a refusal shows it: every secret of `auth` replaced
/// by `[REDACTED]`, control characters removed, surrounding space trimmed,
/// and at most [`REPLY_MAX_CHARS`] characters.
pub fn shown_reply(reply: &str, auth: &CalDavAuth) -> String {
    let mut text = reply.to_string();
    for secret in auth.secrets() {
        if !secret.is_empty() {
            text = text.replace(&secret, "[REDACTED]");
        }
    }
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(REPLY_MAX_CHARS)
        .collect()
}

/// A calendar account's check that did not pass: the server's status, if it
/// answered, and its reply (or what failed before it could), as
/// [`shown_reply`] shows it. `host_not_allowed` when the address rule
/// refused the server before any connection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("calendar check failed ({}): {reply}", status.map(|s| s.to_string()).unwrap_or_else(|| "no answer".to_string()))]
pub struct CalendarCheckFailure {
    pub status: Option<u16>,
    pub reply: String,
    pub host_not_allowed: bool,
}

/// The check the credential service runs before storing a calendar account
/// (K5).
#[async_trait]
pub trait CalendarProbe: Send + Sync {
    /// `PROPFIND` (Depth 0) on the account's principal for
    /// `current-user-principal` and `calendar-home-set`; `Ok` only when the
    /// server answers a multistatus naming a home set.
    async fn check(
        &self,
        settings: &CalendarSettings,
        auth: &CalDavAuth,
    ) -> Result<(), CalendarCheckFailure>;

    /// The check of a calendar account connected by password (K10): the
    /// account's principal and calendar user address read from `server` by
    /// [`caldav::CalDavClient::discover_account`], authenticated by `auth`.
    /// `Ok` only when a calendar home set is named.
    async fn discover_account(
        &self,
        server: &str,
        auth: &CalDavAuth,
    ) -> Result<caldav::AccountDiscovery, CalendarCheckFailure>;
}

/// The check over a [`CalDavTransport`]: the client's own discovery.
pub struct CalDavProbe {
    transport: Arc<dyn CalDavTransport>,
}

impl CalDavProbe {
    pub fn new(transport: Arc<dyn CalDavTransport>) -> Self {
        Self { transport }
    }

    /// The production check, over [`ReqwestTransport`].
    pub fn https() -> Self {
        Self::new(Arc::new(ReqwestTransport::new()))
    }
}

#[async_trait]
impl CalendarProbe for CalDavProbe {
    async fn check(
        &self,
        settings: &CalendarSettings,
        auth: &CalDavAuth,
    ) -> Result<(), CalendarCheckFailure> {
        let client = caldav::CalDavClient::new(self.transport.as_ref(), settings, auth)
            .map_err(|e| e.check_failure(auth))?;
        client
            .discover()
            .await
            .map(|_| ())
            .map_err(|e| e.check_failure(auth))
    }

    async fn discover_account(
        &self,
        server: &str,
        auth: &CalDavAuth,
    ) -> Result<caldav::AccountDiscovery, CalendarCheckFailure> {
        let settings = CalendarSettings {
            server: server.to_string(),
            principal: server.to_string(),
            address: String::new(),
        };
        let client = caldav::CalDavClient::new(self.transport.as_ref(), &settings, auth)
            .map_err(|e| e.check_failure(auth))?;
        client
            .discover_account()
            .await
            .map_err(|e| e.check_failure(auth))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calendar_scope_makes_a_binding_a_calendar_account_and_fixes_googles_root() {
        let granted = vec![
            "email".to_string(),
            GOOGLE_CALDAV_SCOPE.to_string(),
            "openid".to_string(),
        ];
        assert!(grants_calendar_scope(&granted));
        assert_eq!(
            oauth_calendar_settings(&granted, "a@example.test"),
            Some(CalendarSettings {
                server: "https://apidata.googleusercontent.com/caldav/v2/".to_string(),
                principal: "a@example.test/user".to_string(),
                address: "a@example.test".to_string(),
            })
        );
        let mail_only = vec!["https://mail.google.com/".to_string(), "email".to_string()];
        assert!(!grants_calendar_scope(&mail_only));
        assert_eq!(oauth_calendar_settings(&mail_only, "a@example.test"), None);
        let narrower = vec!["https://www.googleapis.com/auth/calendar.readonly".to_string()];
        assert!(
            !grants_calendar_scope(&narrower),
            "a narrower scope is not the scope the record names"
        );
    }

    #[test]
    fn a_shown_reply_redacts_the_token_removes_control_characters_and_is_cut() {
        let auth = CalDavAuth::Bearer(SensitiveString::new("ya29.secret-token"));
        let reply = format!(
            "401 Unauthorized\r\nWWW-Authenticate: Bearer ya29.secret-token\u{7}{}",
            "x".repeat(600)
        );
        let shown = shown_reply(&reply, &auth);
        assert!(
            !shown.contains("ya29.secret-token"),
            "the token is not redacted: {shown}"
        );
        assert!(
            shown.contains("[REDACTED]"),
            "no redaction is shown: {shown}"
        );
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert_eq!(shown.chars().count(), REPLY_MAX_CHARS);
        assert_eq!(format!("{auth:?}"), "Bearer([REDACTED])");
    }
}
