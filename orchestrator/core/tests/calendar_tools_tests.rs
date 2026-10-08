// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Calendar accounts over CalDAV, AEGIS ADR-138 K2, K4 and K5.
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
