// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Built-in templates and deployment logic.

use anyhow::{Context, Result};
use colored::Colorize;
use uuid::Uuid;

use crate::daemon::DaemonClient;

// ─── Templates ──────────────────────────────────────────────────────────────

pub const HELLO_WORLD_TEMPLATE: &str =
    include_str!("../../templates/agents/hello-world-agent.yaml");
pub const CODE_QUALITY_JUDGE_TEMPLATE: &str =
    include_str!("../../templates/agents/code-quality-judge.yaml");
pub const TOOL_CALL_POLICY_JUDGE_TEMPLATE: &str =
    include_str!("../../templates/agents/tool-call-policy-judge.yaml");

pub const WORKFLOW_GENERATOR_WORKFLOW_NAME: &str = "builtin-workflow-generator";
pub const WORKFLOW_GENERATOR_WORKFLOW_TEMPLATE: &str =
    include_str!("../../templates/workflows/builtin-workflow-generator.yaml");

pub const WORKFLOW_GENERATOR_PLANNER_AGENT_NAME: &str = "workflow-generator-planner-agent";
pub const WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/workflow-generator-planner-agent.yaml");

pub const AGENT_GENERATOR_AGENT_NAME: &str = "agent-creator-agent";
pub const AGENT_GENERATOR_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/agent-creator-agent.yaml");

pub const AGENT_GENERATOR_JUDGE_NAME: &str = "agent-generator-judge";
pub const AGENT_GENERATOR_JUDGE_TEMPLATE: &str =
    include_str!("../../templates/agents/agent-generator-judge.yaml");

pub const WORKFLOW_GENERATOR_JUDGE_NAME: &str = "workflow-generator-judge";
pub const WORKFLOW_GENERATOR_JUDGE_TEMPLATE: &str =
    include_str!("../../templates/agents/workflow-generator-judge.yaml");

pub const WORKFLOW_CREATOR_AGENT_NAME: &str = "workflow-creator-validator-agent";
pub const WORKFLOW_CREATOR_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/workflow-creator-validator-agent.yaml");

// ─── Deployment Logic ───────────────────────────────────────────────────────

pub async fn deploy_all_builtins(client: &DaemonClient, force: bool) -> Result<()> {
    // Agents
    deploy_builtin_agent(client, "hello-world-agent", HELLO_WORLD_TEMPLATE, force).await?;
    deploy_builtin_agent(
        client,
        "code-quality-judge",
        CODE_QUALITY_JUDGE_TEMPLATE,
        force,
    )
    .await?;
    deploy_builtin_agent(
        client,
        "tool-call-policy-judge",
        TOOL_CALL_POLICY_JUDGE_TEMPLATE,
        force,
    )
    .await?;
    deploy_builtin_agent(
        client,
        WORKFLOW_GENERATOR_PLANNER_AGENT_NAME,
        WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE,
        force,
    )
    .await?;
    deploy_builtin_agent(
        client,
        AGENT_GENERATOR_AGENT_NAME,
        AGENT_GENERATOR_AGENT_TEMPLATE,
        force,
    )
    .await?;
    deploy_builtin_agent(
        client,
        AGENT_GENERATOR_JUDGE_NAME,
        AGENT_GENERATOR_JUDGE_TEMPLATE,
        force,
    )
    .await?;
    deploy_builtin_agent(
        client,
        WORKFLOW_GENERATOR_JUDGE_NAME,
        WORKFLOW_GENERATOR_JUDGE_TEMPLATE,
        force,
    )
    .await?;
    deploy_builtin_agent(
        client,
        WORKFLOW_CREATOR_AGENT_NAME,
        WORKFLOW_CREATOR_AGENT_TEMPLATE,
        force,
    )
    .await?;

    // Workflows
    deploy_builtin_workflow(
        client,
        WORKFLOW_GENERATOR_WORKFLOW_NAME,
        WORKFLOW_GENERATOR_WORKFLOW_TEMPLATE,
        force,
    )
    .await?;

    Ok(())
}

pub fn resolve_templates_root(config_path: Option<&std::path::PathBuf>) -> std::path::PathBuf {
    let base_dir = config_path
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .or_else(|| dirs_next::home_dir().map(|h| h.join(".aegis")))
        .unwrap_or_else(|| std::path::PathBuf::from(".aegis"));
    base_dir.join("templates")
}

pub fn sync_generator_templates_to_disk(templates_root: &std::path::Path) -> Result<()> {
    persist_template(
        templates_root,
        "agents",
        "workflow-generator-planner-agent.yaml",
        WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "agents",
        "agent-creator-agent.yaml",
        AGENT_GENERATOR_AGENT_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "agents",
        "agent-generator-judge.yaml",
        AGENT_GENERATOR_JUDGE_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "agents",
        "workflow-generator-judge.yaml",
        WORKFLOW_GENERATOR_JUDGE_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "agents",
        "workflow-creator-validator-agent.yaml",
        WORKFLOW_CREATOR_AGENT_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "agents",
        "hello-world-agent.yaml",
        HELLO_WORLD_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "agents",
        "code-quality-judge.yaml",
        CODE_QUALITY_JUDGE_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "agents",
        "tool-call-policy-judge.yaml",
        TOOL_CALL_POLICY_JUDGE_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "workflows",
        "builtin-workflow-generator.yaml",
        WORKFLOW_GENERATOR_WORKFLOW_TEMPLATE,
    )?;
    Ok(())
}

fn persist_template(
    templates_root: &std::path::Path,
    category: &str,
    file_name: &str,
    content: &str,
) -> Result<()> {
    let dir = templates_root.join(category);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create template directory {}", dir.display()))?;
    let file_path = dir.join(file_name);
    std::fs::write(&file_path, content)
        .with_context(|| format!("Failed to write bundled template {}", file_path.display()))?;
    Ok(())
}

async fn deploy_builtin_agent(
    client: &DaemonClient,
    name: &str,
    template_yaml: &str,
    force: bool,
) -> Result<Uuid> {
    if !force {
        if let Some(id) = client.lookup_agent(name).await? {
            return Ok(id);
        }
    }

    println!(
        "  {} Deploying built-in agent: {}",
        "→".dimmed(),
        name.cyan()
    );

    let manifest: aegis_orchestrator_sdk::AgentManifest =
        serde_yaml::from_str(template_yaml).context("Failed to parse built-in template YAML")?;
    manifest
        .validate()
        .map_err(|e| anyhow::anyhow!("Built-in template validation failed: {e}"))?;

    client
        .deploy_agent(manifest, force)
        .await
        .context("Failed to deploy built-in template")
}

async fn deploy_builtin_workflow(
    client: &DaemonClient,
    name: &str,
    template_yaml: &str,
    force: bool,
) -> Result<()> {
    if !force && client.describe_workflow(name).await.is_ok() {
        return Ok(());
    }

    println!(
        "  {} Deploying built-in workflow: {}",
        "→".dimmed(),
        name.cyan()
    );

    // Validate template before deployment.
    let workflow =
        aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
            template_yaml,
        )
        .context("Failed to parse built-in workflow template YAML")?;
    aegis_orchestrator_core::domain::workflow::WorkflowValidator::check_for_cycles(&workflow)
        .context("Built-in workflow template failed cycle validation")?;

    client
        .deploy_workflow_manifest_with_force(template_yaml, force)
        .await
        .context("Failed to deploy built-in workflow template")
}
