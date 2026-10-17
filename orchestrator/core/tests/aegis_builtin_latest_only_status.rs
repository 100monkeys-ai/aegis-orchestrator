// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Agents and workflows are active or archived (AEGIS ADR-148;
//! Jeshua, 2026-10-16 02:4xZ and 02:48Z).
//!
//! The live lists and finds read active rows; `?status=archived|all` reads
//! the others. A tenant archives and unarchives its own rows by id, or by
//! name and version; a built-in's status is set by its deploy, never by a
//! caller; a name with several rows and no version is ambiguous. An archived
//! row is read with its versions and executions, and a new run naming it by
//! id, or by name and version, is refused with the active version's name
//! (A10). Executions roll a name up to every version's id.

use aegis_orchestrator_core::application::ports::{StartWorkflowParams, WorkflowEnginePort};
use aegis_orchestrator_core::application::register_workflow::{
    RegisterWorkflowUseCase, StandardRegisterWorkflowUseCase,
};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;
use aegis_orchestrator_core::domain::agent::{
    Agent, AgentManifest, AgentScope, AgentSpec, AgentStatus, ImagePullPolicy, ManifestMetadata,
    RuntimeConfig, TaskConfig,
};
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::repository::{
    AgentListing, AgentRepository, ArchivedTarget, StatusChange, StatusFilter,
    WorkflowExecutionRepository, WorkflowListing, WorkflowRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    WorkflowExecution, WorkflowId, WorkflowScope, WorkflowStatus,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

struct AcceptingEngine;

#[async_trait]
impl WorkflowEnginePort for AcceptingEngine {
    async fn register_workflow(&self, _def: &TemporalWorkflowDefinition) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_workflow(&self, _params: StartWorkflowParams<'_>) -> anyhow::Result<String> {
        Ok("run".to_string())
    }
}

fn engine() -> Arc<tokio::sync::RwLock<Option<Arc<dyn WorkflowEnginePort>>>> {
    Arc::new(tokio::sync::RwLock::new(Some(
        Arc::new(AcceptingEngine) as Arc<dyn WorkflowEnginePort>
    )))
}

fn workflow_yaml(name: &str, version: &str) -> String {
    format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: {name}
  version: "{version}"
spec:
  initial_state: START
  states:
    START:
      kind: System
      command: echo "start"
      transitions:
        - condition: always
          target: END
    END:
      kind: System
      command: echo "done"
      transitions: []
"#
    )
}

async fn register(
    repo: &Arc<InMemoryWorkflowRepository>,
    tenant: &TenantId,
    name: &str,
    version: &str,
    scope: WorkflowScope,
) -> WorkflowId {
    let use_case = StandardRegisterWorkflowUseCase::new(
        repo.clone(),
        engine(),
        Arc::new(EventBus::new(16)),
        Arc::new(InMemoryAgentRepository::new()),
    );
    let registered = use_case
        .register_workflow_for_tenant(tenant, &workflow_yaml(name, version), true, scope)
        .await
        .expect("the workflow registers");
    WorkflowId(uuid::Uuid::parse_str(&registered.workflow_id).unwrap())
}

fn manifest(name: &str, version: &str) -> AgentManifest {
    AgentManifest {
        api_version: "100monkeys.ai/v1".to_string(),
        kind: "Agent".to_string(),
        metadata: ManifestMetadata {
            name: name.to_string(),
            version: version.to_string(),
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
        },
        spec: AgentSpec {
            runtime: RuntimeConfig {
                language: Some("python".to_string()),
                version: Some("3.11".to_string()),
                image: None,
                image_pull_policy: ImagePullPolicy::IfNotPresent,
                isolation: "inherit".to_string(),
                model: "default".to_string(),
                temperature: None,
            },
            task: Some(TaskConfig {
                instruction: Some("noop".to_string()),
                prompt_template: None,
                input_data: None,
            }),
            context: vec![],
            contexts: Vec::new(),
            execution: None,
            security: None,
            default_schedule: None,
            tools: vec![],
            env: HashMap::new(),
            volumes: vec![],
            advanced: None,
            input_schema: None,
            security_context: None,
            output_handler: None,
            program: None,
        },
    }
}

async fn save_agent(
    repo: &InMemoryAgentRepository,
    tenant: &TenantId,
    name: &str,
    version: &str,
    scope: AgentScope,
) -> Agent {
    let mut agent = Agent::new(manifest(name, version));
    agent.scope = scope;
    agent.tenant_id = tenant.clone();
    repo.save_for_tenant(tenant, &agent).await.unwrap();
    agent
}

fn alice() -> TenantId {
    TenantId::for_consumer_user("alice").expect("a tenant for alice")
}

fn start_request(tenant: &TenantId, workflow_id: &str, version: Option<&str>) -> StartWorkflowExecutionRequest {
    StartWorkflowExecutionRequest {
        workflow_id: workflow_id.to_string(),
        input: json!({}),
        blackboard: None,
        version: version.map(str::to_string),
        tenant_id: Some(tenant.clone()),
        security_context_name: None,
        intent: None,
    }
}

/// The older global row is archived, read by its id with that status, and
/// listed by `?status=archived` and `?status=all`; the live list reads the
/// active row only.
#[tokio::test]
async fn an_archived_global_row_is_read_and_listed_by_status() {
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let system = TenantId::system();
    let old = register(&repo, &system, "builtin-flow", "1.0.0", WorkflowScope::Global).await;
    let new = register(&repo, &system, "builtin-flow", "1.1.0", WorkflowScope::Global).await;

    let read = repo.find_by_id_for_tenant(&system, old).await.unwrap().unwrap();
    assert_eq!(read.status, WorkflowStatus::Archived);
    let ids = |rows: Vec<aegis_orchestrator_core::domain::workflow::Workflow>| {
        rows.into_iter().map(|w| w.id).collect::<Vec<_>>()
    };
    assert_eq!(
        ids(repo.list_by_status(WorkflowListing::Global, StatusFilter::Active).await.unwrap()),
        vec![new]
    );
    assert_eq!(
        ids(repo.list_by_status(WorkflowListing::Global, StatusFilter::Archived).await.unwrap()),
        vec![old]
    );
    let mut all = ids(repo.list_by_status(WorkflowListing::Global, StatusFilter::All).await.unwrap());
    all.sort_by_key(|id| id.0);
    let mut both = vec![old, new];
    both.sort_by_key(|id| id.0);
    assert_eq!(all, both);
}

/// `?status` reads `active`, `archived` or `all`, `active` when absent, and
/// refuses anything else.
#[test]
fn the_status_query_reads_three_words() {
    assert_eq!(StatusFilter::parse(None), Ok(StatusFilter::Active));
    assert_eq!(StatusFilter::parse(Some("active")), Ok(StatusFilter::Active));
    assert_eq!(StatusFilter::parse(Some("archived")), Ok(StatusFilter::Archived));
    assert_eq!(StatusFilter::parse(Some("all")), Ok(StatusFilter::All));
    assert!(StatusFilter::parse(Some("paused")).is_err());
}

/// A tenant archives and unarchives its own workflow version by name and
/// version; by name alone, a name with two versions is ambiguous.
#[tokio::test]
async fn a_tenant_archives_and_unarchives_its_workflow_version() {
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let tenant = alice();
    let v1 = register(&repo, &tenant, "my-flow", "1.0.0", WorkflowScope::Tenant).await;
    register(&repo, &tenant, "my-flow", "1.1.0", WorkflowScope::Tenant).await;

    match repo
        .change_status(&tenant, "my-flow", None, WorkflowStatus::Archived)
        .await
        .unwrap()
    {
        StatusChange::Ambiguous { name, versions } => {
            assert_eq!(name, "my-flow");
            assert_eq!(versions, 2);
        }
        other => panic!("expected an ambiguous name, got {other:?}"),
    }
    match repo
        .change_status(&tenant, "my-flow", Some("1.0.0"), WorkflowStatus::Archived)
        .await
        .unwrap()
    {
        StatusChange::Changed(w) => assert_eq!((w.id, w.status), (v1, WorkflowStatus::Archived)),
        other => panic!("expected the version archived, got {other:?}"),
    }
    let live = repo.list_all_for_tenant(&tenant).await.unwrap();
    assert_eq!(live.len(), 1, "the live list leaves the archived version out");
    match repo
        .change_status(&tenant, &v1.0.to_string(), None, WorkflowStatus::Active)
        .await
        .unwrap()
    {
        StatusChange::Changed(w) => assert_eq!(w.status, WorkflowStatus::Active),
        other => panic!("expected the version unarchived, got {other:?}"),
    }
    assert_eq!(repo.list_all_for_tenant(&tenant).await.unwrap().len(), 2);
    assert!(matches!(
        repo.change_status(&tenant, "no-such-flow", None, WorkflowStatus::Archived)
            .await
            .unwrap(),
        StatusChange::NotFound
    ));
}

/// A built-in's status is set by its deploy: a caller asking to change it is
/// answered `Global`, and nothing changes.
#[tokio::test]
async fn a_builtin_status_is_not_changed_by_a_caller() {
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let system = TenantId::system();
    let id = register(&repo, &system, "builtin-flow", "1.0.0", WorkflowScope::Global).await;
    assert!(matches!(
        repo.change_status(&system, &id.0.to_string(), None, WorkflowStatus::Archived)
            .await
            .unwrap(),
        StatusChange::Global
    ));
    let read = repo.find_by_id_for_tenant(&system, id).await.unwrap().unwrap();
    assert_eq!(read.status, WorkflowStatus::Active);

    let agents = InMemoryAgentRepository::new();
    let builtin = save_agent(&agents, &system, "builtin-agent", "1.0.0", AgentScope::Global).await;
    assert!(matches!(
        agents
            .change_status(&system, &builtin.id.0.to_string(), None, AgentStatus::Archived)
            .await
            .unwrap(),
        StatusChange::Global
    ));
}

/// A tenant archives its own agent version; the live list and the name
/// resolution leave it out, and `?status=archived` reads it.
#[tokio::test]
async fn a_tenant_archives_its_agent_version() {
    let agents = InMemoryAgentRepository::new();
    let tenant = alice();
    let v1 = save_agent(&agents, &tenant, "alpha", "1.0.0", AgentScope::Tenant).await;
    let v2 = save_agent(&agents, &tenant, "alpha", "1.1.0", AgentScope::Tenant).await;

    assert!(matches!(
        agents
            .change_status(&tenant, "alpha", None, AgentStatus::Archived)
            .await
            .unwrap(),
        StatusChange::Ambiguous { versions: 2, .. }
    ));
    match agents
        .change_status(&tenant, "alpha", Some("1.1.0"), AgentStatus::Archived)
        .await
        .unwrap()
    {
        StatusChange::Changed(a) => assert_eq!((a.id, a.status), (v2.id, AgentStatus::Archived)),
        other => panic!("expected the version archived, got {other:?}"),
    }
    let resolved = agents.resolve_by_name(&tenant, "alpha").await.unwrap().unwrap();
    assert_eq!(resolved.id, v1.id, "the name resolves to the active version");
    let archived: Vec<_> = agents
        .list_by_status(AgentListing::Visible(&tenant), StatusFilter::Archived)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(archived, vec![v2.id]);
    let versions = agents.list_by_name_for_tenant(&tenant, "alpha").await.unwrap();
    assert_eq!(versions.len(), 2, "an archived version is still one of the name's versions");
}

/// A new run naming an archived workflow by its id, or by name and version,
/// is refused with the name's active version (A10); by name alone it runs
/// the active version.
#[tokio::test]
async fn a_run_naming_an_archived_workflow_is_refused_with_the_active_version() {
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let system = TenantId::system();
    let old = register(&repo, &system, "builtin-flow", "1.0.0", WorkflowScope::Global).await;
    register(&repo, &system, "builtin-flow", "1.1.0", WorkflowScope::Global).await;
    let use_case = StandardStartWorkflowExecutionUseCase::new(
        repo.clone(),
        Arc::new(InMemoryWorkflowExecutionRepository::new()),
        engine(),
        Arc::new(EventBus::new(16)),
    );
    let tenant = alice();

    for request in [
        start_request(&tenant, &old.0.to_string(), None),
        start_request(&tenant, "builtin-flow", Some("1.0.0")),
    ] {
        let err = use_case
            .start_execution_for_tenant(&tenant, request, None)
            .await
            .expect_err("a run of an archived row is refused");
        let refusal = err
            .downcast_ref::<ArchivedTarget>()
            .unwrap_or_else(|| panic!("expected the archived refusal, got: {err:#}"));
        assert_eq!(refusal.active_version.as_deref(), Some("1.1.0"));
        assert_eq!(
            refusal.to_string(),
            "workflow 'builtin-flow' version '1.0.0' is archived; the active version is '1.1.0'"
        );
    }
    use_case
        .start_execution_for_tenant(&tenant, start_request(&tenant, "builtin-flow", None), None)
        .await
        .expect("by name alone the active version runs");
}

/// An archived agent named for a new run is refused with the active version.
#[test]
fn an_archived_agent_refusal_names_the_active_version() {
    let mut agent = Agent::new(manifest("alpha", "1.0.0"));
    agent.status = AgentStatus::Archived;
    let refusal = ArchivedTarget::agent(&agent, Some("1.1.0".to_string()));
    assert_eq!(
        refusal.to_string(),
        "agent 'alpha' version '1.0.0' is archived; the active version is '1.1.0'"
    );
    let none = ArchivedTarget::agent(&agent, None);
    assert_eq!(
        none.to_string(),
        "agent 'alpha' version '1.0.0' is archived; no version of 'alpha' is active"
    );
}

/// A name's executions are every version's: the ids of the name roll up the
/// archived version with the active one, and the count sums them.
#[tokio::test]
async fn a_names_executions_roll_up_every_version() {
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let system = TenantId::system();
    let old = register(&repo, &system, "builtin-flow", "1.0.0", WorkflowScope::Global).await;
    let new = register(&repo, &system, "builtin-flow", "1.1.0", WorkflowScope::Global).await;

    let mut ids = repo.version_ids_visible(&alice(), "builtin-flow").await.unwrap();
    ids.sort_by_key(|id| id.0);
    let mut both = vec![old, new];
    both.sort_by_key(|id| id.0);
    assert_eq!(ids, both, "the name rolls up to every version's id");

    let executions = InMemoryWorkflowExecutionRepository::new();
    for id in [old, old, new] {
        let workflow = repo.find_by_id_for_tenant(&system, id).await.unwrap().unwrap();
        let mut execution = WorkflowExecution::new(&workflow, ExecutionId::new(), json!({}));
        execution.tenant_id = system.clone();
        executions.save_for_tenant(&system, &execution).await.unwrap();
    }
    assert_eq!(
        executions.count_by_workflows_for_tenant(&system, &ids).await.unwrap(),
        3
    );
}
