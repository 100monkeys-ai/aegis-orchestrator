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

fn resolve_stack_root(config_path: Option<&std::path::PathBuf>) -> std::path::PathBuf {
    config_path
        .and_then(|config| config.parent().map(|parent| parent.to_path_buf()))
        .or_else(|| dirs_next::home_dir().map(|home| home.join(".aegis")))
        .unwrap_or_else(|| std::path::PathBuf::from(".aegis"))
}

pub fn resolve_templates_root(config_path: Option<&std::path::PathBuf>) -> std::path::PathBuf {
    resolve_stack_root(config_path).join("templates")
}

pub fn resolve_generated_root(config_path: Option<&std::path::PathBuf>) -> std::path::PathBuf {
    resolve_stack_root(config_path).join("generated")
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
    let manifest: aegis_orchestrator_sdk::AgentManifest =
        serde_yaml::from_str(template_yaml).context("Failed to parse built-in template YAML")?;
    manifest
        .validate()
        .map_err(|e| anyhow::anyhow!("Built-in template validation failed: {e}"))?;

    if !force {
        if let Some(id) = client.lookup_agent(&manifest.metadata.name).await? {
            println!(
                "  {} Reusing existing built-in agent '{}' (id: {}) without verifying version; \
                 use --force to redeploy from the bundled template if needed.",
                "→".dimmed(),
                manifest.metadata.name.cyan(),
                id
            );
            return Ok(id);
        }
    }

    println!(
        "  {} Deploying built-in agent: {}",
        "→".dimmed(),
        name.cyan()
    );

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
        println!(
            "  {} Reusing existing built-in workflow '{}' without verifying version; \
             use --force to redeploy from the bundled template if needed.",
            "→".dimmed(),
            name.cyan(),
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_generator_template_uses_nested_output_fields() {
        let workflow: serde_yaml::Value =
            serde_yaml::from_str(WORKFLOW_GENERATOR_WORKFLOW_TEMPLATE).unwrap();
        let states = &workflow["spec"]["states"];

        let generate_missing_agents_input = states["GENERATE_MISSING_AGENTS"]["input"]
            .as_str()
            .expect("GENERATE_MISSING_AGENTS input should be a string");
        assert!(
            generate_missing_agents_input.contains("{{PLAN.output}}"),
            "GENERATE_MISSING_AGENTS should reference the planner state's output payload"
        );
        assert!(
            generate_missing_agents_input.contains("missing_judge_agents"),
            "GENERATE_MISSING_AGENTS should mention missing judge agents"
        );

        let generate_and_register_workflow_input = states["GENERATE_AND_REGISTER_WORKFLOW"]
            ["input"]
            .as_str()
            .expect("GENERATE_AND_REGISTER_WORKFLOW input should be a string");
        assert!(
            generate_and_register_workflow_input.contains("{{PLAN.output}}"),
            "GENERATE_AND_REGISTER_WORKFLOW should reference the planner state's output payload"
        );
        assert!(
            generate_and_register_workflow_input.contains("{{GENERATE_MISSING_AGENTS.output}}"),
            "GENERATE_AND_REGISTER_WORKFLOW should reference the agent generation state's output payload"
        );
        assert!(
            generate_and_register_workflow_input.contains("resolved existing judge agents"),
            "GENERATE_AND_REGISTER_WORKFLOW should require resolved judge agents"
        );
    }

    #[test]
    fn agent_creator_template_requires_full_sequence_before_deploying() {
        assert!(
            AGENT_GENERATOR_AGENT_TEMPLATE.contains("HARD PRECONDITION"),
            "agent creator template should make the required sequence a hard precondition"
        );
        assert!(
            AGENT_GENERATOR_AGENT_TEMPLATE.contains(
                "Do not call `aegis.agent.create` or `aegis.agent.update` from a draft manifest"
            ),
            "agent creator template should forbid deployment from unvalidated drafts"
        );
        assert!(
            AGENT_GENERATOR_AGENT_TEMPLATE
                .contains("Skipping, reordering, or collapsing steps 1–4 is a policy violation"),
            "agent creator template should reject shortcutting the required workflow"
        );
    }

    #[test]
    fn workflow_creator_template_requires_full_sequence_before_deploying() {
        assert!(
            WORKFLOW_CREATOR_AGENT_TEMPLATE.contains("HARD PRECONDITION"),
            "workflow creator template should make the required sequence a hard precondition"
        );
        assert!(
            WORKFLOW_CREATOR_AGENT_TEMPLATE.contains(
                "Do not call `aegis.workflow.create` or `aegis.workflow.update` from a draft manifest"
            ),
            "workflow creator template should forbid deployment from unvalidated drafts"
        );
        assert!(
            WORKFLOW_CREATOR_AGENT_TEMPLATE
                .contains("Skipping, reordering, or collapsing steps 1–5 is a policy violation"),
            "workflow creator template should reject shortcutting the required workflow"
        );
        assert!(
            WORKFLOW_CREATOR_AGENT_TEMPLATE
                .contains("Always run workflow validation before any create or update call"),
            "workflow creator template should require workflow validation before deployment"
        );
    }

    #[test]
    fn workflow_generator_planner_template_includes_judge_dependencies() {
        assert!(
            WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE.contains("required_judge_agents"),
            "planner template should explicitly model required judge dependencies"
        );
        assert!(
            WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE.contains("missing_judge_agents"),
            "planner template should explicitly model missing judge dependencies"
        );
        assert!(
            WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE.contains("judge_generation_prompts"),
            "planner template should provide prompts for missing judges"
        );
    }

    #[test]
    fn workflow_generator_validator_template_uses_resolved_judge_agents() {
        assert!(
            WORKFLOW_CREATOR_AGENT_TEMPLATE.contains("Resolve judge names"),
            "workflow creator template should require resolved judge names"
        );
        assert!(
            WORKFLOW_CREATOR_AGENT_TEMPLATE.contains("judge_agents"),
            "workflow creator template should still pass explicit judge agents to workflow.create"
        );
    }

    #[test]
    fn workflow_generator_judge_template_handles_missing_audit_history_conservatively() {
        assert!(
            WORKFLOW_GENERATOR_JUDGE_TEMPLATE
                .contains("If tool call history is missing, treat that as insufficient evidence"),
            "judge template should not claim validation failure without audit evidence"
        );
        assert!(
            WORKFLOW_GENERATOR_JUDGE_TEMPLATE.contains(
                "A deployment call without a prior `aegis.schema.get`, `aegis.schema.validate`, and `aegis.workflow.validate` in the same run"
            ),
            "judge template should treat a missing workflow validation sequence as a hard process failure"
        );
    }

    #[test]
    fn agent_generator_judge_template_treats_missing_schema_history_as_hard_failure() {
        assert!(
            AGENT_GENERATOR_JUDGE_TEMPLATE.contains(
                "A deployment call without a prior `aegis.schema.get` and `aegis.schema.validate` in the same run"
            ),
            "agent judge template should treat a missing schema sequence as a hard process failure"
        );
    }

    #[test]
    fn workflow_generator_template_remains_three_stage() {
        let workflow: serde_yaml::Value =
            serde_yaml::from_str(WORKFLOW_GENERATOR_WORKFLOW_TEMPLATE).unwrap();
        let states = workflow["spec"]["states"]
            .as_mapping()
            .expect("workflow states should be a mapping");

        assert!(states.contains_key(&serde_yaml::Value::from("PLAN")));
        assert!(states.contains_key(&serde_yaml::Value::from("GENERATE_MISSING_AGENTS")));
        assert!(states.contains_key(&serde_yaml::Value::from("GENERATE_AND_REGISTER_WORKFLOW")));
    }

    #[test]
    fn generated_root_uses_config_parent_directory() {
        let config_path = std::path::PathBuf::from("/tmp/custom-stack/aegis-config.yaml");
        let generated_root = resolve_generated_root(Some(&config_path));
        assert_eq!(
            generated_root,
            std::path::PathBuf::from("/tmp/custom-stack/generated")
        );
    }
}
