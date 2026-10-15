// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A refusal for a missing connection or profile says what is missing and
//! where it is resolved, as data beside its sentence (Zaru ADR-0070 D1, D4,
//! D5; AEGIS ADR-035's Update `error-needs-2026-10-15`): calls through the
//! invoke route's service on a session attested as the Zaru MCP server
//! attests one (the key's user, the tier context, no execution record).
//!
//! Trigger 1:
//! - (a) `calendar.list` with no calendar: 403 `CREDENTIAL_BINDING_REQUIRED`
//!   with `needs` `{"family":"calendar","page":"/vault/connections"}`;
//! - (b) a `github.` call with no binding carries
//!   `{"family":"github","page":"/vault/connections"}`;
//! - (c) a tool the conversation's profile does not admit: 403
//!   `TOOL_NOT_IN_PROFILE`, the sentence alone, and
//!   `{"family":"profile","page":"/vault/profiles/<id>"}`;
//! - (d) the run-with-no-person refusal carries no `needs`: in
//!   `gateway_wire_tests.rs`, whose stub gateway reaches that refusal;
//! - (e) each new sentence is answered as written and names no record (the
//!   pattern of `facing_text_no_record_refs.rs`, which scans the sources).
//!
//! Beside them, D1 (d)'s deleted profile: its sentence and code kept, `needs`
//! naming the Profiles page. The route's body is `seal.rs`'s
//! `refusal_needs_tests` in the CLI crate.

#[path = "support/mcp_connections_orchestrator_fixture.rs"]
mod fixture;
#[path = "support/mail_standins.rs"]
#[allow(dead_code)]
mod mail_standins;

use std::sync::{Arc, Mutex};

use aegis_orchestrator_core::application::profile_service::{
    ProfileAdmission, ProfileError, RunProfiles,
};
use aegis_orchestrator_core::domain::profile::{ToolAllowList, ToolPattern};
use aegis_orchestrator_core::domain::seal_session::{
    EnvelopeVerifier, RefusalNeeds, SealRefusal, SealSessionError,
};
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use async_trait::async_trait;
use fixture::*;
use serde_json::{json, Map, Value};

/// A call as `POST /v1/seal/invoke` receives it, with its own token.
struct Call {
    token: SensitiveString,
    tool: String,
    arguments: Value,
}

impl EnvelopeVerifier for Call {
    fn security_token(&self) -> &SensitiveString {
        &self.token
    }
    fn verify_signature(&self, _: &[u8]) -> Result<(), SealSessionError> {
        Ok(())
    }
    fn extract_tool_name(&self) -> Option<String> {
        Some(self.tool.clone())
    }
    fn extract_arguments(&self) -> Option<Value> {
        Some(self.arguments.clone())
    }
    fn replay_nonce(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

/// The conversation's sentence for a tool its profile does not admit.
const NOT_IN_CONVERSATION_PROFILE: &str =
    "This tool is not in the profile chosen for this conversation.";

fn refusal_of(result: &Result<Value, SealSessionError>) -> SealRefusal {
    match result {
        Ok(value) => panic!("the call was answered, not refused: {value}"),
        Err(e) => e.refusal(),
    }
}

fn needs(family: &str, page: &str) -> Option<RefusalNeeds> {
    Some(RefusalNeeds {
        family: family.to_string(),
        page: page.to_string(),
    })
}

/// The person's one profile over their one mailbox, admitting only
/// `mail.list`; deleted when a test says so.
struct OneProfile {
    id: uuid::Uuid,
    mailbox: String,
    deleted: Mutex<bool>,
}

impl OneProfile {
    fn live(&self, person: Option<&str>, id: uuid::Uuid) -> Result<(), ProfileError> {
        if id == self.id && person == Some(PERSON) && !*self.deleted.lock().unwrap() {
            Ok(())
        } else {
            Err(ProfileError::NotFound)
        }
    }
}

#[async_trait]
impl RunProfiles for OneProfile {
    async fn contexts_of(
        &self,
        _: &TenantId,
        person: Option<&str>,
        id: uuid::Uuid,
    ) -> Result<Map<String, Value>, ProfileError> {
        self.live(person, id)?;
        Ok(json!({ "imap": [self.mailbox] })
            .as_object()
            .unwrap()
            .clone())
    }

    async fn admission_of(
        &self,
        _: &TenantId,
        person: Option<&str>,
        id: uuid::Uuid,
    ) -> Result<ProfileAdmission, ProfileError> {
        self.live(person, id)?;
        Ok(ProfileAdmission {
            tools: ToolAllowList(vec![ToolPattern::parse("mail.list").unwrap()]),
            families: vec!["mail".to_string(), "imap".to_string()],
        })
    }
}

/// The fixture's node reading `profile` for a conversation's calls.
async fn on_profile() -> (Fixture, Arc<OneProfile>) {
    let Fixture {
        service,
        sessions,
        approvals,
        tenant,
        token,
        mailbox,
        _dirs,
    } = fixture(1, Arc::new(StopAtTheServer)).await;
    let profile = Arc::new(OneProfile {
        id: uuid::Uuid::new_v4(),
        mailbox: mailbox.0.to_string(),
        deleted: Mutex::new(false),
    });
    let f = Fixture {
        service: service.with_profiles(profile.clone()),
        sessions,
        approvals,
        tenant,
        token,
        mailbox,
        _dirs,
    };
    (f, profile)
}

/// A `mail.send` of the conversation on `profile`.
async fn send_on(f: &Fixture, profile: uuid::Uuid) -> Result<Value, SealSessionError> {
    f.service
        .invoke_tool_with_meta(
            &Call {
                token: f.token.clone().into(),
                tool: "mail.send".to_string(),
                arguments: json!({
                    "mailbox": f.mailbox.0.to_string(),
                    "to": ["ann@example.test"],
                    "subject": "Figures",
                    "body": "Thanks, received."
                }),
            },
            Some(&json!({ "profile": profile.to_string() })),
        )
        .await
}

// ---------------------------------------------------------------------------
// Trigger 1
// ---------------------------------------------------------------------------

/// (a) D1 (a) to (c): a person with no calendar is refused `calendar.list`
/// 403 `CREDENTIAL_BINDING_REQUIRED` with D3's sentence and `needs` naming
/// the calendar family and the Connections page.
#[tokio::test]
async fn a_calendar_list_with_no_calendar_needs_a_calendar_on_the_connections_page() {
    let f = fixture(1, Arc::new(StopAtTheServer)).await;
    let refusal = refusal_of(
        &f.call("calendar.list", json!({ "calendar_id": "personal" }))
            .await,
    );
    assert_eq!(
        (
            refusal.http_status,
            refusal.code,
            refusal.status,
            refusal.message.as_str(),
            refusal.needs
        ),
        (
            403,
            "CREDENTIAL_BINDING_REQUIRED",
            "policy_violation",
            not_connected("a calendar").as_str(),
            needs("calendar", "/vault/connections")
        )
    );
}

/// (b) D1 (b): a remote server's tool with no binding of the person's
/// carries the server's own name as the family.
#[tokio::test]
async fn b_a_github_call_with_no_binding_needs_github_on_the_connections_page() {
    let f = fixture(1, Arc::new(StopAtTheServer)).await;
    let refusal = refusal_of(&f.call("github.list_issues", json!({})).await);
    assert_eq!(
        (refusal.code, refusal.message.as_str(), refusal.needs),
        (
            "CREDENTIAL_BINDING_REQUIRED",
            not_connected("GitHub").as_str(),
            needs("github", "/vault/connections")
        )
    );
}

/// (b) D1 (b): every connection family maps from its binding key, `imap`
/// to `mailbox`, `caldav` to `calendar`, a server to its own name.
#[tokio::test]
async fn b_every_family_is_named_from_its_binding_key() {
    let f = fixture_over(
        Arc::new(NoMailboxes),
        aegis_orchestrator_core::domain::credential::CredentialBindingId::new(),
        Arc::new(StopAtTheServer),
    )
    .await;
    let calls = [
        ("mailbox", "mail.list", json!({})),
        ("calendar", "calendar.list", json!({ "calendar_id": "x" })),
        ("github", "github.list_issues", json!({})),
        ("nuclear-notes", "nuclear-notes.pages.read", json!({})),
        ("notes-own", "notes-own.search", json!({})),
    ];
    let mut wrong = Vec::new();
    for (family, tool, arguments) in calls {
        let refusal = refusal_of(&f.call(tool, arguments).await);
        if refusal.needs != needs(family, "/vault/connections") {
            wrong.push(format!("{tool}: {:?}", refusal.needs));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// (c) D1 (d): a tool the conversation's profile does not admit is answered
/// 403 `policy_violation` `TOOL_NOT_IN_PROFILE` with the sentence alone and
/// `needs` naming that profile's page.
#[tokio::test]
async fn c_a_tool_not_in_the_conversations_profile_needs_that_profile() {
    let (f, profile) = on_profile().await;
    let refusal = refusal_of(&send_on(&f, profile.id).await);
    assert_eq!(
        (
            refusal.http_status,
            refusal.code,
            refusal.status,
            refusal.message.as_str(),
            refusal.needs
        ),
        (
            403,
            "TOOL_NOT_IN_PROFILE",
            "policy_violation",
            NOT_IN_CONVERSATION_PROFILE,
            needs("profile", &format!("/vault/profiles/{}", profile.id))
        )
    );
}

/// D1 (d): a profile that no longer exists keeps its sentence and its code
/// and gains `needs` naming the Profiles page.
#[tokio::test]
async fn a_deleted_profile_keeps_its_answer_and_needs_the_profiles_page() {
    let (f, profile) = on_profile().await;
    *profile.deleted.lock().unwrap() = true;
    let refusal = refusal_of(&send_on(&f, profile.id).await);
    assert_eq!(
        (
            refusal.http_status,
            refusal.code,
            refusal.message.as_str(),
            refusal.needs
        ),
        (
            422,
            "INVALID_ARGUMENTS",
            "Invalid tool arguments: The profile chosen for this run no longer exists.",
            needs("profile", "/vault/profiles")
        )
    );
}

/// (e) ADR-0056, AEGIS ADR-134, ADR-140 D16: the new sentence is answered as
/// written and, with the deleted profile's, names no record.
#[tokio::test]
async fn e_the_new_sentences_are_answered_as_written_and_name_no_record() {
    let (f, profile) = on_profile().await;
    let narrowed = refusal_of(&send_on(&f, profile.id).await).message;
    *profile.deleted.lock().unwrap() = true;
    let gone = refusal_of(&send_on(&f, profile.id).await).message;
    let mut wrong = Vec::new();
    if narrowed != NOT_IN_CONVERSATION_PROFILE {
        wrong.push(format!("narrowed away: {narrowed}"));
    }
    for said in [&narrowed, &gone] {
        if record_pattern().is_match(said) {
            wrong.push(format!("names a record: {said}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
