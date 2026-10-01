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
    SemanticAgentValidatorConfig,
};
use aegis_orchestrator_core::domain::agent::{
    Agent, AgentId, AgentManifest, AgentScope, AgentSpec, AgentStatus, ManifestMetadata,
    RuntimeConfig,
};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration, LlmInteraction, TrajectoryStep,
};
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::shared_kernel::ImagePullPolicy;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::validation::{
    GradientValidator, JudgeFault, ValidationContext, ValidationPipeline, ValidatorEntry,
    ValidatorKind,
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
            execution: None,
            security: None,
            schedule: None,
            tools: vec![],
            env: std::collections::HashMap::new(),
            volumes: vec![],
            advanced: None,
            input_schema: None,
            output_handler: None,
            security_context: None,
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
