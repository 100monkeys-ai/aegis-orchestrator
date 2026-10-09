// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The profile service (AEGIS ADR-140 D1 to D5) over the in-memory store, a
//! fixed set of the owner's bindings, a security context like the
//! deployment's `zaru-free` and a small tool catalogue.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};

use super::{
    AvailableTool, ProfileBindingSource, ProfileDraft, ProfileError, ProfilePatch, ProfileService,
    ProfileToolCatalogue,
};
use crate::domain::credential::{
    CalendarSettings, CredentialBindingId, CredentialMetadata, CredentialProvider, CredentialScope,
    CredentialStatus, CredentialType, MailSecurity, MailboxSettings, UserCredentialBinding,
};
use crate::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use crate::domain::profile::{narrow_refusal, ProfileId, NAME_TAKEN_REFUSAL, REPOSITORY_REFUSAL};
use crate::domain::secrets::SecretPath;
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::security_context::{Capability, SecurityContext, SecurityContextMetadata};
use crate::domain::tenant::TenantId;
use crate::infrastructure::repositories::postgres_profile::InMemoryProfileRepository;
use crate::infrastructure::security_context::InMemorySecurityContextRepository;

const OWNER: &str = "owner-sub";
const OTHER: &str = "other-sub";

fn person(sub: &str) -> UserIdentity {
    UserIdentity {
        sub: sub.into(),
        realm_slug: "zaru-consumer".into(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Free,
            tenant_id: tenant_of(sub),
        },
    }
}

fn tenant_of(sub: &str) -> TenantId {
    TenantId::for_consumer_user(sub).expect("per-user tenant id")
}

fn binding(
    owner: &str,
    label: &str,
    credential_type: CredentialType,
    provider: &str,
) -> UserCredentialBinding {
    UserCredentialBinding {
        id: CredentialBindingId::new(),
        owner_user_id: owner.to_string(),
        tenant_id: tenant_of(owner),
        credential_type,
        provider: CredentialProvider::new(provider),
        secret_path: SecretPath::new("aegis-system", "kv", "test/key"),
        scope: CredentialScope::Personal,
        status: CredentialStatus::Active,
        metadata: CredentialMetadata {
            label: label.to_string(),
            tags: None,
            service_url: None,
            external_account_id: None,
            oauth_scopes: None,
            mailbox: None,
            reach: None,
            calendar: None,
        },
        grants: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn mailbox_settings(address: &str) -> MailboxSettings {
    MailboxSettings {
        address: address.to_string(),
        display_name: None,
        imap_host: "imap.example.com".into(),
        imap_port: 993,
        imap_security: MailSecurity::Tls,
        smtp_host: "smtp.example.com".into(),
        smtp_port: 465,
        smtp_security: MailSecurity::Tls,
        username: address.to_string(),
    }
}

/// The owner's Gmail (an OAuth binding with mail and calendar settings), a
/// second mailbox by IMAP, a GitHub binding, and another person's mailbox.
struct Bindings {
    gmail: UserCredentialBinding,
    imap: UserCredentialBinding,
    github: UserCredentialBinding,
    others: UserCredentialBinding,
}

fn bindings() -> Bindings {
    let mut gmail = binding(OWNER, "Gmail", CredentialType::OAuth2, "google");
    gmail.metadata.mailbox = Some(mailbox_settings("owner@example.com"));
    gmail.metadata.calendar = Some(CalendarSettings {
        server: "https://apidata.googleusercontent.com/caldav/v2/".into(),
        principal: "owner@example.com/user".into(),
        address: "owner@example.com".into(),
    });
    let mut imap = binding(OWNER, "Work mail", CredentialType::Mailbox, "imap");
    imap.metadata.mailbox = Some(mailbox_settings("owner@work.example"));
    let github = binding(OWNER, "GitHub", CredentialType::OAuth2, "github");
    let mut others = binding(OTHER, "Theirs", CredentialType::Mailbox, "imap");
    others.metadata.mailbox = Some(mailbox_settings("other@example.com"));
    Bindings {
        gmail,
        imap,
        github,
        others,
    }
}

/// The bindings every person holds, changeable by a test.
struct HeldBindings(Mutex<Vec<UserCredentialBinding>>);

#[async_trait]
impl ProfileBindingSource for HeldBindings {
    async fn owner_bindings(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.owner_user_id == user_sub && &b.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
}

struct Catalogue;

impl ProfileToolCatalogue for Catalogue {
    fn builtin_tools(&self) -> Vec<(String, String)> {
        [
            ("mail.list", "Lists threads."),
            ("mail.reply", "Replies in a thread."),
            ("mail.send", "Sends a message."),
            ("mail.delete", "Deletes a thread."),
            ("calendar.list", "Lists events."),
            ("calendar.create", "Creates an event."),
            ("cmd.run", "Runs a command."),
        ]
        .into_iter()
        .map(|(n, d)| (n.to_string(), d.to_string()))
        .collect()
    }
    fn capability_names(&self) -> Vec<String> {
        vec!["github.create_issue".into(), "aegis.git.push".into()]
    }
    fn requires_approval(&self, tool_name: &str) -> bool {
        matches!(
            tool_name,
            "mail.send" | "mail.reply" | "mail.delete" | "calendar.create" | "github.create_issue"
        )
    }
}

fn capability(pattern: &str) -> Capability {
    Capability {
        tool_pattern: pattern.to_string(),
        path_allowlist: None,
        command_allowlist: None,
        subcommand_allowlist: None,
        domain_allowlist: None,
        max_response_size: None,
        rate_limit: None,
        max_concurrent: None,
    }
}

/// Like the deployment's `zaru-free`: the mail tools by name but
/// `mail.delete`, the calendar by family, GitHub by pattern.
async fn contexts() -> Arc<dyn SecurityContextRepository> {
    let repo = InMemorySecurityContextRepository::new();
    repo.save(SecurityContext {
        name: "zaru-free".into(),
        description: "test".into(),
        capabilities: [
            "mail.list",
            "mail.reply",
            "mail.send",
            "calendar.*",
            "github.*",
        ]
        .into_iter()
        .map(capability)
        .collect(),
        deny_list: Vec::new(),
        metadata: SecurityContextMetadata {
            created_at: Utc::now(),
            updated_at: Utc::now(),
            version: 1,
        },
    })
    .await
    .unwrap();
    Arc::new(repo)
}

struct Harness {
    service: ProfileService,
    held: Arc<HeldBindings>,
    b: Bindings,
}

async fn harness() -> Harness {
    let b = bindings();
    let held = Arc::new(HeldBindings(Mutex::new(vec![
        b.gmail.clone(),
        b.imap.clone(),
        b.github.clone(),
        b.others.clone(),
    ])));
    let service = ProfileService::new(
        Arc::new(InMemoryProfileRepository::new()),
        held.clone(),
        contexts().await,
        Arc::new(Catalogue),
        None,
    );
    Harness { service, held, b }
}

fn draft(value: Value) -> ProfileDraft {
    serde_json::from_value(value).expect("the draft parses")
}

fn patch(value: Value) -> ProfilePatch {
    serde_json::from_value(value).expect("the patch parses")
}

fn id_of(b: &UserCredentialBinding) -> String {
    b.id.0.to_string()
}

fn refused(result: Result<impl std::fmt::Debug, ProfileError>) -> String {
    match result {
        Err(ProfileError::Refused(sentence)) => sentence,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// D1, D3: the owner makes, reads, lists, changes and deletes a profile;
/// another person reads, changes and deletes none of it.
#[tokio::test]
async fn the_owner_makes_reads_lists_changes_and_deletes_a_profile() {
    let h = harness().await;
    let (owner, tenant) = (person(OWNER), tenant_of(OWNER));
    let made = h
        .service
        .create(
            &owner,
            &tenant,
            draft(json!({
                "name": " Fundraising ",
                "bindings": [id_of(&h.b.gmail), id_of(&h.b.imap), id_of(&h.b.github)],
                "tools": ["mail.reply", "calendar.*"],
                "notes_workspace": "fundraising",
                "instructions": "Keep replies short.",
            })),
        )
        .await
        .expect("the profile is made");
    let id = made.profile.id;
    assert_eq!(made.profile.name, "Fundraising");
    assert_eq!(made.profile.bindings.len(), 3, "two mailboxes and GitHub");
    assert_eq!(
        made.profile.tools.as_strings(),
        vec!["mail.reply", "calendar.*"]
    );

    let read = h.service.get(&owner, &tenant, &id).await.expect("read");
    assert_eq!(read.profile, made.profile);
    assert!(read.bindings.iter().all(|b| b.state == "active"));
    assert_eq!(read.bindings[0].label.as_deref(), Some("Gmail"));

    let changed = h
        .service
        .update(
            &owner,
            &tenant,
            &id,
            patch(json!({"name": "Replies", "tools": [], "instructions": null})),
        )
        .await
        .expect("changed");
    assert_eq!(changed.profile.name, "Replies");
    assert!(changed.profile.tools.0.is_empty());
    assert_eq!(changed.profile.instructions, None);
    assert_eq!(
        changed.profile.notes_workspace.as_deref(),
        Some("fundraising")
    );

    let listed = h.service.list(&owner, &tenant).await.expect("listed");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].profile.name, "Replies");

    let (other, other_tenant) = (person(OTHER), tenant_of(OTHER));
    let mut answered_other = Vec::new();
    answered_other.push(h.service.get(&other, &other_tenant, &id).await.err());
    answered_other.push(
        h.service
            .update(&other, &other_tenant, &id, patch(json!({"name": "Mine"})))
            .await
            .err(),
    );
    answered_other.push(h.service.delete(&other, &other_tenant, &id).await.err());
    assert!(
        answered_other
            .iter()
            .all(|answer| answer == &Some(ProfileError::NotFound)),
        "another person's calls were not answered as for no profile: {answered_other:?}"
    );
    assert!(h
        .service
        .list(&other, &other_tenant)
        .await
        .unwrap()
        .is_empty());

    h.service
        .delete(&owner, &tenant, &id)
        .await
        .expect("deleted");
    assert_eq!(
        h.service.get(&owner, &tenant, &id).await.err(),
        Some(ProfileError::NotFound)
    );
    assert!(h.service.list(&owner, &tenant).await.unwrap().is_empty());
}

/// D1: another person's binding, and the owner's own revoked one, are
/// refused at create and at update.
#[tokio::test]
async fn a_binding_not_the_owners_own_active_one_is_refused() {
    let h = harness().await;
    let (owner, tenant) = (person(OWNER), tenant_of(OWNER));
    let theirs = id_of(&h.b.others);
    let sentence = refused(
        h.service
            .create(
                &owner,
                &tenant,
                draft(json!({"name": "A", "bindings": [theirs]})),
            )
            .await,
    );
    assert_eq!(
        sentence,
        format!("'{theirs}' is not an active connection of yours.")
    );

    h.held.0.lock().unwrap()[1].status = CredentialStatus::Revoked;
    let revoked = id_of(&h.b.imap);
    let mut sentences = vec![refused(
        h.service
            .create(
                &owner,
                &tenant,
                draft(json!({"name": "B", "bindings": [revoked]})),
            )
            .await,
    )];
    let made = h
        .service
        .create(
            &owner,
            &tenant,
            draft(json!({"name": "C", "bindings": [id_of(&h.b.gmail)]})),
        )
        .await
        .expect("made over an active binding");
    sentences.push(refused(
        h.service
            .update(
                &owner,
                &tenant,
                &made.profile.id,
                patch(json!({"bindings": [id_of(&h.b.gmail), revoked]})),
            )
            .await,
    ));
    let expected = format!("'{revoked}' is not an active connection of yours.");
    assert!(
        sentences.iter().all(|s| s == &expected),
        "a revoked binding was not refused with its sentence: {sentences:?}"
    );
}

/// D1: a name the owner already uses, ignoring case, is refused; another
/// person may use it.
#[tokio::test]
async fn a_name_taken_ignoring_case_is_refused() {
    let h = harness().await;
    let (owner, tenant) = (person(OWNER), tenant_of(OWNER));
    let first = h
        .service
        .create(&owner, &tenant, draft(json!({"name": "Fundraising"})))
        .await
        .expect("made");
    let second = h
        .service
        .create(&owner, &tenant, draft(json!({"name": "Other"})))
        .await
        .expect("made");
    let sentences = vec![
        refused(
            h.service
                .create(&owner, &tenant, draft(json!({"name": "FUNDRAISING"})))
                .await,
        ),
        refused(
            h.service
                .update(
                    &owner,
                    &tenant,
                    &second.profile.id,
                    patch(json!({"name": "fundraising "})),
                )
                .await,
        ),
    ];
    assert!(
        sentences.iter().all(|s| s == NAME_TAKEN_REFUSAL),
        "a taken name was not refused: {sentences:?}"
    );
    h.service
        .update(
            &owner,
            &tenant,
            &first.profile.id,
            patch(json!({"name": "fundraising"})),
        )
        .await
        .expect("a profile keeps its own name in another case");
    h.service
        .create(
            &person(OTHER),
            &tenant_of(OTHER),
            draft(json!({"name": "Fundraising"})),
        )
        .await
        .expect("another person may use the name");
}

/// D2: a pattern the owner's security context does not admit for the
/// profile's bindings is refused with D2's sentence: a tool the plan does
/// not allow, a family outside the bindings', a family the plan reaches
/// nothing of; the plan's own tools are accepted.
#[tokio::test]
async fn a_pattern_the_plan_does_not_allow_is_refused_with_the_narrowing_sentence() {
    let h = harness().await;
    let (owner, tenant) = (person(OWNER), tenant_of(OWNER));
    let mailbox_and_github = json!([id_of(&h.b.imap), id_of(&h.b.github)]);
    let mut wrong = Vec::new();
    for pattern in [
        "mail.delete",
        "calendar.*",
        "cmd.run",
        "githubx.*",
        "githubx.list",
    ] {
        let answer = h
            .service
            .create(
                &owner,
                &tenant,
                draft(json!({"name": pattern, "bindings": mailbox_and_github, "tools": [pattern]})),
            )
            .await;
        match answer {
            Err(ProfileError::Refused(sentence)) if sentence == narrow_refusal(pattern) => {}
            other => wrong.push(format!("{pattern}: {other:?}")),
        }
    }
    assert!(
        wrong.is_empty(),
        "patterns not refused with D2's sentence: {wrong:?}"
    );
    assert_eq!(
        narrow_refusal("mail.delete"),
        "A profile can only narrow the tools your plan allows; mail.delete is not one of them."
    );
    h.service
        .create(
            &owner,
            &tenant,
            draft(json!({
                "name": "Allowed",
                "bindings": mailbox_and_github,
                "tools": ["mail.reply", "mail.*", "github.*", "github.create_issue"],
            })),
        )
        .await
        .expect("patterns the plan allows are accepted");
}

/// D3 (the grant's R1): the tools the plan admits for the bindings'
/// families, `gated` for a gated one; a remote server's family answered by
/// its named capabilities and one wildcard entry.
#[tokio::test]
async fn available_tools_answers_only_admitted_tools_with_their_gate() {
    let h = harness().await;
    let (owner, tenant) = (person(OWNER), tenant_of(OWNER));
    let tools = h
        .service
        .available_tools(
            &owner,
            &tenant,
            &json!([id_of(&h.b.gmail), id_of(&h.b.github)]),
        )
        .await
        .expect("answered");
    let tool =
        |name: &str, family: &str, gated: bool| (name.to_string(), family.to_string(), gated);
    let answered: Vec<(String, String, bool)> = tools
        .iter()
        .map(|t: &AvailableTool| (t.name.clone(), t.family.clone(), t.gated))
        .collect();
    assert_eq!(
        answered,
        vec![
            tool("mail.list", "mail", false),
            tool("mail.reply", "mail", true),
            tool("mail.send", "mail", true),
            tool("calendar.list", "calendar", false),
            tool("calendar.create", "calendar", true),
            tool("github.create_issue", "github", true),
            tool("github.*", "github", false),
        ],
        "mail.delete and cmd.run must not be offered; gated tools must say so"
    );
    let theirs = id_of(&h.b.others);
    assert_eq!(
        refused(
            h.service
                .available_tools(&owner, &tenant, &json!([theirs]))
                .await
        ),
        format!("'{theirs}' is not an active connection of yours.")
    );
}

/// D4: a binding revoked or deleted after the profile was made stays in it
/// and is answered `removed` on read; the profile is not changed, and its
/// owner may still rename it and drop the binding by an update.
#[tokio::test]
async fn a_removed_binding_is_answered_removed_on_read() {
    let h = harness().await;
    let (owner, tenant) = (person(OWNER), tenant_of(OWNER));
    let made = h
        .service
        .create(
            &owner,
            &tenant,
            draft(json!({
                "name": "Mail",
                "bindings": [id_of(&h.b.gmail), id_of(&h.b.imap), id_of(&h.b.github)],
            })),
        )
        .await
        .expect("made");
    {
        let mut held = h.held.0.lock().unwrap();
        held[1].revoke();
        held.retain(|b| b.id != h.b.github.id);
    }
    let read = h
        .service
        .get(&owner, &tenant, &made.profile.id)
        .await
        .expect("read");
    let states: Vec<(CredentialBindingId, &str)> = read
        .bindings
        .iter()
        .map(|b| (b.binding_id, b.state))
        .collect();
    assert_eq!(
        states,
        vec![
            (h.b.gmail.id, "active"),
            (h.b.imap.id, "removed"),
            (h.b.github.id, "removed"),
        ],
        "a revoked or deleted binding was not answered removed"
    );
    assert_eq!(read.profile.bindings, made.profile.bindings);
    h.service
        .update(
            &owner,
            &tenant,
            &made.profile.id,
            patch(json!({"name": "Renamed"})),
        )
        .await
        .expect("a profile holding a removed binding can still be renamed");
    let dropped = h
        .service
        .update(
            &owner,
            &tenant,
            &made.profile.id,
            patch(json!({"bindings": [id_of(&h.b.gmail)]})),
        )
        .await
        .expect("the removed rows are dropped by an update");
    assert_eq!(dropped.profile.bindings, vec![h.b.gmail.id]);
}

/// D1: a repository default that is not one of the owner's is refused.
#[tokio::test]
async fn a_repository_not_the_owners_is_refused() {
    let h = harness().await;
    let sentence = refused(
        h.service
            .create(
                &person(OWNER),
                &tenant_of(OWNER),
                draft(json!({"name": "Code", "repository": ProfileId::new().to_string()})),
            )
            .await,
    );
    assert_eq!(sentence, REPOSITORY_REFUSAL);
}

/// AEGIS ADR-140 D6: a run on a profile is given its active bindings as the
/// per-server map, mailboxes under `imap` (two of one profile told apart per
/// call by `mailbox`), calendar accounts under `caldav`, every other binding
/// under its provider; a removed binding is left out; another person's
/// profile, or one deleted, is answered as none.
#[tokio::test]
async fn a_run_on_a_profile_is_given_its_active_bindings_per_server() {
    use super::RunProfiles;
    let h = harness().await;
    let (owner, tenant) = (person(OWNER), tenant_of(OWNER));
    let made = h
        .service
        .create(
            &owner,
            &tenant,
            draft(json!({
                "name": "Fundraising",
                "bindings": [id_of(&h.b.gmail), id_of(&h.b.imap), id_of(&h.b.github)],
                "tools": ["mail.reply"],
            })),
        )
        .await
        .expect("made");
    let id = made.profile.id.0;
    let mut complaints = Vec::new();
    let contexts = h
        .service
        .contexts_of(&tenant, Some(OWNER), id)
        .await
        .expect("the owner's profile is read");
    let expected = json!({
        "imap": [id_of(&h.b.gmail), id_of(&h.b.imap)],
        "caldav": [id_of(&h.b.gmail)],
        "github": [id_of(&h.b.github)],
    });
    if Value::Object(contexts) != expected {
        complaints.push("the profile's bindings were not written per server".to_string());
    }
    let admission = h
        .service
        .admission_of(&tenant, Some(OWNER), id)
        .await
        .expect("admission read");
    for (tool, admitted) in [
        ("mail.reply", true),
        ("mail.send", false),
        ("calendar.create", false),
        ("github.create_issue", false),
        ("web.search", true),
        ("aegis.task.execute", true),
    ] {
        if admission.admits(tool) != admitted {
            complaints.push(format!("{tool} admitted: {}", admission.admits(tool)));
        }
    }
    h.held.0.lock().unwrap().iter_mut().for_each(|b| {
        if b.id == h.b.imap.id {
            b.status = CredentialStatus::Revoked;
        }
    });
    let after = h
        .service
        .contexts_of(&tenant, Some(OWNER), id)
        .await
        .unwrap();
    if after.get("imap") != Some(&json!([id_of(&h.b.gmail)])) {
        complaints.push(format!("a removed mailbox stayed in the run: {after:?}"));
    }
    if h.service
        .contexts_of(&tenant_of(OTHER), Some(OTHER), id)
        .await
        != Err(ProfileError::NotFound)
    {
        complaints.push("another person's run was given the profile".to_string());
    }
    h.service
        .delete(&owner, &tenant, &made.profile.id)
        .await
        .unwrap();
    if h.service.contexts_of(&tenant, Some(OWNER), id).await != Err(ProfileError::NotFound) {
        complaints.push("a deleted profile was still given".to_string());
    }
    if h.service.admission_of(&tenant, Some(OWNER), id).await != Err(ProfileError::NotFound) {
        complaints.push("a deleted profile still admitted".to_string());
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// AEGIS ADR-140 D9: deleting a profile revokes the standing choices made
/// in it, and only those.
#[tokio::test]
async fn deleting_a_profile_revokes_its_standing_choices() {
    use crate::application::tool_approval_service::ToolApprovalService;
    use crate::domain::tool_approval::{
        ToolApprovalPolicy, ToolApprovalPolicyEffect, ToolApprovalPolicyId, ToolApprovalRepository,
    };
    use crate::infrastructure::event_bus::EventBus;
    use crate::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
    let h = harness().await;
    let (owner, tenant) = (person(OWNER), tenant_of(OWNER));
    let repo = Arc::new(InMemoryToolApprovalRepository::new());
    h.service
        .set_standing_choices(Arc::new(ToolApprovalService::new(
            repo.clone(),
            Arc::new(EventBus::new(16)),
        )));
    let made = h
        .service
        .create(
            &owner,
            &tenant,
            draft(json!({ "name": "Fundraising", "bindings": [id_of(&h.b.gmail)], "tools": [] })),
        )
        .await
        .unwrap();
    let policy = |profile_id: Option<uuid::Uuid>| ToolApprovalPolicy {
        id: ToolApprovalPolicyId::new(),
        tenant_id: tenant.clone(),
        user_sub: OWNER.to_string(),
        tool_name: "mail.reply".to_string(),
        binding_id: Some(id_of(&h.b.gmail)),
        profile_id,
        effect: ToolApprovalPolicyEffect::Allow,
        created_at: Utc::now(),
        created_by: OWNER.to_string(),
        revoked_at: None,
    };
    repo.insert_policy(&policy(Some(made.profile.id.0)))
        .await
        .unwrap();
    repo.insert_policy(&policy(None)).await.unwrap();
    h.service
        .delete(&owner, &tenant, &made.profile.id)
        .await
        .unwrap();
    let left = repo.list_active_policies(&tenant, OWNER).await.unwrap();
    assert_eq!(
        left.iter().map(|p| p.profile_id).collect::<Vec<_>>(),
        vec![None],
        "the profile's choice was not revoked, or the raw-binding choice went with it"
    );
}
