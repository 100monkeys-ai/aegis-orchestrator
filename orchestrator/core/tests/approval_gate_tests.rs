// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The approval gate with the real `mail.send` contract (AEGIS ADR-126, its
//! Update of 2026-10-04 clause 1; ADR-125's Update of 2026-10-07 (3) clause
//! 12): the catalogue's contract keys a pending request and an "always
//! allow" policy on the `mailbox` argument and lists exactly `mailbox`,
//! `to`, `cc`, `subject` and `body` in the summary. The gate in the dispatch
//! path is tested in the crate (`tool_invocation_service/approval_gate_tests.rs`).

use aegis_orchestrator_core::application::tool_approval_service::{
    GateOutcome, GatedCall, ToolApprovalService,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::tool_approval::{
    ToolApprovalPolicy, ToolApprovalPolicyId, ToolApprovalRepository,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use serde_json::{json, Value};
use std::sync::Arc;

const USER: &str = "gate-user";

fn send_args(mailbox: &str) -> Value {
    json!({
        "mailbox": mailbox,
        "to": ["ann@example.test", "bob@example.test"],
        "cc": ["accounts@example.test"],
        "subject": "Invoice paid",
        "body": "Paid today.",
        "unlisted": "never shown"
    })
}

#[tokio::test]
async fn the_mail_send_contract_keys_the_request_and_the_policy_on_mailbox_and_lists_five_fields() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let repo = Arc::new(InMemoryToolApprovalRepository::new());
    let gate = ToolApprovalService::new(repo.clone(), Arc::new(EventBus::new(64)));
    let tenant = TenantId::default();
    let call = |args: &Value| {
        let args = args.clone();
        let gate = &gate;
        let tenant = &tenant;
        let router = &router;
        async move {
            gate.gate(GatedCall {
                tenant_id: tenant,
                user_sub: Some(USER),
                execution_id: ExecutionId::new(),
                agent_id: AgentId::new(),
                tool_name: "mail.send",
                arguments: &args,
                security_context_name: "zaru-pro",
                conversation_id: None,
                contract: router.approval_contract("mail.send"),
            })
            .await
            .unwrap()
        }
    };

    let mut wrong = Vec::new();
    match call(&send_args("b-1")).await {
        GateOutcome::Pending { result } => {
            let expected = "mail.send\nmailbox: b-1\nto: ann@example.test, bob@example.test\ncc: accounts@example.test\nsubject: Invoice paid\nbody: Paid today.";
            if result["summary"] != expected {
                wrong.push(format!("the summary is {:?}", result["summary"]));
            }
        }
        other => wrong.push(format!("a first call was not pending: {other:?}")),
    }
    let rows = repo
        .list_requests_for_user(&tenant, USER, None)
        .await
        .unwrap();
    if rows.first().and_then(|r| r.binding_id.as_deref()) != Some("b-1") {
        wrong.push(format!(
            "the request is keyed on {:?}, not the mailbox",
            rows.first().map(|r| r.binding_id.clone())
        ));
    }

    repo.insert_policy(&ToolApprovalPolicy {
        id: ToolApprovalPolicyId::new(),
        tenant_id: tenant.clone(),
        user_sub: USER.to_string(),
        tool_name: "mail.send".to_string(),
        binding_id: Some("b-1".to_string()),
        created_at: chrono::Utc::now(),
        created_by: USER.to_string(),
        revoked_at: None,
    })
    .await
    .unwrap();
    if !matches!(call(&send_args("b-1")).await, GateOutcome::Proceed { .. }) {
        wrong.push("the policy on b-1 did not allow a send on b-1".to_string());
    }
    if !matches!(call(&send_args("b-2")).await, GateOutcome::Pending { .. }) {
        wrong.push("the policy on b-1 allowed a send on another mailbox".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
