// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Validation Event Tests
//!
//! Provides validation event tests functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** Implements validation event tests

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::validation_service::ValidationService;
use aegis_orchestrator_core::domain::agent::{
    Agent, AgentId, AgentManifest, AgentScope, AgentStatus,
};
use aegis_orchestrator_core::domain::events::{ExecutionEvent, ValidationEvent};
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration, LlmInteraction,
};
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

struct MockExecutionService;

#[async_trait]
impl ExecutionService for MockExecutionService {
    async fn start_child_execution(
        &self,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _parent_execution_id: ExecutionId,
    ) -> anyhow::Result<ExecutionId> {
        Ok(ExecutionId::new())
    }
    async fn start_execution(
        &self,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _security_context_name: String,
        _identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        Ok(ExecutionId::new())
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
    async fn get_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: ExecutionId,
    ) -> anyhow::Result<Execution> {
        // Return a completed execution so ValidationService can parse output
        let mut exec = Execution::new(
            AgentId::new(),
            ExecutionInput {
                intent: None,
                input: serde_json::Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
            },
            3,
            "aegis-system-operator".to_string(),
        );

        // Use public methods to populate state
        exec.start();
        exec.start_iteration("validate".to_string()).unwrap();

        let output = r#"
```json
{
    "score": 0.95,
    "confidence": 0.9,
    "reasoning": "Excellent code quality",
    "signals": []
}
```
"#;
        exec.complete_iteration(output.to_string());
        exec.complete();

        Ok(exec)
    }
    // We need to return iterations separately as per trait?
    async fn get_execution_unscoped(&self, _id: ExecutionId) -> anyhow::Result<Execution> {
        anyhow::bail!("MockExecutionService::get_execution_unscoped not exercised in this test")
    }
    async fn get_iterations_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _exec_id: ExecutionId,
    ) -> anyhow::Result<Vec<Iteration>> {
        let output = r#"
```json
{
    "score": 0.95,
    "confidence": 0.9,
    "reasoning": "Excellent code quality",
    "signals": []
}
```
"#;
        let iteration = Iteration {
            number: 1,
            status: aegis_orchestrator_core::domain::execution::IterationStatus::Success,
            action: "validate".to_string(),
            output: Some(output.to_string()),
            validation_results: None,
            error: None,
            code_changes: None,
            started_at: chrono::Utc::now(),
            ended_at: Some(chrono::Utc::now()),
            llm_interactions: vec![],
            trajectory: None,
            policy_violations: vec![],
        };
        Ok(vec![iteration])
    }

    async fn cancel_execution(&self, _id: ExecutionId) -> anyhow::Result<()> {
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
    async fn list_executions(
        &self,
        _agent_id: Option<AgentId>,
        _limit: usize,
    ) -> anyhow::Result<Vec<Execution>> {
        Ok(vec![])
    }
    async fn delete_execution(&self, _id: ExecutionId) -> anyhow::Result<()> {
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
        _trajectory: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

struct MockAgentLifecycleService;

#[async_trait]
impl AgentLifecycleService for MockAgentLifecycleService {
    async fn deploy_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _manifest: AgentManifest,
        _force: bool,
        _scope: AgentScope,
        _caller_identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> anyhow::Result<AgentId> {
        anyhow::bail!("MockAgentLifecycleService: deploy_agent_for_tenant not exercised")
    }

    async fn get_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: AgentId,
    ) -> anyhow::Result<Agent> {
        use aegis_orchestrator_core::domain::agent::{AgentSpec, ManifestMetadata, RuntimeConfig};
        use aegis_orchestrator_core::domain::shared_kernel::ImagePullPolicy;
        let manifest = AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: ManifestMetadata {
                name: "mock-judge".to_string(),
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
                // No input_schema — exercises the fallback path.
                input_schema: None,
                output_handler: None,
                security_context: None,
            },
        };
        Ok(Agent {
            id,
            tenant_id: TenantId::system(),
            name: "mock-judge".to_string(),
            scope: AgentScope::Tenant,
            manifest,
            status: AgentStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
    }

    async fn update_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: AgentId,
        _manifest: AgentManifest,
    ) -> anyhow::Result<()> {
        anyhow::bail!("MockAgentLifecycleService: update_agent_for_tenant not exercised")
    }

    async fn delete_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: AgentId,
    ) -> anyhow::Result<()> {
        anyhow::bail!("MockAgentLifecycleService: delete_agent_for_tenant not exercised")
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
        Ok(None)
    }

    async fn lookup_agent_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(None)
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
        Ok(None)
    }
}

// ValidationService::run_judge logic:
// 1. fetches judge agent manifest via AgentLifecycleService
// 2. builds payload from spec.input_schema properties (or falls back for no-schema judges)
// 3. calls get_execution -> checks status
// 4. calls iterations().last() on the returned execution object

#[tokio::test]
async fn test_validation_event_streaming() {
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let exec_service = Arc::new(MockExecutionService);
    let agent_service = Arc::new(MockAgentLifecycleService);
    let val_service =
        ValidationService::new(event_bus.clone(), exec_service.clone(), agent_service);

    let execution_id = ExecutionId::new();
    let mut receiver = event_bus.subscribe_execution(execution_id);

    let request = aegis_orchestrator_core::domain::validation::ValidationRequest {
        content: "fn test() {}".to_string(),
        criteria: "valid rust".to_string(),
        context: None,
    };

    // Updated to use tuples with weights (judge_id, weight)
    let judges = vec![(AgentId::new(), 1.0)];
    let test_agent_id = AgentId::new();

    // Spawn validation in background
    let handle = tokio::spawn(async move {
        val_service
            .validate_with_judges(
                execution_id,
                test_agent_id,
                1, // iteration_number
                request,
                judges,
                None, // Use default consensus config
                60,   // timeout_seconds
                500,  // poll_interval_ms
            )
            .await
    });

    // Listen for events
    // Expect GradientValidationPerformed
    let event1 = timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("Timeout waiting for event 1")
        .expect("Event channel closed");

    let ExecutionEvent::Validation(ValidationEvent::GradientValidationPerformed {
        execution_id: eid,
        score,
        ..
    }) = event1
    else {
        panic!("Expected GradientValidationPerformed, got {event1:?}");
    };
    assert_eq!(eid, execution_id);
    assert_eq!(score, 0.95);

    // Expect MultiJudgeConsensus
    let event2 = timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("Timeout waiting for event 2")
        .expect("Event channel closed");

    let ExecutionEvent::Validation(ValidationEvent::MultiJudgeConsensus {
        execution_id: eid,
        final_score,
        ..
    }) = event2
    else {
        panic!("Expected MultiJudgeConsensus, got {event2:?}");
    };
    assert_eq!(eid, execution_id);
    // Consensus of one judge with 0.95 and high confidence should be close to 0.95
    assert!(final_score > 0.9);

    // Ensure validation completed successfully
    let result = handle.await.unwrap();
    assert!(result.is_ok());
}
