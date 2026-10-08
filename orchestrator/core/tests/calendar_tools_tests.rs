// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Calendar accounts over CalDAV, AEGIS ADR-138 K2, K4 and K5, the read
//! tools of K5a, K6, K6a, K6c and K6d (the module `read_tools`), and the
//! write tools of K6, K6a, K6b, K6c, K6e, K7 and K7a (the module
//! `read_tools::write_tools`).
//!
//! - K4: an OAuth callback granted the calendar scope stores the binding's
//!   CalDAV settings (`metadata.calendar`) with its address as label; one
//!   without the scope stores none.
//! - K5: before anything is stored, the principal must answer a `PROPFIND`
//!   (Depth 0) with the token as a bearer, naming a calendar home set; a
//!   refusal stores nothing, leaves the binding pending and answers
//!   `calendar_unreachable` with no token in it.
//! - K2: the client's discovery, calendars, `calendar-query` with
//!   `time-range` and `expand`, and `GET` with its `etag`, every URL kept on
//!   the server's origin.
//! - Migration 047 applied twice changes nothing, and the repository stores
//!   and reads the settings (`AEGIS_TEST_POSTGRES_URL`).
//! - K5a, K6, K6a, K6c, K6d: `calendar.calendars`, `calendar.list` and
//!   `calendar.read` in the tool service: who may use an account and the
//!   six refusals, the run's `account` enum, the `caldav` pool, the window's
//!   refusals, the query, `truncated`, the times as the server gave them,
//!   the capped description, the identifiers kept on the server's origin,
//!   and the catalogue's entries (skip the judge, not gated).
//! - K6, K6a, K6b, K6c, K6e, K7, K7a: `calendar.create`, `calendar.update`,
//!   `calendar.delete` and `calendar.respond`: each write's request (`PUT`
//!   with `If-None-Match: *` or `If-Match`, `DELETE` with `If-Match`, times
//!   in UTC, `SEQUENCE`, `PARTSTAT`), a `412` answered with its sentence and
//!   nothing changed, each K6a refusal before any change, the four
//!   contracts and the catalogue's mark no capability entry clears, the
//!   admission before the gate writing the account's id and the event's
//!   values over the model's, a refused admission writing no row, and an
//!   approved run reading the event again.
//!
//! Every server these tests talk to is on 127.0.0.1: the CalDAV stand-in
//! (`support/caldav_standins.rs`) and a mockito token endpoint. No request
//! leaves the machine, and none reaches Google.

#[path = "support/caldav_standins.rs"]
mod caldav_standins;

use aegis_orchestrator_core::application::credential_service::{
    is_calendar_binding, CredentialError, CredentialManagementService, OAuthProviderConfig,
    OAuthProviderRegistry, StandardCredentialManagementService,
};
use aegis_orchestrator_core::domain::credential::{
    CalendarSettings, CredentialBindingId, CredentialBindingRepository, CredentialGrant,
    CredentialMetadata, CredentialProvider, CredentialScope, CredentialStatus, CredentialType,
    GrantTarget, OAuthPendingState, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::secrets::{AccessContext, SecretPath, SensitiveString};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::calendar::caldav::{CalDavClient, CalDavError};
use aegis_orchestrator_core::infrastructure::calendar::ical::parse_calendar;
use aegis_orchestrator_core::infrastructure::calendar::xml::{self, CALDAV, DAV};
use aegis_orchestrator_core::infrastructure::calendar::{
    CalDavAuth, CalDavProbe, ReqwestTransport, GOOGLE_CALDAV_SCOPE,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::PostgresCredentialBindingRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use caldav_standins::{CalDavStandIn, StandInCalendar, StandInConfig, StandInEvent};
use chrono::{DateTime, TimeZone, Utc};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Executor, Row};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::RwLock;

const USER: &str = "user-sub-calendar";
const ADDRESS: &str = "jeshua@workspace.example.test";
const TOKEN: &str = "ya29.Mk7-calendar-access-token";
const PROVIDER: &str = "google-calendar";
const REDIRECT: &str = "https://ask.example/vault/connections/callback";
const PRINCIPAL_PATH: &str = "/caldav/v2/jeshua@workspace.example.test/user";
const HOME_SET: &str = "/caldav/v2/jeshua@workspace.example.test/";

// ---------------------------------------------------------------------------
// In-memory binding store
// ---------------------------------------------------------------------------

#[derive(Default)]
struct InMemoryRepo {
    bindings: RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>,
    pending: RwLock<HashMap<String, OAuthPendingState>>,
}

#[async_trait]
impl CredentialBindingRepository for InMemoryRepo {
    async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()> {
        self.bindings
            .write()
            .await
            .insert(binding.id, binding.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        Ok(self.bindings.read().await.get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        _tenant_id: &TenantId,
        _owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok(self.bindings.read().await.values().cloned().collect())
    }
    async fn find_active_grants_for_target(
        &self,
        _tenant_id: &TenantId,
        _owner_user_id: &str,
        _provider: &CredentialProvider,
        _target: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>> {
        Ok(Vec::new())
    }
    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
        self.bindings.write().await.remove(id);
        Ok(())
    }
    async fn save_oauth_state(
        &self,
        state: &str,
        binding_id: &CredentialBindingId,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> anyhow::Result<()> {
        self.pending.write().await.insert(
            state.to_string(),
            OAuthPendingState {
                state: state.to_string(),
                binding_id: *binding_id,
                pkce_verifier: pkce_verifier.to_string(),
                redirect_uri: redirect_uri.to_string(),
                created_at: Utc::now(),
            },
        );
        Ok(())
    }
    async fn find_oauth_state(&self, state: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(self.pending.read().await.get(state).cloned())
    }
    async fn delete_oauth_state(&self, state: &str) -> anyhow::Result<()> {
        self.pending.write().await.remove(state);
        Ok(())
    }
    async fn delete_expired_oauth_states(&self, _older_than: DateTime<Utc>) -> anyhow::Result<u64> {
        Ok(0)
    }
}

fn tenant() -> TenantId {
    TenantId::for_consumer_user(USER).unwrap()
}

/// The registry entry `google-calendar` as K3 configures it: the calendar
/// scope and `email`.
fn calendar_registry(token_url: String) -> OAuthProviderRegistry {
    let mut registry = OAuthProviderRegistry::new();
    registry.insert(
        CredentialProvider::new(PROVIDER),
        OAuthProviderConfig {
            authorization_url: "https://accounts.example/o/oauth2/v2/auth".into(),
            token_url: token_url.into(),
            client_id: "google-client-id".to_string(),
            client_secret: Some(SensitiveString::new("google-client-secret")),
            redirect_uri_allowlist: vec![REDIRECT.to_string()],
            scopes: vec![GOOGLE_CALDAV_SCOPE.to_string(), "email".to_string()],
            extra_authorization_params: BTreeMap::from([(
                "access_type".to_string(),
                "offline".to_string(),
            )]),
            display_name: Some("Google Calendar".to_string()),
        },
    );
    registry
}

fn id_token(email: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        serde_json::json!({"iss":"https://accounts.example","email":email,"email_verified":true})
            .to_string(),
    );
    format!("{header}.{claims}.c2lnbmF0dXJl")
}

fn token_body(scope: &str, id_token_email: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "access_token": TOKEN,
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_token": "1//calendar-refresh-token",
        "scope": scope,
    });
    if let Some(email) = id_token_email {
        body["id_token"] = serde_json::json!(id_token(email));
    }
    body
}

fn granted_calendar_scope() -> String {
    format!("{GOOGLE_CALDAV_SCOPE} https://www.googleapis.com/auth/userinfo.email")
}

fn standin_config(accepted_token: &str, home_set: Option<&str>) -> StandInConfig {
    StandInConfig {
        accepted_token: accepted_token.to_string(),
        principal: PRINCIPAL_PATH.to_string(),
        home_set: home_set.map(str::to_string),
        calendars: Vec::new(),
    }
}

/// What a connect left behind.
struct Connect {
    repo: Arc<InMemoryRepo>,
    secrets: Arc<SecretsManager>,
    binding: CredentialBindingId,
    standin: CalDavStandIn,
    outcome: anyhow::Result<CredentialBindingId>,
}

/// Initiate and call back a `google-calendar` connect whose token endpoint
/// answers `token_body`, the check's requests going to a stand-in serving
/// `config`, with the userinfo at `userinfo` when given.
async fn connect(
    server: &mut mockito::ServerGuard,
    token_body: serde_json::Value,
    config: StandInConfig,
    userinfo: Option<String>,
) -> Connect {
    let _exchange = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::UrlEncoded(
            "grant_type".into(),
            "authorization_code".into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(token_body.to_string())
        .create_async()
        .await;
    let standin = CalDavStandIn::start(config).await;
    let repo = Arc::new(InMemoryRepo::default());
    let event_bus = Arc::new(EventBus::new(256));
    let secrets = Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::new()),
        event_bus.clone(),
    ));
    let mut service = StandardCredentialManagementService::with_http_client(
        repo.clone(),
        secrets.clone(),
        event_bus,
        Arc::new(calendar_registry(format!("{}/token", server.url()))),
        reqwest::Client::new(),
    )
    .with_calendar_probe(Arc::new(CalDavProbe::new(Arc::new(
        ReqwestTransport::to_origin(standin.origin()),
    ))));
    if let Some(url) = userinfo {
        service = service.with_mail_userinfo_url(url);
    }
    let init = service
        .initiate_oauth_connection(
            USER,
            &tenant(),
            CredentialProvider::new(PROVIDER),
            REDIRECT.to_string(),
        )
        .await
        .unwrap();
    let binding = repo.pending.read().await[&init.state].binding_id;
    let outcome = service
        .complete_oauth_connection(&init.state, "auth-code")
        .await;
    Connect {
        repo,
        secrets,
        binding,
        standin,
        outcome,
    }
}

/// Whether a secret holding a token was written for `binding`.
async fn token_written(c: &Connect, binding: &CredentialBindingId) -> bool {
    let b = c.repo.find_by_id(binding).await.unwrap().unwrap();
    c.secrets
        .read_secret(
            &b.secret_path.effective_mount(),
            &b.secret_path.path,
            &AccessContext::system("test"),
        )
        .await
        .map(|stored| stored.contains_key("access_token"))
        .unwrap_or(false)
}

fn google_settings(address: &str) -> CalendarSettings {
    CalendarSettings {
        server: "https://apidata.googleusercontent.com/caldav/v2/".to_string(),
        principal: format!("{address}/user"),
        address: address.to_string(),
    }
}

// ---------------------------------------------------------------------------
// K4 and K5: the callback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_callback_granted_the_calendar_scope_stores_its_caldav_settings_after_the_principal_answers(
) {
    let mut server = mockito::Server::new_async().await;
    let c = connect(
        &mut server,
        token_body(&granted_calendar_scope(), Some(ADDRESS)),
        standin_config(TOKEN, Some(HOME_SET)),
        None,
    )
    .await;
    let requests = c.standin.requests();
    assert_eq!(
        requests.len(),
        1,
        "the check did not send exactly one request: {requests:?}"
    );
    let check = &requests[0];
    assert_eq!(check.method, "PROPFIND", "the check is not a PROPFIND");
    assert_eq!(
        check.path, PRINCIPAL_PATH,
        "the check did not go to the server joined with <address>/user"
    );
    assert_eq!(check.header("depth"), Some("0"), "the check is not Depth 0");
    assert_eq!(
        check.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str()),
        "the check did not carry the token as a bearer"
    );
    let asked = xml::parse(&check.body).expect("the check's body is XML");
    let prop = asked
        .child(DAV, "prop")
        .expect("the body asks for properties");
    assert!(
        prop.child(DAV, "current-user-principal").is_some()
            && prop.child(CALDAV, "calendar-home-set").is_some(),
        "the check does not ask for current-user-principal and calendar-home-set: {}",
        check.body
    );

    let id = c
        .outcome
        .as_ref()
        .unwrap_or_else(|e| panic!("the connect did not complete: {e:#}"));
    let b = c.repo.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(b.status, CredentialStatus::Active);
    assert_eq!(b.credential_type, CredentialType::OAuth2);
    assert_eq!(b.provider, CredentialProvider::new(PROVIDER));
    assert_eq!(
        b.metadata.calendar,
        Some(google_settings(ADDRESS)),
        "the binding does not carry the calendar settings with the id_token's address"
    );
    assert_eq!(b.metadata.label, ADDRESS, "the label is not the address");
    assert_eq!(b.metadata.external_account_id.as_deref(), Some(ADDRESS));
    assert_eq!(b.metadata.mailbox, None, "a calendar grant made a mailbox");
    assert!(is_calendar_binding(&b));
    assert!(token_written(&c, id).await, "the token was not stored");

    // The shape calendar-in-conversation reads from `GET /v1/credentials`.
    let listed = serde_json::to_value(&b).unwrap();
    assert_eq!(
        listed["metadata"]["calendar"],
        serde_json::json!({
            "server": "https://apidata.googleusercontent.com/caldav/v2/",
            "principal": format!("{ADDRESS}/user"),
            "address": ADDRESS,
        })
    );
}

#[tokio::test]
async fn a_callback_without_the_calendar_scope_stores_no_calendar_and_sends_no_propfind() {
    let mut server = mockito::Server::new_async().await;
    let c = connect(
        &mut server,
        token_body(
            "openid https://www.googleapis.com/auth/userinfo.email",
            Some(ADDRESS),
        ),
        standin_config(TOKEN, Some(HOME_SET)),
        None,
    )
    .await;
    let id = c
        .outcome
        .as_ref()
        .unwrap_or_else(|e| panic!("the connect did not complete: {e:#}"));
    let b = c.repo.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(
        b.metadata.calendar, None,
        "a binding without the scope became a calendar"
    );
    assert!(
        !is_calendar_binding(&b),
        "a binding without calendar settings reads as a calendar account"
    );
    assert!(
        c.standin.requests().is_empty(),
        "a binding without the calendar scope was checked over CalDAV"
    );
    let listed = serde_json::to_value(&b).unwrap();
    assert!(
        listed["metadata"].get("calendar").is_none(),
        "metadata.calendar is written for a binding that is not a calendar: {listed}"
    );
}

#[tokio::test]
async fn a_principal_refusing_the_token_answers_calendar_unreachable_and_stores_nothing() {
    let mut server = mockito::Server::new_async().await;
    let c = connect(
        &mut server,
        token_body(&granted_calendar_scope(), Some(ADDRESS)),
        standin_config("ya29.some-other-token", Some(HOME_SET)),
        None,
    )
    .await;
    let err = c
        .outcome
        .as_ref()
        .err()
        .expect("a refused PROPFIND completed the connect");
    match err.downcast_ref::<CredentialError>() {
        Some(CredentialError::CalendarUnreachable { status, reply }) => {
            assert_eq!(*status, Some(401), "the refusal does not carry the status");
            assert!(
                reply.starts_with("Unauthorized: the credentials presented"),
                "the refusal does not carry the server's reply: {reply}"
            );
            assert!(
                !reply.contains(TOKEN),
                "the reply carries the token: {reply}"
            );
            assert!(
                reply.contains("[REDACTED]"),
                "the token was not redacted: {reply}"
            );
        }
        other => panic!("not calendar_unreachable: {other:?} ({err:#})"),
    }
    assert!(
        !format!("{err:#} {err:?}").contains(TOKEN),
        "the error carries the token"
    );
    let b = c.repo.find_by_id(&c.binding).await.unwrap().unwrap();
    assert_eq!(
        b.status,
        CredentialStatus::PendingOAuth,
        "a refused check did not leave the binding pending"
    );
    assert_eq!(b.metadata.calendar, None, "a refused check stored settings");
    assert!(
        !token_written(&c, &c.binding).await,
        "a refused check stored the token"
    );
}

#[tokio::test]
async fn an_answer_naming_no_calendar_home_set_answers_calendar_unreachable_and_stores_nothing() {
    let mut server = mockito::Server::new_async().await;
    let c = connect(
        &mut server,
        token_body(&granted_calendar_scope(), Some(ADDRESS)),
        standin_config(TOKEN, None),
        None,
    )
    .await;
    match c
        .outcome
        .as_ref()
        .err()
        .and_then(|e| e.downcast_ref::<CredentialError>())
    {
        Some(CredentialError::CalendarUnreachable { status, reply }) => {
            assert_eq!(*status, Some(207));
            assert!(reply.contains("calendar-home-set"), "{reply}");
        }
        other => panic!("an answer with no home set was not refused: {other:?}"),
    }
    let b = c.repo.find_by_id(&c.binding).await.unwrap().unwrap();
    assert_eq!(b.status, CredentialStatus::PendingOAuth);
    assert_eq!(b.metadata.calendar, None);
    assert!(!token_written(&c, &c.binding).await);
}

#[tokio::test]
async fn without_an_id_token_the_calendar_address_comes_from_the_userinfo() {
    let mut server = mockito::Server::new_async().await;
    let userinfo = server
        .mock("GET", "/userinfo")
        .match_header("authorization", format!("Bearer {TOKEN}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::json!({"email": ADDRESS, "email_verified": true}).to_string())
        .create_async()
        .await;
    let url = format!("{}/userinfo", server.url());
    let c = connect(
        &mut server,
        token_body(&granted_calendar_scope(), None),
        standin_config(TOKEN, Some(HOME_SET)),
        Some(url),
    )
    .await;
    let id = c
        .outcome
        .as_ref()
        .unwrap_or_else(|e| panic!("the connect did not complete: {e:#}"));
    let b = c.repo.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(
        b.metadata.calendar,
        Some(google_settings(ADDRESS)),
        "the binding does not carry the calendar settings with the userinfo's address"
    );
    assert_eq!(b.metadata.label, ADDRESS);
    assert_eq!(c.standin.requests()[0].path, PRINCIPAL_PATH);
    userinfo.assert_async().await;
}

// ---------------------------------------------------------------------------
// K2: the client
// ---------------------------------------------------------------------------

const SERVER: &str = "https://caldav.example.test/dav/";
const WORK: &str = "/dav/a@example.test/work/";
const HOLIDAYS: &str = "/dav/a@example.test/holidays/";

fn occurrence(uid: &str, recurrence: &str, start: &str, end: &str, summary: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Stand-in//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nRECURRENCE-ID:{recurrence}\r\nDTSTART:{start}\r\nDTEND:{end}\r\nSUMMARY:{summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

const PLANNING: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Stand-in//EN\r\nBEGIN:VEVENT\r\nUID:planning@example.test\r\nDTSTART;TZID=Europe/Berlin:20261009T100000\r\nDTEND;TZID=Europe/Berlin:20261009T110000\r\nSUMMARY:Planning\\, Q4 & next\r\nDESCRIPTION:Bring the numbers\\nand the plan\r\nORGANIZER;CN=Jane:mailto:jane@example.test\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:a@example.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

fn client_config() -> StandInConfig {
    StandInConfig {
        accepted_token: TOKEN.to_string(),
        principal: "/dav/a@example.test/user".to_string(),
        home_set: Some("/dav/a@example.test/".to_string()),
        calendars: vec![
            StandInCalendar {
                href: WORK.to_string(),
                name: "Work".to_string(),
                description: Some("Meetings & reviews".to_string()),
                color: Some("#16A765FF".to_string()),
                writable: true,
                events: vec![
                    StandInEvent {
                        name: "planning.ics".to_string(),
                        etag: "\"etag-planning-1\"".to_string(),
                        ics: PLANNING.to_string(),
                    },
                    StandInEvent {
                        name: "standup%20daily.ics".to_string(),
                        etag: "\"etag-standup-7\"".to_string(),
                        ics: occurrence(
                            "standup@example.test",
                            "20261012T080000Z",
                            "20261012T080000Z",
                            "20261012T081500Z",
                            "Stand-up",
                        ),
                    },
                ],
            },
            StandInCalendar {
                href: HOLIDAYS.to_string(),
                name: "Holidays".to_string(),
                description: None,
                color: None,
                writable: false,
                events: Vec::new(),
            },
        ],
    }
}

fn client_settings() -> CalendarSettings {
    CalendarSettings {
        server: SERVER.to_string(),
        principal: "a@example.test/user".to_string(),
        address: "a@example.test".to_string(),
    }
}

fn window() -> (DateTime<Utc>, DateTime<Utc>) {
    (
        Utc.with_ymd_and_hms(2026, 10, 8, 9, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 10, 15, 9, 0, 0).unwrap(),
    )
}

#[tokio::test]
async fn the_client_discovers_the_home_set_and_lists_its_calendars_with_names_colours_and_privileges(
) {
    let standin = CalDavStandIn::start(client_config()).await;
    let transport = ReqwestTransport::to_origin(standin.origin());
    let auth = CalDavAuth::Bearer(SensitiveString::new(TOKEN));
    let settings = client_settings();
    let client = CalDavClient::new(&transport, &settings, &auth).expect("settings usable");
    let found = client.discover().await.expect("discovery");
    assert_eq!(
        found.home_set.as_str(),
        "https://caldav.example.test/dav/a@example.test/",
        "the home set is not resolved against the server"
    );
    let calendars = client.calendars(&found.home_set).await.expect("calendars");
    assert_eq!(
        calendars
            .iter()
            .map(|c| c.href.as_str())
            .collect::<Vec<_>>(),
        vec![WORK, HOLIDAYS],
        "the home set's own collection, or a calendar, is listed wrongly"
    );
    let work = &calendars[0];
    assert_eq!(work.name.as_deref(), Some("Work"));
    assert_eq!(work.description.as_deref(), Some("Meetings & reviews"));
    assert_eq!(work.color.as_deref(), Some("#16A765FF"));
    assert!(
        work.writable,
        "a calendar with the write privilege reads as read-only"
    );
    let holidays = &calendars[1];
    assert_eq!(holidays.description, None);
    assert!(!holidays.writable, "a read-only calendar reads as writable");

    let requests = standin.requests();
    let listing = &requests[1];
    assert_eq!(listing.method, "PROPFIND");
    assert_eq!(listing.path, "/dav/a@example.test/");
    assert_eq!(listing.header("depth"), Some("1"));
    assert!(listing.body.contains("current-user-privilege-set"));
}

#[tokio::test]
async fn the_client_queries_a_window_with_time_range_and_expand_and_reads_each_event_with_its_etag()
{
    let standin = CalDavStandIn::start(client_config()).await;
    let transport = ReqwestTransport::to_origin(standin.origin());
    let auth = CalDavAuth::Bearer(SensitiveString::new(TOKEN));
    let settings = client_settings();
    let client = CalDavClient::new(&transport, &settings, &auth).expect("settings usable");
    let (start, end) = window();
    let events = client.events(WORK, start, end).await.expect("events");

    let requests = standin.requests();
    assert_eq!(requests.len(), 1);
    let query = &requests[0];
    assert_eq!(query.method, "REPORT", "the query is not a REPORT");
    assert_eq!(query.path, WORK);
    assert_eq!(query.header("depth"), Some("1"));
    assert_eq!(
        query.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    let body = xml::parse(&query.body).expect("the query is XML");
    assert!(body.is(CALDAV, "calendar-query"), "not a calendar-query");
    assert!(
        query
            .body
            .contains(r#"<c:expand start="20261008T090000Z" end="20261015T090000Z"/>"#),
        "the query does not ask the server to expand the window: {}",
        query.body
    );
    assert!(
        query
            .body
            .contains(r#"<c:time-range start="20261008T090000Z" end="20261015T090000Z"/>"#),
        "the query does not filter on the window: {}",
        query.body
    );

    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].event_id, "planning.ics");
    assert_eq!(events[0].etag.as_deref(), Some("\"etag-planning-1\""));
    assert_eq!(events[0].href, format!("{WORK}planning.ics"));
    assert_eq!(
        events[1].event_id, "standup daily.ics",
        "an event id is not the decoded last segment"
    );
    let planning = parse_calendar(&events[0].data).expect("iCalendar");
    let planning = planning.events().next().expect("an event");
    assert_eq!(planning.summary().as_deref(), Some("Planning, Q4 & next"));
    let standup = parse_calendar(&events[1].data).expect("iCalendar");
    let standup = standup.events().next().expect("an occurrence");
    assert!(standup.repeats());
    assert_eq!(
        standup.start().and_then(|s| s.rfc3339()).as_deref(),
        Some("2026-10-12T08:00:00Z"),
        "an expanded occurrence is not read in UTC"
    );
}

#[tokio::test]
async fn the_client_reads_one_event_by_get_with_its_etag_and_a_missing_one_is_refused() {
    let standin = CalDavStandIn::start(client_config()).await;
    let transport = ReqwestTransport::to_origin(standin.origin());
    let auth = CalDavAuth::Bearer(SensitiveString::new(TOKEN));
    let settings = client_settings();
    let client = CalDavClient::new(&transport, &settings, &auth).expect("settings usable");
    let event = client.event(WORK, "planning.ics").await.expect("the event");
    assert_eq!(
        event.etag.as_deref(),
        Some("\"etag-planning-1\""),
        "the GET's ETag is not read"
    );
    assert_eq!(event.data, PLANNING);
    let read = parse_calendar(&event.data).unwrap();
    let read = read.events().next().unwrap();
    assert_eq!(
        read.start().and_then(|s| s.tzid).as_deref(),
        Some("Europe/Berlin")
    );
    assert_eq!(
        read.description().as_deref(),
        Some("Bring the numbers\nand the plan")
    );
    let requests = standin.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, format!("{WORK}planning.ics"));

    match client.event(WORK, "gone.ics").await {
        Err(CalDavError::Refused { status: 404, .. }) => {}
        other => panic!("a missing event was not refused with 404: {other:?}"),
    }
}

#[tokio::test]
async fn a_calendar_or_event_off_the_servers_origin_is_refused_before_any_request() {
    let standin = CalDavStandIn::start(client_config()).await;
    let transport = ReqwestTransport::to_origin(standin.origin());
    let auth = CalDavAuth::Bearer(SensitiveString::new(TOKEN));
    let settings = client_settings();
    let client = CalDavClient::new(&transport, &settings, &auth).expect("settings usable");
    let (start, end) = window();
    for calendar in [
        "https://elsewhere.example.test/dav/work/",
        "//elsewhere.example.test/dav/work/",
        "http://caldav.example.test/dav/work/",
        "https://caldav.example.test:8443/dav/work/",
    ] {
        match client.events(calendar, start, end).await {
            Err(CalDavError::OutsideServer(_)) => {}
            other => panic!("'{calendar}' was not refused as off the server: {other:?}"),
        }
    }
    for event_id in ["../planning.ics", "a/b.ics", "", ".."] {
        match client.event(WORK, event_id).await {
            Err(CalDavError::InvalidEventId(_)) => {}
            other => panic!("'{event_id}' was not refused as an event id: {other:?}"),
        }
    }
    assert!(
        standin.requests().is_empty(),
        "a refused reference was requested: {:?}",
        standin.requests()
    );
    let elsewhere = CalendarSettings {
        principal: "https://elsewhere.example.test/user".to_string(),
        ..client_settings()
    };
    assert!(matches!(
        CalDavClient::new(&transport, &elsewhere, &auth),
        Err(CalDavError::OutsideServer(_))
    ));
}

#[tokio::test]
async fn the_production_transport_reaches_only_https() {
    let transport = ReqwestTransport::new();
    let auth = CalDavAuth::Bearer(SensitiveString::new(TOKEN));
    let settings = CalendarSettings {
        server: "http://127.0.0.1:9/dav/".to_string(),
        principal: "a/user".to_string(),
        address: "a@example.test".to_string(),
    };
    let client = CalDavClient::new(&transport, &settings, &auth).expect("settings usable");
    match client.discover().await {
        Err(CalDavError::Unreachable(reason)) => {
            assert!(
                reason.contains("must be reached over https"),
                "an http server was dialled rather than refused: {reason}"
            );
        }
        other => panic!("an http server was reached: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Migration 047 and the repository
// ---------------------------------------------------------------------------

const MIGRATION_011: &str = include_str!("../../../cli/migrations/011_credential_bindings.sql");
const MIGRATION_035: &str =
    include_str!("../../../cli/migrations/035_credential_mailbox_settings.sql");
const MIGRATION_042: &str =
    include_str!("../../../cli/migrations/042_credential_binding_reach.sql");
const MIGRATION_047: &str =
    include_str!("../../../cli/migrations/047_credential_calendar_settings.sql");

async fn pool_in_fresh_schema(url: &str) -> (PgPool, String) {
    let schema = format!("cal_{}", uuid::Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .expect("connect");
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create schema");
    let search_path = format!("SET search_path TO {schema}, public");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _| {
            let sql = search_path.clone();
            Box::pin(async move {
                conn.execute(sql.as_str()).await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .expect("connect in schema");
    (pool, schema)
}

async fn row_json(pool: &PgPool, id: uuid::Uuid) -> serde_json::Value {
    serde_json::from_str(
        &sqlx::query("SELECT row_to_json(c)::text AS j FROM credential_bindings c WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
            .get::<String, _>("j"),
    )
    .unwrap()
}

#[tokio::test]
async fn migration_047_applied_twice_changes_nothing_and_the_repository_stores_calendar_settings() {
    let Ok(url) = std::env::var("AEGIS_TEST_POSTGRES_URL") else {
        eprintln!("skipped: no AEGIS_TEST_POSTGRES_URL");
        return;
    };
    let (pool, schema) = pool_in_fresh_schema(&url).await;
    pool.execute(MIGRATION_011).await.expect("migration 011");
    pool.execute(MIGRATION_035).await.expect("migration 035");
    pool.execute(MIGRATION_042).await.expect("migration 042");

    // A row written before migration 047, as production holds them.
    let existing = uuid::Uuid::new_v4();
    let tenant = TenantId::for_consumer_user("owner-sub").unwrap();
    sqlx::query(
        "INSERT INTO credential_bindings (id, owner_user_id, tenant_id, credential_type, provider, \
         label, secret_path, scope, status, oauth_scopes, external_account_id) \
         VALUES ($1, 'owner-sub', $2, 'oauth2', 'google', 'a@example.test', \
         'users/x/owner-sub/credentials/k', 'personal', 'active', ARRAY['email'], 'a@example.test')",
    )
    .bind(existing)
    .bind(tenant.as_str())
    .execute(&pool)
    .await
    .expect("insert existing row");
    let mut before = row_json(&pool, existing).await;

    pool.execute(MIGRATION_047).await.expect("migration 047");
    let once = row_json(&pool, existing).await;
    pool.execute(MIGRATION_047)
        .await
        .expect("migration 047 applied a second time");
    let twice = row_json(&pool, existing).await;
    let column: (String, String) = sqlx::query_as(
        "SELECT data_type, is_nullable FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = 'credential_bindings' \
         AND column_name = 'calendar_settings'",
    )
    .bind(&schema)
    .fetch_one(&pool)
    .await
    .expect("migration 047 added no calendar_settings column");
    before["calendar_settings"] = serde_json::Value::Null;
    let mut wrong = Vec::new();
    if once != before {
        wrong.push(format!(
            "migration 047 changed an existing row: {once} (was {before})"
        ));
    }
    if twice != once {
        wrong.push(format!(
            "migration 047 applied twice changed a row: {twice} (was {once})"
        ));
    }
    if column != ("jsonb".to_string(), "YES".to_string()) {
        wrong.push(format!(
            "calendar_settings is {column:?}, not a nullable jsonb"
        ));
    }

    let repo = PostgresCredentialBindingRepository::new(pool.clone());
    let read = repo
        .find_by_id(&CredentialBindingId(existing))
        .await
        .unwrap()
        .expect("the existing row reads");
    if read.metadata.calendar.is_some() {
        wrong.push("the existing row reads as a calendar account".to_string());
    }
    let id = CredentialBindingId::new();
    let now = Utc::now();
    let calendar = UserCredentialBinding {
        id,
        owner_user_id: "owner-sub".to_string(),
        tenant_id: tenant.clone(),
        credential_type: CredentialType::OAuth2,
        provider: CredentialProvider::new(PROVIDER),
        secret_path: SecretPath::for_tenant(tenant.clone(), "kv", format!("c/{}", id.0)),
        scope: CredentialScope::Personal,
        status: CredentialStatus::Active,
        metadata: CredentialMetadata {
            label: ADDRESS.to_string(),
            tags: None,
            service_url: None,
            external_account_id: Some(ADDRESS.to_string()),
            oauth_scopes: Some(vec![GOOGLE_CALDAV_SCOPE.to_string()]),
            mailbox: None,
            reach: None,
            calendar: Some(google_settings(ADDRESS)),
        },
        grants: Vec::new(),
        created_at: now,
        updated_at: now,
    };
    repo.save(&calendar).await.expect("save a calendar account");
    let stored = row_json(&pool, id.0).await;
    if stored["calendar_settings"] != serde_json::to_value(google_settings(ADDRESS)).unwrap() {
        wrong.push(format!(
            "calendar_settings is not stored as written: {}",
            stored["calendar_settings"]
        ));
    }
    let read = repo.find_by_id(&id).await.unwrap().expect("reads back");
    if read.metadata.calendar != Some(google_settings(ADDRESS)) {
        wrong.push(format!(
            "the calendar settings do not read back: {:?}",
            read.metadata.calendar
        ));
    }
    pool.execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .expect("drop schema");
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// K5a, K6's read half, K6a's read refusals, K6c, K6d: the read tools
// ---------------------------------------------------------------------------

/// The three read tools in the tool service, against the loopback CalDAV
/// stand-in: who may use an account (K5a's six sentences, a chosen account
/// by name, the run's `account` enum, the `caldav` pool the start check
/// reads), `calendar.calendars`, `calendar.list` (the query, the window's
/// refusals, `truncated`, occurrences in UTC), `calendar.read` (`tzid`,
/// `etag`, the capped description), the identifiers' origin (K6d), and the
/// catalogue's entries (skip the judge, not gated).
mod read_tools {
    use super::*;
    use aegis_orchestrator_core::application::agent::AgentLifecycleService;
    use aegis_orchestrator_core::application::credential_service::{
        ContextBinding, ToolCalendar, ToolCalendarSource, ToolCallActor, ToolCredentialSource,
    };
    use aegis_orchestrator_core::application::execution::ExecutionService;
    use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
    use aegis_orchestrator_core::application::tool_invocation_service::{
        ToolInvocationResult, ToolInvocationService,
    };
    use aegis_orchestrator_core::application::tools::builtin_calendar::{
        BAD_LIMIT, CHOSEN_DIFFERENT, END_NOT_AFTER_START, NONE_CHOSEN, NOT_AMONG_CHOSEN,
        NOT_GRANTED, NO_PERSON, WINDOW_TOO_WIDE,
    };
    use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus};
    use aegis_orchestrator_core::domain::events::ExecutionEvent;
    use aegis_orchestrator_core::domain::execution::{
        Execution, ExecutionId, ExecutionInput, Iteration,
    };
    use aegis_orchestrator_core::domain::fsal::AegisFSAL;
    use aegis_orchestrator_core::domain::mcp::ToolInputContract;
    use aegis_orchestrator_core::domain::repository::AgentVersion;
    use aegis_orchestrator_core::domain::seal_session::{CallerAnswer, SealSessionError};
    use aegis_orchestrator_core::domain::security_context::{
        SecurityContext, SecurityContextRepository,
    };
    use aegis_orchestrator_core::infrastructure::event_bus::DomainEvent;
    use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
    use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
    use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
    use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
    use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
    use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
    use anyhow::Result;
    use futures::Stream;
    use serde_json::{json, Value};
    use std::pin::Pin;

    const CONTEXT: &str = "calendar-read-test-context";
    const PERSON: &str = "calendar-read-person";

    fn agent() -> Agent {
        let manifest: AgentManifest = serde_yaml::from_str(
            r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: calendar-read-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: ["calendar.calendars", "calendar.list", "calendar.read"]
"#,
        )
        .unwrap();
        Agent {
            id: AgentId::new(),
            tenant_id: TenantId::default(),
            scope: aegis_orchestrator_core::domain::agent::AgentScope::default(),
            name: manifest.metadata.name.clone(),
            manifest,
            status: AgentStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn security_context() -> SecurityContext {
        SecurityContext {
            name: CONTEXT.to_string(),
            description: "calendar read test".to_string(),
            capabilities: vec![
                aegis_orchestrator_core::domain::security_context::Capability {
                    tool_pattern: "calendar.*".to_string(),
                    path_allowlist: None,
                    command_allowlist: None,
                    subcommand_allowlist: None,
                    domain_allowlist: None,
                    max_response_size: None,
                    rate_limit: None,
                    max_concurrent: None,
                },
            ],
            deny_list: vec![],
            metadata: aegis_orchestrator_core::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        }
    }

    /// One of the person's calendar accounts as the stub source answers it.
    #[derive(Clone)]
    struct Account {
        id: CredentialBindingId,
        name: String,
        granted: bool,
    }

    impl Account {
        fn new(name: &str, granted: bool) -> Self {
            Self {
                id: CredentialBindingId::new(),
                name: name.to_string(),
                granted,
            }
        }
    }

    /// The person's calendar accounts, each answered to its owner only,
    /// with its context name and the stand-in's settings.
    struct Accounts(Vec<Account>);

    #[async_trait]
    impl ToolCalendarSource for Accounts {
        async fn tool_calendar(
            &self,
            actor: &ToolCallActor<'_>,
            binding_id: &CredentialBindingId,
        ) -> anyhow::Result<Option<ToolCalendar>> {
            if actor.user_id != PERSON {
                return Ok(None);
            }
            Ok(self
                .0
                .iter()
                .find(|a| &a.id == binding_id)
                .map(|a| ToolCalendar {
                    binding_id: a.id,
                    settings: client_settings(),
                    auth: CalDavAuth::Bearer(SensitiveString::new(TOKEN)),
                    granted: a.granted,
                }))
        }

        async fn calendar_contexts(
            &self,
            _tenant_id: &TenantId,
            user_id: &str,
        ) -> anyhow::Result<Vec<ContextBinding>> {
            Ok(self.named(user_id))
        }
    }

    impl Accounts {
        fn named(&self, user_id: &str) -> Vec<ContextBinding> {
            if user_id != PERSON {
                return Vec::new();
            }
            self.0
                .iter()
                .map(|a| ContextBinding {
                    id: a.id,
                    name: a.name.clone(),
                    reach: None,
                })
                .collect()
        }
    }

    #[async_trait]
    impl ToolCredentialSource for Accounts {
        async fn tool_server_credential(
            &self,
            _actor: &ToolCallActor<'_>,
            _server: &str,
        ) -> anyhow::Result<Option<SensitiveString>> {
            Ok(None)
        }

        async fn context_bindings(
            &self,
            _tenant_id: &TenantId,
            user_id: &str,
            server: &str,
        ) -> anyhow::Result<Vec<ContextBinding>> {
            Ok(if server == "caldav" {
                self.named(user_id)
            } else {
                Vec::new()
            })
        }
    }

    /// One run: its person (none for a service account) and its
    /// `contexts` input.
    struct Run {
        person: Option<&'static str>,
        contexts: Option<Value>,
    }

    fn run_of(contexts: Option<Value>) -> Run {
        Run {
            person: Some(PERSON),
            contexts,
        }
    }

    struct Harness {
        service: Arc<ToolInvocationService>,
        agent_id: AgentId,
        runs: Vec<ExecutionId>,
    }

    /// A tool service with the calendar tools over `accounts`, every request
    /// going to `standin`, and one execution per run.
    async fn harness(accounts: Vec<Account>, standin: &CalDavStandIn, runs: Vec<Run>) -> Harness {
        let agent = agent();
        let agent_id = agent.id;
        let executions: Vec<Execution> = runs
            .iter()
            .map(|run| {
                let mut e = Execution::new_with_id(
                    ExecutionId::new(),
                    agent_id,
                    ExecutionInput {
                        intent: None,
                        input: match &run.contexts {
                            Some(contexts) => json!({ "contexts": contexts }),
                            None => json!({}),
                        },
                        workspace_volume_id: None,
                        workspace_volume_mount_path: None,
                        workspace_remote_path: None,
                        workflow_execution_id: None,
                        attachments: Vec::new(),
                    },
                    5,
                    CONTEXT.to_string(),
                );
                e.tenant_id = TenantId::default();
                e.initiating_user_sub = run.person.map(str::to_string);
                e
            })
            .collect();
        let ids = executions.iter().map(|e| e.id).collect();
        let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
        security_context_repo
            .save(security_context())
            .await
            .unwrap();
        let storage_root = std::env::temp_dir().join(format!(
            "aegis-calendar-read-tests-{}",
            uuid::Uuid::new_v4()
        ));
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
            Arc::new(InMemoryVolumeRepository::new()),
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoOpPublisher),
        ));
        let accounts = Arc::new(Accounts(accounts));
        let service = ToolInvocationService::new(
            Arc::new(InMemorySealSessionRepository::new()),
            security_context_repo,
            Arc::new(SealMiddleware::new()),
            Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
            fsal,
            NfsVolumeRegistry::new(),
            Arc::new(OneAgent(agent)),
            Arc::new(Executions(
                executions.into_iter().map(|e| (e.id, e)).collect(),
            )),
            Arc::new(
                aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(),
            ),
            Arc::new(EventBus::new(256)),
            None,
        )
        .with_tool_credentials(accounts.clone())
        .with_calendar_tools_over(
            accounts,
            Arc::new(ReqwestTransport::to_origin(standin.origin())),
        );
        Harness {
            service: Arc::new(service),
            agent_id,
            runs: ids,
        }
    }

    impl Harness {
        async fn call(
            &self,
            run: usize,
            tool: &str,
            args: Value,
        ) -> std::result::Result<ToolInvocationResult, SealSessionError> {
            self.service
                .invoke_tool_internal(
                    &self.agent_id,
                    self.runs[run],
                    TenantId::default(),
                    0,
                    Vec::new(),
                    tool.to_string(),
                    args,
                )
                .await
        }
    }

    /// The value a call answered directly.
    fn answered(result: &std::result::Result<ToolInvocationResult, SealSessionError>) -> Value {
        match result {
            Ok(ToolInvocationResult::Direct(value)) => value.clone(),
            other => panic!("the call did not answer: {other:?}"),
        }
    }

    /// The sentence a refusal tells its caller.
    fn told(result: &std::result::Result<ToolInvocationResult, SealSessionError>) -> String {
        match result {
            Ok(ToolInvocationResult::Direct(value)) => format!("answered {value}"),
            Ok(_) => "the call was dispatched".to_string(),
            Err(SealSessionError::Answered {
                answer: CallerAnswer::CredentialBindingRequired { message },
                ..
            }) => message.clone(),
            Err(SealSessionError::InvalidArguments(message))
            | Err(SealSessionError::UpstreamUnavailable(message)) => message.clone(),
            Err(other) => format!("{other:?}"),
        }
    }

    const REVIEW: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Stand-in//EN\r\nBEGIN:VEVENT\r\nUID:review@example.test\r\nDTSTART:20261008T150000Z\r\nDTEND:20261008T160000Z\r\nSUMMARY:Budget review\r\nLOCATION:Room 4\r\nSTATUS:CONFIRMED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    /// A weekly event as a `GET` answers it: an override first, then the
    /// series' master.
    const WEEKLY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Stand-in//EN\r\nBEGIN:VEVENT\r\nUID:weekly@example.test\r\nRECURRENCE-ID:20261013T090000Z\r\nDTSTART:20261013T100000Z\r\nDTEND:20261013T110000Z\r\nSUMMARY:Weekly (moved)\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:weekly@example.test\r\nDTSTART:20261006T090000Z\r\nDTEND:20261006T100000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Weekly\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn long_event(description: &str) -> String {
        let mut ics = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Stand-in//EN\r\nBEGIN:VEVENT\r\nUID:long@example.test\r\nDTSTART;VALUE=DATE:20261010\r\nDTEND;VALUE=DATE:20261011\r\nSUMMARY:Offsite\r\n");
        let line = format!("DESCRIPTION:{description}");
        ics.push_str(&aegis_orchestrator_core::infrastructure::calendar::ical::fold(&line));
        ics.push_str("\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
        ics
    }

    /// The client's stand-in with three more events on Work: a UTC review,
    /// a weekly series with an override, and an all-day event with a
    /// description longer than the cap.
    fn tools_config() -> StandInConfig {
        let mut config = client_config();
        let work = &mut config.calendars[0];
        work.events.push(StandInEvent {
            name: "review.ics".to_string(),
            etag: "\"etag-review-2\"".to_string(),
            ics: REVIEW.to_string(),
        });
        config
    }

    fn read_config() -> StandInConfig {
        let mut config = client_config();
        let work = &mut config.calendars[0];
        work.events.push(StandInEvent {
            name: "weekly.ics".to_string(),
            etag: "\"etag-weekly-3\"".to_string(),
            ics: WEEKLY.to_string(),
        });
        work.events.push(StandInEvent {
            name: "long.ics".to_string(),
            etag: "\"etag-long-4\"".to_string(),
            ics: long_event(&"x".repeat(32_005)),
        });
        config
    }

    fn week() -> Value {
        json!({"start": "2026-10-08T09:00:00Z", "end": "2026-10-15T09:00:00Z"})
    }

    fn with(base: Value, more: Value) -> Value {
        let mut base = base;
        for (k, v) in more.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[tokio::test]
    async fn the_three_read_tools_are_listed_declaring_account_skip_the_judge_and_are_not_gated() {
        let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
        let tools = router.list_tools().await.unwrap();
        let mut wrong = Vec::new();
        for (name, required) in [
            ("calendar.calendars", vec!["account"]),
            ("calendar.list", vec!["account", "calendar_id"]),
            ("calendar.read", vec!["account", "calendar_id", "event_id"]),
        ] {
            let Some(tool) = tools.iter().find(|t| t.name == name) else {
                wrong.push(format!("{name} is not listed"));
                continue;
            };
            if tool.input_schema["properties"]["account"]["description"]
                != "The id of one of your calendar accounts."
            {
                wrong.push(format!("{name}'s schema does not declare account"));
            }
            let schema_required: Vec<&str> = tool.input_schema["required"]
                .as_array()
                .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            if schema_required != required {
                wrong.push(format!("{name}'s schema requires {schema_required:?}"));
            }
            if ToolInputContract::required_fields(name) != required.as_slice() {
                wrong.push(format!(
                    "{name}'s input contract requires {:?}",
                    ToolInputContract::required_fields(name)
                ));
            }
            if !router.is_skip_judge(name).await {
                wrong.push(format!("{name} does not skip the judge"));
            }
            if router.requires_approval(name) {
                wrong.push(format!("{name} is gated"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calendar_calendars_answers_each_calendar_with_its_name_colour_and_whether_it_is_writable(
    ) {
        let standin = CalDavStandIn::start(client_config()).await;
        let work = Account::new("Work account", false);
        let h = harness(
            vec![work.clone()],
            &standin,
            vec![run_of(Some(json!({"caldav": [work.id.0.to_string()]})))],
        )
        .await;
        let result = h
            .call(
                0,
                "calendar.calendars",
                json!({"account": work.id.0.to_string()}),
            )
            .await;
        let answer = answered(&result);
        assert_eq!(
            answer,
            json!({
                "account": work.id.0.to_string(),
                "calendars": [
                    {"calendar_id": WORK, "name": "Work", "description": "Meetings & reviews", "color": "#16A765FF", "writable": true},
                    {"calendar_id": HOLIDAYS, "name": "Holidays", "description": null, "writable": false},
                ]
            }),
            "calendar.calendars answered otherwise"
        );
        let methods: Vec<(String, String)> = standin
            .requests()
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect();
        assert_eq!(
            methods,
            vec![
                (
                    "PROPFIND".to_string(),
                    "/dav/a@example.test/user".to_string()
                ),
                ("PROPFIND".to_string(), "/dav/a@example.test/".to_string()),
            ],
            "calendar.calendars did not discover the home set and list it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calendar_list_queries_the_window_and_answers_its_events_by_start_utc_as_the_server_gave_it(
    ) {
        let standin = CalDavStandIn::start(tools_config()).await;
        let work = Account::new("Work account", true);
        let h = harness(vec![work.clone()], &standin, vec![run_of(None)]).await;
        let result = h
            .call(
                0,
                "calendar.list",
                with(
                    week(),
                    json!({"account": work.id.0.to_string(), "calendar_id": WORK}),
                ),
            )
            .await;
        let answer = answered(&result);
        let requests = standin.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].method, "REPORT");
        assert_eq!(requests[0].path, WORK);
        assert!(
            requests[0]
                .body
                .contains(r#"<c:expand start="20261008T090000Z" end="20261015T090000Z"/>"#)
                && requests[0]
                    .body
                    .contains(r#"<c:time-range start="20261008T090000Z" end="20261015T090000Z"/>"#),
            "calendar.list did not query its window with time-range and expand: {}",
            requests[0].body
        );
        let titles: Vec<&str> = answer["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["title"].as_str().unwrap_or("?"))
            .collect();
        assert_eq!(
            titles,
            vec!["Budget review", "Planning, Q4 & next", "Stand-up"],
            "the events are not answered by start"
        );
        assert_eq!(answer["truncated"], json!(false));
        assert_eq!(answer["account"], json!(work.id.0.to_string()));
        assert_eq!(answer["calendar_id"], json!(WORK));
        assert_eq!(answer["start"], json!("2026-10-08T09:00:00Z"));
        assert_eq!(answer["end"], json!("2026-10-15T09:00:00Z"));
        assert_eq!(
            answer["events"][2],
            json!({
                "event_id": "standup daily.ics",
                "uid": "standup@example.test",
                "title": "Stand-up",
                "start": "2026-10-12T08:00:00Z",
                "start_tzid": null,
                "end": "2026-10-12T08:15:00Z",
                "end_tzid": null,
                "all_day": false,
                "location": null,
                "organizer": null,
                "attendees": [],
                "status": null,
                "repeats": true,
                "recurrence_id": "2026-10-12T08:00:00Z",
            }),
            "an expanded occurrence is not answered in UTC with its recurrence"
        );
        let planning = &answer["events"][1];
        assert_eq!(
            (planning["start"].clone(), planning["start_tzid"].clone()),
            (json!("2026-10-09T10:00:00"), json!("Europe/Berlin")),
            "a time the server gave with a TZID is not answered as written with its tzid"
        );
        assert_eq!(
            planning["organizer"],
            json!({"address": "jane@example.test", "name": "Jane"})
        );
        assert_eq!(
            planning["attendees"],
            json!([{"address": "a@example.test", "name": null, "answer": "ACCEPTED"}])
        );
        assert!(
            planning.get("description").is_none(),
            "calendar.list answers a description"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calendar_list_matches_its_query_and_says_truncated_past_its_limit() {
        let standin = CalDavStandIn::start(tools_config()).await;
        let work = Account::new("Work account", true);
        let h = harness(vec![work.clone()], &standin, vec![run_of(None)]).await;
        let base = with(
            week(),
            json!({"account": work.id.0.to_string(), "calendar_id": WORK}),
        );
        let mut wrong = Vec::new();
        for (case, more, titles, truncated) in [
            (
                "limit 2",
                json!({"limit": 2}),
                vec!["Budget review", "Planning, Q4 & next"],
                true,
            ),
            (
                "limit 3",
                json!({"limit": 3}),
                vec!["Budget review", "Planning, Q4 & next", "Stand-up"],
                false,
            ),
            (
                "a description's words",
                json!({"query": "NUMBERS"}),
                vec!["Planning, Q4 & next"],
                false,
            ),
            (
                "a location",
                json!({"query": "room 4"}),
                vec!["Budget review"],
                false,
            ),
            (
                "a title",
                json!({"query": "stand"}),
                vec!["Stand-up"],
                false,
            ),
            ("nothing", json!({"query": "holiday"}), vec![], false),
        ] {
            let answer = answered(&h.call(0, "calendar.list", with(base.clone(), more)).await);
            let got: Vec<&str> = answer["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["title"].as_str().unwrap_or("?"))
                .collect();
            if got != titles || answer["truncated"] != json!(truncated) {
                wrong.push(format!(
                    "{case}: answered {got:?}, truncated {}",
                    answer["truncated"]
                ));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calendar_list_refuses_a_window_over_92_days_an_end_not_after_start_and_a_bad_limit_sending_nothing(
    ) {
        let standin = CalDavStandIn::start(tools_config()).await;
        let work = Account::new("Work account", true);
        let h = harness(vec![work.clone()], &standin, vec![run_of(None)]).await;
        let base = json!({"account": work.id.0.to_string(), "calendar_id": WORK});
        let mut wrong = Vec::new();
        for (more, expected) in [
            (
                json!({"start": "2026-10-01T00:00:00Z", "end": "2027-01-01T00:00:01Z"}),
                WINDOW_TOO_WIDE,
            ),
            (
                json!({"start": "2026-10-08T09:00:00Z", "end": "2026-10-08T09:00:00Z"}),
                END_NOT_AFTER_START,
            ),
            (
                json!({"start": "2026-10-08T09:00:00Z", "end": "2026-10-01T09:00:00Z"}),
                END_NOT_AFTER_START,
            ),
            (json!({"limit": 101}), BAD_LIMIT),
            (
                json!({"start": "next tuesday"}),
                "'start' must be a time in RFC 3339 form with an offset.",
            ),
        ] {
            let said = told(
                &h.call(0, "calendar.list", with(base.clone(), more.clone()))
                    .await,
            );
            if said != expected {
                wrong.push(format!("{more}: told {said:?}"));
            }
        }
        let sent = standin.requests();
        if !sent.is_empty() {
            wrong.push(format!("a refused call sent {} requests", sent.len()));
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn calendar_read_answers_the_event_with_its_description_tzid_and_etag() {
        let standin = CalDavStandIn::start(read_config()).await;
        let work = Account::new("Work account", true);
        let h = harness(vec![work.clone()], &standin, vec![run_of(None)]).await;
        let read = |event: &str| json!({"account": work.id.0.to_string(), "calendar_id": WORK, "event_id": event});
        let planning = answered(&h.call(0, "calendar.read", read("planning.ics")).await);
        assert_eq!(
            planning,
            json!({
                "account": work.id.0.to_string(),
                "calendar_id": WORK,
                "event_id": "planning.ics",
                "uid": "planning@example.test",
                "title": "Planning, Q4 & next",
                "start": "2026-10-09T10:00:00",
                "start_tzid": "Europe/Berlin",
                "end": "2026-10-09T11:00:00",
                "end_tzid": "Europe/Berlin",
                "all_day": false,
                "location": null,
                "organizer": {"address": "jane@example.test", "name": "Jane"},
                "attendees": [{"address": "a@example.test", "name": null, "answer": "ACCEPTED"}],
                "status": null,
                "repeats": false,
                "recurrence_id": null,
                "description": "Bring the numbers\nand the plan",
                "description_truncated": false,
                "etag": "\"etag-planning-1\"",
            }),
            "calendar.read answered otherwise"
        );
        let get = standin.requests().pop().unwrap();
        assert_eq!(
            (get.method.as_str(), get.path.as_str()),
            ("GET", "/dav/a@example.test/work/planning.ics"),
            "calendar.read did not GET the event"
        );

        let long = answered(&h.call(0, "calendar.read", read("long.ics")).await);
        assert_eq!(
            long["description"].as_str().map(|d| d.chars().count()),
            Some(32_000),
            "a long description is not capped at 32000 characters"
        );
        assert_eq!(long["description_truncated"], json!(true));
        assert_eq!(
            (long["start"].clone(), long["all_day"].clone()),
            (json!("2026-10-10"), json!(true))
        );

        let weekly = answered(&h.call(0, "calendar.read", read("weekly.ics")).await);
        assert_eq!(
            (
                weekly["title"].clone(),
                weekly["recurrence_id"].clone(),
                weekly["repeats"].clone()
            ),
            (json!("Weekly"), json!(null), json!(true)),
            "a repeating event's resource is not answered by its series' master"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_calendar_off_the_accounts_server_or_an_event_id_of_two_segments_is_refused_before_any_request(
    ) {
        let standin = CalDavStandIn::start(client_config()).await;
        let work = Account::new("Work account", true);
        let h = harness(vec![work.clone()], &standin, vec![run_of(None)]).await;
        let account = work.id.0.to_string();
        let mut wrong = Vec::new();
        for (tool, args, expected) in [
            (
                "calendar.list",
                json!({"account": account, "calendar_id": "https://elsewhere.example.test/dav/x/"}),
                "'https://elsewhere.example.test/dav/x/' is not on this calendar account's server",
            ),
            (
                "calendar.read",
                json!({"account": account, "calendar_id": "//elsewhere.example.test/x/", "event_id": "a.ics"}),
                "'//elsewhere.example.test/x/' is not on this calendar account's server",
            ),
            (
                "calendar.read",
                json!({"account": account, "calendar_id": WORK, "event_id": "../holidays/a.ics"}),
                "'event_id' must be an event id calendar.list answered.",
            ),
        ] {
            let said = told(&h.call(0, tool, args.clone()).await);
            if said != expected {
                wrong.push(format!("{args}: told {said:?}"));
            }
        }
        if !standin.requests().is_empty() {
            wrong.push(format!("requests were sent: {:?}", standin.requests()));
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_account_is_refused_with_k5a_sentences_and_a_chosen_one_is_used_by_its_name() {
        let standin = CalDavStandIn::start(client_config()).await;
        let work = Account::new("Work account", false);
        let home = Account::new("Home account", true);
        let other = CredentialBindingId::new();
        let h = harness(
            vec![work.clone(), home.clone()],
            &standin,
            vec![
                Run {
                    person: None,
                    contexts: None,
                },
                run_of(None),
                run_of(Some(json!({"caldav": null}))),
                run_of(Some(json!({"caldav": [home.id.0.to_string()]}))),
                run_of(Some(
                    json!({"caldav": [home.id.0.to_string(), work.id.0.to_string()]}),
                )),
            ],
        )
        .await;
        let calls = |id: &CredentialBindingId| json!({"account": id.0.to_string()});
        let mut wrong = Vec::new();
        for (case, run, args, expected) in [
            ("no person", 0, calls(&work.id), NO_PERSON.to_string()),
            (
                "not the person's",
                1,
                calls(&other),
                format!(
                    "'{}' is not an active calendar connection of yours.",
                    other.0
                ),
            ),
            ("none chosen", 2, calls(&home.id), NONE_CHOSEN.to_string()),
            (
                "one chosen, another named",
                3,
                calls(&work.id),
                CHOSEN_DIFFERENT.to_string(),
            ),
            (
                "two chosen, a third named",
                4,
                calls(&other),
                format!(
                    "'{}' is not an active calendar connection of yours.",
                    other.0
                ),
            ),
            (
                "nothing chosen, not granted",
                1,
                calls(&work.id),
                NOT_GRANTED.to_string(),
            ),
            (
                "a name not chosen",
                3,
                json!({"account": "Work account"}),
                "'Work account' is not an active calendar connection of yours.".to_string(),
            ),
            (
                "not a string",
                1,
                json!({"account": 7}),
                "'account' must be the id of one of your calendar accounts.".to_string(),
            ),
        ] {
            let said = told(&h.call(run, "calendar.calendars", args).await);
            if said != expected {
                wrong.push(format!("{case}: told {said:?}"));
            }
        }
        // Several chosen, one of the person's own not among them: K5a's
        // fifth sentence. The person's third account is not in the set.
        let third = Account::new("Third account", true);
        let h3 = harness(
            vec![work.clone(), home.clone(), third.clone()],
            &standin,
            vec![run_of(Some(
                json!({"caldav": [home.id.0.to_string(), work.id.0.to_string()]}),
            ))],
        )
        .await;
        let said = told(&h3.call(0, "calendar.calendars", calls(&third.id)).await);
        if said != NOT_AMONG_CHOSEN {
            wrong.push(format!("several chosen, another named: told {said:?}"));
        }
        let before = standin.requests().len();
        if before != 0 {
            wrong.push(format!("a refused call sent {before} requests"));
        }
        // Chosen accounts named by their context names are used.
        for (run, name) in [(3, "Home account"), (4, "Work account")] {
            let result = h
                .call(run, "calendar.calendars", json!({"account": name}))
                .await;
            match &result {
                Ok(ToolInvocationResult::Direct(answer)) if answer["calendars"].is_array() => {}
                other => wrong.push(format!("{name} by its name: {}", told(other))),
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_with_chosen_calendar_accounts_lists_account_as_their_names() {
        let standin = CalDavStandIn::start(client_config()).await;
        let work = Account::new("Work", true);
        let home = Account::new("Home", true);
        let h = harness(
            vec![work.clone(), home.clone()],
            &standin,
            vec![
                run_of(Some(
                    json!({"caldav": [home.id.0.to_string(), work.id.0.to_string()]}),
                )),
                run_of(None),
            ],
        )
        .await;
        let mut wrong = Vec::new();
        for (run, expected) in [
            (
                0,
                json!({
                    "type": "string",
                    "enum": ["Home", "Work"],
                    "description": "Which of your calendar accounts this call uses: Home; Work"
                }),
            ),
            (
                1,
                json!({"type": "string", "description": "The id of one of your calendar accounts."}),
            ),
        ] {
            let listed = h
                .service
                .get_available_tools_for_agent_run(
                    &TenantId::default(),
                    h.agent_id,
                    h.runs[run],
                    CONTEXT,
                )
                .await
                .unwrap();
            let mut names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
            names.sort();
            if names != vec!["calendar.calendars", "calendar.list", "calendar.read"] {
                wrong.push(format!("run {run}: listed {names:?}"));
            }
            for tool in &listed {
                if tool.input_schema["properties"]["account"] != expected {
                    wrong.push(format!(
                        "run {run}: {}'s account reads {}",
                        tool.name, tool.input_schema["properties"]["account"]
                    ));
                }
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test]
    async fn the_caldav_key_reads_the_persons_active_calendar_accounts_and_answers_their_token() {
        let mut server = mockito::Server::new_async().await;
        let c = connect(
            &mut server,
            token_body(&granted_calendar_scope(), Some(ADDRESS)),
            standin_config(TOKEN, Some(HOME_SET)),
            None,
        )
        .await;
        let connected = *c
            .outcome
            .as_ref()
            .unwrap_or_else(|e| panic!("the connect did not complete: {e:#}"));
        let calendar = c.repo.find_by_id(&connected).await.unwrap().unwrap();
        // Beside it: an expired calendar account, and an OAuth binding with
        // no calendar settings; neither is one of the person's calendars.
        let mut expired = calendar.clone();
        expired.id = CredentialBindingId::new();
        expired.status = CredentialStatus::Expired;
        expired.metadata.label = "expired@example.test".to_string();
        c.repo.save(&expired).await.unwrap();
        let mut plain = calendar.clone();
        plain.id = CredentialBindingId::new();
        plain.metadata.calendar = None;
        plain.metadata.label = "plain@example.test".to_string();
        c.repo.save(&plain).await.unwrap();
        let service = StandardCredentialManagementService::with_http_client(
            c.repo.clone(),
            c.secrets.clone(),
            Arc::new(EventBus::new(16)),
            Arc::new(calendar_registry(format!("{}/token", server.url()))),
            reqwest::Client::new(),
        );
        let pool = ToolCredentialSource::context_bindings(&service, &tenant(), USER, "caldav")
            .await
            .unwrap();
        assert_eq!(
            pool.iter()
                .map(|b| (b.id, b.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(connected, ADDRESS)],
            "the caldav key does not read the person's active calendar accounts"
        );
        let names = ToolCalendarSource::calendar_contexts(&service, &tenant(), USER)
            .await
            .unwrap();
        assert_eq!(names, pool, "the tools' names are not the start check's");

        let actor = ToolCallActor {
            tenant_id: &tenant(),
            user_id: USER,
            agent_id: AgentId::new(),
            workflow_id: None,
            context: aegis_orchestrator_core::domain::execution::ContextChoice::NotGiven,
        };
        let found = service
            .tool_calendar(&actor, &connected)
            .await
            .unwrap()
            .expect("the connected calendar is not the person's");
        assert_eq!(found.settings, google_settings(ADDRESS));
        let CalDavAuth::Bearer(token) = &found.auth else {
            panic!("an OAuth calendar account is not reached with its token as a bearer");
        };
        assert_eq!(
            token.expose(),
            TOKEN,
            "the account's token is not its access token"
        );
        for (case, id) in [("expired", expired.id), ("no calendar", plain.id)] {
            assert!(
                service.tool_calendar(&actor, &id).await.unwrap().is_none(),
                "{case} is answered as a calendar account"
            );
        }
        let stranger = ToolCallActor {
            user_id: "someone-else",
            ..actor
        };
        assert!(
            service
                .tool_calendar(&stranger, &connected)
                .await
                .unwrap()
                .is_none(),
            "another person's calendar is answered"
        );
    }

    // -----------------------------------------------------------------------
    // Test doubles the dispatch reads
    // -----------------------------------------------------------------------
    struct Executions(HashMap<ExecutionId, Execution>);

    #[async_trait]
    impl ExecutionService for Executions {
        async fn start_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn start_execution_with_id(
            &self,
            execution_id: ExecutionId,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            Ok(execution_id)
        }
        async fn start_child_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: ExecutionId,
        ) -> Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn get_execution_for_tenant(
            &self,
            _: &TenantId,
            id: ExecutionId,
        ) -> Result<Execution> {
            self.get_execution_unscoped(id).await
        }
        async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
            self.0
                .get(&id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("execution not found"))
        }
        async fn get_iterations_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> Result<Vec<Iteration>> {
            anyhow::bail!("not exercised")
        }
        async fn cancel_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn stream_execution(
            &self,
            _: ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
            anyhow::bail!("not exercised")
        }
        async fn stream_agent_events(
            &self,
            _: AgentId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
            anyhow::bail!("not exercised")
        }
        async fn list_executions_for_tenant(
            &self,
            _: &TenantId,
            _: Option<AgentId>,
            _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
            _: usize,
        ) -> Result<Vec<Execution>> {
            anyhow::bail!("not exercised")
        }
        async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn record_llm_interaction(
            &self,
            _: ExecutionId,
            _: u8,
            _: aegis_orchestrator_core::domain::execution::LlmInteraction,
        ) -> Result<()> {
            Ok(())
        }
        async fn store_iteration_trajectory(
            &self,
            _: ExecutionId,
            _: u8,
            _: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
        ) -> Result<()> {
            Ok(())
        }
    }

    /// Resolves every agent to one agent with no `tool_validation`, so the
    /// inner-loop judge does not run.
    struct OneAgent(Agent);

    #[async_trait]
    impl AgentLifecycleService for OneAgent {
        async fn deploy_agent_for_tenant(
            &self,
            _: &TenantId,
            _: AgentManifest,
            _: bool,
            _: aegis_orchestrator_core::domain::agent::AgentScope,
            _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> Result<AgentId> {
            anyhow::bail!("not exercised")
        }
        async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
            Ok(self.0.clone())
        }
        async fn update_agent_for_tenant(
            &self,
            _: &TenantId,
            _: AgentId,
            _: AgentManifest,
        ) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn list_agents_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
            Ok(vec![self.0.clone()])
        }
        async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
            Ok(Some(self.0.id))
        }
        async fn lookup_agent_visible_for_tenant(
            &self,
            _: &TenantId,
            _: &str,
        ) -> Result<Option<AgentId>> {
            Ok(Some(self.0.id))
        }
        async fn lookup_agent_for_tenant_with_version(
            &self,
            _: &TenantId,
            _: &str,
            _: &str,
        ) -> Result<Option<AgentId>> {
            anyhow::bail!("not exercised")
        }
        async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
            Ok(vec![self.0.clone()])
        }
        async fn list_versions_for_tenant(
            &self,
            _: &TenantId,
            _: AgentId,
        ) -> Result<Vec<AgentVersion>> {
            Ok(vec![])
        }
    }

    struct NoOpPublisher;

    #[async_trait]
    impl aegis_orchestrator_core::domain::fsal::EventPublisher for NoOpPublisher {
        async fn publish_storage_event(
            &self,
            _event: aegis_orchestrator_core::domain::events::StorageEvent,
        ) {
        }
    }

    // -----------------------------------------------------------------------
    // K6, K6a, K6b, K6c, K6e, K7, K7a: the write tools
    // -----------------------------------------------------------------------

    /// The four calendar writes, against the loopback CalDAV stand-in: the
    /// tools' requests driven directly (ungated, as the approved run drives
    /// them), and the tool service with the approval gate for the contracts,
    /// the admission before the gate and the approved run.
    mod write_tools {
        use super::*;
        use crate::caldav_standins::Recorded;
        use aegis_orchestrator_core::application::tool_approval_service::ToolApprovalService;
        use aegis_orchestrator_core::application::tools::builtin_calendar::{
            CalendarActing, CalendarTools, BAD_DESCRIPTION, BAD_RESPONSE, BAD_TITLE, EVENT_CHANGED,
            MIXED_TIMES, NOTHING_TO_UPDATE, NOT_ATTENDEE, NOT_ORGANISER, NO_ETAG, REPEATS,
            TOO_MANY_ATTENDEES,
        };
        use aegis_orchestrator_core::domain::execution::ServerChoice;
        use aegis_orchestrator_core::domain::node_config::ToolCapabilityConfig;
        use aegis_orchestrator_core::domain::tool_approval::{
            ApprovalContract, ToolApprovalDecision, ToolApprovalId, ToolApprovalRepository,
            ToolApprovalStatus,
        };
        use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;

        /// An event the account organises: zoned times, one attendee who
        /// accepted and one who has not answered, an alarm and a property
        /// the tools do not read.
        const MINE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Stand-in//EN\r\nBEGIN:VEVENT\r\nUID:mine@example.test\r\nDTSTAMP:20261001T000000Z\r\nDTSTART;TZID=Europe/Berlin:20261009T100000\r\nDTEND;TZID=Europe/Berlin:20261009T110000\r\nSUMMARY:Board review\r\nLOCATION:Room 4\r\nSEQUENCE:3\r\nX-STAND-IN-KEEP:untouched\r\nORGANIZER;CN=Me:mailto:a@example.test\r\nATTENDEE;CN=Ann;PARTSTAT=ACCEPTED:mailto:ann@example.test\r\nATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:dave@example.test\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        /// An event Jane organises, to which the account is invited.
        const THEIRS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Stand-in//EN\r\nBEGIN:VEVENT\r\nUID:theirs@example.test\r\nDTSTAMP:20261001T000000Z\r\nDTSTART:20261010T090000Z\r\nDTEND:20261010T100000Z\r\nSUMMARY:Jane's sync\r\nSEQUENCE:1\r\nORGANIZER;CN=Jane:mailto:jane@example.test\r\nATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:MAILTO:A@Example.Test\r\nATTENDEE;CN=Sam;PARTSTAT=ACCEPTED:mailto:sam@example.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        /// A weekly event the account organises.
        const WEEKLY_MINE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Stand-in//EN\r\nBEGIN:VEVENT\r\nUID:weekly-mine@example.test\r\nDTSTART:20261006T090000Z\r\nDTEND:20261006T100000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Weekly\r\nORGANIZER:mailto:a@example.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        /// An event with no organiser and no attendees.
        const SOLO: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Stand-in//EN\r\nBEGIN:VEVENT\r\nUID:solo@example.test\r\nDTSTART:20261011T090000Z\r\nDTEND:20261011T100000Z\r\nSUMMARY:Focus time\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        fn event(name: &str, etag: &str, ics: &str) -> StandInEvent {
            StandInEvent {
                name: name.to_string(),
                etag: etag.to_string(),
                ics: ics.to_string(),
            }
        }

        /// The client's stand-in with the write tests' events on Work.
        fn write_config() -> StandInConfig {
            let mut config = client_config();
            config.calendars[0].events.extend([
                event("mine.ics", "\"etag-mine-1\"", MINE),
                event("theirs.ics", "\"etag-theirs-1\"", THEIRS),
                event("weekly-mine.ics", "\"etag-weekly-1\"", WEEKLY_MINE),
                event("solo.ics", "\"etag-solo-1\"", SOLO),
                event("no-etag.ics", "", SOLO),
            ]);
            config
        }

        /// A conversation of the person with nothing chosen: admitted on
        /// ownership alone.
        fn conversation() -> CalendarActing {
            CalendarActing {
                tenant_id: TenantId::default(),
                user_id: Some(PERSON.to_string()),
                agent_id: AgentId::new(),
                workflow_id: None,
                choice: ServerChoice::NotGiven,
                has_execution_record: false,
            }
        }

        fn tools(account: &Account, standin: &CalDavStandIn) -> CalendarTools {
            CalendarTools::with_transport(
                Arc::new(Accounts(vec![account.clone()])),
                Arc::new(ReqwestTransport::to_origin(standin.origin())),
            )
        }

        /// The sentence a refusal tells its caller.
        fn refusal(error: &SealSessionError) -> String {
            match error {
                SealSessionError::Answered {
                    answer:
                        CallerAnswer::Conflict(message)
                        | CallerAnswer::CredentialBindingRequired { message },
                    ..
                } => message.clone(),
                SealSessionError::InvalidArguments(message)
                | SealSessionError::UpstreamUnavailable(message) => message.clone(),
                other => format!("{other:?}"),
            }
        }

        /// What a tool's call answered, or the sentence its refusal tells.
        fn said(result: &std::result::Result<Value, SealSessionError>) -> String {
            match result {
                Ok(value) => format!("answered {value}"),
                Err(e) => refusal(e),
            }
        }

        /// The sentence a service call's refusal tells its caller.
        fn told_by(result: &std::result::Result<ToolInvocationResult, SealSessionError>) -> String {
            match result {
                Ok(ToolInvocationResult::Direct(value)) => format!("answered {value}"),
                Ok(_) => "the call was dispatched".to_string(),
                Err(e) => refusal(e),
            }
        }

        /// The requests that would change something.
        fn writes(requests: &[Recorded]) -> Vec<String> {
            requests
                .iter()
                .filter(|r| r.method == "PUT" || r.method == "DELETE")
                .map(|r| format!("{} {}", r.method, r.path))
                .collect()
        }

        /// The unfolded content lines of an iCalendar body.
        fn lines(ics: &str) -> Vec<String> {
            aegis_orchestrator_core::infrastructure::calendar::ical::unfold(ics)
                .split("\r\n")
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        }

        fn path(name: &str) -> String {
            format!("{WORK}{name}")
        }

        // --- each write's request ----------------------------------------

        #[tokio::test]
        async fn calendar_create_puts_a_new_resource_with_if_none_match_its_times_in_utc_and_its_invitations(
        ) {
            let standin = CalDavStandIn::start(write_config()).await;
            let account = Account::new("Work account", true);
            let tools = tools(&account, &standin);
            let mut wrong = Vec::new();
            let result = tools
                .invoke(
                    "calendar.create",
                    &json!({
                        "account": account.id.0.to_string(),
                        "calendar_id": WORK,
                        "title": "Board review, Q4",
                        "start": "2026-10-09T10:00:00+02:00",
                        "end": "2026-10-09T11:00:00+02:00",
                        "location": "Room 4",
                        "attendees": ["ann@example.test", "bob@example.test"],
                    }),
                    &conversation(),
                )
                .await;
            let answer = match &result {
                Ok(answer) => answer.clone(),
                Err(e) => panic!("calendar.create failed: {}", refusal(e)),
            };
            let event_id = answer["event_id"].as_str().unwrap_or_default().to_string();
            let id = event_id.strip_suffix(".ics").unwrap_or_default();
            if uuid::Uuid::parse_str(id).is_err() {
                wrong.push(format!("the event id {event_id:?} is not <uuid>.ics"));
            }
            if answer["uid"] != json!(format!("{id}@example.test")) {
                wrong.push(format!("the uid is {}", answer["uid"]));
            }
            if answer["account"] != json!(account.id.0.to_string()) || answer["calendar_id"] != WORK
            {
                wrong.push(format!("the answer is {answer}"));
            }
            let requests = standin.requests();
            let puts: Vec<&Recorded> = requests.iter().filter(|r| r.method == "PUT").collect();
            match puts.as_slice() {
                [put] => {
                    if put.path != path(&event_id) {
                        wrong.push(format!("the PUT went to {}", put.path));
                    }
                    if put.header("if-none-match") != Some("*") || put.header("if-match").is_some()
                    {
                        wrong.push(format!(
                            "the PUT's conditions are {:?} / {:?}",
                            put.header("if-none-match"),
                            put.header("if-match")
                        ));
                    }
                    if put.header("content-type") != Some("text/calendar; charset=utf-8") {
                        wrong.push(format!("the PUT is {:?}", put.header("content-type")));
                    }
                    let written = lines(&put.body);
                    for line in [
                        format!("UID:{id}@example.test"),
                        "DTSTART:20261009T080000Z".to_string(),
                        "DTEND:20261009T090000Z".to_string(),
                        "SUMMARY:Board review\\, Q4".to_string(),
                        "LOCATION:Room 4".to_string(),
                        "SEQUENCE:0".to_string(),
                        "ORGANIZER:mailto:a@example.test".to_string(),
                        "ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:ann@example.test"
                            .to_string(),
                        "ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@example.test"
                            .to_string(),
                    ] {
                        if !written.contains(&line) {
                            wrong.push(format!("the PUT's body has no line {line:?}"));
                        }
                    }
                }
                other => wrong.push(format!("{} PUTs, not 1", other.len())),
            }
            match standin.event(WORK, &event_id) {
                Some(stored) => {
                    if answer["etag"] != json!(stored.etag) {
                        wrong.push(format!(
                            "the answered etag {} is not the resource's {}",
                            answer["etag"], stored.etag
                        ));
                    }
                }
                None => wrong.push("the stand-in does not hold the new event".to_string()),
            }

            let all_day = tools
                .invoke(
                    "calendar.create",
                    &json!({
                        "account": account.id.0.to_string(),
                        "calendar_id": WORK,
                        "title": "Offsite",
                        "start": "2026-10-12",
                        "end": "2026-10-14",
                    }),
                    &conversation(),
                )
                .await;
            match &all_day {
                Ok(answer) => {
                    let id = answer["event_id"].as_str().unwrap_or_default();
                    let body = standin
                        .event(WORK, id)
                        .map(|e| lines(&e.ics))
                        .unwrap_or_default();
                    if !body.contains(&"DTSTART;VALUE=DATE:20261012".to_string())
                        || !body.contains(&"DTEND;VALUE=DATE:20261014".to_string())
                    {
                        wrong.push(format!("an all-day event was written as {body:?}"));
                    }
                    if body
                        .iter()
                        .any(|l| l.starts_with("ORGANIZER") || l.starts_with("ATTENDEE"))
                    {
                        wrong.push("an event without attendees names an organiser".to_string());
                    }
                }
                Err(e) => wrong.push(format!("the all-day create failed: {}", refusal(e))),
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }

        #[tokio::test]
        async fn calendar_update_reads_then_puts_with_if_match_changing_only_the_named_properties_and_counting_sequence(
        ) {
            let standin = CalDavStandIn::start(write_config()).await;
            let account = Account::new("Work account", true);
            let tools = tools(&account, &standin);
            let mut wrong = Vec::new();
            let before = lines(MINE);
            let result = tools
                .invoke(
                    "calendar.update",
                    &json!({
                        "account": account.id.0.to_string(),
                        "calendar_id": WORK,
                        "event_id": "mine.ics",
                        "title": "Board review (moved)",
                        "start": "2026-10-09T12:00:00Z",
                        "end": "2026-10-09T13:30:00+00:00",
                    }),
                    &conversation(),
                )
                .await;
            let requests = standin.requests();
            let order: Vec<String> = requests
                .iter()
                .map(|r| format!("{} {}", r.method, r.path))
                .collect();
            if order
                != [
                    format!("GET {}", path("mine.ics")),
                    format!("PUT {}", path("mine.ics")),
                ]
            {
                wrong.push(format!("the requests were {order:?}"));
            }
            if let Some(put) = requests.iter().find(|r| r.method == "PUT") {
                if put.header("if-match") != Some("\"etag-mine-1\"")
                    || put.header("if-none-match").is_some()
                {
                    wrong.push(format!(
                        "the PUT's condition is If-Match {:?}",
                        put.header("if-match")
                    ));
                }
                let after = lines(&put.body);
                let changed: Vec<&String> = after.iter().filter(|l| !before.contains(l)).collect();
                let dropped: Vec<&String> = before.iter().filter(|l| !after.contains(l)).collect();
                let changed_names: Vec<&str> = changed
                    .iter()
                    .map(|l| l.split([':', ';']).next().unwrap_or_default())
                    .collect();
                if changed_names != ["DTSTAMP", "DTSTART", "DTEND", "SUMMARY", "SEQUENCE"] {
                    wrong.push(format!("the changed lines are {changed:?}"));
                }
                if dropped.len() != changed.len() {
                    wrong.push(format!("lines were dropped: {dropped:?}"));
                }
                for line in [
                    "SUMMARY:Board review (moved)",
                    "DTSTART:20261009T120000Z",
                    "DTEND:20261009T133000Z",
                    "SEQUENCE:4",
                ] {
                    if !after.iter().any(|l| l == line) {
                        wrong.push(format!("the PUT's body has no line {line:?}"));
                    }
                }
                if after.iter().any(|l| l == "DTSTAMP:20261001T000000Z") {
                    wrong.push("DTSTAMP was not refreshed".to_string());
                }
            }
            match &result {
                Ok(answer) => {
                    let stored = standin.event(WORK, "mine.ics").map(|e| e.etag);
                    if answer["etag"] != json!(stored) || answer["event_id"] != "mine.ics" {
                        wrong.push(format!("the answer is {answer}"));
                    }
                }
                Err(e) => wrong.push(format!("calendar.update failed: {}", refusal(e))),
            }

            // The attendees replaced: Ann kept with her answer, Dave removed,
            // Carol invited.
            let replaced = tools
                .invoke(
                    "calendar.update",
                    &json!({
                        "account": account.id.0.to_string(),
                        "calendar_id": WORK,
                        "event_id": "mine.ics",
                        "attendees": ["ANN@example.test", "carol@example.test"],
                    }),
                    &conversation(),
                )
                .await;
            if let Err(e) = &replaced {
                wrong.push(format!("the attendee update failed: {}", refusal(e)));
            }
            let attendees: Vec<String> = standin
                .event(WORK, "mine.ics")
                .map(|e| lines(&e.ics))
                .unwrap_or_default()
                .into_iter()
                .filter(|l| l.starts_with("ATTENDEE"))
                .collect();
            if attendees
                != [
                    "ATTENDEE;CN=Ann;PARTSTAT=ACCEPTED:mailto:ann@example.test",
                    "ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:carol@example.test",
                ]
            {
                wrong.push(format!("the attendees are {attendees:?}"));
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }

        #[tokio::test]
        async fn calendar_delete_reads_then_deletes_with_if_match() {
            let standin = CalDavStandIn::start(write_config()).await;
            let account = Account::new("Work account", true);
            let result = tools(&account, &standin)
                .invoke(
                    "calendar.delete",
                    &json!({"account": account.id.0.to_string(), "calendar_id": WORK, "event_id": "mine.ics"}),
                    &conversation(),
                )
                .await;
            let mut wrong = Vec::new();
            match &result {
                Ok(answer) => {
                    if answer["deleted"] != true || answer["event_id"] != "mine.ics" {
                        wrong.push(format!("the answer is {answer}"));
                    }
                }
                Err(e) => wrong.push(format!("calendar.delete failed: {}", refusal(e))),
            }
            let requests = standin.requests();
            let order: Vec<String> = requests
                .iter()
                .map(|r| format!("{} {}", r.method, r.path))
                .collect();
            if order
                != [
                    format!("GET {}", path("mine.ics")),
                    format!("DELETE {}", path("mine.ics")),
                ]
            {
                wrong.push(format!("the requests were {order:?}"));
            }
            if let Some(delete) = requests.iter().find(|r| r.method == "DELETE") {
                if delete.header("if-match") != Some("\"etag-mine-1\"") {
                    wrong.push(format!(
                        "the DELETE's condition is If-Match {:?}",
                        delete.header("if-match")
                    ));
                }
            }
            if standin.event(WORK, "mine.ics").is_some() {
                wrong.push("the event is still there".to_string());
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }

        #[tokio::test]
        async fn calendar_respond_sets_the_accounts_partstat_and_puts_with_if_match() {
            let standin = CalDavStandIn::start(write_config()).await;
            let account = Account::new("Work account", true);
            let result = tools(&account, &standin)
                .invoke(
                    "calendar.respond",
                    &json!({"account": account.id.0.to_string(), "calendar_id": WORK, "event_id": "theirs.ics", "response": "tentative"}),
                    &conversation(),
                )
                .await;
            let mut wrong = Vec::new();
            match &result {
                Ok(answer) => {
                    if answer["response"] != "tentative" || answer["event_id"] != "theirs.ics" {
                        wrong.push(format!("the answer is {answer}"));
                    }
                }
                Err(e) => wrong.push(format!("calendar.respond failed: {}", refusal(e))),
            }
            let requests = standin.requests();
            match requests.iter().find(|r| r.method == "PUT") {
                Some(put) => {
                    if put.header("if-match") != Some("\"etag-theirs-1\"") {
                        wrong.push(format!(
                            "the PUT's condition is If-Match {:?}",
                            put.header("if-match")
                        ));
                    }
                    let before = lines(THEIRS);
                    let after = lines(&put.body);
                    let changed: Vec<&String> =
                        after.iter().filter(|l| !before.contains(l)).collect();
                    let names: Vec<&str> = changed
                        .iter()
                        .map(|l| l.split([':', ';']).next().unwrap_or_default())
                        .collect();
                    if names != ["DTSTAMP", "ATTENDEE"]
                        || !after.contains(
                            &"ATTENDEE;PARTSTAT=TENTATIVE;RSVP=TRUE:MAILTO:A@Example.Test"
                                .to_string(),
                        )
                    {
                        wrong.push(format!("the changed lines are {changed:?}"));
                    }
                    if after.len() != before.len() {
                        wrong.push(format!(
                            "the body has {} lines, not {}",
                            after.len(),
                            before.len()
                        ));
                    }
                }
                None => wrong.push("no PUT was sent".to_string()),
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }

        // --- a 412, and every K6a refusal before any change ---------------

        #[tokio::test]
        async fn a_412_on_any_write_is_answered_with_its_sentence_and_nothing_changes() {
            let standin = CalDavStandIn::start(write_config()).await;
            let account = Account::new("Work account", true);
            let tools = tools(&account, &standin);
            let id = account.id.0.to_string();
            let mut wrong = Vec::new();
            standin.move_etag_after_get(true);
            for (tool, args, name) in [
                (
                    "calendar.update",
                    json!({"account": id, "calendar_id": WORK, "event_id": "mine.ics", "title": "Changed"}),
                    "mine.ics",
                ),
                (
                    "calendar.delete",
                    json!({"account": id, "calendar_id": WORK, "event_id": "mine.ics"}),
                    "mine.ics",
                ),
                (
                    "calendar.respond",
                    json!({"account": id, "calendar_id": WORK, "event_id": "theirs.ics", "response": "declined"}),
                    "theirs.ics",
                ),
            ] {
                let before = standin.event(WORK, name).map(|e| e.ics);
                let result = tools.invoke(tool, &args, &conversation()).await;
                if said(&result) != EVENT_CHANGED {
                    wrong.push(format!("{tool} after a change answered: {}", said(&result)));
                }
                if standin.event(WORK, name).map(|e| e.ics) != before {
                    wrong.push(format!("{tool} changed the event"));
                }
            }
            standin.move_etag_after_get(false);
            standin.refuse_writes_412(true);
            let count = standin.events(WORK).len();
            let created = tools
                .invoke(
                    "calendar.create",
                    &json!({"account": id, "calendar_id": WORK, "title": "New", "start": "2026-10-09T10:00:00Z", "end": "2026-10-09T11:00:00Z"}),
                    &conversation(),
                )
                .await;
            if said(&created) != EVENT_CHANGED {
                wrong.push(format!("a create answered 412 said: {}", said(&created)));
            }
            if standin.events(WORK).len() != count {
                wrong.push("a refused create added an event".to_string());
            }
            if let Err(SealSessionError::Answered { answer, .. }) = &created {
                if !matches!(answer, CallerAnswer::Conflict(_)) {
                    wrong.push(format!("a 412 is answered as {answer:?}, not a conflict"));
                }
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }

        #[tokio::test]
        async fn every_k6a_refusal_comes_before_any_change() {
            let standin = CalDavStandIn::start(write_config()).await;
            let account = Account::new("Work account", true);
            let tools = tools(&account, &standin);
            let id = account.id.0.to_string();
            let create = |extra: Value| {
                let mut args = json!({"account": id, "calendar_id": WORK, "title": "New", "start": "2026-10-09T10:00:00Z", "end": "2026-10-09T11:00:00Z"});
                for (k, v) in extra.as_object().unwrap() {
                    args[k] = v.clone();
                }
                args
            };
            let many: Vec<String> = (0..51).map(|i| format!("p{i}@example.test")).collect();
            let mut wrong = Vec::new();
            // (tool, arguments, sentence, whether a request may be sent: the
            // event is read, never written)
            let cases: Vec<(&str, Value, String, bool)> = vec![
                ("calendar.update", json!({"account": id, "calendar_id": WORK, "event_id": "weekly-mine.ics", "title": "x"}), REPEATS.to_string(), true),
                ("calendar.delete", json!({"account": id, "calendar_id": WORK, "event_id": "weekly-mine.ics"}), REPEATS.to_string(), true),
                ("calendar.update", json!({"account": id, "calendar_id": WORK, "event_id": "theirs.ics", "title": "x"}), NOT_ORGANISER.to_string(), true),
                ("calendar.delete", json!({"account": id, "calendar_id": WORK, "event_id": "theirs.ics"}), NOT_ORGANISER.to_string(), true),
                ("calendar.respond", json!({"account": id, "calendar_id": WORK, "event_id": "solo.ics", "response": "accepted"}), NOT_ATTENDEE.to_string(), true),
                ("calendar.update", json!({"account": id, "calendar_id": WORK, "event_id": "mine.ics", "end": "2026-10-09T07:00:00Z"}), END_NOT_AFTER_START.to_string(), true),
                ("calendar.update", json!({"account": id, "calendar_id": WORK, "event_id": "no-etag.ics", "title": "x"}), NO_ETAG.to_string(), true),
                ("calendar.create", create(json!({"end": "2026-10-09T10:00:00Z"})), END_NOT_AFTER_START.to_string(), false),
                ("calendar.create", create(json!({"end": "2026-10-10"})), MIXED_TIMES.to_string(), false),
                ("calendar.create", create(json!({"start": "tomorrow"})), "'start' must be a time in RFC 3339 form with an offset, or a date written YYYY-MM-DD.".to_string(), false),
                ("calendar.create", create(json!({"attendees": ["ann@example.test", "not an address"]})), "'not an address' is not an email address this tool can invite.".to_string(), false),
                ("calendar.create", create(json!({"attendees": many})), TOO_MANY_ATTENDEES.to_string(), false),
                ("calendar.create", create(json!({"title": "Two\nlines"})), BAD_TITLE.to_string(), false),
                ("calendar.create", create(json!({"title": "t".repeat(1001)})), BAD_TITLE.to_string(), false),
                ("calendar.create", create(json!({"description": "d".repeat(32_001)})), BAD_DESCRIPTION.to_string(), false),
                ("calendar.respond", json!({"account": id, "calendar_id": WORK, "event_id": "theirs.ics", "response": "maybe"}), BAD_RESPONSE.to_string(), false),
                ("calendar.update", json!({"account": id, "calendar_id": WORK, "event_id": "mine.ics"}), NOTHING_TO_UPDATE.to_string(), false),
            ];
            for (tool, args, expected, may_read) in cases {
                let sent = standin.requests().len();
                let result = tools.invoke(tool, &args, &conversation()).await;
                let shown: String = said(&result).chars().take(160).collect();
                if said(&result) != expected {
                    wrong.push(format!("{tool} {expected:?}: said {shown}"));
                }
                let after = standin.requests();
                if !may_read && after.len() != sent {
                    wrong.push(format!("{tool} {expected:?}: a request was sent"));
                }
                let written = writes(&after[sent..]);
                if !written.is_empty() {
                    wrong.push(format!("{tool} {expected:?}: wrote {written:?}"));
                }
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }

        // --- the tool service with the approval gate ----------------------

        #[tokio::test]
        async fn the_four_writes_are_gated_with_their_contracts_and_no_capability_entry_clears_the_mark(
        ) {
            let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
            let tools = router.list_tools().await.unwrap();
            let mut wrong = Vec::new();
            let invites =
                "Attendees may be sent an invitation or an update by the calendar's server.";
            for (name, required, summary, says_invites) in [
                (
                    "calendar.create",
                    vec!["account", "calendar_id", "title", "start", "end"],
                    vec![
                        "account",
                        "calendar_id",
                        "title",
                        "start",
                        "end",
                        "attendees",
                        "location",
                    ],
                    true,
                ),
                (
                    "calendar.update",
                    vec!["account", "calendar_id", "event_id"],
                    vec![
                        "account",
                        "event_id",
                        "current_title",
                        "current_start",
                        "title",
                        "start",
                        "end",
                        "attendees",
                    ],
                    true,
                ),
                (
                    "calendar.delete",
                    vec!["account", "calendar_id", "event_id"],
                    vec!["account", "event_id", "title", "start", "end", "attendees"],
                    false,
                ),
                (
                    "calendar.respond",
                    vec!["account", "calendar_id", "event_id", "response"],
                    vec![
                        "account",
                        "event_id",
                        "title",
                        "start",
                        "organizer",
                        "repeats",
                        "response",
                    ],
                    false,
                ),
            ] {
                let Some(tool) = tools.iter().find(|t| t.name == name) else {
                    wrong.push(format!("{name} is not listed"));
                    continue;
                };
                let schema_required: Vec<&str> = tool.input_schema["required"]
                    .as_array()
                    .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                if schema_required != required {
                    wrong.push(format!("{name}'s schema requires {schema_required:?}"));
                }
                if ToolInputContract::required_fields(name) != required.as_slice() {
                    wrong.push(format!(
                        "{name}'s input contract requires {:?}",
                        ToolInputContract::required_fields(name)
                    ));
                }
                for offered in ["current_title", "current_start", "organizer", "repeats"] {
                    if tool.input_schema["properties"].get(offered).is_some() {
                        wrong.push(format!("{name} offers {offered}"));
                    }
                }
                if !router.requires_approval(name) {
                    wrong.push(format!("{name} is not gated"));
                }
                if router.is_skip_judge(name).await {
                    wrong.push(format!("{name} skips the judge"));
                }
                let expected = ApprovalContract {
                    binding_argument: Some("account".to_string()),
                    approval_summary: Some(summary.iter().map(|s| s.to_string()).collect()),
                };
                if router.approval_contract(name) != expected {
                    wrong.push(format!(
                        "{name}'s approval contract is {:?}",
                        router.approval_contract(name)
                    ));
                }
                if tool.description.contains(invites) != says_invites {
                    wrong.push(format!("{name}'s description: {}", tool.description));
                }
            }
            // A node configuration whose entries say `false` clears nothing.
            let mut dispatchers = ToolRouter::builtin_dispatchers();
            for dispatcher in &mut dispatchers {
                for capability in &mut dispatcher.capabilities {
                    capability.requires_approval = false;
                }
            }
            let entries: Vec<ToolCapabilityConfig> = serde_yaml::from_str(
                "- tool_pattern: calendar.*\n  requires_approval: false\n- tool_pattern: calendar.create\n  requires_approval: false\n",
            )
            .unwrap();
            let cleared = ToolRouter::new(dispatchers).with_tool_capabilities(&entries);
            for name in [
                "calendar.create",
                "calendar.update",
                "calendar.delete",
                "calendar.respond",
            ] {
                if !cleared.requires_approval(name) {
                    wrong.push(format!("{name}'s mark was cleared by an entry at false"));
                }
            }
            for name in ["calendar.calendars", "calendar.list", "calendar.read"] {
                if cleared.requires_approval(name) || router.requires_approval(name) {
                    wrong.push(format!("{name} is gated"));
                }
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }

        fn writing_agent() -> Agent {
            let manifest: AgentManifest = serde_yaml::from_str(
                r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: calendar-write-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: ["calendar.calendars", "calendar.list", "calendar.read", "calendar.create", "calendar.update", "calendar.delete", "calendar.respond"]
"#,
            )
            .unwrap();
            Agent {
                id: AgentId::new(),
                tenant_id: TenantId::default(),
                scope: aegis_orchestrator_core::domain::agent::AgentScope::default(),
                name: manifest.metadata.name.clone(),
                manifest,
                status: AgentStatus::Active,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }
        }

        struct Gated {
            service: Arc<ToolInvocationService>,
            approvals: Arc<ToolApprovalService>,
            repo: Arc<InMemoryToolApprovalRepository>,
            event_bus: Arc<EventBus>,
            agent_id: AgentId,
            runs: Vec<ExecutionId>,
        }

        /// The tool service with the calendar tools over `accounts`, every
        /// request going to `standin`, the approval gate, and one execution
        /// of [`PERSON`] per entry of `runs` (its `contexts`).
        async fn gated(
            accounts: Vec<Account>,
            standin: &CalDavStandIn,
            runs: &[Option<Value>],
        ) -> Gated {
            let agent = writing_agent();
            let agent_id = agent.id;
            let executions: Vec<Execution> = runs
                .iter()
                .map(|contexts| {
                    let mut e = Execution::new_with_id(
                        ExecutionId::new(),
                        agent_id,
                        ExecutionInput {
                            intent: None,
                            input: match contexts {
                                Some(contexts) => json!({ "contexts": contexts }),
                                None => json!({}),
                            },
                            workspace_volume_id: None,
                            workspace_volume_mount_path: None,
                            workspace_remote_path: None,
                            workflow_execution_id: None,
                            attachments: Vec::new(),
                        },
                        5,
                        CONTEXT.to_string(),
                    );
                    e.tenant_id = TenantId::default();
                    e.initiating_user_sub = Some(PERSON.to_string());
                    e
                })
                .collect();
            let ids = executions.iter().map(|e| e.id).collect();
            let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
            security_context_repo
                .save(security_context())
                .await
                .unwrap();
            let storage_root = std::env::temp_dir().join(format!(
                "aegis-calendar-write-tests-{}",
                uuid::Uuid::new_v4()
            ));
            let fsal = Arc::new(AegisFSAL::new(
                Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
                Arc::new(InMemoryVolumeRepository::new()),
                Arc::new(parking_lot::RwLock::new(HashMap::new())),
                Arc::new(NoOpPublisher),
            ));
            let event_bus = Arc::new(EventBus::new(1024));
            let repo = Arc::new(InMemoryToolApprovalRepository::new());
            let approvals = Arc::new(ToolApprovalService::new(repo.clone(), event_bus.clone()));
            let accounts = Arc::new(Accounts(accounts));
            let service = ToolInvocationService::new(
                Arc::new(InMemorySealSessionRepository::new()),
                security_context_repo,
                Arc::new(SealMiddleware::new()),
                Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
                fsal,
                NfsVolumeRegistry::new(),
                Arc::new(OneAgent(agent)),
                Arc::new(Executions(
                    executions.into_iter().map(|e| (e.id, e)).collect(),
                )),
                Arc::new(
                    aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(),
                ),
                event_bus.clone(),
                None,
            )
            .with_tool_approvals(approvals.clone())
            .with_tool_credentials(accounts.clone())
            .with_calendar_tools_over(
                accounts,
                Arc::new(ReqwestTransport::to_origin(standin.origin())),
            );
            Gated {
                service: Arc::new(service),
                approvals,
                repo,
                event_bus,
                agent_id,
                runs: ids,
            }
        }

        impl Gated {
            async fn call(
                &self,
                run: usize,
                tool: &str,
                args: Value,
            ) -> std::result::Result<ToolInvocationResult, SealSessionError> {
                self.service
                    .invoke_tool_internal(
                        &self.agent_id,
                        self.runs[run],
                        TenantId::default(),
                        0,
                        Vec::new(),
                        tool.to_string(),
                        args,
                    )
                    .await
            }

            async fn rows(
                &self,
            ) -> Vec<aegis_orchestrator_core::domain::tool_approval::ToolApprovalRequest>
            {
                self.repo
                    .list_requests_for_user(&TenantId::default(), PERSON, None)
                    .await
                    .unwrap()
            }
        }

        fn pending(
            result: &std::result::Result<ToolInvocationResult, SealSessionError>,
        ) -> Option<Value> {
            match result {
                Ok(ToolInvocationResult::Direct(value))
                    if value["status"] == "approval_pending" =>
                {
                    Some(value.clone())
                }
                _ => None,
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn the_admission_names_the_account_by_id_and_writes_the_events_values_over_the_models(
        ) {
            let standin = CalDavStandIn::start(write_config()).await;
            let work = Account::new("Work account", false);
            let id = work.id.0.to_string();
            let h = gated(
                vec![work.clone()],
                &standin,
                &[Some(json!({"caldav": [id.clone()]}))],
            )
            .await;
            let mut wrong = Vec::new();
            let cases = [
                (
                    "calendar.create",
                    json!({"account": "Work account", "calendar_id": WORK, "title": "Plan", "start": "2026-10-09T10:00:00+02:00", "end": "2026-10-09T11:00:00+02:00", "attendees": ["ann@example.test", "bob@example.test"], "location": "Room 4"}),
                    format!("calendar.create\naccount: {id}\ncalendar_id: {WORK}\ntitle: Plan\nstart: 2026-10-09T10:00:00+02:00\nend: 2026-10-09T11:00:00+02:00\nattendees: ann@example.test, bob@example.test\nlocation: Room 4"),
                    vec![],
                ),
                (
                    "calendar.update",
                    json!({"account": "Work account", "calendar_id": WORK, "event_id": "mine.ics", "title": "Board review (moved)", "current_title": "Harmless", "current_start": "never"}),
                    format!("calendar.update\naccount: {id}\nevent_id: mine.ics\ncurrent_title: Board review\ncurrent_start: 2026-10-09T10:00:00 Europe/Berlin\ntitle: Board review (moved)\nstart: \nend: \nattendees: "),
                    vec![("current_title", json!("Board review")), ("current_start", json!("2026-10-09T10:00:00 Europe/Berlin"))],
                ),
                (
                    "calendar.delete",
                    json!({"account": "Work account", "calendar_id": WORK, "event_id": "mine.ics", "title": "Nothing", "start": "never", "attendees": []}),
                    format!("calendar.delete\naccount: {id}\nevent_id: mine.ics\ntitle: Board review\nstart: 2026-10-09T10:00:00 Europe/Berlin\nend: 2026-10-09T11:00:00 Europe/Berlin\nattendees: ann@example.test, dave@example.test"),
                    vec![("title", json!("Board review")), ("attendees", json!(["ann@example.test", "dave@example.test"]))],
                ),
                (
                    "calendar.respond",
                    json!({"account": "Work account", "calendar_id": WORK, "event_id": "theirs.ics", "response": "accepted", "organizer": "me", "repeats": true}),
                    format!("calendar.respond\naccount: {id}\nevent_id: theirs.ics\ntitle: Jane's sync\nstart: 2026-10-10T09:00:00Z\norganizer: Jane <jane@example.test>\nrepeats: false\nresponse: accepted"),
                    vec![("organizer", json!("Jane <jane@example.test>")), ("repeats", json!(false))],
                ),
            ];
            for (tool, args, summary, stored) in cases {
                let result = h.call(0, tool, args).await;
                match pending(&result) {
                    Some(value) => {
                        if value["summary"] != summary.as_str() {
                            wrong.push(format!("{tool}'s summary is {:?}", value["summary"]));
                        }
                    }
                    None => wrong.push(format!("{tool} did not wait: {}", told_by(&result))),
                }
                let rows = h.rows().await;
                match rows.iter().find(|r| r.tool_name == tool) {
                    Some(row) => {
                        if row.arguments["account"] != json!(id) {
                            wrong.push(format!(
                                "{tool}'s stored account is {}",
                                row.arguments["account"]
                            ));
                        }
                        for (name, value) in stored {
                            if row.arguments[name] != value {
                                wrong.push(format!(
                                    "{tool}'s stored {name} is {}, not the event's {value}",
                                    row.arguments[name]
                                ));
                            }
                        }
                    }
                    None => wrong.push(format!("{tool} wrote no row")),
                }
            }
            let methods: Vec<String> = standin
                .requests()
                .iter()
                .map(|r| r.method.clone())
                .collect();
            if methods.iter().any(|m| m != "GET") {
                wrong.push(format!("a pending call changed the calendar: {methods:?}"));
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_call_its_admission_refuses_writes_no_row_and_asks_no_one() {
            let standin = CalDavStandIn::start(write_config()).await;
            let work = Account::new("Work account", true);
            let home = Account::new("Home account", true);
            let id = work.id.0.to_string();
            let h = gated(
                vec![work.clone(), home.clone()],
                &standin,
                &[
                    Some(json!({"caldav": [id.clone()]})),
                    Some(json!({"caldav": [home.id.0.to_string()]})),
                ],
            )
            .await;
            let mut events = h.event_bus.subscribe();
            let mut wrong = Vec::new();
            for (run, tool, args, expected) in [
                (
                    0,
                    "calendar.update",
                    json!({"account": id, "calendar_id": WORK, "event_id": "weekly-mine.ics", "title": "x"}),
                    REPEATS.to_string(),
                ),
                (
                    0,
                    "calendar.delete",
                    json!({"account": id, "calendar_id": WORK, "event_id": "theirs.ics"}),
                    NOT_ORGANISER.to_string(),
                ),
                (
                    0,
                    "calendar.respond",
                    json!({"account": id, "calendar_id": WORK, "event_id": "solo.ics", "response": "accepted"}),
                    NOT_ATTENDEE.to_string(),
                ),
                (
                    0,
                    "calendar.create",
                    json!({"account": id, "calendar_id": WORK, "title": "a\nb", "start": "2026-10-09T10:00:00Z", "end": "2026-10-09T11:00:00Z"}),
                    BAD_TITLE.to_string(),
                ),
                (
                    0,
                    "calendar.create",
                    json!({"account": id, "calendar_id": "https://elsewhere.example.test/x/", "title": "a", "start": "2026-10-09T10:00:00Z", "end": "2026-10-09T11:00:00Z"}),
                    "'https://elsewhere.example.test/x/' is not on this calendar account's server"
                        .to_string(),
                ),
                (
                    1,
                    "calendar.delete",
                    json!({"account": id, "calendar_id": WORK, "event_id": "mine.ics"}),
                    CHOSEN_DIFFERENT.to_string(),
                ),
            ] {
                let result = h.call(run, tool, args).await;
                if told_by(&result) != expected {
                    wrong.push(format!(
                        "{tool} {expected:?} was not refused before the gate: {}",
                        told_by(&result)
                    ));
                }
            }
            if !h.rows().await.is_empty() {
                wrong.push(format!(
                    "{} approval rows were written",
                    h.rows().await.len()
                ));
            }
            while let Ok(event) = events.try_recv() {
                if format!("{event:?}").contains("ApprovalRequested") {
                    wrong.push("an approval was requested".to_string());
                }
            }
            if !writes(&standin.requests()).is_empty() {
                wrong.push(format!(
                    "the calendar changed: {:?}",
                    writes(&standin.requests())
                ));
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn an_approved_update_reads_the_event_again_and_writes_with_the_etag_it_then_reads() {
            let standin = CalDavStandIn::start(write_config()).await;
            let work = Account::new("Work account", false);
            let id = work.id.0.to_string();
            let h = gated(
                vec![work.clone()],
                &standin,
                &[Some(json!({"caldav": [id.clone()]}))],
            )
            .await;
            let result = h
                .call(
                    0,
                    "calendar.update",
                    json!({"account": id, "calendar_id": WORK, "event_id": "mine.ics", "title": "Board review (moved)"}),
                )
                .await;
            let approval_id = pending(&result)
                .and_then(|v| v["approval_id"].as_str().map(str::to_string))
                .unwrap_or_else(|| panic!("calendar.update did not wait: {}", told_by(&result)));
            // Someone else moves the room after the person was asked.
            standin.set_event(
                WORK,
                event(
                    "mine.ics",
                    "\"etag-mine-2\"",
                    &MINE.replace("LOCATION:Room 4", "LOCATION:Room 9"),
                ),
            );
            let decided = h
                .approvals
                .decide(
                    ToolApprovalId::from_string(&approval_id).unwrap(),
                    &TenantId::default(),
                    PERSON,
                    ToolApprovalDecision::Once,
                    h.service.as_ref(),
                )
                .await
                .unwrap();
            let mut wrong = Vec::new();
            if decided.status != ToolApprovalStatus::ApprovedOnce {
                wrong.push(format!(
                    "the request reads {:?} {:?}",
                    decided.status, decided.error
                ));
            }
            let requests = standin.requests();
            let reads = requests
                .iter()
                .filter(|r| r.method == "GET" && r.path == path("mine.ics"))
                .count();
            if reads != 2 {
                wrong.push(format!(
                    "the event was read {reads} times, not at the admission and again at the run"
                ));
            }
            match requests.iter().find(|r| r.method == "PUT") {
                Some(put) => {
                    if put.header("if-match") != Some("\"etag-mine-2\"") {
                        wrong.push(format!(
                            "the run wrote with If-Match {:?}, not the etag it read",
                            put.header("if-match")
                        ));
                    }
                    let body = lines(&put.body);
                    if !body.contains(&"LOCATION:Room 9".to_string())
                        || !body.contains(&"SUMMARY:Board review (moved)".to_string())
                    {
                        wrong.push(format!("the run wrote {body:?}"));
                    }
                }
                None => wrong.push(format!(
                    "the approved run wrote nothing: {:?}",
                    decided.error
                )),
            }
            assert!(wrong.is_empty(), "{wrong:#?}");
        }
    }
}

// ---------------------------------------------------------------------------
// A calendar account connected by password (AEGIS ADR-138 K10, K10a)
// ---------------------------------------------------------------------------

mod password_form {
    //! `create_caldav_calendar` against the stand-in: the address rule
    //! before any request, the check by a `PROPFIND` with HTTP Basic, the
    //! binding stored only after it, the password in no answer, reply or
    //! event, and the seven tools serving the stored account with Basic.

    use super::*;
    use aegis_orchestrator_core::application::credential_service::{
        CreateCalDavCalendarCommand, ToolCalendarSource,
    };
    use aegis_orchestrator_core::application::tools::builtin_calendar::{
        CalendarActing, CalendarTools,
    };
    use aegis_orchestrator_core::domain::agent::AgentId;
    use aegis_orchestrator_core::domain::execution::ServerChoice;
    use aegis_orchestrator_core::infrastructure::calendar::{admit, shown_reply, TransportFailure};
    use aegis_orchestrator_core::infrastructure::event_bus::EventReceiver;
    use aegis_orchestrator_core::infrastructure::mail::MailResolver;
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::Mutex;

    const URL: &str = "https://caldav.example.test/dav/";
    const ROOT: &str = "/dav/";
    const PRINCIPAL: &str = "/dav/a@example.test/user";
    const USERNAME: &str = "alice";
    const PASSWORD: &str = "Mk7-caldav-password-form-secret";
    const MAILTO: &str = "mailto:a@example.test";

    /// The `Authorization` a request carries with `username` and `password`.
    fn basic(username: &str, password: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
        )
    }

    /// The client's stand-in, its root naming the principal, accepting the
    /// form's user name and password by Basic.
    async fn password_standin(address: bool) -> CalDavStandIn {
        let standin = CalDavStandIn::start(client_config()).await;
        standin.accept_basic(USERNAME, PASSWORD);
        standin.serve_root(ROOT);
        if address {
            standin.serve_address(MAILTO);
        }
        standin
    }

    /// Answers each listed host its addresses, any other host a public one,
    /// and records every lookup.
    struct Names {
        answers: Vec<(String, Vec<SocketAddr>)>,
        lookups: Mutex<Vec<String>>,
    }

    impl Names {
        fn new(answers: &[(&str, &[&str])]) -> Arc<Self> {
            Arc::new(Self {
                answers: answers
                    .iter()
                    .map(|(h, a)| {
                        (
                            h.to_string(),
                            a.iter()
                                .map(|ip| SocketAddr::new(ip.parse().unwrap(), 443))
                                .collect(),
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
    impl MailResolver for Names {
        async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            self.lookups.lock().unwrap().push(host.to_string());
            Ok(self
                .answers
                .iter()
                .find(|(h, _)| h == host)
                .map(|(_, a)| a.clone())
                .unwrap_or_else(|| vec![SocketAddr::new("192.0.2.44".parse().unwrap(), port)]))
        }
    }

    struct Service {
        service: Arc<StandardCredentialManagementService>,
        repo: Arc<InMemoryRepo>,
        secrets: Arc<SecretsManager>,
        events: EventReceiver,
    }

    /// The credential service with its calendar check over `transport`.
    fn service(transport: ReqwestTransport) -> Service {
        let repo = Arc::new(InMemoryRepo::default());
        let event_bus = Arc::new(EventBus::new(256));
        let events = event_bus.subscribe();
        let secrets = Arc::new(SecretsManager::from_store(
            Arc::new(TestSecretStore::new()),
            event_bus.clone(),
        ));
        let service = StandardCredentialManagementService::new(
            repo.clone(),
            secrets.clone(),
            event_bus,
            Arc::new(OAuthProviderRegistry::new()),
        )
        .with_calendar_probe(Arc::new(CalDavProbe::new(Arc::new(transport))));
        Service {
            service: Arc::new(service),
            repo,
            secrets,
            events,
        }
    }

    fn command(url: &str, password: &str, label: Option<&str>) -> CreateCalDavCalendarCommand {
        CreateCalDavCalendarCommand {
            owner_user_id: USER.to_string(),
            tenant_id: tenant(),
            label: label.map(str::to_string),
            scope: CredentialScope::Personal,
            url: url.to_string(),
            username: USERNAME.to_string(),
            password: SensitiveString::new(password),
        }
    }

    /// Every event published so far, as text.
    fn events_text(events: &mut EventReceiver) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(format!("{event:?}"));
        }
        seen
    }

    #[tokio::test]
    async fn a_calendar_connected_by_password_is_stored_after_a_basic_propfind_names_its_home_set()
    {
        let standin = password_standin(true).await;
        let mut s = service(ReqwestTransport::to_origin(standin.origin()));
        let binding = s
            .service
            .create_caldav_calendar(command(URL, PASSWORD, None))
            .await
            .expect("the account the stand-in accepts is stored");

        let mut wrong = Vec::new();
        if binding.credential_type != CredentialType::Calendar {
            wrong.push(format!("type {:?}", binding.credential_type));
        }
        if binding.provider != CredentialProvider::new("caldav") {
            wrong.push(format!("provider {}", binding.provider));
        }
        if binding.status != CredentialStatus::Active {
            wrong.push(format!("status {:?}", binding.status));
        }
        let expected = CalendarSettings {
            server: URL.to_string(),
            principal: PRINCIPAL.to_string(),
            address: "a@example.test".to_string(),
        };
        if binding.metadata.calendar.as_ref() != Some(&expected) {
            wrong.push(format!(
                "the calendar settings are {:?}, not {expected:?}",
                binding.metadata.calendar
            ));
        }
        if binding.metadata.label != "a@example.test" {
            wrong.push(format!("label {}", binding.metadata.label));
        }
        if !is_calendar_binding(&binding) {
            wrong.push("the binding is not a calendar account".to_string());
        }
        let stored = s.repo.find_by_id(&binding.id).await.unwrap();
        if stored.as_ref().map(|b| (b.id, b.metadata.calendar.clone()))
            != Some((binding.id, binding.metadata.calendar.clone()))
        {
            wrong.push("the binding was not saved as answered".to_string());
        }
        let secret = s
            .secrets
            .read_secret(
                &binding.secret_path.effective_mount(),
                &binding.secret_path.path,
                &AccessContext::system("test"),
            )
            .await
            .expect("the secret is written");
        if secret.get("username").map(|v| v.expose()) != Some(USERNAME)
            || secret.get("password").map(|v| v.expose()) != Some(PASSWORD)
        {
            wrong.push("OpenBao does not hold the user name and password".to_string());
        }

        let requests = standin.requests();
        let propfinds: Vec<(&str, Option<&str>, bool)> = requests
            .iter()
            .map(|r| {
                (
                    r.path.as_str(),
                    r.header("authorization"),
                    r.body.contains("calendar-user-address-set"),
                )
            })
            .collect();
        let auth = basic(USERNAME, PASSWORD);
        let want = vec![
            (ROOT, Some(auth.as_str()), true),
            (PRINCIPAL, Some(auth.as_str()), true),
        ];
        if propfinds != want {
            wrong.push(format!(
                "the check sent {propfinds:?}, not a Basic PROPFIND on the root then the principal"
            ));
        }
        let answered = serde_json::to_string(&binding).unwrap();
        let encoded = basic(USERNAME, PASSWORD);
        for (name, text) in [("the answer", answered)].into_iter().chain(
            events_text(&mut s.events)
                .into_iter()
                .map(|e| ("an event", e)),
        ) {
            if text.contains(PASSWORD) || text.contains(&encoded[6..]) {
                wrong.push(format!("{name} carries the password: {text}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test]
    async fn without_a_calendar_user_address_the_user_name_is_the_address_and_a_label_is_kept() {
        let standin = password_standin(false).await;
        let s = service(ReqwestTransport::to_origin(standin.origin()));
        let binding = s
            .service
            .create_caldav_calendar(command(URL, PASSWORD, Some("Work calendar")))
            .await
            .expect("stored");
        let settings = binding
            .metadata
            .calendar
            .clone()
            .expect("calendar settings");
        assert_eq!(
            (settings.address.as_str(), binding.metadata.label.as_str()),
            (USERNAME, "Work calendar"),
            "the address is not the user name, or the label given was not kept"
        );
    }

    #[tokio::test]
    async fn a_refused_check_answers_calendar_unreachable_stores_nothing_and_repeats_no_password() {
        let mut wrong = Vec::new();
        for case in ["a wrong password", "no calendar home set"] {
            let standin = if case == "no calendar home set" {
                let mut config = client_config();
                config.home_set = None;
                let standin = CalDavStandIn::start(config).await;
                standin.accept_basic(USERNAME, PASSWORD);
                standin.serve_root(ROOT);
                standin
            } else {
                password_standin(true).await
            };
            let password = if case == "a wrong password" {
                "Mk7-wrong-password"
            } else {
                PASSWORD
            };
            let mut s = service(ReqwestTransport::to_origin(standin.origin()));
            let outcome = s
                .service
                .create_caldav_calendar(command(URL, password, None))
                .await;
            match outcome
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<CredentialError>())
            {
                Some(CredentialError::CalendarUnreachable { status, reply }) => {
                    let encoded = basic(USERNAME, password);
                    if reply.contains(password) || reply.contains(&encoded[6..]) {
                        wrong.push(format!("{case}: the reply carries the password: {reply}"));
                    }
                    if case == "a wrong password" && *status != Some(401) {
                        wrong.push(format!("{case}: status {status:?}"));
                    }
                }
                _ => wrong.push(format!(
                    "{case}: answered {:?}, not calendar_unreachable",
                    outcome.as_ref().map(|b| b.id)
                )),
            }
            if !s.repo.bindings.read().await.is_empty() {
                wrong.push(format!("{case}: a binding was stored"));
            }
            for event in events_text(&mut s.events) {
                if event.contains("CredentialCreated") {
                    wrong.push(format!("{case}: a binding was announced: {event}"));
                }
            }
            if standin.requests().is_empty() {
                wrong.push(format!("{case}: the server was never asked"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test]
    async fn a_server_outside_the_address_rule_is_refused_before_any_request_and_nothing_is_stored()
    {
        let standin = password_standin(true).await;
        let names = Names::new(&[("intranet.example.test", &["203.0.113.9", "10.0.0.7"])]);
        let s = service(ReqwestTransport::to_origin_with_resolver(
            standin.origin(),
            names.clone(),
        ));
        let mut wrong = Vec::new();
        for (url, says) in [
            ("http://caldav.example.test/dav/", "https"),
            ("https://caldav.example.test:8443/dav/", "8443"),
            ("https://127.0.0.1/dav/", "loopback"),
            ("https://10.1.2.3/dav/", "private"),
            ("https://[::ffff:127.0.0.1]/dav/", "loopback"),
            ("https://intranet.example.test/dav/", "10.0.0.7"),
        ] {
            let outcome = s
                .service
                .create_caldav_calendar(command(url, PASSWORD, None))
                .await;
            match outcome
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<CredentialError>())
            {
                Some(CredentialError::CalendarHostNotAllowed { field, reason }) => {
                    if field != "url" || !reason.contains(says) {
                        wrong.push(format!("{url}: field {field}, reason {reason}"));
                    }
                }
                other => wrong.push(format!(
                    "{url}: answered {other:?}, not calendar_host_not_allowed"
                )),
            }
        }
        if !standin.requests().is_empty() {
            wrong.push(format!(
                "the stand-in was reached: {:?}",
                standin
                    .requests()
                    .iter()
                    .map(|r| r.path.clone())
                    .collect::<Vec<_>>()
            ));
        }
        if !s.repo.bindings.read().await.is_empty() {
            wrong.push("a binding was stored".to_string());
        }
        if names.lookups() != vec!["intranet.example.test".to_string()] {
            wrong.push(format!(
                "the names looked up were {:?}, not the one host once",
                names.lookups()
            ));
        }
        // The control: a public server on 443 does reach the stand-in.
        if let Err(e) = s
            .service
            .create_caldav_calendar(command(URL, PASSWORD, None))
            .await
        {
            wrong.push(format!("the control, a public server, was refused: {e}"));
        }
        if standin.requests().is_empty() {
            wrong.push("the control never reached the stand-in".to_string());
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test]
    async fn the_address_rule_resolves_once_admits_only_public_addresses_on_443_and_keeps_them() {
        let names = Names::new(&[
            ("public.example.test", &["203.0.113.9", "2001:db8::9"]),
            ("mixed.example.test", &["203.0.113.9", "169.254.169.254"]),
        ]);
        let mut wrong = Vec::new();
        let url = |u: &str| url::Url::parse(u).unwrap();
        match admit(&url("https://public.example.test/dav/"), names.as_ref()).await {
            Ok(admitted) => {
                let want: Vec<SocketAddr> = vec![
                    "203.0.113.9:443".parse().unwrap(),
                    "[2001:db8::9]:443".parse().unwrap(),
                ];
                if admitted.addrs != want || admitted.host != "public.example.test" {
                    wrong.push(format!("admitted {admitted:?}"));
                }
            }
            Err(e) => wrong.push(format!("a public server was refused: {e}")),
        }
        for (u, says) in [
            ("https://mixed.example.test/", "169.254.169.254"),
            ("https://public.example.test:8443/", "8443"),
            ("https://192.168.1.4/", "private"),
            ("https://[fe80::1]/", "link-local"),
        ] {
            match admit(&url(u), names.as_ref()).await {
                Err(TransportFailure::NotAllowed(reason)) if reason.contains(says) => {}
                other => wrong.push(format!("{u}: {other:?}")),
            }
        }
        if names.lookups()
            != vec![
                "public.example.test".to_string(),
                "mixed.example.test".to_string(),
            ]
        {
            wrong.push(format!(
                "lookups {:?}: a literal or a refused port was resolved, or a name twice",
                names.lookups()
            ));
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn a_reply_shown_from_a_basic_account_redacts_the_password_and_its_encoding() {
        let auth = CalDavAuth::Basic {
            username: USERNAME.to_string(),
            password: SensitiveString::new(PASSWORD),
        };
        let encoded = basic(USERNAME, PASSWORD);
        let reply = format!("401: you sent {encoded} for {USERNAME} with {PASSWORD}");
        let shown = shown_reply(&reply, &auth);
        assert!(
            !shown.contains(PASSWORD) && !shown.contains(&encoded[6..]),
            "the reply repeats the password: {shown}"
        );
        assert_eq!(format!("{auth:?}"), "Basic([REDACTED])");
    }

    #[tokio::test]
    async fn the_calendar_tools_serve_an_account_connected_by_password_with_basic() {
        let standin = password_standin(true).await;
        let s = service(ReqwestTransport::to_origin(standin.origin()));
        let binding = s
            .service
            .create_caldav_calendar(command(URL, PASSWORD, None))
            .await
            .expect("stored");
        let tools = CalendarTools::with_transport(
            s.service.clone() as Arc<dyn ToolCalendarSource>,
            Arc::new(ReqwestTransport::to_origin(standin.origin())),
        );
        let acting = CalendarActing {
            tenant_id: tenant(),
            user_id: Some(USER.to_string()),
            agent_id: AgentId::new(),
            workflow_id: None,
            choice: ServerChoice::NotGiven,
            has_execution_record: false,
        };
        let account = binding.id.to_string();
        let mut wrong = Vec::new();
        let pool = s.service.calendar_contexts(&tenant(), USER).await.unwrap();
        if !pool.iter().any(|c| c.id == binding.id) {
            wrong.push("the caldav pool does not hold the account".to_string());
        }
        let before = standin.requests().len();
        let calendars = tools
            .invoke(
                "calendar.calendars",
                &json!({ "account": account }),
                &acting,
            )
            .await;
        match &calendars {
            Ok(answer) if answer["calendars"].as_array().is_some_and(|c| c.len() == 2) => {}
            other => wrong.push(format!("calendar.calendars answered {other:?}")),
        }
        let listed = tools
            .invoke(
                "calendar.list",
                &json!({
                    "account": account,
                    "calendar_id": WORK,
                    "start": "2026-10-08T09:00:00Z",
                    "end": "2026-10-15T09:00:00Z",
                }),
                &acting,
            )
            .await;
        match &listed {
            Ok(answer) if answer["events"].as_array().is_some_and(|e| !e.is_empty()) => {}
            other => wrong.push(format!("calendar.list answered {other:?}")),
        }
        let created = tools
            .invoke(
                "calendar.create",
                &json!({
                    "account": account,
                    "calendar_id": WORK,
                    "title": "Written by password",
                    "start": "2026-10-20T09:00:00Z",
                    "end": "2026-10-20T10:00:00Z",
                }),
                &acting,
            )
            .await;
        match &created {
            Ok(answer)
                if answer["uid"]
                    .as_str()
                    .is_some_and(|u| u.ends_with("@example.test")) => {}
            other => wrong.push(format!("calendar.create answered {other:?}")),
        }
        let auth = basic(USERNAME, PASSWORD);
        let sent: Vec<(String, Option<String>)> = standin.requests()[before..]
            .iter()
            .map(|r| {
                (
                    format!("{} {}", r.method, r.path),
                    r.header("authorization").map(str::to_string),
                )
            })
            .collect();
        if sent.is_empty()
            || sent
                .iter()
                .any(|(_, a)| a.as_deref() != Some(auth.as_str()))
        {
            wrong.push(format!("the tools' requests were not all Basic: {sent:?}"));
        }
        if !sent.iter().any(|(r, _)| r.starts_with("PUT ")) {
            wrong.push(format!("no event was written: {sent:?}"));
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
