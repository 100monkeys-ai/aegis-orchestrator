// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An API key's listing names no tool of a connection family its profile
//! does not name, and with no profile none at all (Zaru ADR-0071 D1, D3,
//! Trigger 1 (c), the listing half): the session listing over the node of
//! `mcp_connections_orchestrator`, whose person holds one mailbox, no
//! calendar and no remote server binding. The route's half, which reads
//! the key and `X-Zaru-Profile`, is `seal.rs`'s `listing_credential_tests`
//! in the CLI crate.

#[path = "support/mcp_connections_orchestrator_fixture.rs"]
mod fixture;
#[path = "support/mail_standins.rs"]
#[allow(dead_code)]
mod mail_standins;

use std::sync::Arc;

use aegis_orchestrator_core::application::profile_service::{
    ProfileAdmission, ProfileError, RunProfiles,
};
use aegis_orchestrator_core::domain::profile::{ToolAllowList, ToolPattern};
use aegis_orchestrator_core::domain::seal_session::AttestingCredential;
use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use async_trait::async_trait;
use fixture::*;
use serde_json::{json, Map, Value};

/// The person's profile `id` naming their mailbox, admitting `mail.list`.
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
            tools: ToolAllowList(vec![ToolPattern::parse("mail.list").unwrap()]),
            families: vec!["mail".to_string(), "imap".to_string()],
        })
    }
}

/// The session's listing, its session attested by `credential`.
async fn listed_by(f: &Fixture, credential: AttestingCredential) -> Vec<String> {
    let session = f
        .sessions
        .find_active_by_security_token(&f.token.clone().into())
        .await
        .unwrap()
        .expect("the session is active")
        .with_attesting_credential(credential);
    f.sessions.save(session.clone()).await.unwrap();
    f.service
        .get_available_tools_for_session(&session)
        .await
        .expect("the session's listing")
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

fn of_families(listed: &[String]) -> Vec<&String> {
    listed
        .iter()
        .filter(|name| {
            [
                "mail.",
                "calendar.",
                "github.",
                "nuclear-notes.",
                "notes-own.",
            ]
            .iter()
            .any(|family| name.starts_with(family))
        })
        .collect()
}

/// (c) D1, D3: a key with no profile, its person holding a mailbox, is
/// listed no tool of any connection family, and the tier's other tools.
#[tokio::test]
async fn c_a_key_with_no_profile_is_listed_no_connection_familys_tool() {
    let f = fixture(1, Arc::new(StopAtTheServer)).await;
    let listed = listed_by(&f, AttestingCredential::ApiKey { profile: None }).await;
    assert!(
        of_families(&listed).is_empty(),
        "a key with no profile is listed: {:?}",
        of_families(&listed)
    );
    assert!(
        listed.iter().any(|name| name.starts_with("aegis.")),
        "the tier's tools of no family are missing: {listed:?}"
    );
}

/// D3: a key whose profile names the mailbox and admits `mail.list` is
/// listed `mail.list` and no other tool of a connection family.
#[tokio::test]
async fn a_keys_profile_lists_only_the_families_and_tools_it_names() {
    let Fixture {
        service,
        sessions,
        approvals,
        tenant,
        token,
        mailbox,
        _dirs,
    } = fixture(1, Arc::new(StopAtTheServer)).await;
    let profile = uuid::Uuid::new_v4();
    let f = Fixture {
        service: service.with_profiles(Arc::new(MailboxProfile {
            id: profile,
            mailbox: mailbox.0.to_string(),
        })),
        sessions,
        approvals,
        tenant,
        token,
        mailbox,
        _dirs,
    };
    let listed = listed_by(
        &f,
        AttestingCredential::ApiKey {
            profile: Some(profile),
        },
    )
    .await;
    assert_eq!(of_families(&listed), vec!["mail.list"], "{listed:?}");
}
