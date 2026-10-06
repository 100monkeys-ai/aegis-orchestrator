// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! AEGIS ADR-131 U30, U31 and U32: the prompt `goal-judge` reads, rendered as an
//! execution renders it (its manifest's `prompt_template` over its
//! instruction and the judge input as `{{input}}`), for the PDF run of
//! 2026-10-05: an agent execution that ran no tool, produced no file, and
//! whose whole output names a file.

use std::sync::Arc;

use aegis_orchestrator_core::application::goal_service::{
    ExecutionView, GoalCaller, GoalService, GoalWorld, JudgeProgress,
};
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::goal::{BoundExecution, BoundKind, Goal, GoalChannel};
use aegis_orchestrator_core::domain::node_config::GoalsConfig;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::prompt_template_engine::{
    PromptContext, PromptTemplateEngine,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
use serde_json::Value;

const GOAL_JUDGE_YAML: &str = include_str!("../../../cli/templates/agents/goal-judge.yaml");

/// The PDF run as the orchestrator's rows hold it: completed, one try, no
/// tool call, no produced file, its output a path.
struct PdfRun;

#[async_trait::async_trait]
impl GoalWorld for PdfRun {
    async fn read_execution(&self, _: &Goal, b: &BoundExecution) -> Option<ExecutionView> {
        Some(ExecutionView {
            execution_id: b.execution_id,
            kind: "agent",
            agent_or_workflow: "delivery-itinerary-pdf-agent".to_string(),
            status: "completed".to_string(),
            started_at: b.started_at,
            ended_at: Some(b.started_at),
            iterations: Some(1),
            tool_calls_executed: Some(0),
            dispatches: Some(Vec::new()),
            produced_files: Some(Vec::new()),
            last_output: Some("/workspace/delivery_itinerary.pdf".to_string()),
            last_error: None,
            steps_unread: None,
            bound_until: b.started_at,
        })
    }
    async fn pending_approvals(&self, _: &Goal, _: &[ExecutionId]) -> Vec<(ExecutionId, String)> {
        Vec::new()
    }
    async fn start_judge(&self, _: &Goal, _: Value) -> Result<ExecutionId, String> {
        Err("not exercised".to_string())
    }
    async fn judge_progress(&self, _: &Goal, _: ExecutionId) -> JudgeProgress {
        JudgeProgress::Running
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[tokio::test]
async fn the_rendered_goal_judge_prompt_says_last_output_is_the_agents_own_text_and_carries_the_facts(
) {
    let service = GoalService::new(
        Arc::new(InMemoryGoalRepository::new()),
        Arc::new(EventBus::new(16)),
        GoalsConfig::default(),
    );
    let caller = GoalCaller {
        tenant_id: TenantId::for_consumer_user("user-a").unwrap(),
        user_sub: "user-a".to_string(),
    };
    let goal = service
        .create(
            &caller,
            "Make me a PDF of the delivery itinerary.",
            "conversation-1",
            GoalChannel::Web,
        )
        .await
        .unwrap();
    service
        .bind(goal.id, ExecutionId::new(), BoundKind::Agent)
        .await
        .unwrap();
    let input = service
        .judge_input(
            &PdfRun,
            &goal,
            "Your PDF is ready at /workspace/delivery_itinerary.pdf.",
        )
        .await
        .unwrap();

    let manifest: serde_yaml::Value = serde_yaml::from_str(GOAL_JUDGE_YAML).unwrap();
    let task = &manifest["spec"]["task"];
    let prompt = PromptTemplateEngine::new()
        .render(
            task["prompt_template"].as_str().unwrap(),
            &PromptContext::new()
                .instruction(task["instruction"].as_str().unwrap())
                .input(input),
        )
        .unwrap();
    let prompt = one_line(&prompt);

    let mut complaints = Vec::new();
    for sentence in [
        "last_output is the agent's own text: what the model wrote, not proof of what it did.",
        "A file or an action counts as done only if tool_calls_executed, dispatches or \
         produced_files show it.",
        "Score evidence 0 only when the companion_answer or a last_output claims a file and \
         neither holds",
        // U32: a workflow's facts are its steps', each entry naming its step.
        "For a workflow or intent execution those three are its step executions' facts: \
         tool_calls_executed is their sum, and each entry of dispatches and produced_files \
         carries the execution_id of the step that made it, in the steps' start order; \
         steps_unread counts the steps whose record could not be read, whose facts are missing \
         from those three. A workflow or intent execution with no step executions has those \
         three null.",
        "(for a workflow or intent execution, the step its entries' execution_id names)",
        "Where an execution's facts are null (a workflow or intent execution with no step \
         executions), or for the steps steps_unread counts, they are unknown",
        "\"tool_calls_executed\":0",
        "\"dispatches\":[]",
        "\"produced_files\":[]",
    ] {
        if prompt.contains(sentence) {
            println!("rendered goal-judge prompt says: {sentence}");
        } else {
            complaints.push(format!("the rendered prompt does not say: {sentence}"));
        }
    }
    assert!(
        complaints.is_empty(),
        "U30, U31, U32: {complaints:#?}\nthe prompt: {prompt}"
    );
}
