// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Deny by default on a call that acts as its person (Zaru ADR-0071 D1 (c),
//! D2, D3): the orchestrator, not a header, tells the person's own chat turn
//! from a key, by the credential that attested the session. Calls through
//! the invoke route's service on sessions with no execution record, for a
//! person whose one mailbox, one GitHub binding and one Carbon Forge
//! binding are all connected.
//!
//! Trigger 1:
//! - (c), the call half: a session attested by an API key, with no profile
//!   and no choice, is refused `mail.`, `github.` and `nuclear-notes.` calls
//!   with D1 (e)'s key sentence and `needs`
//!   `{"family":"profile","page":"/vault/profiles"}`; the key's own profile
//!   naming the mailbox, or bindings the call chose, reach it;
//! - (d) a session attested by an interactive token whose turn chose nothing
//!   for `imap` is refused `mail.list` with ADR-0070's "connected but not
//!   chosen" sentence and the mailbox family's `needs`; the turn that chose
//!   the mailbox reaches it.
//!
//! Beside them: a family the person has not connected keeps its own refusal
//! (ADR-0069 D3), and the new sentence names no record.

#[path = "support/mcp_connections_orchestrator_fixture.rs"]
mod fixture;
#[path = "support/mail_standins.rs"]
#[allow(dead_code)]
mod mail_standins;

use std::sync::Arc;

use aegis_orchestrator_core::application::credential_service::{
    not_chosen_sentence, ContextBinding, ToolCallActor, ToolCredentialSource,
};
use aegis_orchestrator_core::application::profile_service::{
    ProfileAdmission, ProfileError, RunProfiles,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::credential::CredentialBindingId;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::profile::{ToolAllowList, ToolPattern};
use aegis_orchestrator_core::domain::seal_session::{
    AttestingCredential, EnvelopeVerifier, RefusalNeeds, SealSession, SealSessionError,
};
use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use async_trait::async_trait;
use fixture::*;
use serde_json::{json, Map, Value};

/// D1 (e)'s key sentence.
const KEY_SENTENCE: &str = "This key reaches none of your connections. Choose a profile for it on the API keys page in Zaru Web, or name one in the request.";

/// ADR-0070 D2's sentence for a mailbox connected but not chosen.
const MAILBOX_NOT_CHOSEN: &str = "Your mailbox is connected but not chosen for this conversation.";

/// What the remote servers' credential source answers: the call passed
/// every check of the orchestrator's own.
const SERVER_TOKEN: &str = "Mk21-the-persons-own-server-token";

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

/// The person's one GitHub binding and one Carbon Forge binding, each
/// answering its token.
struct OneBindingPerServer {
    github: CredentialBindingId,
    notes: CredentialBindingId,
}

#[async_trait]
impl ToolCredentialSource for OneBindingPerServer {
    async fn tool_server_credential(
        &self,
        _: &ToolCallActor<'_>,
        _: &str,
    ) -> anyhow::Result<Option<SensitiveString>> {
        Ok(Some(SensitiveString::new(SERVER_TOKEN)))
    }

    async fn context_bindings(
        &self,
        _: &TenantId,
        user_id: &str,
        server: &str,
    ) -> anyhow::Result<Vec<ContextBinding>> {
        let id = match (user_id, server) {
            (PERSON, "github") => self.github,
            (PERSON, "nuclear-notes") => self.notes,
            _ => return Ok(Vec::new()),
        };
        Ok(vec![ContextBinding {
            id,
            name: format!("{server} account"),
            reach: None,
        }])
    }

    async fn own_server_credential(
        &self,
        _: &TenantId,
        user_id: &str,
        server: &str,
    ) -> anyhow::Result<Option<SensitiveString>> {
        Ok(
            (user_id == PERSON && matches!(server, "github" | "nuclear-notes"))
                .then(|| SensitiveString::new(SERVER_TOKEN)),
        )
    }
}

/// The person's one profile, naming their mailbox and admitting every mail
/// tool.
struct MailboxProfile {
    id: uuid::Uuid,
    mailbox: String,
}

#[async_trait]
impl RunProfiles for MailboxProfile {
    async fn contexts_of(
        &self,
        _: &TenantId,
        person: Option<&str>,
        id: uuid::Uuid,
    ) -> Result<Map<String, Value>, ProfileError> {
        if id != self.id || person != Some(PERSON) {
            return Err(ProfileError::NotFound);
        }
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
        if id != self.id || person != Some(PERSON) {
            return Err(ProfileError::NotFound);
        }
        Ok(ProfileAdmission {
            tools: ToolAllowList(vec![ToolPattern::parse("mail.*").unwrap()]),
            families: vec!["mail".to_string(), "imap".to_string()],
        })
    }
}

/// The node of the fixture with the person's server bindings and profile,
/// and a fresh session attested by `credential`, whose token it answers.
async fn node(credential: impl FnOnce(uuid::Uuid) -> AttestingCredential) -> (Fixture, String) {
    let Fixture {
        service,
        sessions,
        approvals,
        tenant,
        token: _,
        mailbox,
        _dirs,
    } = fixture(1, Arc::new(StopAtTheServer)).await;
    let profile = uuid::Uuid::new_v4();
    let service = service
        .with_tool_credentials(Arc::new(OneBindingPerServer {
            github: CredentialBindingId::new(),
            notes: CredentialBindingId::new(),
        }))
        .with_profiles(Arc::new(MailboxProfile {
            id: profile,
            mailbox: mailbox.0.to_string(),
        }));
    let token = format!("token-{}", uuid::Uuid::new_v4());
    sessions
        .save(
            SealSession::new(
                AgentId::new(),
                ExecutionId::new(),
                vec![],
                token.clone(),
                tier_context(),
                tenant.clone(),
            )
            .with_principal_metadata(
                Some(PERSON.to_string()),
                Some(PERSON.to_string()),
                None,
                Some("free".to_string()),
            )
            .with_attesting_credential(credential(profile)),
        )
        .await
        .unwrap();
    (
        Fixture {
            service,
            sessions,
            approvals,
            tenant,
            token: token.clone(),
            mailbox,
            _dirs,
        },
        token,
    )
}

async fn call(
    f: &Fixture,
    token: &str,
    tool: &str,
    meta: Option<Value>,
) -> Result<Value, SealSessionError> {
    f.service
        .invoke_tool_with_meta(
            &Call {
                token: token.to_string().into(),
                tool: tool.to_string(),
                arguments: json!({}),
            },
            meta.as_ref(),
        )
        .await
}

/// The refusal's code, sentence and `needs`, or what answered.
fn refused(result: &Result<Value, SealSessionError>) -> (String, String, Option<RefusalNeeds>) {
    match result {
        Ok(value) => ("answered".to_string(), value.to_string(), None),
        Err(e) => {
            let refusal = e.refusal();
            (refusal.code.to_string(), refusal.message, refusal.needs)
        }
    }
}

fn needs(family: &str, page: &str) -> Option<RefusalNeeds> {
    Some(RefusalNeeds {
        family: family.to_string(),
        page: page.to_string(),
    })
}

/// Trigger 1 (c), the call half: a key with no profile, on a call that
/// names none and chose no binding, reaches none of the person's
/// connections, though each is connected.
#[tokio::test]
async fn c_a_key_with_no_profile_and_no_choice_reaches_no_connection() {
    let (f, token) = node(|_| AttestingCredential::ApiKey { profile: None }).await;
    for tool in [
        "mail.list",
        "github.list_issues",
        "nuclear-notes.pages_read",
    ] {
        for meta in [None, Some(json!({ "contexts": {} }))] {
            assert_eq!(
                refused(&call(&f, &token, tool, meta.clone()).await),
                (
                    "CREDENTIAL_BINDING_REQUIRED".to_string(),
                    KEY_SENTENCE.to_string(),
                    needs("profile", "/vault/profiles")
                ),
                "{tool} with _meta {meta:?}"
            );
        }
    }
}

/// D3: the key's own profile stands for the call's when the call names none
/// and chose no binding, and it reaches the mailbox it names; a binding the
/// call chose reaches it on a key with no profile.
#[tokio::test]
async fn a_keys_own_profile_or_the_calls_chosen_binding_reaches_the_mailbox() {
    let (f, token) = node(|profile| AttestingCredential::ApiKey {
        profile: Some(profile),
    })
    .await;
    let answer = told(&call(&f, &token, "mail.list", None).await);
    assert!(answer.contains(REACHED), "the key's profile: {answer}");

    let (f, token) = node(|_| AttestingCredential::ApiKey { profile: None }).await;
    let chosen = json!({ "contexts": { "imap": [f.mailbox.0.to_string()] } });
    let answer = told(&call(&f, &token, "mail.list", Some(chosen)).await);
    assert!(
        answer.contains(REACHED),
        "the call's chosen mailbox: {answer}"
    );
}

/// D1 (c), D3: a key's profile that names no binding of the family reaches
/// none of it.
#[tokio::test]
async fn a_keys_profile_naming_no_github_binding_reaches_no_github() {
    let (f, token) = node(|profile| AttestingCredential::ApiKey {
        profile: Some(profile),
    })
    .await;
    assert_eq!(
        refused(&call(&f, &token, "github.list_issues", None).await),
        (
            "CREDENTIAL_BINDING_REQUIRED".to_string(),
            KEY_SENTENCE.to_string(),
            needs("profile", "/vault/profiles")
        )
    );
}

/// Trigger 1 (d): the person's own chat turn that chose nothing for `imap`
/// is refused `mail.list` with ADR-0070's sentence for a connected family
/// not chosen; the turn that chose the mailbox reaches it.
#[tokio::test]
async fn d_an_interactive_turn_with_nothing_chosen_for_imap_is_refused_mail_list() {
    let (f, token) = node(|_| AttestingCredential::Interactive).await;
    assert_eq!(
        refused(&call(&f, &token, "mail.list", Some(json!({ "contexts": {} }))).await),
        (
            "CREDENTIAL_BINDING_REQUIRED".to_string(),
            MAILBOX_NOT_CHOSEN.to_string(),
            needs("mailbox", "/vault/connections")
        )
    );
    let chosen = json!({ "contexts": { "imap": [f.mailbox.0.to_string()] } });
    let answer = told(&call(&f, &token, "mail.list", Some(chosen)).await);
    assert!(
        answer.contains(REACHED),
        "the turn's chosen mailbox: {answer}"
    );
}

/// ADR-0069 D3 stands: a family the person has not connected is refused
/// with its own sentence, for a key and a turn alike.
#[tokio::test]
async fn a_family_not_connected_keeps_its_own_refusal() {
    for credential in [
        AttestingCredential::ApiKey { profile: None },
        AttestingCredential::Interactive,
    ] {
        let (f, token) = node(move |_| credential).await;
        assert_eq!(
            refused(&call(&f, &token, "calendar.list", Some(json!({ "contexts": {} }))).await),
            (
                "CREDENTIAL_BINDING_REQUIRED".to_string(),
                not_connected("a calendar"),
                needs("calendar", "/vault/connections")
            ),
            "{credential:?}"
        );
    }
}

/// ADR-0070 D2, ADR-0056, AEGIS ADR-134: the sentence for a connected
/// family not chosen, for every family, is as written and names no record.
#[test]
fn the_not_chosen_sentences_are_as_written_and_name_no_record() {
    for (key, what) in [
        ("imap", "mailbox"),
        ("caldav", "calendar"),
        ("github", "GitHub"),
        ("nuclear-notes", "Carbon Forge"),
        ("notes-own", "notes-own"),
    ] {
        let sentence = not_chosen_sentence(key);
        assert_eq!(
            sentence,
            format!("Your {what} is connected but not chosen for this conversation.")
        );
        assert!(!record_pattern().is_match(&sentence), "{sentence}");
    }
    assert_eq!(not_chosen_sentence("imap"), MAILBOX_NOT_CHOSEN);
}
