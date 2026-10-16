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

// ── Zaru ADR-0071 D1 (d): a repository by URL through the run's named GitHub binding ──

const GITHUB_BINDING: &str = "5c7e9a1b-4d6f-4b8a-9e0c-2f4a6c8e0b3d";

/// Start a root run naming its repository by URL with `contexts` (none when
/// `None`); answers the GitHub choice each resolution was given.
async fn github_choices_for(
    contexts: Option<serde_json::Value>,
) -> Vec<crate::domain::execution::ServerChoice> {
    let tenant_id = CoreTenantId::consumer();
    let agent = make_agent("fixer", None, None);
    let agent_repo = Arc::new(InMemoryAgentRepository::new());
    agent_repo
        .save_for_tenant(&tenant_id, &agent)
        .await
        .unwrap();
    let runtime = Arc::new(TestRuntime::default());
    let service = StandardExecutionService::new(
        agent_repo,
        Arc::new(TestVolumeService {
            volumes: HashMap::new(),
        }),
        Arc::new(Supervisor::new(runtime.clone())),
        Arc::new(InMemoryExecutionRepository::new()),
        Arc::new(EventBus::with_default_capacity()),
        Arc::new(crate::domain::node_config::NodeConfigManifest::default()),
    );
    let repositories = RecordingRunRepositories::new(None);
    service.set_repositories(repositories.clone());
    let mut input = serde_json::json!({
        "tenant_id": "zaru-consumer",
        "task": "fix the bug",
        "repositories": [{ "url": "https://github.com/100monkeys-ai/aegis-examples.git" }],
    });
    if let Some(contexts) = contexts {
        input["contexts"] = contexts;
    }
    service
        .start_execution(
            agent.id,
            input_with(input),
            "test-ctx".to_string(),
            Some(&repository_person()),
        )
        .await
        .unwrap_or_else(|e| panic!("the run did not start: {e}"));
    wait_for_spawn(runtime.as_ref()).await;
    let choices = repositories.github_choices.lock().unwrap().clone();
    choices
}

/// The root start hands the URL's resolution the GitHub binding the run
/// named, and, when it named none, nothing.
#[tokio::test]
async fn a_root_start_resolves_a_url_through_the_github_binding_it_named() {
    let github = crate::domain::credential::CredentialBindingId(
        uuid::Uuid::parse_str(GITHUB_BINDING).unwrap(),
    );
    assert_eq!(
        github_choices_for(Some(serde_json::json!({ "github": GITHUB_BINDING }))).await,
        vec![crate::domain::execution::ServerChoice::Bindings(vec![
            github
        ])]
    );
    assert_eq!(
        github_choices_for(None).await,
        vec![crate::domain::execution::ServerChoice::NotGiven]
    );
}
