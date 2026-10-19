// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Deny by default: a run and a schedule's run reach a person's connection
//! only when a profile, or bindings chosen one by one, name it; "no profile"
//! means no connections (Zaru ADR-0071 D1, D3). Calls through the tool
//! service by runs with execution records, against the real credential
//! service, for a person whose one mailbox is granted to all their agents
//! and to the calling agent.
//!
//! Trigger 1:
//! - (a) a run with `contexts: {}` (and one with no `contexts`) and no
//!   profile calls `mail.list` and is refused with D1 (e)'s run sentence and
//!   `needs` `{"family":"profile","page":"/vault/profiles"}`, the grant
//!   notwithstanding;
//! - (b) the same run with a profile naming the mailbox reaches it;
//! - beside (a) and (b), D1 (b) inside a run: an absent `mailbox` is the
//!   run's chosen mailbox, and with nothing chosen it is refused as (a);
//! - (e) the run a schedule with neither a profile nor contexts fires
//!   carries neither, and its `mail.list` reaches no connection;
//! - (g) D1 (e)'s two sentences are answered as written and name no record
//!   (the source scan is `facing_text_no_record_refs.rs`).

#[path = "support/deny_by_default_orchestrator_fixture.rs"]
mod fixture;

use std::sync::Arc;

use aegis_orchestrator_core::application::credential_service::{
    no_profile_names_sentence, KEY_REACHES_NONE,
};
use aegis_orchestrator_core::application::schedule_service::{ScheduledRunPort, ServiceRunStarter};
use aegis_orchestrator_core::application::tools::builtin_mail::{
    MailActing, MailTools, NONE_CHOSEN,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::credential::GrantTarget;
use aegis_orchestrator_core::domain::execution::ServerChoice;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::schedule::{RecurrenceInput, Schedule, ScheduleDraft};
use aegis_orchestrator_core::domain::seal_session::RefusalNeeds;
use chrono::Utc;
use fixture::*;
use serde_json::json;

/// D1 (e)'s key sentence.
const KEY_SENTENCE: &str = "This key reaches none of your connections. Choose a profile for it on the API keys page in Zaru Web, or name one in the request.";

fn profiles_page() -> Option<RefusalNeeds> {
    Some(RefusalNeeds {
        family: "profile".to_string(),
        page: "/vault/profiles".to_string(),
    })
}

/// (a) D1 (a), (b), (e): a run that names nothing reaches no mailbox,
/// whatever is granted: `contexts: {}` and an absent `contexts` alike are
/// refused `CREDENTIAL_BINDING_REQUIRED` with the run sentence and `needs`
/// naming the Profiles page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_a_run_with_no_profile_and_empty_contexts_reaches_no_granted_mailbox() {
    let f = fixture().await;
    let mut wrong = Vec::new();
    for (case, input) in [
        ("contexts {}", json!({ "contexts": {} })),
        ("no contexts", json!({})),
    ] {
        let run = f.run(input);
        let result = f
            .call(
                run,
                "mail.list",
                json!({ "mailbox": f.mailbox.0.to_string() }),
            )
            .await;
        if reached(&result) {
            wrong.push(format!("{case}: the granted mailbox was reached"));
            continue;
        }
        let refusal = refusal_of(&result).expect("refused");
        if (
            refusal.http_status,
            refusal.code,
            refusal.message.as_str(),
            refusal.needs.clone(),
        ) != (
            403,
            "CREDENTIAL_BINDING_REQUIRED",
            NO_PROFILE_NAMES_MAILBOX,
            profiles_page(),
        ) {
            wrong.push(format!(
                "{case}: {} {} {:?} {:?}",
                refusal.http_status, refusal.code, refusal.message, refusal.needs
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// (b) D1 (b), D3: the same run on a profile naming the mailbox (its
/// bindings written into `contexts` at start, AEGIS ADR-140 D6) reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b_the_same_run_with_a_profile_naming_the_mailbox_reaches_it() {
    let f = fixture().await;
    let run = f.run(json!({
        "profile": f.profile.to_string(),
        "contexts": { "imap": [f.mailbox.0.to_string()] },
    }));
    let result = f
        .call(
            run,
            "mail.list",
            json!({ "mailbox": f.mailbox.0.to_string() }),
        )
        .await;
    assert!(
        reached(&result),
        "the profile's mailbox was not reached: {:?}",
        refusal_of(&result).map(|r| r.message)
    );
}

/// D1 (b): inside a run, an absent `mailbox` is resolved by the run's
/// named mailboxes only: with the person's mailbox chosen the call reaches
/// it; with nothing chosen it is refused with the run sentence though the
/// mailbox is granted to all agents; with none chosen, with that sentence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_mailbox_inside_a_run_is_the_runs_chosen_one_only() {
    let vault = Vault::new();
    let mailbox = vault.mailbox(&[GrantTarget::AllAgents]).await;
    let tools = MailTools::with_connector(vault.service.clone(), Arc::new(StopAtTheServer));
    let acting = |choice| MailActing {
        tenant_id: tenant(),
        user_id: Some(PERSON.to_string()),
        agent_id: AgentId::new(),
        workflow_id: None,
        choice,
        has_execution_record: true,
        tier: None,
    };
    let mut wrong = Vec::new();
    let chosen = tools
        .invoke(
            "mail.list",
            &json!({}),
            &acting(ServerChoice::Bindings(vec![mailbox])),
        )
        .await;
    match &chosen {
        Err(e) if format!("{e:?}").contains(REACHED) => {}
        other => wrong.push(format!("the chosen mailbox was not reached: {other:?}")),
    }
    for (case, choice, expected) in [
        (
            "nothing chosen",
            ServerChoice::NotGiven,
            NO_PROFILE_NAMES_MAILBOX,
        ),
        ("none chosen", ServerChoice::None, NONE_CHOSEN),
    ] {
        match tools.invoke("mail.list", &json!({}), &acting(choice)).await {
            Err(e) if e.refusal().message == expected => {}
            other => wrong.push(format!("{case}: {other:?}")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// (e) D3, ADR-139 N8 amended: a schedule with neither a profile nor
/// contexts starts a run carrying neither, and that run's `mail.list`
/// reaches no connection, the grant notwithstanding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e_a_schedules_fire_with_neither_profile_nor_contexts_reaches_no_connection() {
    let f = fixture().await;
    let owner = UserIdentity {
        sub: PERSON.into(),
        realm_slug: "zaru-consumer".into(),
        email: Some("person@example.test".into()),
        email_verified: true,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Pro,
            tenant_id: tenant(),
        },
    };
    let schedule = Schedule::create(
        ScheduleDraft {
            name: Some("triage".into()),
            target_kind: Some("agent".into()),
            target: Some("mail-triage".into()),
            intent: Some("triage the inbox".into()),
            recurrence: Some(RecurrenceInput {
                cron: Some("0 15 * * *".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        &owner,
        tenant(),
        Utc::now(),
    )
    .expect("a schedule");
    let starter = ServiceRunStarter::new(
        f.runs.clone(),
        Arc::new(OneAgent(f.agent.clone())),
        None,
        None,
    );
    let as_owner = schedule.owner.to_identity(&schedule.tenant_id).unwrap();
    starter.start(&schedule, &as_owner).await.expect("started");
    let input = f.runs.started.lock().unwrap()[0].input.clone();
    let mut wrong = Vec::new();
    if input.get("contexts").is_some() || input.get("profile").is_some() {
        wrong.push(format!("the fired run carries a choice: {input}"));
    }
    let run = f.run(input);
    let result = f
        .call(
            run,
            "mail.list",
            json!({ "mailbox": f.mailbox.0.to_string() }),
        )
        .await;
    if reached(&result) {
        wrong.push("the scheduled run reached the granted mailbox".to_string());
    } else {
        let refusal = refusal_of(&result).expect("refused");
        if (refusal.message.as_str(), refusal.needs.clone())
            != (NO_PROFILE_NAMES_MAILBOX, profiles_page())
        {
            wrong.push(format!("told {:?} {:?}", refusal.message, refusal.needs));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// (g) D1 (e), ADR-0056, AEGIS ADR-134: the run sentence for every family
/// and the key sentence are as written, and none names a record.
#[test]
fn g_the_refusal_sentences_are_as_written_and_name_no_record() {
    let mut wrong = Vec::new();
    for (key, family) in [
        ("imap", "mailbox"),
        ("caldav", "calendar"),
        ("github", "GitHub"),
        ("nuclear-notes", "Carbon Forge"),
        ("notes-own", "notes-own"),
    ] {
        let said = no_profile_names_sentence(key);
        let written = format!(
            "This run has no profile that names your {family}. Choose a profile that names it, then start the run again."
        );
        if said != written {
            wrong.push(format!("{key}: {said}"));
        }
        if record_pattern().is_match(&said) {
            wrong.push(format!("names a record: {said}"));
        }
    }
    if KEY_REACHES_NONE != KEY_SENTENCE || record_pattern().is_match(KEY_REACHES_NONE) {
        wrong.push(format!("the key sentence: {KEY_REACHES_NONE}"));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
