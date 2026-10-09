// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The profile routes (AEGIS ADR-140 D3), driven through the daemon's real
//! authentication stack against the real profile service over the
//! in-memory store, one IMAP mailbox of the owner's, and a security context
//! admitting `mail.reply` only.

use std::sync::Arc;

use aegis_orchestrator_core::application::profile_service::{
    ProfileBindingSource, ProfileService, ProfileToolCatalogue,
};
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialMetadata, CredentialProvider, CredentialScope, CredentialStatus,
    CredentialType, MailSecurity, MailboxSettings, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::iam::AegisRole;
use aegis_orchestrator_core::domain::profile::{PERSON_REFUSAL, UNAVAILABLE_REFUSAL};
use aegis_orchestrator_core::domain::secrets::SecretPath;
use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
use aegis_orchestrator_core::domain::security_context::{
    Capability, SecurityContext, SecurityContextMetadata,
};
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_profile::InMemoryProfileRepository;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use reqwest::Method;
use serde_json::json;

use super::{profiles_router, ProfilesState};
use crate::daemon::handlers::test_support::{
    consumer, identity_provider, operator, send, serve, service_account,
};

const SCOPES: &str = "profile:read profile:write";
const OWNER: &str = "owner-sub";
const OTHER: &str = "other-sub";

struct OneMailbox(UserCredentialBinding);

#[async_trait::async_trait]
impl ProfileBindingSource for OneMailbox {
    async fn owner_bindings(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok([self.0.clone()]
            .into_iter()
            .filter(|b| b.owner_user_id == user_sub && &b.tenant_id == tenant_id)
            .collect())
    }
}

struct MailCatalogue;

impl ProfileToolCatalogue for MailCatalogue {
    fn builtin_tools(&self) -> Vec<(String, String)> {
        vec![
            ("mail.reply".into(), "Replies in a thread.".into()),
            ("mail.send".into(), "Sends a message.".into()),
        ]
    }
    fn capability_names(&self) -> Vec<String> {
        Vec::new()
    }
    fn requires_approval(&self, tool_name: &str) -> bool {
        tool_name.starts_with("mail.")
    }
}

fn mailbox() -> UserCredentialBinding {
    let address = "owner@example.com".to_string();
    UserCredentialBinding {
        id: CredentialBindingId::new(),
        owner_user_id: OWNER.into(),
        tenant_id: TenantId::for_consumer_user(OWNER).unwrap(),
        credential_type: CredentialType::Mailbox,
        provider: CredentialProvider::imap(),
        secret_path: SecretPath::new("aegis-system", "kv", "test/mailbox"),
        scope: CredentialScope::Personal,
        status: CredentialStatus::Active,
        metadata: CredentialMetadata {
            label: "Mail".into(),
            tags: None,
            service_url: None,
            external_account_id: None,
            oauth_scopes: None,
            mailbox: Some(MailboxSettings {
                address: address.clone(),
                display_name: None,
                imap_host: "imap.example.com".into(),
                imap_port: 993,
                imap_security: MailSecurity::Tls,
                smtp_host: "smtp.example.com".into(),
                smtp_port: 465,
                smtp_security: MailSecurity::Tls,
                username: address,
            }),
            reach: None,
            calendar: None,
        },
        grants: Vec::new(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

async fn base(mailbox: &UserCredentialBinding) -> String {
    let contexts = InMemorySecurityContextRepository::new();
    contexts
        .save(SecurityContext {
            name: "zaru-free".into(),
            description: "test".into(),
            capabilities: vec![Capability {
                tool_pattern: "mail.reply".into(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            }],
            deny_list: Vec::new(),
            metadata: SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        })
        .await
        .unwrap();
    let service = ProfileService::new(
        Arc::new(InMemoryProfileRepository::new()),
        Arc::new(OneMailbox(mailbox.clone())),
        Arc::new(contexts),
        Arc::new(MailCatalogue),
        None,
    );
    let iam = identity_provider(&[
        ("owner", consumer(OWNER), SCOPES),
        ("other", consumer(OTHER), SCOPES),
        ("reader", consumer(OWNER), "profile:read"),
        ("operator", operator(AegisRole::Admin), SCOPES),
        ("service", service_account(), SCOPES),
    ]);
    serve(
        profiles_router(ProfilesState {
            service: Some(Arc::new(service)),
        }),
        Some(iam),
        None,
    )
    .await
}

/// D3: the owner creates, lists, reads, changes and deletes a profile over
/// HTTP; another person's calls on it are answered 404 as for no profile.
#[tokio::test]
async fn the_owner_manages_a_profile_and_another_person_finds_none() {
    let mailbox = mailbox();
    let base = base(&mailbox).await;
    let (status, made) = send(
        &base,
        &Method::POST,
        "/v1/profiles",
        &Some(json!({
            "name": "Replies",
            "bindings": [mailbox.id.0.to_string()],
            "tools": ["mail.reply"],
        })),
        Some("owner"),
    )
    .await;
    assert_eq!(status, 201, "{made}");
    let id = made["profile"]["id"].as_str().expect("an id").to_string();
    assert_eq!(made["profile"]["tools"], json!(["mail.reply"]));
    assert_eq!(
        made["profile"]["bindings"],
        json!([{"binding_id": mailbox.id.0.to_string(), "state": "active", "label": "Mail"}])
    );

    let (status, listed) = send(&base, &Method::GET, "/v1/profiles", &None, Some("owner")).await;
    assert_eq!(
        (status, listed["count"].clone()),
        (200, json!(1)),
        "{listed}"
    );

    let path = format!("/v1/profiles/{id}");
    let mut other_answers = Vec::new();
    for (method, body) in [
        (Method::GET, None),
        (Method::PATCH, Some(json!({"name": "Mine"}))),
        (Method::DELETE, None),
    ] {
        let (status, body) = send(&base, &method, &path, &body, Some("other")).await;
        other_answers.push((method.to_string(), status, body));
    }
    assert!(
        other_answers
            .iter()
            .all(|(_, status, body)| *status == 404 && body["error"] == "Not found"),
        "another person's calls were not answered 404: {other_answers:?}"
    );
    let (_, other_list) = send(&base, &Method::GET, "/v1/profiles", &None, Some("other")).await;
    assert_eq!(other_list["count"], json!(0));

    let (status, changed) = send(
        &base,
        &Method::PATCH,
        &path,
        &Some(json!({"name": "Answers", "tools": []})),
        Some("owner"),
    )
    .await;
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["profile"]["name"], "Answers");
    assert_eq!(changed["profile"]["tools"], json!([]));

    let (status, _) = send(&base, &Method::DELETE, &path, &None, Some("owner")).await;
    assert_eq!(status, 204);
    let (status, _) = send(&base, &Method::GET, &path, &None, Some("owner")).await;
    assert_eq!(status, 404);
}

/// D2 at the route: a tool the plan does not allow is answered 400 with
/// the narrowing sentence, and nothing is stored.
#[tokio::test]
async fn a_tool_the_plan_does_not_allow_is_answered_400() {
    let mailbox = mailbox();
    let base = base(&mailbox).await;
    let (status, body) = send(
        &base,
        &Method::POST,
        "/v1/profiles",
        &Some(json!({
            "name": "Sends",
            "bindings": [mailbox.id.0.to_string()],
            "tools": ["mail.send"],
        })),
        Some("owner"),
    )
    .await;
    assert_eq!(
        (status, body["error"].clone()),
        (
            400,
            json!("A profile can only narrow the tools your plan allows; mail.send is not one of them.")
        )
    );
    let (_, listed) = send(&base, &Method::GET, "/v1/profiles", &None, Some("owner")).await;
    assert_eq!(listed["count"], json!(0));
}

/// D3: `available-tools` answers what the plan admits for the bindings,
/// `gated` as the catalogue marks it.
#[tokio::test]
async fn available_tools_answers_the_plans_tools() {
    let mailbox = mailbox();
    let base = base(&mailbox).await;
    let (status, body) = send(
        &base,
        &Method::POST,
        "/v1/profiles/available-tools",
        &Some(json!({"bindings": [mailbox.id.0.to_string()]})),
        Some("reader"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["tools"],
        json!([{"name": "mail.reply", "description": "Replies in a thread.", "family": "mail", "gated": true}])
    );
}

/// D3: a write needs `profile:write`; an operator and a service account
/// are refused with the plain sentence; a node without the service answers
/// 503.
#[tokio::test]
async fn scopes_and_callers_are_held_to_the_route_table() {
    let mailbox = mailbox();
    let base = base(&mailbox).await;
    let body = Some(json!({"name": "A"}));
    let (status, _) = send(&base, &Method::POST, "/v1/profiles", &body, Some("reader")).await;
    assert_eq!(status, 403, "a write without profile:write was not refused");
    let mut refused = Vec::new();
    for caller in ["operator", "service"] {
        let (status, answer) = send(&base, &Method::GET, "/v1/profiles", &None, Some(caller)).await;
        refused.push((caller, status, answer["error"].clone()));
    }
    assert_eq!(
        refused,
        vec![
            ("operator", 403, json!(PERSON_REFUSAL)),
            ("service", 403, json!(PERSON_REFUSAL)),
        ]
    );

    let iam = identity_provider(&[("owner", consumer(OWNER), SCOPES)]);
    let unavailable = serve(
        profiles_router(ProfilesState { service: None }),
        Some(iam),
        None,
    )
    .await;
    let (status, answer) = send(
        &unavailable,
        &Method::GET,
        "/v1/profiles",
        &None,
        Some("owner"),
    )
    .await;
    assert_eq!(
        (status, answer["error"].clone()),
        (503, json!(UNAVAILABLE_REFUSAL))
    );
}
