// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
// Zaru ADR-0071 D3: "A child run or an agent state inherits its parent's
// choice, as ADR-140 D6 says, and never widens it." A child that names its
// own `contexts` may choose only bindings its parent chose, server by
// server, or `null`; anything wider refuses its start. With no choice of
// its own it takes its parent's, as before.

// Included into `execution.rs`'s test module, whose helpers it uses.

const OTHER_BINDING: &str = "8a2c4e6f-1b3d-4f5a-9c7e-0d2f4a6c8e1b";
const MAILBOX: &str = "2b4d6f8a-3c5e-4a7b-8d9f-1e3a5c7e9b2d";

/// Start a child of a parent whose input carries `parent_contexts` (none
/// when `None`), with the child's own `child_contexts`; answers the start's
/// result and the child's persisted `contexts`.
async fn child_of(
    parent_contexts: Option<serde_json::Value>,
    child_contexts: serde_json::Value,
) -> (Result<ExecutionId>, Option<serde_json::Value>) {
    let tenant = CoreTenantId::from_string("u-abc123").unwrap();
    let parent_agent = make_agent("parent-worker", None, None);
    let child_agent = make_agent("child-worker", None, None);
    let mut parent_execution = make_parent_execution_with_tenant(parent_agent.id, "u-abc123");
    if let Some(contexts) = parent_contexts {
        parent_execution.input.input["contexts"] = contexts;
    }
    let (service, execution_repo) =
        build_child_spawn_service(&tenant, &parent_agent, &child_agent, &parent_execution).await;
    let started = service
        .start_child_execution(
            child_agent.id,
            input_with(serde_json::json!({ "task": "read", "contexts": child_contexts })),
            parent_execution.id,
        )
        .await;
    let persisted = match &started {
        Ok(id) => execution_repo
            .find_by_id_for_tenant(&tenant, *id)
            .await
            .unwrap()
            .and_then(|child| child.input.input.get("contexts").cloned()),
        Err(_) => None,
    };
    (started, persisted)
}

/// The refusal of a child's start whose choice for `server` is wider than
/// its parent's.
fn widens(server: &str) -> String {
    format!(
        "A run started by another run can only use connections that run chose; the connection chosen for '{server}' is not one of them."
    )
}

fn refusal(started: Result<ExecutionId>) -> String {
    match started {
        Err(e) => match e.downcast_ref::<ExecutionError>() {
            Some(ExecutionError::Refused(sentence)) => sentence.clone(),
            _ => panic!("the child's start failed, not refused: {e:#}"),
        },
        Ok(id) => panic!("the child {id} was started with a wider choice than its parent's"),
    }
}

#[tokio::test]
async fn a_child_cannot_choose_a_binding_its_parent_did_not() {
    let (started, _) = child_of(
        Some(serde_json::json!({ "nuclear-notes": CONTEXT_BINDING })),
        serde_json::json!({ "nuclear-notes": OTHER_BINDING }),
    )
    .await;
    assert_eq!(refusal(started), widens("nuclear-notes"));
}

#[tokio::test]
async fn a_child_cannot_choose_a_connection_its_parent_named_nothing_for() {
    let (started, _) = child_of(
        Some(serde_json::json!({ "nuclear-notes": CONTEXT_BINDING })),
        serde_json::json!({ "imap": MAILBOX }),
    )
    .await;
    assert_eq!(refusal(started), widens("imap"));
}

#[tokio::test]
async fn a_child_of_a_parent_that_chose_nothing_cannot_choose_anything() {
    let (started, _) = child_of(
        None,
        serde_json::json!({ "nuclear-notes": CONTEXT_BINDING }),
    )
    .await;
    assert_eq!(refusal(started), widens("nuclear-notes"));
}

#[tokio::test]
async fn a_child_whose_parent_chose_none_cannot_choose_one() {
    let (started, _) = child_of(
        Some(serde_json::json!({ "nuclear-notes": null })),
        serde_json::json!({ "nuclear-notes": CONTEXT_BINDING }),
    )
    .await;
    assert_eq!(refusal(started), widens("nuclear-notes"));
}

/// Narrowing stands: one of the parent's bindings, or `null`, is the
/// child's own choice and is kept.
#[tokio::test]
async fn a_child_may_narrow_its_parents_choice() {
    let parent = serde_json::json!({ "nuclear-notes": [CONTEXT_BINDING, OTHER_BINDING] });
    for child in [
        serde_json::json!({ "nuclear-notes": OTHER_BINDING }),
        serde_json::json!({ "nuclear-notes": null }),
        serde_json::json!({ "nuclear-notes": null, "imap": null }),
    ] {
        let (started, persisted) = child_of(Some(parent.clone()), child.clone()).await;
        started.unwrap_or_else(|e| panic!("the narrower choice {child} was refused: {e:#}"));
        assert_eq!(
            persisted,
            Some(child),
            "the child's own choice was not kept"
        );
    }
}

/// An agent state is a run its workflow execution started: its own choice
/// is refused when it names a binding the workflow did not choose.
#[tokio::test]
async fn an_agent_state_cannot_choose_a_binding_its_workflow_did_not() {
    let tenant_id = CoreTenantId::consumer();
    let agent = nuclear_notes_reader(true);
    let workflow_execution_id = uuid::Uuid::new_v4();
    let workflows =
        Arc::new(crate::infrastructure::repositories::InMemoryWorkflowExecutionRepository::new());
    let now = Utc::now();
    crate::domain::repository::WorkflowExecutionRepository::save_for_tenant(
        workflows.as_ref(),
        &tenant_id,
        &crate::domain::workflow::WorkflowExecution {
            id: crate::domain::execution::ExecutionId(workflow_execution_id),
            workflow_id: crate::domain::workflow::WorkflowId::new(),
            tenant_id: tenant_id.clone(),
            status: crate::domain::execution::ExecutionStatus::Running,
            current_state: crate::domain::workflow::StateName::new("START").unwrap(),
            blackboard: crate::domain::workflow::Blackboard::new(),
            input: serde_json::json!({
                "topic": "units",
                "contexts": { "nuclear-notes": CONTEXT_BINDING },
            }),
            state_outputs: HashMap::new(),
            final_output: None,
            started_at: now,
            last_transition_at: now,
            initiating_user_sub: Some("u-starter".to_string()),
        },
    )
    .await
    .unwrap();
    let (service, _runtime, _gw) =
        workspace_mount_service(&tenant_id, &[&agent], HashMap::new(), &[]).await;
    let service = service.with_workflow_executions(workflows);
    let mut input = workflow_step_input(VolumeId::new(), workflow_execution_id);
    if let serde_json::Value::Object(map) = &mut input.input {
        map.insert(
            "contexts".to_string(),
            serde_json::json!({ "nuclear-notes": OTHER_BINDING }),
        );
    }

    let started = service
        .start_execution(
            agent.id,
            input,
            "test-ctx".to_string(),
            Some(&temporal_worker()),
        )
        .await;
    assert_eq!(refusal(started), widens("nuclear-notes"));
}
