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

/// The built-in judge of every goal (AEGIS ADR-131 D3).
pub const GOAL_JUDGE_NAME: &str = "goal-judge";
pub const GOAL_JUDGE_TEMPLATE: &str = include_str!("../../templates/agents/goal-judge.yaml");

pub const WORKFLOW_CREATOR_AGENT_NAME: &str = "workflow-creator-validator-agent";
pub const WORKFLOW_CREATOR_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/workflow-creator-validator-agent.yaml");

pub const SKILL_VALIDATOR_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/skill-validator.yaml");
pub const SKILL_IMPORT_WORKFLOW_TEMPLATE: &str =
    include_str!("../../templates/workflows/skill-import.yaml");

pub const INTENT_EXECUTION_WORKFLOW_NAME: &str = "builtin-intent-to-execution";
pub const INTENT_EXECUTION_WORKFLOW_TEMPLATE: &str =
    include_str!("../../templates/workflows/builtin-intent-to-execution.yaml");

pub const AEGIS_OUTPUT_FORMATTER_AGENT_NAME: &str = "aegis-output-formatter-agent";
pub const AEGIS_OUTPUT_FORMATTER_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/aegis-output-formatter-agent.yaml");

pub const AEGIS_CODE_VALIDATOR_AGENT_NAME: &str = "aegis-code-validator-agent";
pub const AEGIS_CODE_VALIDATOR_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/aegis-code-validator-agent.yaml");

pub const AEGIS_PYTHON_EXECUTOR_AGENT_NAME: &str = "aegis-python-executor-agent";
pub const AEGIS_PYTHON_EXECUTOR_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/aegis-python-executor-agent.yaml");

pub const AEGIS_JAVASCRIPT_EXECUTOR_AGENT_NAME: &str = "aegis-javascript-executor-agent";
pub const AEGIS_JAVASCRIPT_EXECUTOR_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/aegis-javascript-executor-agent.yaml");

pub const AEGIS_BASH_EXECUTOR_AGENT_NAME: &str = "aegis-bash-executor-agent";
pub const AEGIS_BASH_EXECUTOR_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/aegis-bash-executor-agent.yaml");

pub const AEGIS_DOCUMENT_RENDERER_AGENT_NAME: &str = "aegis-document-renderer-agent";
pub const AEGIS_DOCUMENT_RENDERER_AGENT_TEMPLATE: &str =
    include_str!("../../templates/agents/aegis-document-renderer-agent.yaml");

// ─── Canonical Registries ───────────────────────────────────────────────────

struct BuiltinTemplateSpec {
    category: &'static str,
    file_name: &'static str,
    content: &'static str,
}

/// Canonical registry of all built-in agent templates for deployment.
pub const BUILTIN_AGENTS: &[(&str, &str)] = &[
    ("hello-world-agent", HELLO_WORLD_TEMPLATE),
    ("code-quality-judge", CODE_QUALITY_JUDGE_TEMPLATE),
    ("tool-call-policy-judge", TOOL_CALL_POLICY_JUDGE_TEMPLATE),
    (
        WORKFLOW_GENERATOR_PLANNER_AGENT_NAME,
        WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE,
    ),
    (AGENT_GENERATOR_AGENT_NAME, AGENT_GENERATOR_AGENT_TEMPLATE),
    (AGENT_GENERATOR_JUDGE_NAME, AGENT_GENERATOR_JUDGE_TEMPLATE),
    (
        WORKFLOW_GENERATOR_JUDGE_NAME,
        WORKFLOW_GENERATOR_JUDGE_TEMPLATE,
    ),
    (WORKFLOW_CREATOR_AGENT_NAME, WORKFLOW_CREATOR_AGENT_TEMPLATE),
    (GOAL_JUDGE_NAME, GOAL_JUDGE_TEMPLATE),
    ("skill-validator", SKILL_VALIDATOR_AGENT_TEMPLATE),
    (
        AEGIS_OUTPUT_FORMATTER_AGENT_NAME,
        AEGIS_OUTPUT_FORMATTER_AGENT_TEMPLATE,
    ),
    (
        AEGIS_CODE_VALIDATOR_AGENT_NAME,
        AEGIS_CODE_VALIDATOR_AGENT_TEMPLATE,
    ),
    (
        AEGIS_PYTHON_EXECUTOR_AGENT_NAME,
        AEGIS_PYTHON_EXECUTOR_AGENT_TEMPLATE,
    ),
    (
        AEGIS_JAVASCRIPT_EXECUTOR_AGENT_NAME,
        AEGIS_JAVASCRIPT_EXECUTOR_AGENT_TEMPLATE,
    ),
    (
        AEGIS_BASH_EXECUTOR_AGENT_NAME,
        AEGIS_BASH_EXECUTOR_AGENT_TEMPLATE,
    ),
    (
        AEGIS_DOCUMENT_RENDERER_AGENT_NAME,
        AEGIS_DOCUMENT_RENDERER_AGENT_TEMPLATE,
    ),
    (
        "requirements-analyst",
        include_str!("../../templates/agents/requirements-analyst.yaml"),
    ),
    (
        "architect-agent",
        include_str!("../../templates/agents/architect-agent.yaml"),
    ),
    (
        "tester-agent",
        include_str!("../../templates/agents/tester-agent.yaml"),
    ),
    (
        "coder-agent",
        include_str!("../../templates/agents/coder-agent.yaml"),
    ),
    (
        "code-reviewer-agent",
        include_str!("../../templates/agents/code-reviewer-agent.yaml"),
    ),
    (
        "critic-agent",
        include_str!("../../templates/agents/critic-agent.yaml"),
    ),
    (
        "security-auditor-agent",
        include_str!("../../templates/agents/security-auditor-agent.yaml"),
    ),
];

/// Canonical registry of all built-in workflow templates.
pub const BUILTIN_WORKFLOWS: &[(&str, &str)] = &[
    (
        WORKFLOW_GENERATOR_WORKFLOW_NAME,
        WORKFLOW_GENERATOR_WORKFLOW_TEMPLATE,
    ),
    ("skill-import", SKILL_IMPORT_WORKFLOW_TEMPLATE),
    (
        INTENT_EXECUTION_WORKFLOW_NAME,
        INTENT_EXECUTION_WORKFLOW_TEMPLATE,
    ),
    (
        "the-forge",
        include_str!("../../templates/workflows/the-forge.yaml"),
    ),
    (
        "carbon-forge",
        include_str!("../../templates/workflows/carbon-forge.yaml"),
    ),
    (
        "carbon-deploy",
        include_str!("../../templates/workflows/carbon-deploy.yaml"),
    ),
    (
        "transcribe-audio",
        include_str!("../../templates/workflows/transcribe-audio.yaml"),
    ),
];

/// All builtin templates that should be synced to disk.
const BUILTIN_TEMPLATES: &[BuiltinTemplateSpec] = &[
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "hello-world-agent.yaml",
        content: HELLO_WORLD_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "code-quality-judge.yaml",
        content: CODE_QUALITY_JUDGE_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "tool-call-policy-judge.yaml",
        content: TOOL_CALL_POLICY_JUDGE_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "workflow-generator-planner-agent.yaml",
        content: WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "agent-creator-agent.yaml",
        content: AGENT_GENERATOR_AGENT_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "agent-generator-judge.yaml",
        content: AGENT_GENERATOR_JUDGE_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "workflow-generator-judge.yaml",
        content: WORKFLOW_GENERATOR_JUDGE_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "workflow-creator-validator-agent.yaml",
        content: WORKFLOW_CREATOR_AGENT_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "goal-judge.yaml",
        content: GOAL_JUDGE_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "skill-validator.yaml",
        content: SKILL_VALIDATOR_AGENT_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "aegis-output-formatter-agent.yaml",
        content: AEGIS_OUTPUT_FORMATTER_AGENT_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "aegis-code-validator-agent.yaml",
        content: AEGIS_CODE_VALIDATOR_AGENT_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "aegis-python-executor-agent.yaml",
        content: AEGIS_PYTHON_EXECUTOR_AGENT_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "aegis-javascript-executor-agent.yaml",
        content: AEGIS_JAVASCRIPT_EXECUTOR_AGENT_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "agents",
        file_name: "aegis-bash-executor-agent.yaml",
        content: AEGIS_BASH_EXECUTOR_AGENT_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "workflows",
        file_name: "builtin-workflow-generator.yaml",
        content: WORKFLOW_GENERATOR_WORKFLOW_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "workflows",
        file_name: "skill-import.yaml",
        content: SKILL_IMPORT_WORKFLOW_TEMPLATE,
    },
    BuiltinTemplateSpec {
        category: "workflows",
        file_name: "builtin-intent-to-execution.yaml",
        content: INTENT_EXECUTION_WORKFLOW_TEMPLATE,
    },
];

// ─── Content Drift Detection ────────────────────────────────────────────────

/// Outcome of comparing an embedded built-in template against the
/// currently-deployed version. Documents the three dispatch states the
/// deploy code must handle; not exposed beyond the test surface today.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinDriftOutcome {
    /// No version is currently deployed under this name — the built-in needs
    /// to be installed.
    NotDeployed,
    /// The deployed manifest semantically matches the embedded template.
    /// No write is required.
    UpToDate,
    /// The deployed manifest differs from the embedded template. The built-in
    /// must be overwritten.
    ContentDrift,
}

/// Compare a deployed `AgentManifest` against the embedded YAML template by
/// canonicalising both sides through `serde_json::Value`. This eliminates
/// trivial differences (whitespace, key ordering, comment handling) and
/// surfaces only true semantic drift.
///
/// Returns `Ok(true)` when the two manifests are byte-equal after
/// canonicalisation.
pub fn agent_manifest_matches(
    deployed: &aegis_orchestrator_sdk::AgentManifest,
    embedded_yaml: &str,
) -> Result<bool> {
    let embedded: aegis_orchestrator_sdk::AgentManifest = serde_yaml::from_str(embedded_yaml)
        .context("Failed to parse embedded built-in agent template for drift comparison")?;
    let deployed_v =
        serde_json::to_value(deployed).context("Failed to canonicalise deployed agent manifest")?;
    let embedded_v = serde_json::to_value(&embedded)
        .context("Failed to canonicalise embedded agent manifest")?;
    Ok(deployed_v == embedded_v)
}

/// Compare a deployed `Workflow` against an embedded YAML workflow template.
/// Only the semantic content (`metadata` + `spec`) is compared — runtime
/// fields (id, tenant_id, scope, timestamps) are intentionally excluded.
///
/// Registration resolves a model step's `model:<class>/<name>` image through
/// the runtime registry before it stores the workflow (the pinned image, no
/// network, the entry's command and resources). With `runtime_registry`, the
/// template is resolved the same way before the comparison, so a stored model
/// step equals its template, and a template or registry entry that changed
/// reads as drift. Without one, the template is compared as written.
pub fn workflow_matches(
    deployed: &aegis_orchestrator_core::domain::workflow::Workflow,
    embedded_yaml: &str,
    runtime_registry: Option<
        &aegis_orchestrator_core::domain::runtime_registry::StandardRuntimeRegistry,
    >,
) -> Result<bool> {
    let mut embedded =
        aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
            embedded_yaml,
        )
        .context("Failed to parse embedded built-in workflow template for drift comparison")?;
    if let Some(registry) = runtime_registry {
        embedded
            .resolve_model_images(Some(registry))
            .context("Failed to resolve embedded built-in workflow's model images")?;
    }
    let deployed_v = serde_json::json!({
        "metadata": serde_json::to_value(&deployed.metadata)
            .context("Failed to canonicalise deployed workflow metadata")?,
        "spec": serde_json::to_value(&deployed.spec)
            .context("Failed to canonicalise deployed workflow spec")?,
    });
    let embedded_v = serde_json::json!({
        "metadata": serde_json::to_value(&embedded.metadata)
            .context("Failed to canonicalise embedded workflow metadata")?,
        "spec": serde_json::to_value(&embedded.spec)
            .context("Failed to canonicalise embedded workflow spec")?,
    });
    Ok(deployed_v == embedded_v)
}

// ─── Deployment Logic ───────────────────────────────────────────────────────

pub async fn deploy_all_builtins(client: &DaemonClient, force: bool) -> Result<()> {
    for (name, template) in BUILTIN_AGENTS {
        deploy_builtin_agent(client, name, template, force).await?;
    }
    for (name, template) in BUILTIN_WORKFLOWS {
        deploy_builtin_workflow(client, name, template, force).await?;
    }
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
    for spec in BUILTIN_TEMPLATES {
        persist_template(templates_root, spec.category, spec.file_name, spec.content)?;
    }
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

    // Force-overwrite escape hatch: if the operator passed `--force` we skip the
    // drift comparison entirely and re-deploy unconditionally.
    if !force {
        if let Some(id) = client.lookup_agent(&manifest.metadata.name).await? {
            // A version is already deployed under this name. Decide whether to
            // overwrite by comparing the embedded template against the live
            // manifest — content drift, not metadata.version, is the gate.
            let deployed = client
                .get_agent(id)
                .await
                .context("Failed to fetch deployed built-in agent for drift comparison")?;
            if agent_manifest_matches(&deployed, template_yaml)? {
                println!(
                    "  {} Built-in agent '{}' (id: {}) already up to date",
                    "✓".dimmed(),
                    manifest.metadata.name.cyan(),
                    id
                );
                return Ok(id);
            }

            println!(
                "  {} Built-in agent '{}' content drift detected — overwriting",
                "↻".dimmed(),
                manifest.metadata.name.cyan(),
            );
            return client
                .deploy_agent(manifest, true, Some("global"))
                .await
                .context("Failed to deploy built-in template (drift overwrite)");
        }
    }

    println!(
        "  {} Deploying built-in agent: {}",
        "→".dimmed(),
        name.cyan()
    );

    client
        .deploy_agent(manifest, force, Some("global"))
        .await
        .context("Failed to deploy built-in template")
}

/// Deploy one built-in workflow template through the daemon.
///
/// The template is parsed before any request, so a malformed built-in fails
/// here. A cycle is not refused: a built-in workflow loops by design (a gate
/// sent back, a failed test run re-entering its coder), and the engine bounds
/// every loop by its visit limits, `max_state_visits` per state and
/// `max_total_transitions` per run (ADR-015, FSM Loop Guards).
async fn deploy_builtin_workflow(
    client: &DaemonClient,
    name: &str,
    template_yaml: &str,
    force: bool,
) -> Result<()> {
    // Validate template before deployment regardless of branch — a malformed
    // built-in must surface immediately, not silently no-op past drift checks.
    aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
        template_yaml,
    )
    .context("Failed to parse built-in workflow template YAML")?;

    if !force {
        if let Ok(deployed_json) = client.describe_workflow(name).await {
            // Pull the canonical YAML the daemon shipped back and re-parse it
            // so we can compare semantic content against the embedded template.
            if let Some(manifest_yaml) = deployed_json
                .get("manifest_yaml")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                let deployed = aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(manifest_yaml)
                    .context("Failed to parse deployed built-in workflow YAML for drift comparison")?;
                if workflow_matches(&deployed, template_yaml, None)? {
                    println!(
                        "  {} Built-in workflow '{}' already up to date",
                        "✓".dimmed(),
                        name.cyan(),
                    );
                    return Ok(());
                }

                println!(
                    "  {} Built-in workflow '{}' content drift detected — overwriting",
                    "↻".dimmed(),
                    name.cyan(),
                );
                return client
                    .deploy_workflow_manifest_with_force_and_scope(
                        template_yaml,
                        true,
                        Some("global"),
                    )
                    .await
                    .context("Failed to deploy built-in workflow template (drift overwrite)");
            }
        }
    }

    println!(
        "  {} Deploying built-in workflow: {}",
        "→".dimmed(),
        name.cyan()
    );

    client
        .deploy_workflow_manifest_with_force_and_scope(template_yaml, force, Some("global"))
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
            generate_missing_agents_input.contains("{{{json PLAN.output}}}"),
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
            generate_and_register_workflow_input.contains("{{{json PLAN.output}}}"),
            "GENERATE_AND_REGISTER_WORKFLOW should reference the planner state's output payload"
        );
        assert!(
            generate_and_register_workflow_input.contains("{{{json GENERATE_MISSING_AGENTS.output}}}"),
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
                .contains("Skipping, reordering, or collapsing steps 1–8 is a policy violation"),
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

        assert!(states.contains_key(serde_yaml::Value::from("PLAN")));
        assert!(states.contains_key(serde_yaml::Value::from("GENERATE_MISSING_AGENTS")));
        assert!(states.contains_key(serde_yaml::Value::from("GENERATE_AND_REGISTER_WORKFLOW")));
    }

    /// AEGIS ADR-131 D3: `goal-judge` is a built-in judge on the alias
    /// `judge`, its model call and its execution both bounded at 300 s, so it
    /// keeps manifest-spec-v1's `llm_timeout_seconds ≤ resources.timeout`
    /// without joining the known exceptions.
    #[test]
    fn goal_judge_is_a_builtin_judge_on_the_alias_judge_bounded_at_300_s() {
        let (_, yaml) = BUILTIN_AGENTS
            .iter()
            .find(|(name, _)| *name == GOAL_JUDGE_NAME)
            .expect("goal-judge is a built-in agent");
        let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(doc["metadata"]["name"].as_str(), Some("goal-judge"));
        assert_eq!(doc["metadata"]["labels"]["role"].as_str(), Some("judge"));
        assert_eq!(doc["metadata"]["labels"]["builtin"].as_str(), Some("true"));
        assert_eq!(doc["spec"]["runtime"]["model"].as_str(), Some("judge"));
        assert_eq!(
            doc["spec"]["runtime"]["model"].as_str(),
            Some(aegis_orchestrator_core::application::goal_service::GOAL_JUDGE_ALIAS),
            "U16 reads the room of the alias goal-judge runs on"
        );
        let prompt_bytes = doc["spec"]["task"]["instruction"].as_str().unwrap().len()
            + doc["spec"]["task"]["prompt_template"]
                .as_str()
                .unwrap()
                .len();
        assert!(
            prompt_bytes < aegis_orchestrator_core::domain::goal::JUDGE_PROMPT_RESERVE_BYTES,
            "U16's reserve holds goal-judge's instruction and template ({prompt_bytes} bytes)"
        );
        // U14, U16: the prompt says in true words what the judge is given.
        let instruction = doc["spec"]["task"]["instruction"].as_str().unwrap();
        for words in [
            "last_output is the execution's output whole, as it produced it",
            "companion_answer: the text the user was shown at the end of the turn just ended, \
             whole, as the user received it",
            "Nothing you are given has been shortened, summarised or sampled",
            "An input too large for you is never sent to you: the system stops the round itself \
             and no judge runs.",
        ] {
            assert!(
                instruction
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .contains(words),
                "goal-judge's instruction says: {words}"
            );
        }
        assert_eq!(
            doc["spec"]["execution"]["llm_timeout_seconds"].as_u64(),
            Some(300)
        );
        assert_eq!(
            doc["spec"]["security"]["resources"]["timeout"].as_str(),
            Some("300s")
        );
        assert_eq!(
            doc["spec"]["security"]["network"]["mode"].as_str(),
            Some("none")
        );
        let tools: Vec<&str> = doc["spec"]["tools"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|t| t.as_str())
            .collect();
        assert_eq!(tools, ["aegis.task.status", "aegis.execution.file"]);
        assert!(
            !KNOWN_LLM_TIMEOUT_ABOVE_EXECUTION_TIMEOUT.contains(&GOAL_JUDGE_NAME),
            "goal-judge obeys the rule; it is no known exception"
        );
        let required: Vec<&str> = doc["spec"]["input_schema"]["required"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|t| t.as_str())
            .collect();
        assert_eq!(required, ["goal", "executions"]);
    }

    #[test]
    fn all_builtin_agent_templates_parse_successfully() {
        for (name, yaml) in BUILTIN_AGENTS {
            let manifest: aegis_orchestrator_sdk::AgentManifest = serde_yaml::from_str(yaml)
                .unwrap_or_else(|e| panic!("Failed to parse builtin agent '{}': {}", name, e));
            manifest
                .validate()
                .unwrap_or_else(|e| panic!("Builtin agent '{}' failed validation: {}", name, e));
        }
    }

    /// The Forge's tester: each try is 900 s and each model call is bounded
    /// at 120 s, so a try keeps 130 s after a command and a command may start
    /// until 770 s into it; its five tries stay, under an execution bound of
    /// 2445 s that fits two of them with their judges (AEGIS ADR-141, Updates
    /// of 2026-10-15 and 2026-10-16).
    /// On run `dcf60c18` its 600 s tries kept 300 s and four of five ended at
    /// the try clock's refusal without an answer.
    #[test]
    fn tester_agent_tries_are_900_s_with_model_calls_bounded_at_120_s() {
        use aegis_orchestrator_core::domain::agent::{ExecutionStrategy, ResourceLimits};
        use aegis_orchestrator_core::domain::supervisor::iteration_timeout;

        let (_, yaml) = BUILTIN_AGENTS
            .iter()
            .find(|(name, _)| *name == "tester-agent")
            .expect("tester-agent is a built-in agent");
        let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let execution: ExecutionStrategy =
            serde_yaml::from_value(doc["spec"]["execution"].clone()).unwrap();
        let resources: ResourceLimits =
            serde_yaml::from_value(doc["spec"]["security"]["resources"].clone()).unwrap();
        assert_eq!(
            iteration_timeout(&execution),
            std::time::Duration::from_secs(900)
        );
        assert_eq!(execution.llm_timeout_seconds, 120);
        assert_eq!(execution.max_retries, 5);
        assert_eq!(resources.parse_timeout_seconds(), Some(2445));
    }

    /// Every `timeout_seconds` a validator of `execution` carries, after the
    /// orchestrator's own defaults are applied.
    fn validator_timeouts(value: &serde_yaml::Value, out: &mut Vec<u64>) {
        match value {
            serde_yaml::Value::Mapping(map) => {
                for (key, child) in map {
                    if key.as_str() == Some("timeout_seconds") {
                        if let Some(secs) = child.as_u64() {
                            out.push(secs);
                        }
                    }
                    validator_timeouts(child, out);
                }
            }
            serde_yaml::Value::Sequence(items) => {
                for item in items {
                    validator_timeouts(item, out);
                }
            }
            _ => {}
        }
    }

    /// Regression test for the built-in workflow generator failing in
    /// production (2026-10-02, generation c1fc25b9 on `sha-fbb3a68`): the
    /// agent creator's execution ended at its `resources.timeout` of 300 s
    /// while its judge was still working, although the template gives one
    /// validation 600 s. A validation runs inside the execution, so an
    /// execution bound below a validator's bound contradicts the template.
    /// The overall bound is `security.resources.timeout`
    /// (`ResourceLimits::parse_timeout_seconds`, read by the supervisor's
    /// `run_loop`); unset, it is `DEFAULT_EXECUTION_TIMEOUT_SECONDS`.
    /// `llm_timeout_seconds` is deliberately not part of the rule: the judges'
    /// clocks are AEGIS ADR-124's and do not move here.
    #[test]
    fn builtin_agent_execution_timeout_is_not_below_its_validators_timeouts() {
        use aegis_orchestrator_core::domain::agent::{ExecutionStrategy, ResourceLimits};
        use aegis_orchestrator_core::domain::supervisor::DEFAULT_EXECUTION_TIMEOUT_SECONDS;

        let mut violations = Vec::new();
        for (name, yaml) in BUILTIN_AGENTS {
            let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
            let spec = &doc["spec"];
            let execution: ExecutionStrategy = serde_yaml::from_value(spec["execution"].clone())
                .unwrap_or_else(|e| {
                    panic!("builtin agent '{name}': execution does not parse: {e}")
                });
            let resources: ResourceLimits =
                serde_yaml::from_value(spec["security"]["resources"].clone()).unwrap_or_else(|e| {
                    panic!("builtin agent '{name}': resources do not parse: {e}")
                });
            let execution_timeout = resources
                .parse_timeout_seconds()
                .unwrap_or(DEFAULT_EXECUTION_TIMEOUT_SECONDS);

            let mut validators = Vec::new();
            for config in [&execution.validation, &execution.tool_validation]
                .into_iter()
                .flatten()
            {
                validator_timeouts(&serde_yaml::to_value(config).unwrap(), &mut validators);
            }

            for secs in validators {
                if execution_timeout < secs {
                    violations.push(format!(
                        "{name}: execution timeout {execution_timeout} s is below its validator timeout_seconds {secs} s"
                    ));
                }
            }
        }
        assert!(
            violations.is_empty(),
            "built-in agents whose execution would end inside one of their own validations:\n{}",
            violations.join("\n")
        );
    }

    /// Built-in agents known on 2026-10-04 to give one model call a longer
    /// bound than their whole execution (AEGIS operations/known-defects-5, the
    /// Low row): the execution bound cuts first, so their larger
    /// `llm_timeout_seconds` is inert. Their clocks are AEGIS ADR-124's and do
    /// not move without a measurement; they are listed here so they stay
    /// visible and a fifth is caught. Remove a name once its template obeys
    /// the rule.
    const KNOWN_LLM_TIMEOUT_ABOVE_EXECUTION_TIMEOUT: &[&str] = &[
        "tool-call-policy-judge",
        "agent-generator-judge",
        "workflow-generator-judge",
        "skill-validator",
    ];

    /// The production model registry's overall budget for one call, every
    /// attempt and the fallback alias included (`llm_overall_timeout_secs`,
    /// `aegis-platform-deployment` `29c8c998`
    /// `podman/pods/core/aegis-config.yaml` line 106). Production's value, not
    /// the code's default, because it is what bounds `smart` there.
    const PRODUCTION_LLM_OVERALL_TIMEOUT_SECS: u64 = 300;

    /// Regression test for `builtin-intent-to-execution` failing in
    /// VALIDATE_CODE (AEGIS known-defects-5 and -7, the High row): the code
    /// validator's one model call on `smart` was ended at its
    /// `llm_timeout_seconds` of 60 by the inner loop with HTTP 502, below the
    /// model's measured answer (59.06 s when it passed, past 60 s three times
    /// of four), and before `smart`'s fallback alias could be reached after
    /// the provider's limit at about 120 s (AEGIS ADR-130 D2). AEGIS ADR-124,
    /// Update of 2026-10-04: the validator runs with `llm_timeout_seconds:
    /// 300` and `resources.timeout: 300s`.
    ///
    /// Two rules. One model call is not bounded longer than the execution
    /// that holds it (manifest-spec-v1, "Constraint Rules":
    /// `llm_timeout_seconds ≤ spec.security.resources.timeout`), for every
    /// built-in agent outside the known set above. The code validator's one
    /// call is not bounded below the registry's overall budget, so the
    /// per-call bound cuts nothing the registry would still answer.
    #[test]
    fn builtin_agent_llm_timeout_fits_its_execution_and_code_validator_reaches_the_registry_budget()
    {
        use aegis_orchestrator_core::domain::agent::{ExecutionStrategy, ResourceLimits};
        use aegis_orchestrator_core::domain::supervisor::DEFAULT_EXECUTION_TIMEOUT_SECONDS;

        let mut violations = Vec::new();
        let mut known_found = Vec::new();
        let mut code_validator_llm_timeout = None;
        for (name, yaml) in BUILTIN_AGENTS {
            let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
            let spec = &doc["spec"];
            let execution: ExecutionStrategy = serde_yaml::from_value(spec["execution"].clone())
                .unwrap_or_else(|e| {
                    panic!("builtin agent '{name}': execution does not parse: {e}")
                });
            let resources: ResourceLimits =
                serde_yaml::from_value(spec["security"]["resources"].clone()).unwrap_or_else(|e| {
                    panic!("builtin agent '{name}': resources do not parse: {e}")
                });
            let execution_timeout = resources
                .parse_timeout_seconds()
                .unwrap_or(DEFAULT_EXECUTION_TIMEOUT_SECONDS);
            let llm_timeout = execution.llm_timeout_seconds;

            if *name == AEGIS_CODE_VALIDATOR_AGENT_NAME {
                code_validator_llm_timeout = Some(llm_timeout);
            }
            if llm_timeout > execution_timeout {
                let line = format!(
                    "{name}: llm_timeout_seconds {llm_timeout} s is above its execution timeout {execution_timeout} s"
                );
                if KNOWN_LLM_TIMEOUT_ABOVE_EXECUTION_TIMEOUT.contains(name) {
                    known_found.push((*name, line));
                } else {
                    violations.push(line);
                }
            }
        }

        for (_, line) in &known_found {
            println!("known, not changed here: {line}");
        }
        assert!(
            violations.is_empty(),
            "built-in agents whose model call is bounded longer than their execution:\n{}",
            violations.join("\n")
        );
        let mut found: Vec<&str> = known_found.iter().map(|(name, _)| *name).collect();
        let mut known = KNOWN_LLM_TIMEOUT_ABOVE_EXECUTION_TIMEOUT.to_vec();
        found.sort_unstable();
        known.sort_unstable();
        assert_eq!(
            found, known,
            "the known set no longer matches the templates: remove a name whose template now obeys the rule"
        );

        let code_validator_llm_timeout = code_validator_llm_timeout
            .unwrap_or_else(|| panic!("{AEGIS_CODE_VALIDATOR_AGENT_NAME} is not a built-in agent"));
        assert!(
            code_validator_llm_timeout >= PRODUCTION_LLM_OVERALL_TIMEOUT_SECS,
            "{AEGIS_CODE_VALIDATOR_AGENT_NAME}: llm_timeout_seconds {code_validator_llm_timeout} s is below the registry's overall budget {PRODUCTION_LLM_OVERALL_TIMEOUT_SECS} s, so smart's fallback alias is unreachable"
        );
    }

    /// Built-in agents whose clocks a record decides, exempt from the
    /// two-tries rule below, each with the record that sets its clock. A
    /// clock here moves only by an Update of its record.
    const TWO_TRIES_EXEMPT: &[(&str, &str)] = &[
        (
            AEGIS_CODE_VALIDATOR_AGENT_NAME,
            "AEGIS ADR-124, Update of 2026-10-04: llm_timeout_seconds 300, resources.timeout 300s",
        ),
        (GOAL_JUDGE_NAME, "AEGIS ADR-131 D3: resources.timeout 300s"),
        (
            "code-quality-judge",
            "AEGIS ADR-052, Update of 2026-10-04 (the judges keep 300 s) and ADR-124",
        ),
        (
            "tool-call-policy-judge",
            "AEGIS ADR-052, Update of 2026-10-04 (the judges keep 300 s) and ADR-124",
        ),
        (
            AGENT_GENERATOR_JUDGE_NAME,
            "AEGIS ADR-052, Update of 2026-10-04 (the judges keep 300 s) and ADR-124",
        ),
        (
            WORKFLOW_GENERATOR_JUDGE_NAME,
            "AEGIS ADR-052, Update of 2026-10-04 (the judges keep 300 s) and ADR-124",
        ),
        (
            AEGIS_PYTHON_EXECUTOR_AGENT_NAME,
            "AEGIS ADR-087: the intent-to-execution pipeline's executors",
        ),
        (
            AEGIS_JAVASCRIPT_EXECUTOR_AGENT_NAME,
            "AEGIS ADR-087: the intent-to-execution pipeline's executors",
        ),
        (
            AEGIS_BASH_EXECUTOR_AGENT_NAME,
            "AEGIS ADR-087: the intent-to-execution pipeline's executors",
        ),
        (
            AEGIS_OUTPUT_FORMATTER_AGENT_NAME,
            "AEGIS ADR-103: the output handler",
        ),
        (
            AEGIS_DOCUMENT_RENDERER_AGENT_NAME,
            "AEGIS ADR-135 D1: one fixed program, max_iterations 1",
        ),
        (
            AGENT_GENERATOR_AGENT_NAME,
            "AEGIS ADR-052, Update of 2026-10-04: resources.timeout 600s",
        ),
        (
            WORKFLOW_CREATOR_AGENT_NAME,
            "AEGIS ADR-052, Update of 2026-10-04: resources.timeout 600s",
        ),
        (
            WORKFLOW_GENERATOR_PLANNER_AGENT_NAME,
            "AEGIS ADR-052, Update of 2026-10-04: resources.timeout 600s",
        ),
    ];

    /// The seven agents of the Forge.
    const FORGE_AGENTS: &[&str] = &[
        "requirements-analyst",
        "architect-agent",
        "coder-agent",
        "tester-agent",
        "code-reviewer-agent",
        "critic-agent",
        "security-auditor-agent",
    ];

    /// Regression test for Forge run `40e66741` (AEGIS known-defects row
    /// `forge-architect-tries-exceed-state-bound-2026-10-15`): ARCHITECT's
    /// first try ended at the default 600 s iteration timeout and the second
    /// was cut by the execution's 900 s bound. AEGIS ADR-141, Update of
    /// 2026-10-15, clause (2), as the Update of 2026-10-16
    /// `tests-load-before-code-2026-10-16`, clause 2, widens it: every
    /// built-in agent that may make two tries fits two whole tries under its
    /// execution bound, each its clock and its judges', with the wait the
    /// supervisor makes after a failed try and the time a try takes to start
    /// (`resources.timeout >= 2 x (iteration_timeout + judges' timeout_seconds)
    /// + failed_execution_backoff(1) + TRY_START_ALLOWANCE`, the supervisor's
    /// `two_tries_bound`), outside the exemptions above. Run `e7f2c2aa`'s
    /// coder had 899 s left of 1815 s after its first try ran to its 900 s
    /// clock, and its second try was refused. The Forge's seven agents take
    /// `iteration_timeout: "900s"` and exactly the bound their own manifest
    /// computes: 2 x (900 + 300) + 15 + 30 = 2445 s.
    #[test]
    fn builtin_agent_execution_bound_fits_two_tries() {
        use aegis_orchestrator_core::domain::agent::{ExecutionStrategy, ResourceLimits};
        use aegis_orchestrator_core::domain::supervisor::{
            iteration_timeout, two_tries_bound, DEFAULT_EXECUTION_TIMEOUT_SECONDS,
        };

        let names: Vec<&str> = BUILTIN_AGENTS.iter().map(|(name, _)| *name).collect();
        let mut complaints = Vec::new();
        for (exempt, record) in TWO_TRIES_EXEMPT {
            if !names.contains(exempt) {
                complaints.push(format!(
                    "{exempt} is exempt ({record}) but is not a built-in agent"
                ));
            }
        }
        for (name, yaml) in BUILTIN_AGENTS {
            let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
            let spec = &doc["spec"];
            let execution: ExecutionStrategy = serde_yaml::from_value(spec["execution"].clone())
                .unwrap_or_else(|e| {
                    panic!("builtin agent '{name}': execution does not parse: {e}")
                });
            let resources: ResourceLimits =
                serde_yaml::from_value(spec["security"]["resources"].clone()).unwrap_or_else(|e| {
                    panic!("builtin agent '{name}': resources do not parse: {e}")
                });
            let bound = resources
                .parse_timeout_seconds()
                .unwrap_or(DEFAULT_EXECUTION_TIMEOUT_SECONDS);
            let iteration = iteration_timeout(&execution).as_secs();
            let needed = two_tries_bound(&execution).as_secs();

            if FORGE_AGENTS.contains(name)
                && (execution.iteration_timeout.as_deref() != Some("900s")
                    || spec["security"]["resources"]["timeout"].as_str()
                        != Some(format!("{needed}s").as_str()))
            {
                complaints.push(format!(
                    "{name}: a Forge agent takes iteration_timeout \"900s\" and resources.timeout \"{needed}s\", it has {:?} and {:?}",
                    execution.iteration_timeout,
                    spec["security"]["resources"]["timeout"].as_str()
                ));
            }
            if execution.max_retries < 2 || TWO_TRIES_EXEMPT.iter().any(|(n, _)| n == name) {
                continue;
            }
            if bound < needed {
                complaints.push(format!(
                    "{name}: resources.timeout {bound} s is under two whole tries of {iteration} s and their judges', the wait between them and the time a try takes to start ({needed} s)"
                ));
            }
        }
        for name in FORGE_AGENTS {
            if !names.contains(name) {
                complaints.push(format!("{name} is a Forge agent but not a built-in agent"));
            }
        }
        let coder = BUILTIN_AGENTS
            .iter()
            .find(|(name, _)| *name == "coder-agent")
            .map(|(_, yaml)| serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap())
            .expect("coder-agent is a built-in agent");
        if coder["spec"]["security"]["resources"]["timeout"].as_str() != Some("2445s") {
            complaints.push(format!(
                "the coder's resources.timeout is {:?}, not \"2445s\"",
                coder["spec"]["security"]["resources"]["timeout"].as_str()
            ));
        }
        // Clause 4: the coder's try makes at most 24 tool calls, as the
        // tester's does; run `e7f2c2aa`'s try made 37 under the default 50.
        if coder["spec"]["execution"]["max_tool_calls"].as_u64() != Some(24) {
            complaints.push(format!(
                "the coder's max_tool_calls is {:?}, not 24",
                coder["spec"]["execution"]["max_tool_calls"].as_u64()
            ));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// The coder edits an existing file and never rewrites it: on run
    /// `6b8987b3` CODE rewrote the whole README with `fs.write`, turning
    /// every CRLF line into LF and joining its new sentence to the last line.
    #[test]
    fn the_coder_edits_existing_files_and_never_rewrites_them() {
        const STEP: &str = "Change the implementation with fs.edit and fs.multi_edit, replacing only the lines the design touches; use fs.write only for a file that does not exist yet. Never rewrite a whole existing file, never change its line endings or its final newline, and add nothing the design does not ask for.";
        let (_, yaml) = BUILTIN_AGENTS
            .iter()
            .find(|(name, _)| *name == "coder-agent")
            .expect("coder-agent is a built-in agent");
        let manifest: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(yaml).expect("the coder template parses");
        manifest.validate().expect("the coder template validates");
        let instruction = folded_instruction(yaml);
        assert!(
            instruction.contains(&format!("2. {STEP}")),
            "the coder's step 2 must read \"{STEP}\":\n{instruction}"
        );
    }

    /// The tester tests only what an acceptance criterion names: on run
    /// `c0d37ca8` it asserted `content.endswith('\\n')` on a text-mode read,
    /// a final-newline test no criterion named and which cannot pass.
    #[test]
    fn the_tester_tests_only_what_a_criterion_names() {
        const STEP: &str = "Write one test file for each acceptance criterion, in that layout and style, with fs.write, at a path relative to the repository's root. Test only what an acceptance criterion names; never test formatting, whitespace or line endings that no acceptance criterion names, and a design's line endings or escape sequences are not criteria: no test asserts on '\\r', a line ending or a byte sequence. When a test reads a text file, read it in text mode (universal newlines) or normalize '\\r\\n' before matching, so a content match never silently fails on a CRLF file, and never assert on an escaped sequence such as '\\\\n' where a newline is meant.";
        let (_, yaml) = BUILTIN_AGENTS
            .iter()
            .find(|(name, _)| *name == "tester-agent")
            .expect("tester-agent is a built-in agent");
        let manifest: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(yaml).expect("the tester template parses");
        manifest.validate().expect("the tester template validates");
        let instruction = folded_instruction(yaml);
        assert!(
            instruction.contains(&format!("2. {STEP}")),
            "the tester's step 2 must read \"{STEP}\":\n{instruction}"
        );
    }

    /// The tester writes its tests and answers without running them: on run
    /// `d92cb09a` it deleted its tests five times, re-ran the test expected to
    /// fail on the unchanged tree and spent both its 900 s tries without
    /// answering. The suite's own run after CODE is where a test is checked.
    #[test]
    fn the_tester_does_not_run_its_tests_and_answers_once_each_criterion_has_one() {
        const STEP: &str = "Do not run the tests: the suite runs later, in its own container, after the code is written. Your try ends when each acceptance criterion has its test file; the orchestrator then answers with the files you wrote.";
        let (_, yaml) = BUILTIN_AGENTS
            .iter()
            .find(|(name, _)| *name == "tester-agent")
            .expect("tester-agent is a built-in agent");
        let manifest: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(yaml).expect("the tester template parses");
        manifest.validate().expect("the tester template validates");
        let instruction = folded_instruction(yaml);
        let step_3 = instruction.find("3. ").expect("step 3");
        let step_4 = instruction.find("4. ").expect("step 4");
        assert_eq!(
            instruction[step_3..step_4].trim_end(),
            format!("3. {STEP}"),
            "the tester's step 3 must read \"{STEP}\""
        );
    }

    /// The coder stops on a test it cannot pass: on run `c0d37ca8` it spent
    /// its 1800 s on the tester's final-newline test with no rule for
    /// stopping.
    #[test]
    fn the_coder_stops_on_a_test_it_cannot_pass() {
        const STEP: &str = "When your input carries a failed run of the suite, fix what its output names; when it carries a review below 0.95, fix every suggestion the reports make. If one test still fails after three of your attempts and its assertion cannot be met by an implementation the design asks for, stop and answer with the test's name, its assertion, and why it cannot pass; do not change the test.";
        let (_, yaml) = BUILTIN_AGENTS
            .iter()
            .find(|(name, _)| *name == "coder-agent")
            .expect("coder-agent is a built-in agent");
        let manifest: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(yaml).expect("the coder template parses");
        manifest.validate().expect("the coder template validates");
        let instruction = folded_instruction(yaml);
        assert!(
            instruction.contains(&format!("4. {STEP}")),
            "the coder's step 4 must read \"{STEP}\":\n{instruction}"
        );
    }

    /// The Forge's second CODE visit carries the diff the judges reviewed,
    /// beside their reports, and every judge is told that a diff's `-` and
    /// `+` are its markers: on run `e946aeab` a judge read a `-` marker as a
    /// removed dash in the text and the coder put one in.
    #[test]
    fn the_forge_code_revisit_and_judges_read_the_diff_markers_as_markers() {
        const NOTE: &str = "lines beginning with - and + are the diff's markers, not the text";
        let yaml = BUILTIN_WORKFLOWS
            .iter()
            .find(|(name, _)| *name == "the-forge")
            .map(|(_, yaml)| *yaml)
            .expect("the Forge is a built-in");
        aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(yaml)
            .expect("the Forge parses");
        let raw: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let states = &raw["spec"]["states"];

        let code = states["CODE"]["input"].as_str().expect("CODE's input");
        let review = code
            .find("# The last review")
            .expect("CODE's revisit section");
        let revisit = &code[review..];
        let heading = format!("# The change they reviewed (a unified diff: {NOTE})");
        let at = revisit
            .find(&heading)
            .unwrap_or_else(|| panic!("CODE's revisit must carry {heading:?}:\n{revisit}"));
        assert!(
            revisit[at..].contains("{{{DIFF.output.diff}}}"),
            "the reviewed diff follows its heading:\n{revisit}"
        );
        let end = revisit.find("{{/if}}").expect("the revisit section ends");
        assert!(
            at < end,
            "the diff is inside the revisit section, shown only after a review"
        );

        let judges = states["AUDIT"]["agents"]
            .as_sequence()
            .expect("AUDIT's judges");
        assert_eq!(judges.len(), 3);
        for judge in judges {
            let name = judge["agent"].as_str().unwrap_or_default();
            let input = judge["input"].as_str().unwrap_or_default();
            let lower = input.to_lowercase();
            assert!(
                lower.contains(NOTE),
                "{name}'s input must say that {NOTE}:\n{input}"
            );
            assert!(input.contains("{{{DIFF.output.diff}}}"));
        }
    }

    /// Run `250cda5c`: CODE wrote a file no design named and a replacement
    /// with two blank lines on each side; the tester wrote eight test files,
    /// reported three, and one read the repository's `main` branch; CODE's
    /// second visit then deleted the tester's tests because a review asked.
    #[test]
    fn the_forge_agents_keep_to_the_design_the_tests_and_the_working_tree() {
        const REVISIT: &str = "A review's suggestion to change, delete or add a test is not yours to apply: leave the tests as the tester wrote them and name the suggestion in your answer.";
        const CODER: &str = "Create no file the design does not name, and change only the lines the design names; a replacement carries no more blank lines than the surrounding text has.";
        const TESTER: &str = "Write tests only: change no implementation file, and never test the repository's branches, history or remote.";

        let forge = BUILTIN_WORKFLOWS
            .iter()
            .find(|(name, _)| *name == "the-forge")
            .map(|(_, yaml)| *yaml)
            .expect("the Forge is a built-in");
        aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(forge)
            .expect("the Forge parses");
        let raw: serde_yaml::Value = serde_yaml::from_str(forge).unwrap();
        let code = raw["spec"]["states"]["CODE"]["input"]
            .as_str()
            .expect("CODE's input");
        let review = code
            .find("# The last review")
            .expect("CODE's revisit section");
        let revisit = &code[review..];
        let end = revisit.find("{{/if}}").expect("the revisit section ends");
        let at = revisit
            .find(REVISIT)
            .unwrap_or_else(|| panic!("CODE's revisit must carry {REVISIT:?}:\n{revisit}"));
        let reports = revisit
            .find("{{{json AUDIT.consensus.metadata.individual_outputs}}}")
            .expect("the reviews' reports");
        assert!(
            reports < at && at < end,
            "the sentence follows the reviews, inside the revisit section:\n{revisit}"
        );

        for (agent, sentence) in [("coder-agent", CODER), ("tester-agent", TESTER)] {
            let (_, yaml) = BUILTIN_AGENTS
                .iter()
                .find(|(name, _)| *name == agent)
                .unwrap_or_else(|| panic!("{agent} is a built-in agent"));
            let manifest: aegis_orchestrator_sdk::AgentManifest =
                serde_yaml::from_str(yaml).expect("the template parses");
            manifest.validate().expect("the template validates");
            let instruction = folded_instruction(yaml);
            let step_2 = instruction.find("2. ").expect("step 2");
            let step_3 = instruction.find("3. ").expect("step 3");
            let step = &instruction[step_2..step_3];
            assert!(
                step.contains(sentence),
                "{agent}'s step 2 must carry \"{sentence}\":\n{step}"
            );
        }
    }

    /// Run `2c113327`: the architect's design named its change as the bytes
    /// `"\r\n\r\nThis change ..."` of a CRLF README, and the tester asserted
    /// `endswith("\r\n\r\n" + expected)` on a text-mode read, which turns CRLF
    /// into LF, so the test could never pass. A design names text, and no
    /// test asserts a line ending no acceptance criterion names (AEGIS
    /// ADR-141, Update `forge-designs-name-text-not-line-endings-2026-10-15`).
    #[test]
    fn the_forge_designs_name_text_and_tests_never_assert_line_endings() {
        const ARCHITECT: &str = "Name each change as the text it adds, removes or replaces and the lines it touches, never as bytes or escape sequences such as \\r\\n: the edit tools keep each file's line endings.";
        const TESTER: &str = "never test formatting, whitespace or line endings that no acceptance criterion names, and a design's line endings or escape sequences are not criteria: no test asserts on '\\r', a line ending or a byte sequence.";
        const CRITERION: &str = "No test asserts on a line ending, a '\\r' or an escape sequence that no acceptance criterion names.";

        for (agent, sentence) in [("architect-agent", ARCHITECT), ("tester-agent", TESTER)] {
            let (_, yaml) = BUILTIN_AGENTS
                .iter()
                .find(|(name, _)| *name == agent)
                .unwrap_or_else(|| panic!("{agent} is a built-in agent"));
            let manifest: aegis_orchestrator_sdk::AgentManifest =
                serde_yaml::from_str(yaml).expect("the template parses");
            manifest.validate().expect("the template validates");
            let instruction = folded_instruction(yaml);
            let step_2 = instruction.find("2. ").expect("step 2");
            let step_3 = instruction[step_2..]
                .find(" Follow the repository's own conventions")
                .or_else(|| instruction[step_2..].find("3. "))
                .map(|at| step_2 + at)
                .expect("step 2 ends");
            let step = &instruction[step_2..step_3];
            assert!(
                step.contains(sentence),
                "{agent}'s step 2 must carry \"{sentence}\":\n{step}"
            );
        }

        let (_, yaml) = BUILTIN_AGENTS
            .iter()
            .find(|(name, _)| *name == "tester-agent")
            .expect("tester-agent is a built-in agent");
        let raw: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let criteria = raw["spec"]["execution"]["validation"][0]["criteria"]
            .as_str()
            .expect("the tester's judge criteria")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            criteria.contains(CRITERION),
            "the tester's judge criteria must carry \"{CRITERION}\":\n{criteria}"
        );
    }

    #[test]
    fn all_builtin_workflow_templates_parse_successfully() {
        for (name, yaml) in BUILTIN_WORKFLOWS {
            aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
                yaml,
            )
            .unwrap_or_else(|e| panic!("Failed to parse builtin workflow '{}': {}", name, e));
        }
    }

    /// The transition a score takes, by the Temporal worker's comparisons
    /// (`aegis-temporal-worker` `src/workflows/aegis-workflow.ts`
    /// `evaluateCondition` at `ad1ba6c`): `score_above` and `score_below` are
    /// strict, `score_between` is inclusive at both ends, the first match wins.
    fn score_route(
        transitions: &[aegis_orchestrator_core::domain::workflow::TransitionRule],
        score: f64,
    ) -> Option<String> {
        use aegis_orchestrator_core::domain::workflow::TransitionCondition;
        transitions
            .iter()
            .find(|t| match t.condition {
                TransitionCondition::ScoreAbove { threshold } => score > threshold,
                TransitionCondition::ScoreBelow { threshold } => score < threshold,
                TransitionCondition::ScoreBetween { min, max } => score >= min && score <= max,
                _ => false,
            })
            .map(|t| t.target.as_str().to_string())
    }

    /// Regression test for the built-in `skill-import` workflow ending in
    /// FAILED on every run (AEGIS ADR-017, Update of 2026-10-02): its
    /// validation state asked its agent for `quality_score`, while the worker
    /// routes on the `score` an Agent state carries, so the state carried no
    /// score; it routed on quality alone, so an unsafe skill could pass; and a
    /// score of exactly the threshold matched neither strict comparison, so
    /// the run would end inside the validation state.
    #[test]
    fn skill_import_validation_state_carries_a_score_with_a_route_for_every_value() {
        assert!(
            !SKILL_IMPORT_WORKFLOW_TEMPLATE.contains("quality_score"),
            "no state of skill-import may ask for or read `quality_score`: the worker routes on `score`"
        );

        let raw: serde_yaml::Value = serde_yaml::from_str(SKILL_IMPORT_WORKFLOW_TEMPLATE).unwrap();
        let input = raw["spec"]["states"]["PARSE_AND_VALIDATE"]["input"]
            .as_str()
            .expect("PARSE_AND_VALIDATE must have an input");
        assert!(
            input.contains(
                "- score: 0.0-1.0 rating of instruction clarity and coherence; \
                 0.0 when safety_assessment.verdict is \"unsafe\""
            ),
            "PARSE_AND_VALIDATE must ask its agent for `score`, 0.0 when unsafe; its input is:\n{input}"
        );

        let workflow =
            aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
                SKILL_IMPORT_WORKFLOW_TEMPLATE,
            )
            .unwrap();
        let state = workflow
            .spec
            .states
            .iter()
            .find(|(name, _)| name.as_str() == "PARSE_AND_VALIDATE")
            .map(|(_, state)| state)
            .expect("skill-import must have a PARSE_AND_VALIDATE state");

        // The platform's rule for thresholded states (`aegis.workflow.create`'s
        // `collect_thresholded_transition_semantic_violations`): low scores
        // route by an explicit `score_below` branch.
        assert!(
            state.transitions.iter().any(|t| matches!(
                t.condition,
                aegis_orchestrator_core::domain::workflow::TransitionCondition::ScoreBelow { .. }
            )),
            "PARSE_AND_VALIDATE must keep an explicit `score_below` branch for low scores"
        );

        for (score, expected) in [
            (0.0, "FAILED"),
            (0.5, "FAILED"),
            (0.7, "FAILED"),
            (0.71, "COMPOSE_MANIFEST"),
            (1.0, "COMPOSE_MANIFEST"),
        ] {
            assert_eq!(
                score_route(&state.transitions, score).as_deref(),
                Some(expected),
                "a PARSE_AND_VALIDATE score of {score} must route to {expected}"
            );
        }
    }

    /// Regression test for the unsafe verdict that carried no reason
    /// (production run 9d052c2c on `sha-14ea8c0`: "unsafe", score 0.0, no
    /// reasoning, where the well-formed run answered an object with verdict
    /// and reasoning): the validator was asked for `"safe" | "unsafe" with
    /// reasoning`, which names no field for the reasoning. It is asked for one
    /// shape, an object whose `reasoning` is always present.
    #[test]
    fn skill_import_validator_is_asked_for_one_verdict_shape_that_carries_its_reasoning() {
        let raw: serde_yaml::Value = serde_yaml::from_str(SKILL_IMPORT_WORKFLOW_TEMPLATE).unwrap();
        let input = raw["spec"]["states"]["PARSE_AND_VALIDATE"]["input"]
            .as_str()
            .expect("PARSE_AND_VALIDATE must have an input");
        assert!(
            input.contains(
                "- safety_assessment: an object {\"verdict\": \"safe\" or \"unsafe\", \
                 \"reasoning\": \"why\"}, the reasoning always present, whatever the verdict"
            ),
            "PARSE_AND_VALIDATE must ask for one safety_assessment shape carrying its reasoning; \
             its input is:\n{input}"
        );
        assert!(
            !input.contains("\"safe\" | \"unsafe\" with reasoning"),
            "the shapeless verdict line must be gone; the input is:\n{input}"
        );
    }

    /// No built-in workflow splices an input or a state's output into shell
    /// text: a System state's `command` runs with `sh -c` inside the core
    /// process (`orchestrator/core/src/presentation/grpc/server.rs`
    /// `execute_system_command`), and a ContainerRun state with `shell: true`
    /// runs its joined command with `sh -c`, so a template expression there
    /// would be parsed by a shell. A value reaches such a state through `env`,
    /// which the worker renders and the process receives as its environment,
    /// and the command names it in a double-quoted expansion.
    #[test]
    fn no_builtin_workflow_splices_a_value_into_shell_text() {
        use aegis_orchestrator_core::domain::workflow::StateKind;

        let mut splices = Vec::new();
        for (name, yaml) in BUILTIN_WORKFLOWS {
            let workflow =
                aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
                    yaml,
                )
                .unwrap();
            for (state_name, state) in &workflow.spec.states {
                let shell_text = match &state.kind {
                    StateKind::System { command, .. } => Some(command.clone()),
                    StateKind::ContainerRun { command, shell, .. } if *shell => {
                        Some(command.join(" "))
                    }
                    StateKind::ParallelContainerRun { steps, .. } => {
                        let joined: Vec<String> = steps
                            .iter()
                            .filter(|step| step.shell)
                            .map(|step| step.command.join(" "))
                            .collect();
                        (!joined.is_empty()).then(|| joined.join("\n"))
                    }
                    _ => None,
                };
                if let Some(text) = shell_text {
                    if text.contains("{{") {
                        splices.push(format!("{name} {}: {text}", state_name.as_str()));
                    }
                }
            }
        }
        assert!(
            splices.is_empty(),
            "these states splice a template expression into shell text:\n{}",
            splices.join("\n")
        );
    }

    /// An Agent state's output is its agent's answer parsed as JSON when the
    /// answer is a JSON object or array (`aegis-temporal-worker` `ad1ba6c`
    /// `src/activities/index.ts` 95-121), and the worker renders an object
    /// with Handlebars as "[object Object]" (production run 300bd457: the
    /// skill-import composer received "Parsed skill: [object Object]"). So no
    /// built-in workflow renders an Agent state's whole `output` without the
    /// worker's `json` helper (`aegis-workflow.ts` 74), except a state whose
    /// prompt asks for text, named here with the words that ask for it.
    #[test]
    fn no_builtin_workflow_renders_an_agent_states_whole_output_without_json() {
        use aegis_orchestrator_core::domain::workflow::StateKind;

        const TEXT_ANSWERS: &[(&str, &str, &str)] = &[(
            "skill-import",
            "COMPOSE_MANIFEST",
            "Output ONLY the complete YAML manifest, no commentary.",
        )];

        let mut bare = Vec::new();
        for (name, yaml) in BUILTIN_WORKFLOWS {
            let workflow =
                aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
                    yaml,
                )
                .unwrap();
            let raw: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
            let agent_states: Vec<String> = workflow
                .spec
                .states
                .iter()
                .filter(|(_, state)| matches!(state.kind, StateKind::Agent { .. }))
                .map(|(state_name, _)| state_name.as_str().to_string())
                .collect();
            let text_answers: Vec<&str> = TEXT_ANSWERS
                .iter()
                .filter(|(workflow_name, _, _)| workflow_name == name)
                .map(|(_, state, asks)| {
                    let input = raw["spec"]["states"][*state]["input"]
                        .as_str()
                        .unwrap_or("");
                    assert!(
                        input.contains(asks),
                        "{name} {state} is exempt only while its prompt asks for text: {asks:?}"
                    );
                    *state
                })
                .collect();

            for (line_number, line) in yaml.lines().enumerate() {
                if line.trim_start().starts_with('#') {
                    continue;
                }
                let mut rest = line;
                while let Some(start) = rest.find("{{") {
                    let opened = &rest[start..];
                    let Some(end) = opened.find("}}") else { break };
                    let expression = opened[..end].trim_start_matches('{').trim();
                    rest = &opened[end + 2..];
                    let path = expression.strip_prefix("blackboard.").unwrap_or(expression);
                    if let Some(state) = path.strip_suffix(".output") {
                        if agent_states.iter().any(|s| s == state) && !text_answers.contains(&state)
                        {
                            bare.push(format!("{name}:{}: {}", line_number + 1, line.trim()));
                        }
                    }
                }
            }
        }
        assert!(
            bare.is_empty(),
            "these lines render an Agent state's whole output without the json helper:\n{}",
            bare.join("\n")
        );
    }

    /// Regression test for ADR-113 attachment-layering bug.
    ///
    /// The three generation entry-point templates (agent-creator-agent,
    /// workflow-creator-validator-agent, workflow-generator-planner-agent) must
    /// signal **presence-only** that the dispatch carries attachments. They MUST
    /// NOT iterate the per-file fields in their `spec.task.prompt_template` —
    /// listing concrete `volume_id`/`path`/`name` values to the generator LLM
    /// risks the LLM hardcoding those values into the manifests it produces, which
    /// freezes a single dispatch's data into a reusable capability archetype.
    ///
    /// Per-file iteration belongs in the GENERATED agent's `spec.task.instruction`
    /// — a Handlebars template the LLM writes into the new manifest, which the
    /// temporal worker then hydrates at the generated agent's runtime with the
    /// dispatch-specific attachments.
    #[test]
    fn generation_prompts_signal_attachments_presence_only() {
        let cases = [
            ("agent-creator-agent", AGENT_GENERATOR_AGENT_TEMPLATE),
            (
                "workflow-creator-validator-agent",
                WORKFLOW_CREATOR_AGENT_TEMPLATE,
            ),
            (
                "workflow-generator-planner-agent",
                WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE,
            ),
        ];

        for (name, yaml) in cases {
            let manifest: serde_yaml::Value = serde_yaml::from_str(yaml)
                .unwrap_or_else(|e| panic!("Failed to parse builtin agent '{}': {}", name, e));
            let prompt_template = manifest["spec"]["task"]["prompt_template"]
                .as_str()
                .unwrap_or_else(|| {
                    panic!(
                        "Builtin agent '{}' must expose spec.task.prompt_template as a string",
                        name
                    )
                });

            assert!(
                prompt_template.contains("{{#if attachments}}"),
                "Builtin agent '{}' prompt_template must guard the attachments block with {{{{#if attachments}}}} so the LLM is signalled when a dispatch carries attachments. ADR-113 regression.",
                name
            );
            assert!(
                !prompt_template.contains("{{#each attachments}}"),
                "Builtin agent '{}' prompt_template MUST NOT iterate dispatch attachments with {{{{#each attachments}}}} in the generation prompt — that risks the LLM hardcoding specific file refs into the generated manifest. The generation prompt is presence-only; per-file iteration belongs in the GENERATED agent's spec.task.instruction. ADR-113 regression.",
                name
            );
            for field in [
                "{{this.volume_id}}",
                "{{this.path}}",
                "{{this.name}}",
                "{{this.mime_type}}",
                "{{this.size}}",
            ] {
                assert!(
                    !prompt_template.contains(field),
                    "Builtin agent '{}' prompt_template MUST NOT surface per-attachment field `{}` to the generator LLM — generation prompts are presence-only. ADR-113 regression.",
                    name,
                    field
                );
            }
        }
    }

    /// Regression test for the corrected ADR-113 attachment-handling design.
    ///
    /// Agent instructions are NOT Handlebars-rendered at runtime, so the
    /// generation prompts MUST NOT teach the agent-creator-agent (or the two
    /// workflow generation agents) to embed `{{#each attachments}}` /
    /// `{{#if attachments}}` blocks in the generated `spec.task.instruction`.
    /// The orchestrator merges `ExecutionInput.attachments` into the JSON
    /// `input` field before prompt rendering, so the generated agent's LLM
    /// reads `input.attachments` directly. Generation prompts MUST instead
    /// teach the LLM to reference `input.attachments` in PROSE.
    #[test]
    fn agent_creator_step_3a_does_not_teach_handlebars_iteration_in_generated_instructions() {
        let cases = [
            ("agent-creator-agent", AGENT_GENERATOR_AGENT_TEMPLATE),
            (
                "workflow-creator-validator-agent",
                WORKFLOW_CREATOR_AGENT_TEMPLATE,
            ),
            (
                "workflow-generator-planner-agent",
                WORKFLOW_GENERATOR_PLANNER_AGENT_TEMPLATE,
            ),
        ];

        for (name, yaml) in cases {
            let manifest: serde_yaml::Value = serde_yaml::from_str(yaml)
                .unwrap_or_else(|e| panic!("{} yaml parses: {}", name, e));
            let instruction = manifest["spec"]["task"]["instruction"]
                .as_str()
                .unwrap_or_else(|| panic!("{} must expose spec.task.instruction", name));

            // The old canonical teaching contained the literal phrase
            // "Use `aegis.attachment.read` on each to read its contents:"
            // immediately followed by `{{#each attachments}}`. Its presence
            // would mean we kept the broken design.
            assert!(
                !instruction.contains("on each to read its contents:"),
                "{} spec.task.instruction still embeds the broken canonical attachments-iteration teaching block — agent instructions are not Handlebars-rendered at runtime; teach the LLM to reference `input.attachments` in prose instead. ADR-113 corrected design regression.",
                name
            );
            // The corrected teaching MUST surface `input.attachments` as the
            // canonical address the generated agent's LLM should read.
            assert!(
                instruction.contains("input.attachments"),
                "{} spec.task.instruction MUST teach the LLM to reference `input.attachments` in prose so the generated agent reads its dispatched files from the merged JSON input. ADR-113 corrected design regression.",
                name
            );
            // The full body of the broken canonical block also included
            // `volume_id: {{this.volume_id}}, path: {{this.path}}` as a list
            // item template. Reject that exact composite — its presence is
            // unambiguous evidence of the old design.
            assert!(
                !instruction.contains("volume_id: {{this.volume_id}}"),
                "{} spec.task.instruction still embeds the broken canonical iteration list — agent instructions are not Handlebars-rendered at runtime. ADR-113 corrected design regression.",
                name
            );
        }
    }

    /// Regression test for Surface 4 of the ADR-113 first-class-attachments
    /// reframe. The agent-creator-agent prompt MUST enumerate the three
    /// canonical dispatch fields — `intent`, `input`, and `attachments` —
    /// as PEER fields in a structural list, not with `attachments` shoved
    /// into a special-case sub-clause. The reframe also drops the literal
    /// "ATTACHMENT-AWARENESS" heading so the rules read as standard
    /// input-handling guidance.
    #[test]
    fn agent_creator_step_3_enumerates_dispatch_fields_as_peers() {
        let manifest: serde_yaml::Value = serde_yaml::from_str(AGENT_GENERATOR_AGENT_TEMPLATE)
            .expect("agent-creator-agent yaml parses");
        let instruction = manifest["spec"]["task"]["instruction"]
            .as_str()
            .expect("agent-creator-agent must expose spec.task.instruction");

        // The old framing called this section "ATTACHMENT-AWARENESS". The
        // reframe drops that heading; its presence means we kept the old
        // single-purpose framing.
        assert!(
            !instruction.contains("ATTACHMENT-AWARENESS"),
            "agent-creator-agent step 3 must not use the 'ATTACHMENT-AWARENESS' heading anymore — attachments is one of the standard dispatch fields, not a special case. Surface 4 reframe regression."
        );

        // The new framing must enumerate intent, input, AND attachments as
        // peer fields. Each must appear as its own bulleted/structural item
        // (not just attachments referenced in a sub-clause). We assert each
        // appears as a backticked field name on its own list line.
        for marker in ["- `intent`", "- `input`", "- `attachments`"] {
            assert!(
                instruction.contains(marker),
                "agent-creator-agent step 3 must enumerate '{}' as a peer dispatch field in a structural list. Surface 4 reframe regression.",
                marker
            );
        }
    }

    /// AEGIS ADR-132 S7c and S7e, the creator half: the creator may call
    /// `aegis.tools.list`, whose `remote_servers` entries name this node's
    /// remote servers, and its step 3 tells it to read them before naming a
    /// remote server's tool. Both clauses are reported together.
    #[test]
    fn agent_creator_lists_tools_and_step_3_says_to_read_the_remote_servers() {
        let manifest: serde_yaml::Value = serde_yaml::from_str(AGENT_GENERATOR_AGENT_TEMPLATE)
            .expect("agent-creator-agent yaml parses");
        let mut complaints = Vec::new();

        let tools: Vec<&str> = manifest["spec"]["tools"]
            .as_sequence()
            .map(|t| t.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let expected = [
            "aegis.runtime.list",
            "aegis.agent.search",
            "aegis.agent.list",
            "aegis.tools.list",
            "aegis.tools.search",
            "aegis.schema.get",
            "aegis.schema.validate",
            "aegis.agent.create",
            "aegis.agent.update",
            "aegis.agent.export",
        ];
        if tools != expected {
            complaints.push(format!(
                "the creator's spec.tools are {tools:?}, not the ten tools {expected:?} naming aegis.tools.list"
            ));
        }

        let flat = manifest["spec"]["task"]["instruction"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let sentence =
            "Before naming a remote server's tool in `spec.tools`, call `aegis.tools.list`: \
             its `remote_servers` entries name this node's remote servers, the form of each \
             server's tools (`<server>.*`) and the `spec.contexts` entry that declares them.";
        if !flat.contains(sentence) {
            complaints.push(format!("the creator's step 3 does not say: {sentence}"));
        }

        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// Regression test for the agent-generator-judge under the corrected
    /// ADR-113 design. The judge MUST require generated manifests to
    /// reference `input.attachments` in prose AND MUST reject Handlebars
    /// iteration blocks in `spec.task.instruction`.
    #[test]
    fn agent_generator_judge_requires_input_attachments_prose_and_rejects_handlebars() {
        let manifest: serde_yaml::Value = serde_yaml::from_str(AGENT_GENERATOR_JUDGE_TEMPLATE)
            .expect("agent-generator-judge yaml parses");
        let instruction = manifest["spec"]["task"]["instruction"]
            .as_str()
            .expect("agent-generator-judge must expose spec.task.instruction");

        assert!(
            instruction.contains("input.attachments"),
            "agent-generator-judge must check that generated manifests reference `input.attachments` in prose. ADR-113 corrected design regression."
        );
        assert!(
            instruction.contains("{{#each attachments"),
            "agent-generator-judge must list `{{{{#each attachments`}} as a forbidden substring it rejects. ADR-113 corrected design regression."
        );
    }

    // ─── Content drift detection regression tests ──────────────────────────
    //
    // These tests cover the four cases of the built-in deploy flow:
    //   1. embedded matches deployed verbatim → no-op (UpToDate)
    //   2. embedded differs from deployed at the same metadata.version
    //      → drift detected, must overwrite WITHOUT requiring --force
    //   3. canonicalisation: trivial whitespace / key-ordering / quoting
    //      differences MUST NOT trigger a false-positive drift
    //   4. force-flag escape hatch: AEGIS_FORCE_DEPLOY_BUILTINS=true is
    //      handled by the caller, not the matcher; the matcher just reports
    //      semantic equality. We assert it does so faithfully.
    //
    // Plus a regression assertion that the LLM-create version-collision
    // gate in `StandardAgentLifecycleService::deploy_agent_for_tenant` is
    // unchanged in scope (built-ins bypass via `force=true`, user-driven
    // calls still go through `force=false`).

    #[test]
    fn agent_drift_matches_when_content_identical() {
        // Use a real built-in template as ground truth for the matcher —
        // identical embedded and deployed bytes must report no drift.
        let deployed: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(HELLO_WORLD_TEMPLATE).unwrap();
        assert!(
            agent_manifest_matches(&deployed, HELLO_WORLD_TEMPLATE).unwrap(),
            "identical embedded + deployed manifests must match"
        );
    }

    #[test]
    fn agent_drift_detected_when_content_differs_at_same_version() {
        // Simulate a deployed manifest that drifted from the embedded
        // template at the SAME metadata.version. Mutating the description
        // is enough — the matcher compares full canonicalised content, not
        // just the version field.
        let mut deployed: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(HELLO_WORLD_TEMPLATE).unwrap();
        deployed.metadata.description =
            Some("MUTATED — simulates content drift from embedded template".to_string());

        // Sanity: same version, different content.
        let embedded: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(HELLO_WORLD_TEMPLATE).unwrap();
        assert_eq!(
            deployed.metadata.version, embedded.metadata.version,
            "test pre-condition: same version, drifted content"
        );

        assert!(
            !agent_manifest_matches(&deployed, HELLO_WORLD_TEMPLATE).unwrap(),
            "content drift at the same metadata.version must be detected — \
             this is the regression that AEGIS_FORCE_DEPLOY_BUILTINS=true was \
             papering over"
        );
    }

    #[test]
    fn agent_drift_ignores_trivial_whitespace_and_key_ordering() {
        // Round-trip the embedded template through serde_yaml: the
        // re-serialised form will differ from the source bytes (key
        // ordering, quoting, comment loss) but be semantically identical.
        // The matcher MUST report no drift, otherwise every deploy would
        // re-write every built-in — re-introducing the band-aid.
        let parsed: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(HELLO_WORLD_TEMPLATE).unwrap();
        let reserialised = serde_yaml::to_string(&parsed).unwrap();
        assert_ne!(
            reserialised.trim(),
            HELLO_WORLD_TEMPLATE.trim(),
            "test pre-condition: re-serialised YAML differs textually from source"
        );
        assert!(
            agent_manifest_matches(&parsed, &reserialised).unwrap(),
            "trivial whitespace / key-ordering / quoting differences MUST NOT \
             register as content drift — that would re-introduce the \
             AEGIS_FORCE_DEPLOY_BUILTINS band-aid every deploy"
        );
    }

    #[test]
    fn builtin_not_deployed_falls_through_to_install() {
        // The `NotDeployed` outcome is represented by callers as "lookup
        // returned None"; the matcher itself isn't invoked. We assert the
        // enum variant exists and is distinct so the call sites have a
        // type-checked third state to dispatch on.
        let outcomes = [
            BuiltinDriftOutcome::NotDeployed,
            BuiltinDriftOutcome::UpToDate,
            BuiltinDriftOutcome::ContentDrift,
        ];
        // Ensure no two variants compare equal — distinct dispatch states.
        for (i, a) in outcomes.iter().enumerate() {
            for (j, b) in outcomes.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
    }

    #[test]
    fn force_deploy_builtins_escape_hatch_bypasses_matcher() {
        // The matcher itself is pure — it always reports semantic equality.
        // The AEGIS_FORCE_DEPLOY_BUILTINS escape hatch is a CALLER-SIDE
        // override: when the env var is set, the deploy code skips calling
        // the matcher and re-writes unconditionally. We verify the matcher
        // correctly reports "match" against the source-of-truth template
        // so the escape hatch is the ONLY thing that triggers re-write of
        // an up-to-date built-in.
        let deployed: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(HELLO_WORLD_TEMPLATE).unwrap();
        let matched = agent_manifest_matches(&deployed, HELLO_WORLD_TEMPLATE).unwrap();
        assert!(
            matched,
            "matcher reports up-to-date for an unchanged built-in; the only \
             remaining trigger for an overwrite must be the operator-side \
             AEGIS_FORCE_DEPLOY_BUILTINS escape hatch"
        );

        // And also assert the daemon server still consumes the env var so
        // the escape hatch is wired end-to-end.
        let server_src = include_str!("../daemon/server.rs");
        assert!(
            server_src.contains("force_deploy_builtins"),
            "AEGIS_FORCE_DEPLOY_BUILTINS escape hatch must remain wired into \
             the daemon server startup deploy path"
        );
        assert!(
            server_src.contains("if force_builtins {"),
            "daemon server must still short-circuit the drift comparison when \
             the operator sets AEGIS_FORCE_DEPLOY_BUILTINS=true"
        );
    }

    #[test]
    fn workflow_drift_matches_when_content_identical() {
        // Round-trip a real built-in workflow template through the parser and
        // assert the matcher reports no drift against itself.
        let workflow =
            aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
                INTENT_EXECUTION_WORKFLOW_TEMPLATE,
            )
            .unwrap();
        assert!(
            workflow_matches(&workflow, INTENT_EXECUTION_WORKFLOW_TEMPLATE, None).unwrap(),
            "identical embedded + deployed workflows must match"
        );
    }

    #[test]
    fn workflow_drift_detected_when_content_differs() {
        // Take the live built-in workflow as 'embedded'. Mutate the parsed
        // 'deployed' to simulate a drifted-on-disk version (description tweak
        // is enough to register as drift since spec serialises into the
        // canonical Value).
        let mut deployed =
            aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
                INTENT_EXECUTION_WORKFLOW_TEMPLATE,
            )
            .unwrap();
        deployed.metadata.description =
            Some("MUTATED — simulates content drift from embedded template".to_string());

        assert!(
            !workflow_matches(&deployed, INTENT_EXECUTION_WORKFLOW_TEMPLATE, None).unwrap(),
            "content drift in workflow metadata must be detected"
        );
    }

    #[test]
    fn llm_agent_create_version_collision_still_rejects() {
        // SCOPE PRESERVATION: this test asserts our content-drift fix does
        // NOT widen scope into the LLM-driven aegis.agent.create / .update
        // path. Built-ins bypass the version-collision gate by calling
        // `deploy_agent_for_tenant(force=true)`. User-driven LLM calls
        // continue to flow through `force=false` and MUST still be rejected
        // when name + version collide.
        //
        // We assert the canonical error string this gate produces is still
        // present in the lifecycle source. If the gate is removed or its
        // wording changes, this test fires and forces a deliberate review of
        // whether the LLM-side contract was changed by accident.
        let lifecycle_src = include_str!("../../../orchestrator/core/src/application/lifecycle.rs");
        assert!(
            lifecycle_src.contains("is already deployed (ID:"),
            "LLM agent.create version-collision gate must still exist in \
             StandardAgentLifecycleService::deploy_agent_for_tenant — the \
             content-drift fix is scoped to platform built-ins only and MUST \
             NOT remove the user-facing version-collision rejection"
        );
        assert!(
            lifecycle_src.contains("Use --force to overwrite it."),
            "LLM agent.create version-collision gate must still surface the \
             --force escape hatch in its error message"
        );
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

    /// The generator's step 6 example, as a manifest: the lines after its
    /// heading, dedented, under a metadata block.
    fn generator_example() -> Option<aegis_orchestrator_core::domain::agent::AgentManifest> {
        let heading = "Example for an agent whose task is a computation (it carries its program):";
        let lines: Vec<&str> = AGENT_GENERATOR_AGENT_TEMPLATE.lines().collect();
        let start = lines.iter().position(|l| l.trim() == heading)?;
        let indent = lines[start + 1].len() - lines[start + 1].trim_start().len();
        let body: Vec<&str> = lines[start + 1..]
            .iter()
            .take_while(|l| l.trim().is_empty() || l.len() - l.trim_start().len() >= indent)
            .map(|l| if l.len() >= indent { &l[indent..] } else { "" })
            .collect();
        let yaml = format!(
            "apiVersion: 100monkeys.ai/v1\nkind: Agent\nmetadata:\n  name: sum-agent\n  version: \"1.0.0\"\n{}\n",
            body.join("\n")
        );
        Some(
            serde_yaml::from_str(&yaml)
                .unwrap_or_else(|e| panic!("the example parses: {e}\n{yaml}")),
        )
    }

    /// T7, AEGIS ADR-005 O7: the generator's example is an agent that carries
    /// its program, runs it with cmd.run, and passes the manifest's own checks
    /// and the tool requirement; the example that had the model write the
    /// solution on each run is gone.
    #[test]
    fn the_generators_example_carries_its_program_and_passes_every_check() {
        let mut complaints = Vec::new();
        for gone in [
            "Write a complete Python solution",
            "Confirm the write succeeded",
        ] {
            if AGENT_GENERATOR_AGENT_TEMPLATE.contains(gone) {
                complaints.push(format!("the generator still teaches \"{gone}\""));
            }
        }
        if !AGENT_GENERATOR_AGENT_TEMPLATE
            .contains("AN AGENT WHOSE TASK IS A COMPUTATION CARRIES ITS PROGRAM.")
        {
            complaints.push("the generator states no program rule".to_string());
        }
        let Some(example) = generator_example() else {
            complaints
                .push("the generator has no example of an agent carrying its program".to_string());
            panic!("{}", complaints.join("\n"));
        };
        if let Err(e) = example.validate() {
            complaints.push(format!("validate refused the example: {e}"));
        }
        if let Err(e) = aegis_orchestrator_core::domain::tool_requirement::check(&example) {
            complaints.push(format!("the tool requirement refused the example: {e}"));
        }
        match &example.spec.program {
            None => complaints.push("the example carries no program".to_string()),
            Some(program) => {
                if program.run != "python /opt/aegis/program/solve.py"
                    || program.sample_input.is_none()
                    || program.files.len() != 1
                {
                    complaints.push(format!("the example's program: {program:?}"));
                }
            }
        }
        let instruction = example
            .spec
            .task
            .as_ref()
            .and_then(|t| t.instruction.clone())
            .unwrap_or_default();
        if !instruction.contains("Call cmd.run with command \"python /opt/aegis/program/solve.py\"")
        {
            complaints.push(format!(
                "the example's instruction does not run the program: {instruction}"
            ));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T7, AEGIS ADR-005 O7: the judge's program audit refuses an agent with
    /// no program with O7's sentence word for word and reads `program_check`.
    #[test]
    fn the_generator_judge_audits_the_program_with_o7s_sentence() {
        let manifest: serde_yaml::Value =
            serde_yaml::from_str(AGENT_GENERATOR_JUDGE_TEMPLATE).expect("the judge parses");
        let instruction = manifest["spec"]["task"]["instruction"]
            .as_str()
            .expect("the judge's instruction")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut complaints = Vec::new();
        for needed in [
            "PROGRAM AUDIT — an agent whose task is a computation carries its program:",
            "note exactly: \"agent '<name>' has no program: its instruction asks the model to write or compute the solution on each run; the agent must carry its program and run it\"",
            "Read the `program_check` that `aegis.agent.create` or `aegis.agent.update` answered",
        ] {
            if !instruction.contains(needed) {
                complaints.push(format!("the judge lacks \"{needed}\""));
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// A template's instruction, its whitespace folded to single spaces.
    fn folded_instruction(template: &str) -> String {
        let manifest: serde_yaml::Value =
            serde_yaml::from_str(template).expect("the template parses");
        manifest["spec"]["task"]["instruction"]
            .as_str()
            .expect("the template's instruction")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// T1: the generator teaches that an agent declares every file it
    /// writes, and its example `execution.outputs` block, added to the
    /// generator's own example, passes the manifest's checks.
    #[test]
    fn the_generator_teaches_an_agent_to_declare_the_files_it_writes() {
        let mut complaints = Vec::new();
        let rule = "AN AGENT WHOSE TASK WRITES A FILE DECLARES IT.";
        let lines: Vec<&str> = AGENT_GENERATOR_AGENT_TEMPLATE.lines().collect();
        let Some(start) = lines.iter().position(|l| l.contains(rule)) else {
            panic!("the generator states no output declaration rule: \"{rule}\" is not in it");
        };
        let Some(at) = lines[start..]
            .iter()
            .position(|l| l.trim() == "execution:")
            .map(|i| start + i)
        else {
            panic!("the generator's output rule has no execution example");
        };
        let indent = lines[at].len() - lines[at].trim_start().len();
        let block: Vec<&str> = lines[at..]
            .iter()
            .take_while(|l| l.len() - l.trim_start().len() >= indent && !l.trim().is_empty())
            .map(|l| &l[indent..])
            .collect();
        println!("the generator's output example:\n{}", block.join("\n"));
        let execution: serde_yaml::Value =
            serde_yaml::from_str(&block.join("\n")).expect("the output example parses");
        let path = execution["execution"]["outputs"][0]["path"].as_str();
        if path != Some("/workspace/itinerary.md") {
            complaints.push(format!("the example declares {path:?}"));
        }
        let Some(mut example) = generator_example() else {
            panic!("the generator has no example manifest to add the outputs to");
        };
        let strategy: aegis_orchestrator_core::domain::agent::ExecutionStrategy =
            serde_yaml::from_value(execution["execution"].clone())
                .expect("the output example is an execution block");
        example.spec.execution = Some(strategy);
        if let Err(e) = example.validate() {
            complaints.push(format!("validate refused the declared output: {e}"));
        }
        let instruction = folded_instruction(AGENT_GENERATOR_AGENT_TEMPLATE);
        if !instruction.contains("Every file the instruction tells the model to write under /workspace is listed, at that exact path, in `spec.execution.outputs`") {
            complaints.push("the rule does not say every written file is listed".to_string());
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T2: the generator judge's output declaration audit carries the
    /// floor's sentence word for word.
    #[test]
    fn the_generator_judge_audits_the_declared_outputs_with_the_floors_sentence() {
        let instruction = folded_instruction(AGENT_GENERATOR_JUDGE_TEMPLATE);
        let mut complaints = Vec::new();
        for needed in [
            "OUTPUT DECLARATION AUDIT — an agent that writes a file declares it:",
            "and `spec.execution.outputs` does not list that path, set `deployability` to 0.0",
            "note exactly: \"agent '<name>' writes <path> but does not declare it in spec.execution.outputs\"",
        ] {
            if !instruction.contains(needed) {
                complaints.push(format!("the judge lacks \"{needed}\""));
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T12: the goal judge is told that `produced_files` also holds the
    /// undeclared files a run's answer named, marked `declared: false`, and
    /// that such a file counts at lower confidence.
    #[test]
    fn the_goal_judge_reads_undeclared_produced_files_at_lower_confidence() {
        let instruction = folded_instruction(GOAL_JUDGE_TEMPLATE);
        let mut complaints = Vec::new();
        for needed in [
            "the outputs the execution declared, and each /workspace file its last_output names that no output declared, marked declared: false",
            "an entry marked declared: false was found in the volume when it completed but no output required it, and counts at lower confidence",
        ] {
            if !instruction.contains(needed) {
                complaints.push(format!("the goal judge lacks \"{needed}\""));
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T7, AEGIS ADR-135 D5: each executor must call a tool before a text
    /// answer completes it, and none forbids a package the image holds.
    #[test]
    fn the_executors_require_a_tool_call() {
        let mut complaints = Vec::new();
        for (name, template) in [
            (
                AEGIS_PYTHON_EXECUTOR_AGENT_NAME,
                AEGIS_PYTHON_EXECUTOR_AGENT_TEMPLATE,
            ),
            (
                AEGIS_BASH_EXECUTOR_AGENT_NAME,
                AEGIS_BASH_EXECUTOR_AGENT_TEMPLATE,
            ),
            (
                AEGIS_JAVASCRIPT_EXECUTOR_AGENT_NAME,
                AEGIS_JAVASCRIPT_EXECUTOR_AGENT_TEMPLATE,
            ),
        ] {
            let manifest: aegis_orchestrator_core::domain::agent::AgentManifest =
                serde_yaml::from_str(template).expect("the executor parses");
            if aegis_orchestrator_core::domain::agent::ToolCallRequirement::of(&manifest)
                != aegis_orchestrator_core::domain::agent::ToolCallRequirement::AnyTool
            {
                complaints.push(format!("{name} does not require a tool call"));
            }
            if template.contains("standard library (no") {
                complaints.push(format!("{name} still forbids every package"));
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// The built-in judges whose verdict a `json_schema` step validates, with
    /// the signal categories each one's instruction asks for.
    fn schema_judges() -> [(&'static str, &'static str, [&'static str; 5]); 5] {
        [
            (
                GOAL_JUDGE_NAME,
                GOAL_JUDGE_TEMPLATE,
                ["delivered", "evidence", "chain", "feasibility", "alignment"],
            ),
            (
                AGENT_GENERATOR_JUDGE_NAME,
                AGENT_GENERATOR_JUDGE_TEMPLATE,
                [
                    "correctness",
                    "completeness",
                    "safety",
                    "deployability",
                    "alignment",
                ],
            ),
            (
                WORKFLOW_GENERATOR_JUDGE_NAME,
                WORKFLOW_GENERATOR_JUDGE_TEMPLATE,
                [
                    "correctness",
                    "transition_safety",
                    "completeness",
                    "deployability",
                    "alignment",
                ],
            ),
            (
                "code-quality-judge",
                CODE_QUALITY_JUDGE_TEMPLATE,
                [
                    "correctness",
                    "security",
                    "maintainability",
                    "performance",
                    "robustness",
                ],
            ),
            (
                "tool-call-policy-judge",
                TOOL_CALL_POLICY_JUDGE_TEMPLATE,
                [
                    "policy_adherence",
                    "tool_specificity",
                    "parameter_validity",
                    "intent_alignment",
                    "execution_safety",
                ],
            ),
        ]
    }

    /// The `json_schema` step of a judge template, as the validation pipeline
    /// builds it: the output validator on format `json` with that schema.
    fn schema_validator_of(
        template: &str,
    ) -> aegis_orchestrator_core::domain::validation::OutputGradientValidator {
        let doc: serde_json::Value = serde_yaml::from_str(template).unwrap();
        let schema = doc["spec"]["execution"]["validation"]
            .as_array()
            .and_then(|steps| steps.iter().find(|s| s["type"] == "json_schema"))
            .map(|step| step["schema"].clone())
            .expect("the judge validates its verdict with a json_schema step");
        aegis_orchestrator_core::domain::validation::OutputGradientValidator::new(
            "json".to_string(),
            Some(schema),
            None,
        )
    }

    fn verdict_of(categories: &[&str], reasoning: &str, message: &str) -> serde_json::Value {
        serde_json::json!({
            "score": 1.0,
            "confidence": 1.0,
            "reasoning": reasoning,
            "signals": categories
                .iter()
                .map(|c| serde_json::json!({"category": c, "score": 1.0, "message": message}))
                .collect::<Vec<_>>(),
        })
    }

    /// The score a judge template's `json_schema` step gives `output`, and why.
    async fn schema_score(template: &str, output: String) -> (f64, String) {
        use aegis_orchestrator_core::domain::validation::{GradientValidator, ValidationContext};
        let result = schema_validator_of(template)
            .validate(&ValidationContext {
                task: "judge".to_string(),
                output,
                exit_code: 0,
                stderr: String::new(),
                worker_mounts: vec![],
                policy_violations: vec![],
                tool_trajectory: vec![],
            })
            .await
            .expect("the schema compiles");
        (result.score, result.reasoning)
    }

    /// A judge's complete verdict in its shortest honest form, "Nothing is
    /// missing." with one-word notes, fenced or bare, passes the judge's own
    /// schema: a schema that refused it would send a finished verdict round
    /// the refine loop to its cap.
    #[tokio::test]
    async fn every_judge_schema_accepts_its_complete_short_verdict_fenced_or_bare() {
        let mut complaints = Vec::new();
        for (name, template, categories) in schema_judges() {
            let verdict = verdict_of(&categories, "Nothing is missing.", "Met.");
            for (form, output) in [
                ("bare", verdict.to_string()),
                (
                    "fenced",
                    format!(
                        "```json\n{}\n```",
                        serde_json::to_string_pretty(&verdict).unwrap()
                    ),
                ),
            ] {
                let (score, why) = schema_score(template, output).await;
                if score < 1.0 {
                    complaints.push(format!(
                        "{name} refuses its own complete short verdict ({form}): {why}"
                    ));
                }
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// A verdict that is not one still fails every judge's schema: not JSON,
    /// an empty reasoning, an empty note, a missing signal.
    #[tokio::test]
    async fn every_judge_schema_still_refuses_a_malformed_verdict() {
        let mut complaints = Vec::new();
        for (name, template, categories) in schema_judges() {
            let mut missing_signal = verdict_of(&categories, "Nothing is missing.", "Met.");
            missing_signal["signals"].as_array_mut().unwrap().pop();
            for (what, output) in [
                ("prose", "The goal is met.".to_string()),
                (
                    "an empty reasoning",
                    verdict_of(&categories, "", "Met.").to_string(),
                ),
                (
                    "an empty note",
                    verdict_of(&categories, "Nothing is missing.", "").to_string(),
                ),
                ("four signals", missing_signal.to_string()),
            ] {
                let (score, _) = schema_score(template, output).await;
                if score > 0.0 {
                    complaints.push(format!("{name} passes a verdict with {what}"));
                }
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// The goal judge's sentence for a goal with no gap asks for one sentence
    /// naming what was done, not a bare "say so".
    #[test]
    fn goal_judge_says_a_met_goal_in_one_sentence_naming_what_was_done() {
        let doc: serde_yaml::Value = serde_yaml::from_str(GOAL_JUDGE_TEMPLATE).unwrap();
        let instruction = doc["spec"]["task"]["instruction"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let no_gap = instruction
            .split(". ")
            .find(|sentence| sentence.contains("When nothing is missing"))
            .unwrap_or("none");
        assert!(
            instruction.contains(
                "When nothing is missing, say so in one sentence that names what was done."
            ),
            "the goal judge's no-gap sentence does not ask for what was done: {no_gap:?}"
        );
    }

    /// The document renderer is a built-in agent whose program is fixed: it is
    /// listed, it is the template on disk, it validates, it runs its one
    /// program with `cmd.run`, and the document never reaches the model (its
    /// prompt carries no `{{input}}`).
    #[test]
    fn document_renderer_is_a_builtin_agent_with_a_fixed_program() {
        let mut complaints: Vec<String> = Vec::new();
        let on_disk = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("templates/agents/aegis-document-renderer-agent.yaml"),
        )
        .unwrap_or_default();
        match BUILTIN_AGENTS
            .iter()
            .find(|(name, _)| *name == "aegis-document-renderer-agent")
        {
            None => complaints
                .push("aegis-document-renderer-agent is not in BUILTIN_AGENTS".to_string()),
            Some((_, yaml)) => {
                if *yaml != on_disk {
                    complaints.push(
                        "the listed template is not cli/templates/agents/aegis-document-renderer-agent.yaml"
                            .to_string(),
                    );
                }
                match serde_yaml::from_str::<aegis_orchestrator_sdk::AgentManifest>(yaml) {
                    Err(e) => complaints.push(format!("the template does not parse: {e}")),
                    Ok(manifest) => {
                        if let Err(e) = manifest.validate() {
                            complaints.push(format!("the template does not validate: {e}"));
                        }
                    }
                }
                let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap_or_default();
                let spec = &doc["spec"];
                let run = spec["program"]["run"].as_str().unwrap_or("");
                if run != "python /opt/aegis/program/render.py" {
                    complaints.push(format!("its program runs {run:?}"));
                }
                let tools: Vec<&str> = spec["tools"]
                    .as_sequence()
                    .map(|t| t.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                if tools != ["cmd.run"] {
                    complaints.push(format!("its tools are {tools:?}"));
                }
                if spec["execution"]["max_iterations"].as_u64() != Some(1) {
                    complaints.push("its max_iterations is not 1".to_string());
                }
                let template = spec["task"]["prompt_template"].as_str().unwrap_or("");
                if template.contains("{{input}}") {
                    complaints.push("its prompt carries {{input}}".to_string());
                }
                if spec["program"]["sample_input"].is_null() {
                    complaints.push("its program has no sample_input".to_string());
                }
                if spec["runtime"]["language"].as_str() != Some("document")
                    || spec["runtime"]["version"].as_str() != Some("1")
                {
                    complaints.push("its runtime is not document 1".to_string());
                }
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// AEGIS ADR-147 (carbon-forge, Proposed, page revision 55541), clauses
    /// C1, C3, C4, C5 and C6 as the template holds them: `carbon-forge` is a
    /// second built-in registered beside `the-forge` (C1); it keeps every
    /// state of the-forge, and DIFF, COMMIT and LAND are the Action states
    /// `tree_diff`, `tree_write` and `tree_land`, with no repository step
    /// left (C3); every ContainerRun mounts the run's tree volume `tree` at
    /// `/workspace` and none the run's repository, and the workflow works on
    /// no repository (C2, C4); a refused landing ends in LAND_REFUSED
    /// carrying the landing's own sentence, Carbon Forge's (C5); `draft_only`
    /// defaults to true and routes the write to COMPLETE (C6).
    #[test]
    fn carbon_forge_is_a_second_builtin_beside_the_forge_on_a_nuclear_notes_tree() {
        let names: Vec<&str> = BUILTIN_WORKFLOWS.iter().map(|(n, _)| *n).collect();
        let forge_at = names
            .iter()
            .position(|n| *n == "the-forge")
            .expect("the-forge is a built-in");
        let Some(carbon_at) = names.iter().position(|n| *n == "carbon-forge") else {
            panic!(
                "AEGIS ADR-147 C1: carbon-forge is not registered beside the-forge; the built-in workflows are {names:?}"
            );
        };
        assert_eq!(
            carbon_at,
            forge_at + 1,
            "AEGIS ADR-147 C1: carbon-forge is registered at {carbon_at}, not beside the-forge at {forge_at}"
        );
        let forge_yaml = BUILTIN_WORKFLOWS[forge_at].1;
        let carbon_yaml = BUILTIN_WORKFLOWS[carbon_at].1;
        aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
            carbon_yaml,
        )
        .unwrap_or_else(|e| panic!("AEGIS ADR-147 C1: carbon-forge does not parse: {e}"));

        let forge: serde_yaml::Value = serde_yaml::from_str(forge_yaml).unwrap();
        let carbon: serde_yaml::Value = serde_yaml::from_str(carbon_yaml).unwrap();
        let mut complaints = Vec::new();
        if carbon["metadata"]["name"].as_str() != Some("carbon-forge") {
            complaints.push(format!(
                "C1: its metadata.name is {:?}",
                carbon["metadata"]["name"]
            ));
        }
        let states = &carbon["spec"]["states"];
        let forge_states = forge["spec"]["states"].as_mapping().unwrap();
        for name in forge_states.keys() {
            if states.get(name).is_none() {
                complaints.push(format!("C3: the-forge's state {name:?} is not kept"));
            }
        }
        for (state, action) in [
            ("DIFF", "tree_diff"),
            ("COMMIT", "tree_write"),
            ("LAND", "tree_land"),
        ] {
            let kind = states[state]["kind"].as_str();
            let named = states[state]["action"].as_str();
            if kind != Some("Action") || named != Some(action) {
                complaints.push(format!(
                    "C3: {state} is kind {kind:?} action {named:?}, not the Action {action}"
                ));
            }
        }
        if let Some(states) = states.as_mapping() {
            for (name, state) in states {
                if let Some(action) = state["action"].as_str() {
                    if action.starts_with("repository_") {
                        complaints.push(format!("C3: {name:?} still runs {action}"));
                    }
                }
                let mounts = state["volumes"].as_sequence().cloned().unwrap_or_default();
                if state["kind"].as_str() == Some("ContainerRun") {
                    let tree_at_workspace = mounts.iter().any(|m| {
                        m["name"].as_str() == Some("tree")
                            && m["mount_path"].as_str() == Some("/workspace")
                    });
                    if !tree_at_workspace {
                        complaints.push(format!(
                            "C4: the ContainerRun {name:?} does not mount the volume tree at /workspace"
                        ));
                    }
                }
                for m in &mounts {
                    if m["name"]
                        .as_str()
                        .is_some_and(|n| n == "repository" || n.starts_with("repository:"))
                    {
                        complaints.push(format!("C4: {name:?} mounts the run's repository"));
                    }
                }
            }
        }
        if !carbon["spec"]["repositories"].is_null() {
            complaints.push(format!(
                "C2: it works on repositories ({:?}); a run names its tree by `trees`",
                carbon["spec"]["repositories"]
            ));
        }
        let draft_only = &carbon["metadata"]["input_schema"]["properties"]["draft_only"];
        if draft_only["default"].as_bool() != Some(true) {
            complaints.push(format!(
                "C6: draft_only's default is {:?}, not true",
                draft_only["default"]
            ));
        }
        let routes = |state: &str| -> Vec<(String, String)> {
            states[state]["transitions"]
                .as_sequence()
                .map(|ts| {
                    ts.iter()
                        .map(|t| {
                            (
                                format!(
                                    "{} {}",
                                    t["condition"].as_str().unwrap_or(""),
                                    t["expression"].as_str().unwrap_or("")
                                )
                                .trim()
                                .to_string(),
                                t["target"].as_str().unwrap_or("").to_string(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let write = routes("COMMIT");
        let draft_first = write
            .iter()
            .position(|(c, t)| c.contains("input.draft_only") && t == "COMPLETE");
        let land_at = write.iter().position(|(_, t)| t == "LAND");
        if !matches!((draft_first, land_at), (Some(d), Some(l)) if d < l) {
            complaints.push(format!(
                "C6: COMMIT does not end a draft_only run at COMPLETE before LAND: {write:?}"
            ));
        }
        if !routes("LAND")
            .iter()
            .any(|(c, t)| c == "on_failure" && t == "LAND_REFUSED")
        {
            complaints.push(format!(
                "C5: a refused landing does not end in LAND_REFUSED: {:?}",
                routes("LAND")
            ));
        }
        if states["LAND_REFUSED"]["args"]["sentence"].as_str() != Some("{{{LAND.output.sentence}}}")
        {
            complaints.push(format!(
                "C5: LAND_REFUSED does not carry the landing's own sentence: {:?}",
                states["LAND_REFUSED"]["args"]["sentence"]
            ));
        }
        assert!(
            complaints.is_empty(),
            "AEGIS ADR-147: carbon-forge is not the template the clauses name:\n{}",
            complaints.join("\n")
        );
    }

    /// AEGIS ADR-149 C1 (deploys key to cuts, Proposed, page revision
    /// 55895): a landed carbon-forge run becomes a cut. LAND goes on to the
    /// Action state CUT (`tree_cut`, its name the dispatch's `cut`), CUT to
    /// COMPLETE or CUT_REFUSED with the cut's own sentence, and COMPLETE
    /// records the cut (riding `ref`) and its snapshot (riding `commit_sha`).
    #[test]
    fn carbon_forge_cuts_the_landed_run() {
        let carbon_yaml = BUILTIN_WORKFLOWS
            .iter()
            .find(|(n, _)| *n == "carbon-forge")
            .map(|(_, y)| *y)
            .expect("carbon-forge is a built-in");
        aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
            carbon_yaml,
        )
        .unwrap_or_else(|e| panic!("AEGIS ADR-149 C1: carbon-forge does not parse: {e}"));
        let carbon: serde_yaml::Value = serde_yaml::from_str(carbon_yaml).unwrap();
        let states = &carbon["spec"]["states"];
        let target = |state: &str, condition: &str| -> Option<String> {
            states[state]["transitions"].as_sequence().and_then(|ts| {
                ts.iter()
                    .find(|t| t["condition"].as_str() == Some(condition))
                    .and_then(|t| t["target"].as_str().map(str::to_string))
            })
        };
        let mut complaints = Vec::new();
        if states["CUT"]["kind"].as_str() != Some("Action")
            || states["CUT"]["action"].as_str() != Some("tree_cut")
        {
            complaints.push(format!(
                "C1: CUT is kind {:?} action {:?}, not the Action tree_cut",
                states["CUT"]["kind"], states["CUT"]["action"]
            ));
        }
        if states["CUT"]["args"]["cut"].as_str() != Some("{{input.cut}}") {
            complaints.push(format!(
                "C1: CUT names its cut {:?}, not the dispatch's",
                states["CUT"]["args"]["cut"]
            ));
        }
        for (state, condition, to) in [
            ("LAND", "on_success", "CUT"),
            ("CUT", "on_success", "COMPLETE"),
            ("CUT", "on_failure", "CUT_REFUSED"),
        ] {
            if target(state, condition).as_deref() != Some(to) {
                complaints.push(format!(
                    "C1: {state} {condition} goes to {:?}, not {to}",
                    target(state, condition)
                ));
            }
        }
        if states["CUT_REFUSED"]["args"]["sentence"].as_str() != Some("{{{CUT.output.sentence}}}") {
            complaints.push(format!(
                "C1: CUT_REFUSED does not carry the cut's own sentence: {:?}",
                states["CUT_REFUSED"]["args"]["sentence"]
            ));
        }
        for (key, value) in [
            ("cut", "{{CUT.output.ref}}"),
            ("snapshot_id", "{{CUT.output.commit_sha}}"),
        ] {
            if states["COMPLETE"]["args"][key].as_str() != Some(value) {
                complaints.push(format!(
                    "C1: COMPLETE's {key} is {:?}, not {value}",
                    states["COMPLETE"]["args"][key]
                ));
            }
        }
        let cut = &carbon["metadata"]["input_schema"]["properties"]["cut"];
        if cut["type"].as_str() != Some("string") {
            complaints.push(format!("C1: input_schema has no string cut: {cut:?}"));
        }
        if carbon["metadata"]["version"].as_str() == Some("1.0.0") {
            complaints.push("C1: the template's version did not bump".to_string());
        }
        assert!(
            complaints.is_empty(),
            "AEGIS ADR-149: carbon-forge does not cut the landed run:\n{}",
            complaints.join("\n")
        );
    }

    /// AEGIS ADR-149 C2, C3, C4: `carbon-deploy` is a built-in registered
    /// after carbon-forge; it takes `cut`, `environment` and `trees`; RENDER
    /// (`archive_render`), PUSH (`archive_push`) and COMPLETE, with
    /// RENDER_REFUSED and PUSH_REFUSED carrying the sentence; COMPLETE's
    /// blackboard keys are the deploy's record; every state is an Action
    /// needing only the tree's name, so a start materializes nothing; its
    /// description says a revert is a deploy of an earlier cut.
    #[test]
    fn carbon_deploy_is_a_builtin_rendering_and_pushing_a_cut() {
        use aegis_orchestrator_core::domain::workflow::{ActionNeed, StateKind};
        let names: Vec<&str> = BUILTIN_WORKFLOWS.iter().map(|(n, _)| *n).collect();
        let carbon_at = names.iter().position(|n| *n == "carbon-forge").unwrap();
        let Some(deploy_at) = names.iter().position(|n| *n == "carbon-deploy") else {
            panic!(
                "AEGIS ADR-149 C2: carbon-deploy is not registered; the built-in workflows are {names:?}"
            );
        };
        assert_eq!(
            deploy_at,
            carbon_at + 1,
            "AEGIS ADR-149 C2: carbon-deploy is not registered after carbon-forge"
        );
        let deploy_yaml = BUILTIN_WORKFLOWS[deploy_at].1;
        let parsed =
            aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
                deploy_yaml,
            )
            .unwrap_or_else(|e| panic!("AEGIS ADR-149 C2: carbon-deploy does not parse: {e}"));
        let deploy: serde_yaml::Value = serde_yaml::from_str(deploy_yaml).unwrap();
        let states = &deploy["spec"]["states"];
        let mut complaints = Vec::new();
        if deploy["metadata"]["name"].as_str() != Some("carbon-deploy") {
            complaints.push(format!(
                "C2: metadata.name is {:?}",
                deploy["metadata"]["name"]
            ));
        }
        let schema = &deploy["metadata"]["input_schema"];
        let required: Vec<&str> = schema["required"]
            .as_sequence()
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        for key in ["cut", "environment"] {
            if schema["properties"][key]["type"].as_str() != Some("string")
                || !required.contains(&key)
            {
                complaints.push(format!("C2: input {key} is not a required string"));
            }
        }
        if schema["properties"]["trees"].is_null() {
            complaints.push("C2: input trees is not taken".to_string());
        }
        for (state, action, refused) in [
            ("RENDER", "archive_render", "RENDER_REFUSED"),
            ("PUSH", "archive_push", "PUSH_REFUSED"),
        ] {
            if states[state]["kind"].as_str() != Some("Action")
                || states[state]["action"].as_str() != Some(action)
            {
                complaints.push(format!("C2: {state} is not the Action {action}"));
            }
            let fails_to = states[state]["transitions"].as_sequence().and_then(|ts| {
                ts.iter()
                    .find(|t| t["condition"].as_str() == Some("on_failure"))
                    .and_then(|t| t["target"].as_str())
            });
            if fails_to != Some(refused) {
                complaints.push(format!("C2: {state} on_failure goes to {fails_to:?}"));
            }
            let sentence = format!("{{{{{{{state}.output.sentence}}}}}}");
            if states[refused]["args"]["sentence"].as_str() != Some(sentence.as_str()) {
                complaints.push(format!(
                    "C2: {refused} does not carry {state}'s sentence: {:?}",
                    states[refused]["args"]["sentence"]
                ));
            }
        }
        if states["RENDER"]["args"]["cut"].as_str() != Some("{{input.cut}}") {
            complaints.push("C2: RENDER does not render the input's cut".to_string());
        }
        for (key, value) in [
            ("outcome", "complete"),
            ("cut", "{{input.cut}}"),
            ("environment", "{{input.environment}}"),
            ("snapshot_id", "{{RENDER.output.branch}}"),
            ("commit_sha1", "{{RENDER.output.commit_sha}}"),
            ("push_job_id", "{{PUSH.output.commit_sha}}"),
        ] {
            if states["COMPLETE"]["args"][key].as_str() != Some(value) {
                complaints.push(format!(
                    "C3: COMPLETE's {key} is {:?}, not {value}",
                    states["COMPLETE"]["args"][key]
                ));
            }
        }
        for (name, state) in &parsed.spec.states {
            match &state.kind {
                StateKind::Action { action, .. } if action.needs() != ActionNeed::RunTree => {}
                _ => complaints.push(format!(
                    "C2: {} would have its tree materialized at the start",
                    name.as_str()
                )),
            }
        }
        let description = deploy["metadata"]["description"].as_str().unwrap_or("");
        if !description.contains("revert is a carbon-deploy of an earlier cut") {
            complaints.push(format!(
                "C4: the description does not say a revert is a carbon-deploy of an earlier cut: {description:?}"
            ));
        }
        assert!(
            complaints.is_empty(),
            "AEGIS ADR-149: carbon-deploy is not the template the clauses name:\n{}",
            complaints.join("\n")
        );
    }

    /// AEGIS ADR-149 C2: `carbon-deploy` 1.1.0 waits for the deploy run on
    /// the remote: PUSH goes on success to WAIT (`archive_deploy_wait` of
    /// the input's cut), WAIT on success to COMPLETE, whose blackboard gains
    /// deploy_run_id and deploy_conclusion, and on failure to DEPLOY_FAILED,
    /// a terminal state carrying WAIT's sentence.
    #[test]
    fn carbon_deploy_waits_for_the_deploy_run_between_push_and_complete() {
        let deploy_yaml = BUILTIN_WORKFLOWS
            .iter()
            .find(|(n, _)| *n == "carbon-deploy")
            .map(|(_, y)| *y)
            .expect("AEGIS ADR-149 C2: carbon-deploy is not registered");
        aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
            deploy_yaml,
        )
        .unwrap_or_else(|e| panic!("AEGIS ADR-149 C2: carbon-deploy does not parse: {e}"));
        let deploy: serde_yaml::Value = serde_yaml::from_str(deploy_yaml).unwrap();
        let states = &deploy["spec"]["states"];
        let target = |state: &str, condition: &str| -> Option<String> {
            states[state]["transitions"].as_sequence().and_then(|ts| {
                ts.iter()
                    .find(|t| t["condition"].as_str() == Some(condition))
                    .and_then(|t| t["target"].as_str())
                    .map(str::to_string)
            })
        };
        let mut complaints = Vec::new();
        if deploy["metadata"]["version"].as_str() != Some("1.1.0") {
            complaints.push(format!(
                "the version is {:?}, not 1.1.0",
                deploy["metadata"]["version"]
            ));
        }
        if target("PUSH", "on_success").as_deref() != Some("WAIT") {
            complaints.push(format!(
                "PUSH on_success goes to {:?}, not WAIT",
                target("PUSH", "on_success")
            ));
        }
        if states["WAIT"]["kind"].as_str() != Some("Action")
            || states["WAIT"]["action"].as_str() != Some("archive_deploy_wait")
            || states["WAIT"]["args"]["cut"].as_str() != Some("{{input.cut}}")
        {
            complaints.push(format!(
                "WAIT is not the Action archive_deploy_wait of the input's cut: {:?}",
                states["WAIT"]
            ));
        }
        for (condition, to) in [("on_success", "COMPLETE"), ("on_failure", "DEPLOY_FAILED")] {
            if target("WAIT", condition).as_deref() != Some(to) {
                complaints.push(format!(
                    "WAIT {condition} goes to {:?}, not {to}",
                    target("WAIT", condition)
                ));
            }
        }
        for (key, value) in [
            ("deploy_run_id", "{{WAIT.output.commit_sha}}"),
            ("deploy_conclusion", "{{WAIT.output.branch}}"),
        ] {
            if states["COMPLETE"]["args"][key].as_str() != Some(value) {
                complaints.push(format!(
                    "COMPLETE's {key} is {:?}, not {value}",
                    states["COMPLETE"]["args"][key]
                ));
            }
        }
        let failed = &states["DEPLOY_FAILED"];
        if failed["args"]["sentence"].as_str() != Some("{{{WAIT.output.sentence}}}")
            || failed["args"]["outcome"].as_str() != Some("deploy_failed")
            || failed["transitions"].as_sequence().map(Vec::len) != Some(0)
        {
            complaints.push(format!(
                "DEPLOY_FAILED is not a terminal state carrying WAIT's sentence: {failed:?}"
            ));
        }
        assert!(
            complaints.is_empty(),
            "AEGIS ADR-149 C2: carbon-deploy does not wait for the deploy run:\n{}",
            complaints.join("\n")
        );
    }
}
