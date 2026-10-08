// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Regression tests for how the orchestrator reads a judge agent's verdict
//! (ADR-016, ADR-017).
//!
//! The production failure they pin (execution 9c5b304e of
//! `haiku-triangulator-agent`, 2026-10-01): the tenant's judge
//! `haiku-judge-agent` answered `{"valid": true, "score": 1.0, "dimensions":
//! {...}, "feedback": "..."}`, a verdict with a score but without the
//! `confidence` and `reasoning` fields the orchestrator required, so every
//! verdict failed to deserialise with "Failed to parse semantic judge output",
//! the parse error was handed to the worker as feedback on its own output, and
//! the execution ended "Max retries exceeded" although the judge had scored it
//! 1.0 and 0.975.

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::validation_service::{
    MultiJudgeAgentValidator, MultiJudgeAgentValidatorConfig, SemanticAgentValidator,
    SemanticAgentValidatorConfig, ValidationService,
};
use aegis_orchestrator_core::domain::agent::{
    Agent, AgentId, AgentManifest, AgentScope, AgentSpec, AgentStatus, ManifestMetadata,
    RuntimeConfig,
};
use aegis_orchestrator_core::domain::events::{ExecutionEvent, ValidationEvent};
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration, LlmInteraction, TrajectoryStep,
};
use aegis_orchestrator_core::domain::goal::{AliasTableJudgeContext, JudgeContextSource};
use aegis_orchestrator_core::domain::node_config::LLMProviderConfig;
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::shared_kernel::ImagePullPolicy;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::validation::{
    GradientValidator, JudgeFault, ValidationContext, ValidationPipeline, ValidationRequest,
    ValidatorEntry, ValidatorKind,
};
use aegis_orchestrator_core::domain::workflow::{ConsensusConfig, ConsensusStrategy};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use async_trait::async_trait;
use std::sync::Arc;

/// Mock execution service whose `get_execution_for_tenant` only returns a
/// completed judge execution when the requested tenant matches the configured
/// expected tenant. Any other tenant returns "Execution not found", which is
/// exactly the failure mode the production bug exhibited.
struct JudgeOutputExecutionService {
    expected_tenant: TenantId,
    judge_execution_id: ExecutionId,
    judge_output: String,
}

#[async_trait]
impl ExecutionService for JudgeOutputExecutionService {
    async fn start_execution(
        &self,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _security_context_name: String,
        _identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }

    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _security_context_name: String,
        _identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        Ok(execution_id)
    }

    async fn start_child_execution(
        &self,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _parent_execution_id: ExecutionId,
    ) -> anyhow::Result<ExecutionId> {
        Ok(self.judge_execution_id)
    }

    async fn get_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> anyhow::Result<Execution> {
        if tenant_id != &self.expected_tenant {
            anyhow::bail!("Execution not found");
        }
        if id != self.judge_execution_id {
            anyhow::bail!("Execution not found");
        }

        let mut exec = Execution::new(
            AgentId::new(),
            ExecutionInput {
                intent: None,
                input: serde_json::Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            3,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("validate".to_string()).unwrap();
        exec.complete_iteration(self.judge_output.clone());
        exec.complete();
        Ok(exec)
    }

    async fn get_execution_unscoped(&self, _id: ExecutionId) -> anyhow::Result<Execution> {
        anyhow::bail!("get_execution_unscoped must not be used by validator pollers")
    }

    async fn get_iterations_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _exec_id: ExecutionId,
    ) -> anyhow::Result<Vec<Iteration>> {
        anyhow::bail!("not exercised")
    }

    async fn cancel_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: ExecutionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stream_execution(
        &self,
        _id: ExecutionId,
    ) -> anyhow::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>,
    > {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn stream_agent_events(
        &self,
        _id: AgentId,
    ) -> anyhow::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = anyhow::Result<DomainEvent>> + Send>>,
    > {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn list_executions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: Option<AgentId>,
        _workflow_id: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
        _limit: usize,
    ) -> anyhow::Result<Vec<Execution>> {
        Ok(vec![])
    }

    async fn delete_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: ExecutionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn record_llm_interaction(
        &self,
        _execution_id: ExecutionId,
        _iteration: u8,
        _interaction: LlmInteraction,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn store_iteration_trajectory(
        &self,
        _execution_id: ExecutionId,
        _iteration: u8,
        _trajectory: Vec<TrajectoryStep>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

struct StubAgentLifecycleService {
    judge_id: AgentId,
}

#[async_trait]
impl AgentLifecycleService for StubAgentLifecycleService {
    async fn deploy_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _manifest: AgentManifest,
        _force: bool,
        _scope: AgentScope,
        _caller_identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> anyhow::Result<AgentId> {
        anyhow::bail!("not exercised")
    }

    async fn get_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: AgentId,
    ) -> anyhow::Result<Agent> {
        Ok(stub_agent(id))
    }

    async fn get_agent_visible(&self, _tenant_id: &TenantId, id: AgentId) -> anyhow::Result<Agent> {
        Ok(stub_agent(id))
    }

    async fn update_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: AgentId,
        _manifest: AgentManifest,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn delete_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: AgentId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn list_agents_for_tenant(&self, _tenant_id: &TenantId) -> anyhow::Result<Vec<Agent>> {
        Ok(vec![])
    }

    async fn list_agents_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
    ) -> anyhow::Result<Vec<Agent>> {
        Ok(vec![])
    }

    async fn lookup_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(self.judge_id))
    }

    async fn lookup_agent_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(self.judge_id))
    }

    async fn list_versions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: AgentId,
    ) -> anyhow::Result<Vec<AgentVersion>> {
        Ok(vec![])
    }

    async fn lookup_agent_for_tenant_with_version(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
        _version: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(self.judge_id))
    }
}

fn stub_agent(id: AgentId) -> Agent {
    let manifest = AgentManifest {
        api_version: "100monkeys.ai/v1".to_string(),
        kind: "Agent".to_string(),
        metadata: ManifestMetadata {
            name: "haiku-judge-agent".to_string(),
            version: "1.0.0".to_string(),
            description: None,
            labels: std::collections::HashMap::new(),
            annotations: std::collections::HashMap::new(),
        },
        spec: AgentSpec {
            runtime: RuntimeConfig {
                language: Some("python".to_string()),
                version: Some("3.11".to_string()),
                image: None,
                image_pull_policy: ImagePullPolicy::IfNotPresent,
                isolation: "inherit".to_string(),
                model: "judge".to_string(),
                temperature: None,
            },
            task: None,
            context: vec![],
            contexts: Vec::new(),
            execution: None,
            security: None,
            default_schedule: None,
            tools: vec![],
            env: std::collections::HashMap::new(),
            volumes: vec![],
            advanced: None,
            input_schema: None,
            output_handler: None,
            security_context: None,
            program: None,
        },
    };
    Agent {
        id,
        tenant_id: TenantId::system(),
        name: "haiku-judge-agent".to_string(),
        scope: AgentScope::Tenant,
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}
/// The shape of iteration 2's verdict in execution 9c5b304e: `valid`, `score`,
/// a `dimensions` map of name to score, and `feedback`; no `confidence`, no
/// `reasoning`, no `signals`.
const ITERATION_2_VERDICT: &str = r#"{"valid": true, "score": 1.0, "dimensions": {"accuracy": 1.0, "completeness": 1.0, "clarity": 1.0}, "feedback": "The triangulation is correct and every source agrees."}"#;

fn validation_ctx() -> ValidationContext {
    ValidationContext {
        task: "triangulate the answer".to_string(),
        output: "the answer".to_string(),
        exit_code: 0,
        stderr: String::new(),
        worker_mounts: vec![],
        tool_trajectory: vec![],
        policy_violations: vec![],
    }
}

fn tenant() -> TenantId {
    TenantId::from_string("u-abc123-deadbeef-cafef00d-12345678").unwrap()
}

fn semantic_validator(judge_output: &str) -> SemanticAgentValidator {
    let judge_id = AgentId::new();
    let exec_service = Arc::new(JudgeOutputExecutionService {
        expected_tenant: tenant(),
        judge_execution_id: ExecutionId::new(),
        judge_output: judge_output.to_string(),
    });
    let lifecycle = Arc::new(StubAgentLifecycleService { judge_id });
    SemanticAgentValidator::new(
        SemanticAgentValidatorConfig {
            judge_agent_name: "haiku-judge-agent".to_string(),
            criteria: "evaluate the output".to_string(),
            timeout_seconds: 5,
            poll_interval_ms: 20,
            parent_execution_id: ExecutionId::new(),
            tenant_id: tenant(),
        },
        lifecycle,
        exec_service,
    )
}

fn multi_judge_validator(judge_output: &str) -> MultiJudgeAgentValidator {
    let judge_id = AgentId::new();
    let exec_service = Arc::new(JudgeOutputExecutionService {
        expected_tenant: tenant(),
        judge_execution_id: ExecutionId::new(),
        judge_output: judge_output.to_string(),
    });
    let lifecycle = Arc::new(StubAgentLifecycleService { judge_id });
    MultiJudgeAgentValidator::new(
        MultiJudgeAgentValidatorConfig {
            judges: vec!["haiku-judge-agent".to_string()],
            consensus_config: ConsensusConfig {
                strategy: ConsensusStrategy::WeightedAverage,
                threshold: None,
                min_agreement_confidence: None,
                n: None,
                min_judges_required: 1,
                confidence_weighting: None,
            },
            min_judges_required: 1,
            criteria: "evaluate the output".to_string(),
            timeout_seconds: 5,
            poll_interval_ms: 20,
            parent_execution_id: ExecutionId::new(),
            tenant_id: tenant(),
        },
        lifecycle,
        exec_service,
        Arc::new(EventBus::new(16)),
    )
}

/// The verdict of iteration 2, verbatim in shape, is read as score 1.0 and the
/// iteration passes the semantic entry's threshold (the default 0.7).
#[tokio::test]
async fn iteration_2_verdict_is_read_as_score_one_and_passes_the_threshold() {
    let pipeline = ValidationPipeline::new(vec![ValidatorEntry {
        kind: ValidatorKind::Semantic,
        validator: Box::new(semantic_validator(ITERATION_2_VERDICT)),
        min_score: 0.7,
        min_confidence: 0.0,
    }]);
    let result = pipeline.validate(&validation_ctx()).await;
    let result = match result {
        Ok(r) => r,
        Err(e) => panic!("the judge scored the iteration 1.0 but the verdict was refused: {e}"),
    };
    assert!(
        result.passed,
        "a verdict scoring 1.0 must pass a 0.7 threshold, blocked by: {:?}",
        result.blocking_reason
    );
    let gradient = result.results.gradient.expect("the verdict is recorded");
    assert_eq!(gradient.score, 1.0, "the verdict's score is read as given");
}

/// A verdict wrapped in a fenced json block, as models often write one, is read.
#[tokio::test]
async fn verdict_in_a_fenced_json_block_is_read() {
    let fenced = format!("Here is my verdict:\n```json\n{ITERATION_2_VERDICT}\n```\n");
    let result = semantic_validator(&fenced)
        .validate(&validation_ctx())
        .await;
    match result {
        Ok(r) => assert_eq!(r.score, 1.0, "the fenced verdict's score is read"),
        Err(e) => panic!("a fenced verdict with a score was refused: {e}"),
    }
}

/// A verdict with no score is a judge fault: the error names the judge agent and
/// quotes what it wrote, rather than reading as a fault in the worker's output.
#[tokio::test]
async fn verdict_without_a_score_is_a_judge_fault_naming_the_judge() {
    let no_score = r#"{"valid": true, "feedback": "looks fine"}"#;
    let err = semantic_validator(no_score)
        .validate(&validation_ctx())
        .await
        .expect_err("a verdict with no score cannot be read");
    let text = err.to_string();
    assert!(
        text.contains("judge fault") && text.contains("haiku-judge-agent"),
        "the error must say it is a judge fault and name the judge, got: {text}"
    );
    assert!(
        text.contains(no_score),
        "the error must quote the judge's output, got: {text}"
    );
    let fault = err
        .downcast_ref::<JudgeFault>()
        .expect("the error is a typed JudgeFault, so a caller can tell it from a worker fault");
    assert_eq!(fault.judge_agent, "haiku-judge-agent");
    assert_eq!(fault.output, no_score);
}

/// The multi-judge path reads the same verdict the same way.
#[tokio::test]
async fn multi_judge_reads_the_iteration_2_verdict() {
    let result = multi_judge_validator(ITERATION_2_VERDICT)
        .validate(&validation_ctx())
        .await;
    match result {
        Ok(r) => assert_eq!(
            r.score, 1.0,
            "the consensus of one judge scoring 1.0 is 1.0"
        ),
        Err(e) => panic!("the multi-judge path refused a verdict with a score: {e}"),
    }
}

// ── A judge fault is the judge's, not the worker's (ADR-017, Update 2026-10-01) ──
//
// A judge whose verdict cannot be read is run once more as a fresh child
// execution with the same input; each fault publishes a `JudgeFault` event, and
// a second fault is returned as the error, a `JudgeFault` that names the judge
// and says it faulted twice.

/// Execution service whose judge child executions answer, in start order, with
/// the outputs it was given; it records each start's input.
struct SequencedJudgeExecutionService {
    outputs: Vec<String>,
    started: std::sync::Mutex<Vec<(ExecutionId, ExecutionInput)>>,
}

impl SequencedJudgeExecutionService {
    fn new(outputs: &[&str]) -> Self {
        Self {
            outputs: outputs.iter().map(|o| o.to_string()).collect(),
            started: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn started_inputs(&self) -> Vec<serde_json::Value> {
        self.started
            .lock()
            .unwrap()
            .iter()
            .map(|(_, input)| input.input.clone())
            .collect()
    }
}

#[async_trait]
impl ExecutionService for SequencedJudgeExecutionService {
    async fn start_execution(
        &self,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _security_context_name: String,
        _identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }

    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _security_context_name: String,
        _identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        Ok(execution_id)
    }

    async fn start_child_execution(
        &self,
        _agent_id: AgentId,
        input: ExecutionInput,
        _parent_execution_id: ExecutionId,
    ) -> anyhow::Result<ExecutionId> {
        let mut started = self.started.lock().unwrap();
        if started.len() >= self.outputs.len() {
            anyhow::bail!(
                "judge started {} times, more than the {} runs scripted",
                started.len() + 1,
                self.outputs.len()
            );
        }
        let id = ExecutionId::new();
        started.push((id, input));
        Ok(id)
    }

    async fn get_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: ExecutionId,
    ) -> anyhow::Result<Execution> {
        let index = self
            .started
            .lock()
            .unwrap()
            .iter()
            .position(|(started_id, _)| *started_id == id)
            .ok_or_else(|| anyhow::anyhow!("Execution not found"))?;
        let mut exec = Execution::new(
            AgentId::new(),
            ExecutionInput {
                intent: None,
                input: serde_json::Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            3,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("validate".to_string()).unwrap();
        exec.complete_iteration(self.outputs[index].clone());
        exec.complete();
        Ok(exec)
    }

    async fn get_execution_unscoped(&self, _id: ExecutionId) -> anyhow::Result<Execution> {
        anyhow::bail!("get_execution_unscoped must not be used by validator pollers")
    }

    async fn get_iterations_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _exec_id: ExecutionId,
    ) -> anyhow::Result<Vec<Iteration>> {
        anyhow::bail!("not exercised")
    }

    async fn cancel_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: ExecutionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stream_execution(
        &self,
        _id: ExecutionId,
    ) -> anyhow::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>,
    > {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn stream_agent_events(
        &self,
        _id: AgentId,
    ) -> anyhow::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = anyhow::Result<DomainEvent>> + Send>>,
    > {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn list_executions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: Option<AgentId>,
        _workflow_id: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
        _limit: usize,
    ) -> anyhow::Result<Vec<Execution>> {
        Ok(vec![])
    }

    async fn delete_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: ExecutionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn record_llm_interaction(
        &self,
        _execution_id: ExecutionId,
        _iteration: u8,
        _interaction: LlmInteraction,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn store_iteration_trajectory(
        &self,
        _execution_id: ExecutionId,
        _iteration: u8,
        _trajectory: Vec<TrajectoryStep>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The JudgeFault events published on `receiver` so far, as
/// (execution id, judge agent, reason, judge output).
fn judge_fault_events(
    receiver: &mut aegis_orchestrator_core::infrastructure::event_bus::EventReceiver,
) -> Vec<(ExecutionId, String, String, String)> {
    let mut faults = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let DomainEvent::Execution(ExecutionEvent::Validation(ValidationEvent::JudgeFault {
            execution_id,
            judge_agent,
            reason,
            judge_output,
            ..
        })) = event
        {
            faults.push((execution_id, judge_agent, reason, judge_output));
        }
    }
    faults
}

const UNREADABLE_VERDICT: &str = "I think the answer is fine.";

/// A judge whose first verdict cannot be read and whose second can: the
/// validator yields the second verdict, the judge was started twice with the
/// same input, and one JudgeFault event names the judge and quotes its output.
#[tokio::test]
async fn a_judge_unreadable_once_is_run_again_and_its_second_verdict_is_read() {
    let parent = ExecutionId::new();
    let exec_service = Arc::new(SequencedJudgeExecutionService::new(&[
        UNREADABLE_VERDICT,
        ITERATION_2_VERDICT,
    ]));
    let event_bus = Arc::new(EventBus::new(64));
    let mut receiver = event_bus.subscribe();
    let validator = SemanticAgentValidator::new(
        SemanticAgentValidatorConfig {
            judge_agent_name: "haiku-judge-agent".to_string(),
            criteria: "evaluate the output".to_string(),
            timeout_seconds: 5,
            poll_interval_ms: 20,
            parent_execution_id: parent,
            tenant_id: tenant(),
        },
        Arc::new(StubAgentLifecycleService {
            judge_id: AgentId::new(),
        }),
        exec_service.clone(),
    )
    .with_event_bus(event_bus.clone());

    let result = validator.validate(&validation_ctx()).await;
    let verdict = match result {
        Ok(v) => v,
        Err(e) => panic!(
            "a judge unreadable once must be run again and its readable second verdict read, got: {e}"
        ),
    };
    assert_eq!(verdict.score, 1.0, "the second verdict's score is read");

    let inputs = exec_service.started_inputs();
    assert_eq!(
        inputs.len(),
        2,
        "the judge is started once more as a fresh child execution, no more"
    );
    assert_eq!(inputs[0], inputs[1], "the second run has the same input");

    let faults = judge_fault_events(&mut receiver);
    assert_eq!(
        faults.len(),
        1,
        "one JudgeFault event for the one unreadable verdict, got: {faults:?}"
    );
    let (execution_id, judge_agent, _reason, judge_output) = &faults[0];
    assert_eq!(
        *execution_id, parent,
        "the event names the judged execution"
    );
    assert_eq!(judge_agent, "haiku-judge-agent");
    assert_eq!(judge_output, UNREADABLE_VERDICT);
}

/// A judge unreadable twice: the error is a JudgeFault naming the judge and
/// saying it faulted twice; the judge was started twice, not more; and each
/// fault published its event.
#[tokio::test]
async fn a_judge_unreadable_twice_is_a_judge_fault_saying_it_faulted_twice() {
    let parent = ExecutionId::new();
    let exec_service = Arc::new(SequencedJudgeExecutionService::new(&[
        UNREADABLE_VERDICT,
        UNREADABLE_VERDICT,
        ITERATION_2_VERDICT,
    ]));
    let event_bus = Arc::new(EventBus::new(64));
    let mut receiver = event_bus.subscribe();
    let validator = SemanticAgentValidator::new(
        SemanticAgentValidatorConfig {
            judge_agent_name: "haiku-judge-agent".to_string(),
            criteria: "evaluate the output".to_string(),
            timeout_seconds: 5,
            poll_interval_ms: 20,
            parent_execution_id: parent,
            tenant_id: tenant(),
        },
        Arc::new(StubAgentLifecycleService {
            judge_id: AgentId::new(),
        }),
        exec_service.clone(),
    )
    .with_event_bus(event_bus.clone());

    let err = validator
        .validate(&validation_ctx())
        .await
        .expect_err("a judge unreadable twice cannot yield a verdict");
    let fault = err
        .downcast_ref::<JudgeFault>()
        .unwrap_or_else(|| panic!("the error must be a typed JudgeFault, got: {err}"));
    assert_eq!(fault.judge_agent, "haiku-judge-agent");
    let text = err.to_string();
    assert!(
        text.contains("judge fault")
            && text.contains("haiku-judge-agent")
            && text.contains("faulted twice"),
        "the error must say judge fault, name the judge and say it faulted twice, got: {text}"
    );
    assert_eq!(
        exec_service.started_inputs().len(),
        2,
        "the judge is run once more after its first fault, and not a third time"
    );
    assert_eq!(
        judge_fault_events(&mut receiver).len(),
        2,
        "each of the two faults publishes a JudgeFault event"
    );
}

/// The multi-judge path: its one judge unreadable twice makes the shortfall of
/// judges an error that carries the JudgeFault, after one re-run of the judge.
#[tokio::test]
async fn multi_judge_with_its_judge_unreadable_twice_carries_the_judge_fault() {
    let exec_service = Arc::new(SequencedJudgeExecutionService::new(&[
        UNREADABLE_VERDICT,
        UNREADABLE_VERDICT,
        ITERATION_2_VERDICT,
    ]));
    let event_bus = Arc::new(EventBus::new(64));
    let mut receiver = event_bus.subscribe();
    let validator = MultiJudgeAgentValidator::new(
        MultiJudgeAgentValidatorConfig {
            judges: vec!["haiku-judge-agent".to_string()],
            consensus_config: ConsensusConfig {
                strategy: ConsensusStrategy::WeightedAverage,
                threshold: None,
                min_agreement_confidence: None,
                n: None,
                min_judges_required: 1,
                confidence_weighting: None,
            },
            min_judges_required: 1,
            criteria: "evaluate the output".to_string(),
            timeout_seconds: 5,
            poll_interval_ms: 20,
            parent_execution_id: ExecutionId::new(),
            tenant_id: tenant(),
        },
        Arc::new(StubAgentLifecycleService {
            judge_id: AgentId::new(),
        }),
        exec_service.clone(),
        event_bus.clone(),
    );

    let err = validator
        .validate(&validation_ctx())
        .await
        .expect_err("no judge produced a readable verdict");
    let fault = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<JudgeFault>())
        .unwrap_or_else(|| panic!("the shortfall must carry the JudgeFault, got: {err:#}"));
    assert_eq!(fault.judge_agent, "haiku-judge-agent");
    assert_eq!(
        exec_service.started_inputs().len(),
        2,
        "the multi-judge's judge is run once more after its first fault, and not a third time"
    );
    assert_eq!(judge_fault_events(&mut receiver).len(), 2);
}

// ── U19: a validation judge's model holds its whole input, or no judge runs ──
//
// AEGIS ADR-131 U19 (Jeshua, 2026-10-04: "We need to make it so that judges
// can see everything as it is"): the validation judges are given the output,
// the task and the trajectory whole; before a judge starts, its prompt is
// measured against its alias's `context_window - max_output_tokens` from the
// alias table, and an input above it runs no judge on a fragment: the
// iteration's validation ends with the sizes in a sentence.

/// The alias table with `judge` at production's entry (`aegis-platform-
/// deployment` d27c51d: 256,000 and 16,384): a prompt limit of 239,616 bytes.
fn production_alias_table() -> Arc<dyn JudgeContextSource> {
    let providers: Vec<LLMProviderConfig> = serde_yaml::from_str(
        "- name: workers-ai\n  type: openai-compatible\n  endpoint: https://example.invalid/v1\n  \
         models:\n    - {alias: judge, model: gemma, capabilities: [chat], context_window: 256000, max_output_tokens: 16384}\n",
    )
    .unwrap();
    Arc::new(AliasTableJudgeContext::from_providers(&providers))
}

fn ctx_with_output(output: String) -> ValidationContext {
    ValidationContext {
        output,
        ..validation_ctx()
    }
}

fn semantic_with(
    exec_service: Arc<SequencedJudgeExecutionService>,
    event_bus: Arc<EventBus>,
    source: Option<Arc<dyn JudgeContextSource>>,
) -> SemanticAgentValidator {
    SemanticAgentValidator::new(
        SemanticAgentValidatorConfig {
            judge_agent_name: "haiku-judge-agent".to_string(),
            criteria: "evaluate the output".to_string(),
            timeout_seconds: 5,
            poll_interval_ms: 20,
            parent_execution_id: ExecutionId::new(),
            tenant_id: tenant(),
        },
        Arc::new(StubAgentLifecycleService {
            judge_id: AgentId::new(),
        }),
        exec_service,
    )
    .with_event_bus(event_bus)
    .with_judge_context(source)
}

/// The JudgeFault an error carries, if any.
fn judge_fault_of(error: &anyhow::Error) -> Option<JudgeFault> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<JudgeFault>())
        .cloned()
}

/// An output of 100,000 characters, far above the stated default's room and
/// within production's `judge`, is given to the judge whole.
#[tokio::test]
async fn a_validation_judge_is_given_an_output_its_alias_holds_whole() {
    let output = "a".repeat(100_000);
    let exec_service = Arc::new(SequencedJudgeExecutionService::new(&[ITERATION_2_VERDICT]));
    let validator = semantic_with(
        exec_service.clone(),
        Arc::new(EventBus::new(16)),
        Some(production_alias_table()),
    );
    let verdict = validator
        .validate(&ctx_with_output(output.clone()))
        .await
        .unwrap_or_else(|e| panic!("a 100,000-character output fits production's judge: {e:#}"));
    assert_eq!(verdict.score, 1.0);
    let inputs = exec_service.started_inputs();
    assert_eq!(inputs.len(), 1, "the judge ran once");
    assert_eq!(
        inputs[0]["output"].as_str().map(str::len),
        Some(100_000),
        "the judge is given the output whole"
    );
}

/// Above production's limit: no judge runs, the error is a JudgeFault whose
/// reason is the sizes in a sentence, and one JudgeFault event is published.
#[tokio::test]
async fn a_semantic_judge_input_above_its_alias_limit_runs_no_judge_and_states_the_sizes() {
    let exec_service = Arc::new(SequencedJudgeExecutionService::new(&[ITERATION_2_VERDICT]));
    let event_bus = Arc::new(EventBus::new(64));
    let mut receiver = event_bus.subscribe();
    let validator = semantic_with(
        exec_service.clone(),
        event_bus,
        Some(production_alias_table()),
    );
    let error = validator
        .validate(&ctx_with_output("a".repeat(300_000)))
        .await
        .expect_err("a 300,000-character output is above production's 239,616 bytes");
    let fault = judge_fault_of(&error).unwrap_or_else(|| panic!("a JudgeFault, got: {error:#}"));
    assert_eq!(fault.judge_agent, "haiku-judge-agent");
    assert!(
        fault.reason.contains("The input is too large to judge: ")
            && fault.reason.contains("against a limit of 239616 bytes."),
        "the reason states the sizes against the alias's limit: {}",
        fault.reason
    );
    assert!(
        format!("{error:#}").starts_with("Judge 'haiku-judge-agent' was not run: "),
        "the stated reason leads: {error:#}"
    );
    assert!(
        exec_service.started_inputs().is_empty(),
        "no judge runs on a fragment or on the whole"
    );
    assert_eq!(judge_fault_events(&mut receiver).len(), 1);
}

/// An alias the table does not map takes the stated default (32,768 and
/// 8,192: 24,576 bytes): never an unbounded send.
#[tokio::test]
async fn a_validation_judge_without_a_configured_alias_is_bounded_by_the_stated_default() {
    let exec_service = Arc::new(SequencedJudgeExecutionService::new(&[ITERATION_2_VERDICT]));
    let validator = semantic_with(exec_service.clone(), Arc::new(EventBus::new(16)), None);
    let error = validator
        .validate(&ctx_with_output("a".repeat(20_000)))
        .await
        .expect_err("20,000 characters and the reserve are above the default's 24,576 bytes");
    let fault = judge_fault_of(&error).unwrap_or_else(|| panic!("a JudgeFault, got: {error:#}"));
    assert!(
        fault.reason.contains("against a limit of 24576 bytes."),
        "{}",
        fault.reason
    );
    assert!(exec_service.started_inputs().is_empty());
}

/// The multi-judge form is measured the same way.
#[tokio::test]
async fn a_multi_judge_input_above_its_alias_limit_runs_no_judge() {
    let exec_service = Arc::new(SequencedJudgeExecutionService::new(&[ITERATION_2_VERDICT]));
    let validator = MultiJudgeAgentValidator::new(
        MultiJudgeAgentValidatorConfig {
            judges: vec!["haiku-judge-agent".to_string()],
            consensus_config: ConsensusConfig {
                strategy: ConsensusStrategy::WeightedAverage,
                threshold: None,
                min_agreement_confidence: None,
                n: None,
                min_judges_required: 1,
                confidence_weighting: None,
            },
            min_judges_required: 1,
            criteria: "evaluate the output".to_string(),
            timeout_seconds: 5,
            poll_interval_ms: 20,
            parent_execution_id: ExecutionId::new(),
            tenant_id: tenant(),
        },
        Arc::new(StubAgentLifecycleService {
            judge_id: AgentId::new(),
        }),
        exec_service.clone(),
        Arc::new(EventBus::new(16)),
    )
    .with_judge_context(Some(production_alias_table()));
    let error = validator
        .validate(&ctx_with_output("a".repeat(300_000)))
        .await
        .expect_err("above the limit");
    let fault = judge_fault_of(&error).unwrap_or_else(|| panic!("a JudgeFault, got: {error:#}"));
    assert!(
        fault.reason.contains("The input is too large to judge: "),
        "{}",
        fault.reason
    );
    assert!(exec_service.started_inputs().is_empty());
}

/// `ValidationService::validate_with_judges` is measured the same way, and
/// passes a content its alias holds whole.
#[tokio::test]
async fn validate_with_judges_is_bounded_by_the_alias_table_it_is_given() {
    let judge = AgentId::new();
    let within = Arc::new(SequencedJudgeExecutionService::new(&[ITERATION_2_VERDICT]));
    let service = ValidationService::new(
        Arc::new(EventBus::new(16)),
        within.clone(),
        Arc::new(StubAgentLifecycleService { judge_id: judge }),
    )
    .with_judge_context(production_alias_table());
    let mut request = ValidationRequest {
        content: "a".repeat(100_000),
        criteria: "evaluate the output".to_string(),
        context: None,
    };
    service
        .validate_with_judges(
            ExecutionId::new(),
            AgentId::new(),
            1,
            request.clone(),
            vec![(judge, 1.0)],
            None,
            5,
            20,
            &tenant(),
        )
        .await
        .unwrap_or_else(|e| panic!("100,000 characters fit production's judge: {e:#}"));
    assert_eq!(within.started_inputs().len(), 1);

    let above = Arc::new(SequencedJudgeExecutionService::new(&[ITERATION_2_VERDICT]));
    let service = ValidationService::new(
        Arc::new(EventBus::new(16)),
        above.clone(),
        Arc::new(StubAgentLifecycleService { judge_id: judge }),
    )
    .with_judge_context(production_alias_table());
    request.content = "a".repeat(300_000);
    let error = service
        .validate_with_judges(
            ExecutionId::new(),
            AgentId::new(),
            1,
            request,
            vec![(judge, 1.0)],
            None,
            5,
            20,
            &tenant(),
        )
        .await
        .expect_err("above the limit");
    let fault = judge_fault_of(&error).unwrap_or_else(|| panic!("a JudgeFault, got: {error:#}"));
    assert!(
        fault.reason.contains("The input is too large to judge: "),
        "{}",
        fault.reason
    );
    assert!(above.started_inputs().is_empty());
}
