// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A new version of a built-in leaves one live row of its name, and a new
//! version of a tenant's agent or workflow leaves the old one live (AEGIS
//! ADR-148; Jeshua, 2026-10-16 02:34Z and 02:48Z).
//!
//! A global (built-in) name keeps one active row: the version saved last.
//! Every older global row of the name is archived, never deleted: it stays
//! readable by its id with its versions and executions. A tenant's new
//! version is a new row beside the old one, and both stay active.

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::lifecycle::StandardAgentLifecycleService;
use aegis_orchestrator_core::application::ports::{StartWorkflowParams, WorkflowEnginePort};
use aegis_orchestrator_core::application::register_workflow::{
    RegisterWorkflowUseCase, StandardRegisterWorkflowUseCase,
};
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;
use aegis_orchestrator_core::domain::agent::{
    Agent, AgentId, AgentManifest, AgentScope, AgentSpec, AgentStatus, ImagePullPolicy,
    ManifestMetadata, RuntimeConfig, TaskConfig,
};
use aegis_orchestrator_core::domain::repository::{AgentRepository, WorkflowRepository};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{WorkflowId, WorkflowScope};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryWorkflowRepository,
};
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use async_trait::async_trait;
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

fn register_use_case(repo: Arc<InMemoryWorkflowRepository>) -> StandardRegisterWorkflowUseCase {
    StandardRegisterWorkflowUseCase::new(
        repo,
        Arc::new(tokio::sync::RwLock::new(Some(
            Arc::new(AcceptingEngine) as Arc<dyn WorkflowEnginePort>
        ))),
        Arc::new(EventBus::new(16)),
        Arc::new(InMemoryAgentRepository::new()),
    )
}

async fn register(
    use_case: &StandardRegisterWorkflowUseCase,
    tenant: &TenantId,
    name: &str,
    version: &str,
    scope: WorkflowScope,
) -> WorkflowId {
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

fn agent_service(repo: Arc<InMemoryAgentRepository>) -> StandardAgentLifecycleService {
    StandardAgentLifecycleService::new(
        repo,
        Arc::new(EventBus::new(16)),
        Arc::new(InMemorySecurityContextRepository::new()),
    )
}

fn alice() -> TenantId {
    TenantId::for_consumer_user("alice").expect("a tenant for alice")
}

/// A second version of a built-in workflow leaves one live global row of its
/// name, the new one; the older row is still read by its id.
#[tokio::test]
async fn a_new_global_workflow_version_leaves_one_live_global_row() {
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let use_case = register_use_case(repo.clone());
    let system = TenantId::system();
    let old = register(
        &use_case,
        &system,
        "builtin-flow",
        "1.0.0",
        WorkflowScope::Global,
    )
    .await;
    let new = register(
        &use_case,
        &system,
        "builtin-flow",
        "1.1.0",
        WorkflowScope::Global,
    )
    .await;

    let live: Vec<_> = repo
        .list_global()
        .await
        .unwrap()
        .into_iter()
        .filter(|w| w.metadata.name == "builtin-flow")
        .map(|w| (w.id, w.metadata.version.clone()))
        .collect();
    assert_eq!(
        live,
        vec![(new, Some("1.1.0".to_string()))],
        "a built-in name keeps one live global row, the version saved last"
    );
    assert!(
        repo.find_by_id_for_tenant(&system, old)
            .await
            .unwrap()
            .is_some(),
        "the older global row is archived, never deleted"
    );
}

/// The live row of a built-in name is the one saved last, never the greatest
/// version text: "2.0.10" is newer than "2.0.9" though it sorts before it.
#[tokio::test]
async fn the_live_global_row_is_the_one_saved_last_not_the_greatest_version_text() {
    let repo = Arc::new(InMemoryWorkflowRepository::new());
    let use_case = register_use_case(repo.clone());
    let system = TenantId::system();
    register(
        &use_case,
        &system,
        "builtin-flow",
        "2.0.9",
        WorkflowScope::Global,
    )
    .await;
    let newest = register(
        &use_case,
        &system,
        "builtin-flow",
        "2.0.10",
        WorkflowScope::Global,
    )
    .await;

    let found = repo
        .find_by_name_for_tenant(&system, "builtin-flow")
        .await
        .unwrap()
        .expect("the name resolves");
    assert_eq!(
        (found.id, found.metadata.version.as_deref()),
        (newest, Some("2.0.10")),
        "the name resolves to the row saved last"
    );
}

/// A tenant's new agent version is a new row; the old version stays live and
/// keeps its id, and force on the same version overwrites in place.
#[tokio::test]
async fn a_new_tenant_agent_version_is_a_new_row_and_the_old_stays_live() {
    let repo = Arc::new(InMemoryAgentRepository::new());
    let service = agent_service(repo.clone());
    let tenant = alice();
    let v1 = service
        .deploy_agent_for_tenant(
            &tenant,
            manifest("alpha", "1.0.0"),
            false,
            AgentScope::Tenant,
            None,
        )
        .await
        .expect("v1 deploys");
    let v2 = service
        .deploy_agent_for_tenant(
            &tenant,
            manifest("alpha", "1.1.0"),
            false,
            AgentScope::Tenant,
            None,
        )
        .await
        .expect("v2 deploys");

    assert_ne!(v1, v2, "a new tenant agent version is a new agent row");
    let old = repo
        .find_by_id_for_tenant(&tenant, v1)
        .await
        .unwrap()
        .expect("the old row is kept");
    assert_eq!(old.manifest.metadata.version, "1.0.0");
    assert_eq!(
        old.status,
        AgentStatus::Active,
        "the old version stays active"
    );

    let again = service
        .deploy_agent_for_tenant(
            &tenant,
            manifest("alpha", "1.0.0"),
            true,
            AgentScope::Tenant,
            None,
        )
        .await
        .expect("force on the same version deploys");
    assert_eq!(again, v1, "force on the same version overwrites in place");
}

/// A built-in agent deployed at a new version leaves one live global row of
/// its name. The deploy updates the newest global row in place, so the
/// workflows that name its id keep it; an older global row of the name, as
/// an earlier deploy left it, is archived and still read by its id.
#[tokio::test]
async fn a_global_agent_deploy_leaves_one_live_global_row() {
    let repo = Arc::new(InMemoryAgentRepository::new());
    let system = TenantId::system();
    let mut ids: Vec<AgentId> = Vec::new();
    for (version, days) in [("0.8.0", 2), ("0.9.0", 1)] {
        let mut row = Agent::new(manifest("beta", version));
        row.scope = AgentScope::Global;
        row.tenant_id = system.clone();
        row.created_at -= chrono::Duration::days(days);
        repo.save_for_tenant(&system, &row).await.unwrap();
        ids.push(row.id);
    }

    let service = agent_service(repo.clone());
    let deployed = service
        .deploy_agent_for_tenant(
            &system,
            manifest("beta", "1.0.0"),
            true,
            AgentScope::Global,
            None,
        )
        .await
        .expect("the built-in deploys");

    let live: Vec<(AgentId, String)> = repo
        .list_visible_for_tenant(&alice())
        .await
        .unwrap()
        .into_iter()
        .filter(|a| a.name == "beta")
        .map(|a| (a.id, a.manifest.metadata.version))
        .collect();
    assert_eq!(
        live,
        vec![(deployed, "1.0.0".to_string())],
        "one live global row of the name"
    );
    assert_eq!(
        deployed, ids[1],
        "the newest global row is updated in place"
    );
    let archived = repo
        .find_by_id_for_tenant(&system, ids[0])
        .await
        .unwrap()
        .expect("the older row is kept");
    assert_eq!(
        archived.status,
        AgentStatus::Archived,
        "the older row is archived"
    );
}
