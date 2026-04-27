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

        assert!(states.contains_key(&serde_yaml::Value::from("PLAN")));
        assert!(states.contains_key(&serde_yaml::Value::from("GENERATE_MISSING_AGENTS")));
        assert!(states.contains_key(&serde_yaml::Value::from("GENERATE_AND_REGISTER_WORKFLOW")));
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

    #[test]
    fn all_builtin_workflow_templates_parse_successfully() {
        for (name, yaml) in BUILTIN_WORKFLOWS {
            aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
                yaml,
            )
            .unwrap_or_else(|e| panic!("Failed to parse builtin workflow '{}': {}", name, e));
        }
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
