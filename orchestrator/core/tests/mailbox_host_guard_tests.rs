// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The mailbox check reaches only public mail servers (AEGIS ADR-125 D1,
//! CORRECTION orchestrator-mailbox-credentials-C3).
//!
//! `POST /v1/credentials/mailboxes` makes the orchestrator open connections
//! to a host and port a user names. The production connector therefore
//! admits only the IMAP ports 993 and 143 and the SMTP ports 465 and 587,
//! resolves each host once, refuses the request when any resolved address is
//! not a public unicast address, and dials only the addresses it checked.
//! Both endpoints are admitted before either session starts, so a refusal
//! opens no connection.
//!
//! The resolver and dialer here are recording stand-ins wrapped by the real
//! production connector: nothing is dialled and no request leaves the
//! machine. One test uses the production connector exactly as the daemon
//! builds it, against `127.0.0.1` and `localhost`, and is refused before any
//! connection.

#[path = "support/mail_standins.rs"]
mod mail_standins;

use aegis_orchestrator_core::application::credential_service::{
    CreateImapMailboxCommand, CredentialError, CredentialManagementService, OAuthProviderRegistry,
    StandardCredentialManagementService,
};
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialProvider,
    CredentialScope, GrantTarget, MailSecurity, MailboxSettings, OAuthPendingState,
    UserCredentialBinding,
};
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::mail::guard::forbidden_address;
use aegis_orchestrator_core::infrastructure::mail::{
    BoxedMailStream, CheckFailureKind, MailDialer, MailResolver, MailboxProbe, RustlsMailConnector,
    SessionMailboxProbe,
};
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mail_standins::PlainConnector;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Recording resolver and dialer
// ---------------------------------------------------------------------------

/// Answers every name with fixed addresses and counts the lookups.
struct FixedResolver {
    answers: HashMap<String, Vec<IpAddr>>,
    lookups: Mutex<Vec<String>>,
}

impl FixedResolver {
    fn new(answers: &[(&str, &[&str])]) -> Arc<Self> {
        Arc::new(Self {
            answers: answers
                .iter()
                .map(|(h, ips)| {
                    (
                        h.to_string(),
                        ips.iter().map(|i| i.parse().unwrap()).collect(),
                    )
                })
                .collect(),
            lookups: Mutex::new(Vec::new()),
        })
    }
    fn lookups(&self) -> Vec<String> {
        self.lookups.lock().unwrap().clone()
    }
}

#[async_trait]
impl MailResolver for FixedResolver {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        self.lookups.lock().unwrap().push(host.to_string());
        Ok(self
            .answers
            .get(host)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect())
    }
}

/// Records every address it is asked to dial and connects to none.
#[derive(Default)]
struct RecordingDialer {
    dialled: Mutex<Vec<SocketAddr>>,
}

impl RecordingDialer {
    fn dialled(&self) -> Vec<SocketAddr> {
        self.dialled.lock().unwrap().clone()
    }
}

#[async_trait]
impl MailDialer for RecordingDialer {
    async fn dial(&self, addr: SocketAddr) -> std::io::Result<BoxedMailStream> {
        self.dialled.lock().unwrap().push(addr);
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "recording dialer: no connection is made",
        ))
    }
}

fn settings(imap: (&str, u16), smtp: (&str, u16)) -> MailboxSettings {
    MailboxSettings {
        address: "outreach@example.test".to_string(),
        display_name: None,
        imap_host: imap.0.to_string(),
        imap_port: imap.1,
        imap_security: MailSecurity::Tls,
        smtp_host: smtp.0.to_string(),
        smtp_port: smtp.1,
        smtp_security: MailSecurity::Starttls,
        username: "outreach@example.test".to_string(),
    }
}

fn guarded(resolver: Arc<FixedResolver>, dialer: Arc<RecordingDialer>) -> SessionMailboxProbe {
    SessionMailboxProbe::new(Arc::new(RustlsMailConnector::with_network(
        resolver, dialer,
    )))
}

fn password() -> SensitiveString {
    SensitiveString::new("Mk7-guard-password")
}

/// The field a refusal names, or a panic naming what came back instead.
async fn refused_field(probe: &SessionMailboxProbe, s: &MailboxSettings) -> String {
    let failure = probe
        .check(s, &password())
        .await
        .expect_err("the guard must refuse");
    match failure.kind {
        CheckFailureKind::HostNotAllowed { field } => field.to_string(),
        other => panic!("expected HostNotAllowed, got {other:?}: {}", failure.reply),
    }
}

// ---------------------------------------------------------------------------
// The address classes
// ---------------------------------------------------------------------------

#[test]
fn every_non_public_address_class_is_forbidden_and_public_unicast_is_not() {
    let forbidden = [
        "127.0.0.1",
        "127.255.0.9",
        "10.0.0.5",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.1.10",
        "169.254.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "100.127.255.254",
        "0.0.0.0",
        "255.255.255.255",
        "224.0.0.1",
        "239.255.255.250",
        "::1",
        "::",
        "fe80::1",
        "febf::1",
        "fc00::1",
        "fd12:3456::1",
        "ff02::1",
        "::ffff:127.0.0.1",
        "::ffff:10.1.2.3",
        "::ffff:169.254.169.254",
        "::ffff:192.168.0.1",
        "::ffff:100.64.1.1",
    ];
    for ip in forbidden {
        let parsed: IpAddr = ip.parse().unwrap();
        assert!(
            forbidden_address(parsed).is_some(),
            "{ip} must be forbidden"
        );
    }
    let public = [
        "8.8.8.8",
        "142.250.115.108",
        "172.15.255.255",
        "172.32.0.1",
        "100.63.255.255",
        "100.128.0.1",
        "169.253.255.255",
        "2607:f8b0:4004:c1b::6c",
        "2001:4860:4860::8888",
        "::ffff:8.8.8.8",
    ];
    for ip in public {
        let parsed: IpAddr = ip.parse().unwrap();
        assert_eq!(forbidden_address(parsed), None, "{ip} must be allowed");
    }
}

// ---------------------------------------------------------------------------
// The production connector as the daemon builds it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_production_connector_refuses_127_0_0_1_and_a_name_resolving_to_it() {
    // Exactly as the daemon builds it: the system resolver (here only
    // /etc/hosts answers `localhost`; every other host is a literal, so no
    // DNS query leaves the machine) and the TCP dialer.
    let probe = SessionMailboxProbe::tls();
    for host in ["127.0.0.1", "localhost"] {
        let s = settings((host, 993), ("203.0.113.20", 587));
        assert_eq!(refused_field(&probe, &s).await, "imap_host", "{host}");
        let s = settings(("203.0.113.10", 993), (host, 465));
        assert_eq!(refused_field(&probe, &s).await, "smtp_host", "{host}");
    }
}

// ---------------------------------------------------------------------------
// Resolution, admission and dialling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_host_is_resolved_once_and_only_the_checked_addresses_are_dialled() {
    let resolver = FixedResolver::new(&[
        ("imap.public.test", &["203.0.113.10"]),
        ("smtp.public.test", &["203.0.113.20"]),
    ]);
    let dialer = Arc::new(RecordingDialer::default());
    let probe = guarded(resolver.clone(), dialer.clone());
    let s = settings(("imap.public.test", 993), ("smtp.public.test", 587));
    let failure = probe
        .check(&s, &password())
        .await
        .expect_err("nothing answers");
    assert!(matches!(failure.kind, CheckFailureKind::Unreachable));
    assert_eq!(
        resolver.lookups(),
        vec![
            "imap.public.test".to_string(),
            "smtp.public.test".to_string()
        ],
        "each host is resolved exactly once"
    );
    assert_eq!(
        dialer.dialled(),
        vec!["203.0.113.10:993".parse::<SocketAddr>().unwrap()],
        "the dial goes to the address that was checked"
    );
}

#[tokio::test]
async fn a_host_with_any_forbidden_address_is_refused_and_nothing_is_dialled() {
    for bad in [
        "10.0.0.7",
        "169.254.169.254",
        "::1",
        "fd00::7",
        "::ffff:127.0.0.1",
        "100.64.3.3",
    ] {
        let resolver = FixedResolver::new(&[
            ("imap.mixed.test", &["203.0.113.10", bad]),
            ("smtp.public.test", &["203.0.113.20"]),
        ]);
        let dialer = Arc::new(RecordingDialer::default());
        let probe = guarded(resolver, dialer.clone());
        let s = settings(("imap.mixed.test", 993), ("smtp.public.test", 587));
        assert_eq!(refused_field(&probe, &s).await, "imap_host", "{bad}");
        assert!(dialer.dialled().is_empty(), "{bad}: {:?}", dialer.dialled());
    }
}

#[tokio::test]
async fn a_forbidden_smtp_host_is_refused_before_the_imap_session_opens() {
    let resolver = FixedResolver::new(&[
        ("imap.public.test", &["203.0.113.10"]),
        ("smtp.internal.test", &["172.20.0.5"]),
    ]);
    let dialer = Arc::new(RecordingDialer::default());
    let probe = guarded(resolver, dialer.clone());
    let s = settings(("imap.public.test", 993), ("smtp.internal.test", 587));
    assert_eq!(refused_field(&probe, &s).await, "smtp_host");
    assert!(dialer.dialled().is_empty(), "{:?}", dialer.dialled());
}

#[tokio::test]
async fn an_ip_literal_host_is_held_to_the_same_rule() {
    for (literal, field_on_imap) in [
        ("10.0.0.5", true),
        ("::1", true),
        ("::ffff:169.254.169.254", true),
        ("[fe80::1]", true),
        ("203.0.113.10", false),
    ] {
        let resolver = FixedResolver::new(&[("smtp.public.test", &["203.0.113.20"])]);
        let dialer = Arc::new(RecordingDialer::default());
        let probe = guarded(resolver.clone(), dialer.clone());
        let s = settings((literal, 993), ("smtp.public.test", 587));
        let failure = probe.check(&s, &password()).await.expect_err("no server");
        if field_on_imap {
            assert!(
                matches!(failure.kind, CheckFailureKind::HostNotAllowed { field } if field == "imap_host"),
                "{literal}: {failure:?}"
            );
            assert!(dialer.dialled().is_empty(), "{literal}");
        } else {
            assert!(
                matches!(failure.kind, CheckFailureKind::Unreachable),
                "{literal}"
            );
            assert_eq!(
                dialer.dialled(),
                vec!["203.0.113.10:993".parse::<SocketAddr>().unwrap()]
            );
        }
        // A literal is never sent to the resolver.
        assert!(
            !resolver
                .lookups()
                .iter()
                .any(|h| h.contains(literal.trim_matches(['[', ']']))),
            "{literal} was resolved: {:?}",
            resolver.lookups()
        );
    }
}

#[tokio::test]
async fn only_the_mail_ports_are_admitted() {
    let resolver = FixedResolver::new(&[
        ("imap.public.test", &["203.0.113.10"]),
        ("smtp.public.test", &["203.0.113.20"]),
    ]);
    for (imap_port, smtp_port, refused) in [
        (2525, 587, Some("imap_port")),
        (22, 587, Some("imap_port")),
        (993, 25, Some("smtp_port")),
        (993, 8080, Some("smtp_port")),
        (143, 465, None),
        (993, 587, None),
    ] {
        let dialer = Arc::new(RecordingDialer::default());
        let probe = guarded(resolver.clone(), dialer.clone());
        let s = settings(
            ("imap.public.test", imap_port),
            ("smtp.public.test", smtp_port),
        );
        let failure = probe.check(&s, &password()).await.expect_err("no server");
        match refused {
            Some(field) => {
                assert!(
                    matches!(failure.kind, CheckFailureKind::HostNotAllowed { field: f } if f == field),
                    "{imap_port}/{smtp_port}: {failure:?}"
                );
                assert!(failure.reply.contains(field), "{}", failure.reply);
                assert!(dialer.dialled().is_empty());
            }
            None => {
                assert!(matches!(failure.kind, CheckFailureKind::Unreachable));
                assert_eq!(dialer.dialled().len(), 1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Through the credential service
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Repo {
    rows: RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>,
}

#[async_trait]
impl CredentialBindingRepository for Repo {
    async fn save(&self, b: &UserCredentialBinding) -> anyhow::Result<()> {
        self.rows.write().await.insert(b.id, b.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        Ok(self.rows.read().await.get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        _t: &TenantId,
        _o: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok(self.rows.read().await.values().cloned().collect())
    }
    async fn find_active_grants_for_target(
        &self,
        _t: &TenantId,
        _o: &str,
        _p: &CredentialProvider,
        _g: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>> {
        Ok(Vec::new())
    }
    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
        self.rows.write().await.remove(id);
        Ok(())
    }
    async fn save_oauth_state(
        &self,
        _s: &str,
        _b: &CredentialBindingId,
        _v: &str,
        _r: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn find_oauth_state(&self, _s: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(None)
    }
    async fn delete_oauth_state(&self, _s: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_expired_oauth_states(&self, _o: DateTime<Utc>) -> anyhow::Result<u64> {
        Ok(0)
    }
}

fn service(probe: SessionMailboxProbe) -> (StandardCredentialManagementService, Arc<Repo>) {
    let repo = Arc::new(Repo::default());
    let bus = Arc::new(EventBus::new(16));
    let secrets = Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::new()),
        bus.clone(),
    ));
    let svc = StandardCredentialManagementService::new(
        repo.clone(),
        secrets,
        bus,
        Arc::new(OAuthProviderRegistry::new()),
    )
    .with_mailbox_probe(Arc::new(probe));
    (svc, repo)
}

fn command(s: MailboxSettings) -> CreateImapMailboxCommand {
    CreateImapMailboxCommand {
        owner_user_id: "guard-owner".to_string(),
        tenant_id: TenantId::for_consumer_user("guard-owner").unwrap(),
        label: None,
        scope: CredentialScope::Personal,
        settings: s,
        password: password(),
    }
}

#[tokio::test]
async fn the_service_answers_mailbox_host_not_allowed_naming_the_field_and_stores_nothing() {
    let resolver = FixedResolver::new(&[
        ("imap.public.test", &["203.0.113.10"]),
        ("metadata.internal.test", &["169.254.169.254"]),
    ]);
    let dialer = Arc::new(RecordingDialer::default());
    let (svc, repo) = service(guarded(resolver, dialer.clone()));
    let err = svc
        .create_imap_mailbox(command(settings(
            ("imap.public.test", 993),
            ("metadata.internal.test", 587),
        )))
        .await
        .expect_err("refused");
    match err.downcast_ref::<CredentialError>() {
        Some(CredentialError::MailboxHostNotAllowed { field, reason }) => {
            assert_eq!(field, "smtp_host");
            assert!(reason.contains("smtp_host"), "{reason}");
        }
        other => panic!("expected MailboxHostNotAllowed, got {other:?}"),
    }
    assert!(repo.rows.read().await.is_empty());
    assert!(dialer.dialled().is_empty());
}

// ---------------------------------------------------------------------------
// The reply a mailbox_unreachable answer carries
// ---------------------------------------------------------------------------

/// An IMAP stand-in refusing LOGIN with a long reply full of control
/// characters (a terminal escape, a bell, a NUL).
async fn noisy_imap() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (r, mut w) = socket.into_split();
                let mut r = BufReader::new(r);
                let _ = w.write_all(b"* OK ready\r\n").await;
                let mut line = String::new();
                while r.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let tag = line.split(' ').next().unwrap_or("*").to_string();
                    let noise =
                        format!("{tag} NO \x1b[31m\x07refused\x00 {}\r\n", "x".repeat(3000));
                    let _ = w.write_all(noise.as_bytes()).await;
                    line.clear();
                }
            });
        }
    });
    port
}

#[tokio::test]
async fn the_reply_in_mailbox_unreachable_is_cut_to_512_characters_without_control_characters() {
    let port = noisy_imap().await;
    let (svc, _repo) = service(SessionMailboxProbe::new(Arc::new(PlainConnector)));
    let err = svc
        .create_imap_mailbox(command(settings(("127.0.0.1", port), ("127.0.0.1", 1))))
        .await
        .expect_err("refused");
    match err.downcast_ref::<CredentialError>() {
        Some(CredentialError::MailboxUnreachable { protocol, reply }) => {
            assert_eq!(protocol, "imap");
            assert!(
                reply.chars().count() <= 512,
                "{} chars",
                reply.chars().count()
            );
            assert!(!reply.chars().any(char::is_control), "{reply:?}");
            assert!(reply.contains("refused"), "{reply}");
        }
        other => panic!("expected MailboxUnreachable, got {other:?}"),
    }
}
