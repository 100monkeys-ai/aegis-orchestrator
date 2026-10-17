// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! At every start, each built-in name keeps one active row: the row of the
//! template's version (AEGIS ADR-148). The step runs whether or not
//! the drift gate deploys the built-in, so a node whose built-ins are up to
//! date still archives the older rows an earlier version left. A name with
//! no row of the template's version, as when its deploy failed, keeps what
//! it has. Nothing is deleted.

use aegis_orchestrator::commands::builtins::{BUILTIN_AGENTS, BUILTIN_WORKFLOWS};
use aegis_orchestrator::daemon::server::archive_superseded_builtins;
use aegis_orchestrator_core::domain::agent::{Agent, AgentManifest, AgentScope, AgentStatus};
use aegis_orchestrator_core::domain::repository::{AgentRepository, WorkflowRepository};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    Workflow, WorkflowId, WorkflowScope, WorkflowStatus,
};
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryWorkflowRepository,
};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

fn global_workflow(template: &str, version: Option<&str>, days_ago: i64) -> Workflow {
    let mut workflow = WorkflowParser::parse_yaml(template).expect("the template parses");
    workflow.id = WorkflowId::new();
    workflow.scope = WorkflowScope::Global;
    workflow.tenant_id = TenantId::system();
    if let Some(version) = version {
        workflow.metadata.version = Some(version.to_string());
    }
    workflow.created_at -= chrono::Duration::days(days_ago);
    workflow
}

fn global_agent(template: &str, version: Option<&str>, days_ago: i64) -> Agent {
    let mut manifest: AgentManifest = serde_yaml::from_str(template).expect("the template parses");
    if let Some(version) = version {
        manifest.metadata.version = version.to_string();
    }
    let mut agent = Agent::new(manifest);
    agent.scope = AgentScope::Global;
    agent.tenant_id = TenantId::system();
    agent.created_at -= chrono::Duration::days(days_ago);
    agent
}

/// Each built-in name with an older global row beside the row of the
/// template's version: the older row is archived, the template's stays
/// active, and both are still read.
#[tokio::test]
async fn the_start_archives_every_older_builtin_row() {
    let system = TenantId::system();
    let workflows = InMemoryWorkflowRepository::new();
    let agents = InMemoryAgentRepository::new();
    let (_, wf_template) = BUILTIN_WORKFLOWS[0];
    let (_, agent_template) = BUILTIN_AGENTS[0];

    let older_wf = global_workflow(wf_template, Some("0.0.1"), 2);
    let current_wf = global_workflow(wf_template, None, 1);
    workflows.save_for_tenant(&system, &older_wf).await.unwrap();
    workflows
        .save_for_tenant(&system, &current_wf)
        .await
        .unwrap();
    let older_agent = global_agent(agent_template, Some("0.0.1"), 2);
    let current_agent = global_agent(agent_template, None, 1);
    agents.save_for_tenant(&system, &older_agent).await.unwrap();
    agents
        .save_for_tenant(&system, &current_agent)
        .await
        .unwrap();

    let outcome = archive_superseded_builtins(&workflows, &agents).await;
    assert_eq!(
        outcome.archived, 2,
        "one older workflow row and one older agent row"
    );
    assert_eq!(outcome.failed, 0);

    let read = |id| workflows.find_by_id_for_tenant(&system, id);
    assert_eq!(
        read(older_wf.id).await.unwrap().unwrap().status,
        WorkflowStatus::Archived
    );
    assert_eq!(
        read(current_wf.id).await.unwrap().unwrap().status,
        WorkflowStatus::Active
    );
    let read_agent = |id| agents.find_by_id_for_tenant(&system, id);
    assert_eq!(
        read_agent(older_agent.id).await.unwrap().unwrap().status,
        AgentStatus::Archived
    );
    assert_eq!(
        read_agent(current_agent.id).await.unwrap().unwrap().status,
        AgentStatus::Active
    );

    let again = archive_superseded_builtins(&workflows, &agents).await;
    assert_eq!(again.archived, 0, "a second start archives nothing more");
}

/// A built-in name with no row of the template's version keeps its live row.
#[tokio::test]
async fn a_name_without_the_templates_version_keeps_its_live_row() {
    let system = TenantId::system();
    let workflows = InMemoryWorkflowRepository::new();
    let agents = InMemoryAgentRepository::new();
    let (_, wf_template) = BUILTIN_WORKFLOWS[0];
    let only = global_workflow(wf_template, Some("0.0.1"), 1);
    workflows.save_for_tenant(&system, &only).await.unwrap();

    let outcome = archive_superseded_builtins(&workflows, &agents).await;
    assert_eq!(outcome.archived, 0);
    let read = workflows
        .find_by_id_for_tenant(&system, only.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.status, WorkflowStatus::Active);
}
