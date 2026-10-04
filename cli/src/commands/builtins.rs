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
pub fn workflow_matches(
    deployed: &aegis_orchestrator_core::domain::workflow::Workflow,
    embedded_yaml: &str,
) -> Result<bool> {
    let embedded =
        aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
            embedded_yaml,
        )
        .context("Failed to parse embedded built-in workflow template for drift comparison")?;
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

async fn deploy_builtin_workflow(
    client: &DaemonClient,
    name: &str,
    template_yaml: &str,
    force: bool,
) -> Result<()> {
    // Validate template before deployment regardless of branch — a malformed
    // built-in must surface immediately, not silently no-op past drift checks.
    let parsed_template =
        aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
            template_yaml,
        )
        .context("Failed to parse built-in workflow template YAML")?;
    aegis_orchestrator_core::domain::workflow::WorkflowValidator::check_for_cycles(
        &parsed_template,
    )
    .context("Built-in workflow template failed cycle validation")?;

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
                if workflow_matches(&deployed, template_yaml)? {
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
            workflow_matches(&workflow, INTENT_EXECUTION_WORKFLOW_TEMPLATE).unwrap(),
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
            !workflow_matches(&deployed, INTENT_EXECUTION_WORKFLOW_TEMPLATE).unwrap(),
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
}
