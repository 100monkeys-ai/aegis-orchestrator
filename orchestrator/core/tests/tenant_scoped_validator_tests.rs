// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Regression tests for tenant-scoped execution lookups in the validator pipeline.
//!
//! These tests pin the contract introduced when `ExecutionService::get_execution(id)`
//! (which silently defaulted to `TenantId::consumer()`) was removed in favor of
//! `get_execution_for_tenant(&tenant, id)`. The production failure they pin: a parent
//! execution running under a per-user tenant (`u-...`) had its semantic-judge poll
//! hit the consumer-default lookup, miss the row, and surface "Execution not found"
//! into `aegis.task.status` even when the underlying iteration had succeeded.
//!
//! Coverage:
//! 1. **Positive**: `SemanticAgentValidator` configured with a per-user tenant
//!    successfully polls a child judge that lives in the same tenant.
//! 2. **Negative**: the same validator configured with a *different* tenant
//!    fails closed — the child judge is not found, proving cross-tenant reads
//!    are impossible by construction.

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::validation_service::{
    SemanticAgentValidator, SemanticAgentValidatorConfig,
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
use aegis_orchestrator_core::domain::validation::{GradientValidator, ValidationContext};
use aegis_orchestrator_core::infrastructure::event_bus::DomainEvent;
use async_trait::async_trait;
use std::sync::Arc;

/// Mock execution service whose `get_execution_for_tenant` only returns a
/// completed judge execution when the requested tenant matches the configured
/// expected tenant. Any other tenant returns "Execution not found", which is
/// exactly the failure mode the production bug exhibited.
struct TenantScopedMockExecutionService {
    expected_tenant: TenantId,
    judge_execution_id: ExecutionId,
    judge_output: String,
}

#[async_trait]
impl ExecutionService for TenantScopedMockExecutionService {
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
            name: "stub-judge".to_string(),
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
        name: "stub-judge".to_string(),
        scope: AgentScope::Tenant,
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn judge_output_json() -> String {
    r#"{ "score": 0.95, "confidence": 0.9, "reasoning": "looks good", "signals": [] }"#.to_string()
}

fn validation_ctx() -> ValidationContext {
    ValidationContext {
        task: "produce a report".to_string(),
        output: "the report".to_string(),
        exit_code: 0,
        stderr: String::new(),
        worker_mounts: vec![],
        tool_trajectory: vec![],
        policy_violations: vec![],
    }
}

/// Regression test for the production "Execution not found" bug under per-user tenants.
///
/// A parent execution running under a per-user tenant `u-abc123` spawns a child judge
/// in the same tenant. The validator's poll must locate the child via
/// `get_execution_for_tenant(&u-abc123, judge_id)` — NOT via a consumer-default lookup.
#[tokio::test]
async fn semantic_validator_polls_judge_in_per_user_tenant() {
    let user_tenant = TenantId::from_string("u-abc123-deadbeef-cafef00d-12345678").unwrap();
    let judge_id = AgentId::new();
    let parent_execution_id = ExecutionId::new();
    let judge_execution_id = ExecutionId::new();

    let exec_service = Arc::new(TenantScopedMockExecutionService {
        expected_tenant: user_tenant.clone(),
        judge_execution_id,
        judge_output: judge_output_json(),
    });
    let lifecycle = Arc::new(StubAgentLifecycleService { judge_id });

    let validator = SemanticAgentValidator::new(
        SemanticAgentValidatorConfig {
            judge_agent_name: "stub-judge".to_string(),
            criteria: "evaluate the output".to_string(),
            timeout_seconds: 5,
            poll_interval_ms: 50,
            parent_execution_id,
            tenant_id: user_tenant.clone(),
        },
        lifecycle,
        exec_service,
    );

    let ctx = validation_ctx();
    let result = validator
        .validate(&ctx)
        .await
        .expect("validator must locate the judge in its own tenant");
    assert!(result.score > 0.9, "expected high score from judge output");
}

/// Negative test: configuring the validator with the *wrong* tenant must fail closed.
///
/// Cross-tenant reads are now impossible by construction. Configuring the validator
/// with a tenant that does not match the judge's tenant means the lookup fails and
/// the validator surfaces an error — proving the consumer-default fallback is gone.
#[tokio::test]
async fn semantic_validator_fails_closed_on_tenant_mismatch() {
    let judge_tenant = TenantId::from_string("u-realuser-cafef00d-12345678-aabbccdd").unwrap();
    let wrong_tenant = TenantId::from_string("u-imposter-deadbeef-87654321-99887766").unwrap();
    let judge_id = AgentId::new();
    let parent_execution_id = ExecutionId::new();
    let judge_execution_id = ExecutionId::new();

    let exec_service = Arc::new(TenantScopedMockExecutionService {
        expected_tenant: judge_tenant.clone(),
        judge_execution_id,
        judge_output: judge_output_json(),
    });
    let lifecycle = Arc::new(StubAgentLifecycleService { judge_id });

    let validator = SemanticAgentValidator::new(
        SemanticAgentValidatorConfig {
            judge_agent_name: "stub-judge".to_string(),
            criteria: "evaluate the output".to_string(),
            timeout_seconds: 2,
            poll_interval_ms: 50,
            parent_execution_id,
            tenant_id: wrong_tenant,
        },
        lifecycle,
        exec_service,
    );

    let ctx = validation_ctx();
    let err = validator
        .validate(&ctx)
        .await
        .expect_err("cross-tenant lookup must fail");
    assert!(
        err.to_string().contains("Execution not found")
            || err.to_string().to_lowercase().contains("not found"),
        "expected tenant-mismatch error, got: {err}"
    );
}
