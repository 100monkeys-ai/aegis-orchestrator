// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Audit 002 §4.5/§4.6/§4.7 regression tests — tenant isolation contract for
//! the workflow cancel / remove / signal handler family.
//!
//! Before the fix, `cancel_workflow_execution_handler`,
//! `remove_workflow_execution_handler`, and `signal_workflow_execution_handler`
//! dispatched to Temporal (or, for `remove`, ran an unscoped DELETE) using
//! only the path-supplied `execution_id` as the gate. Any caller with the
//! relevant scope and a UUID could cancel / delete / signal another tenant's
//! workflow execution.
//!
//! The fix added a tenant-scoped `find_by_id_for_tenant` lookup keyed on the
//! caller's JWT-derived tenant before any side effect. On miss the handler
//! returns 404 (NOT 403 — 403 leaks the existence of the other tenant's
//! execution).
//!
//! These tests pin the *contract* the handlers now rely on:
//!
//! 1. `WorkflowExecutionRepository::find_by_id_for_tenant` returns `None`
//!    when the saved execution belongs to a different tenant — proving the
//!    handler's new precondition correctly rejects cross-tenant access.
//! 2. The same-tenant lookup returns `Some` — proving the new precondition
//!    does not break the legitimate path.
//!
//! If either of these properties regresses, every cancel / remove / signal
//! handler in BC-3 silently re-leaks. The contract is the handler's only
//! defense.

use std::collections::HashMap;
use std::sync::Arc;

use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionStatus};
use aegis_orchestrator_core::domain::repository::WorkflowExecutionRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowExecution,
    WorkflowMetadata, WorkflowSpec, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryWorkflowExecutionRepository;
use uuid::Uuid;

fn build_minimal_workflow(name: &str) -> Workflow {
    let mut states = HashMap::new();
    states.insert(
        StateName::new("START").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo start".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::Always,
                target: StateName::new("END").unwrap(),
                feedback: None,
            }],
            timeout: None,
            max_state_visits: None,
        },
    );
    states.insert(
        StateName::new("END").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo end".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
            max_state_visits: None,
        },
    );
    Workflow::new(
        WorkflowMetadata {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
            output_schema: None,
            output_template: None,
        },
        WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states,
        },
    )
    .expect("workflow")
}

async fn save_execution_for(
    repo: &Arc<InMemoryWorkflowExecutionRepository>,
    tenant: &TenantId,
) -> ExecutionId {
    let workflow = build_minimal_workflow("wf-iso");
    let exec_id = ExecutionId(Uuid::new_v4());
    let mut execution = WorkflowExecution::new(&workflow, exec_id, serde_json::json!({}));
    execution.tenant_id = tenant.clone();
    execution.status = ExecutionStatus::Running;
    repo.save_for_tenant(tenant, &execution)
        .await
        .expect("save");
    exec_id
}

/// Audit 002 §4.5: `cancel_workflow_execution_handler`'s tenant-scoped
/// precondition rejects a cross-tenant lookup, so the handler returns 404
/// before ever reaching Temporal.
#[tokio::test]
async fn cancel_handler_precondition_rejects_cross_tenant_lookup() {
    let repo: Arc<InMemoryWorkflowExecutionRepository> =
        Arc::new(InMemoryWorkflowExecutionRepository::new());
    let tenant_a = TenantId::from_realm_slug("tenant-a").unwrap();
    let tenant_b = TenantId::from_realm_slug("tenant-b").unwrap();

    let exec_b = save_execution_for(&repo, &tenant_b).await;

    // Tenant-A caller hits the cancel handler with tenant-B's execution id.
    // The handler now calls `find_by_id_for_tenant(tenant_a, exec_b)` first.
    let res: Arc<dyn WorkflowExecutionRepository> = repo.clone();
    let cross_tenant = res
        .find_by_id_for_tenant(&tenant_a, exec_b)
        .await
        .expect("repo");
    assert!(
        cross_tenant.is_none(),
        "cancel handler precondition: tenant-A MUST NOT see tenant-B's execution; \
         a Some result here means cancel handler will dispatch to Temporal cross-tenant."
    );

    // Same-tenant lookup still succeeds — legitimate cancels are not broken.
    let same_tenant = res
        .find_by_id_for_tenant(&tenant_b, exec_b)
        .await
        .expect("repo");
    assert!(
        same_tenant.is_some(),
        "cancel handler precondition must not break same-tenant cancel"
    );
}

/// Audit 002 §4.6: `remove_workflow_execution_handler`'s tenant-scoped
/// precondition rejects a cross-tenant lookup, blocking the destructive
/// DELETE. The DELETE statement is *also* now scoped on `tenant_id` for
/// defense-in-depth — both layers must hold.
#[tokio::test]
async fn remove_handler_precondition_rejects_cross_tenant_lookup() {
    let repo: Arc<InMemoryWorkflowExecutionRepository> =
        Arc::new(InMemoryWorkflowExecutionRepository::new());
    let tenant_a = TenantId::from_realm_slug("tenant-a").unwrap();
    let tenant_b = TenantId::from_realm_slug("tenant-b").unwrap();

    let exec_b = save_execution_for(&repo, &tenant_b).await;

    // Cross-tenant precondition: must miss.
    let res: Arc<dyn WorkflowExecutionRepository> = repo.clone();
    assert!(
        res.find_by_id_for_tenant(&tenant_a, exec_b)
            .await
            .expect("repo")
            .is_none(),
        "remove handler precondition: tenant-A MUST NOT delete tenant-B's execution"
    );

    // After the handler would have aborted, tenant-B's execution is still
    // resolvable — proving the row was not (and could not be) destroyed.
    let still_there = res
        .find_by_id_for_tenant(&tenant_b, exec_b)
        .await
        .expect("repo");
    assert!(
        still_there.is_some(),
        "tenant-B's execution row must remain after a blocked cross-tenant remove attempt"
    );
}

/// Audit 002 §4.7: `signal_workflow_execution_handler`'s tenant-scoped
/// precondition rejects a cross-tenant lookup, so the handler returns 404
/// before forwarding the human signal.
#[tokio::test]
async fn signal_handler_precondition_rejects_cross_tenant_lookup() {
    let repo: Arc<InMemoryWorkflowExecutionRepository> =
        Arc::new(InMemoryWorkflowExecutionRepository::new());
    let tenant_a = TenantId::from_realm_slug("tenant-a").unwrap();
    let tenant_b = TenantId::from_realm_slug("tenant-b").unwrap();

    let exec_b = save_execution_for(&repo, &tenant_b).await;

    let res: Arc<dyn WorkflowExecutionRepository> = repo.clone();
    let cross_tenant = res
        .find_by_id_for_tenant(&tenant_a, exec_b)
        .await
        .expect("repo");
    assert!(
        cross_tenant.is_none(),
        "signal handler precondition: tenant-A MUST NOT inject signals into tenant-B's execution"
    );

    // Tenant-B's signal path still works.
    let same_tenant = res
        .find_by_id_for_tenant(&tenant_b, exec_b)
        .await
        .expect("repo");
    assert!(
        same_tenant.is_some(),
        "signal handler precondition must not break same-tenant signal"
    );
}
