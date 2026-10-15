// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The session's listing is the person's callable set (Zaru ADR-0069 D1,
//! Trigger 1 (b)): the tier context's tools, less every connection family
//! the person holds no active binding of. The person of the fixture holds
//! one mailbox and no calendar and no remote server binding.

#[path = "support/mcp_connections_orchestrator_fixture.rs"]
mod fixture;
#[path = "support/mail_standins.rs"]
mod mail_standins;

use std::sync::Arc;

use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
use fixture::*;

/// (b) D1: the listing holds the mail tools (a mailbox is connected) and no
/// `calendar.`, `github.`, `nuclear-notes.` or `notes-own.` tool.
#[tokio::test]
async fn b_the_sessions_listing_holds_no_tool_of_a_family_the_person_has_not_connected() {
    let f = fixture(1, Arc::new(StopAtTheServer)).await;
    let session = f
        .sessions
        .find_active_by_security_token(&f.token.clone().into())
        .await
        .unwrap()
        .expect("the session is active");
    let listed: Vec<String> = f
        .service
        .get_available_tools_for_session(&session)
        .await
        .expect("the session's listing")
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(
        listed.iter().any(|name| name == "mail.list"),
        "the connected mailbox's tools are not listed: {listed:?}"
    );
    let unconnected: Vec<&String> = listed
        .iter()
        .filter(|name| {
            ["calendar.", "github.", "nuclear-notes.", "notes-own."]
                .iter()
                .any(|family| name.starts_with(family))
        })
        .collect();
    assert!(
        unconnected.is_empty(),
        "tools of families not connected are listed: {unconnected:?}"
    );
    assert!(
        listed.iter().any(|name| name.starts_with("aegis.")),
        "the tier's tools of no family are missing: {listed:?}"
    );
}
