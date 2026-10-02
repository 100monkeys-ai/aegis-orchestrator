// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Port adapters for ToolInvocationService.
//!
//! Bridges the daemon's Temporal connectivity and execution repository into the
//! port interfaces required by `ToolInvocationService`.

use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use aegis_orchestrator_core::{
    application::complete_workflow_execution::{
        CompleteWorkflowExecutionRequest, CompleteWorkflowExecutionUseCase, CompletionStatus,
        StandardCompleteWorkflowExecutionUseCase,
    },
    domain::execution::{ExecutionId, ExecutionStatus},
    domain::node_config::{resolve_env_value, NodeConfigManifest},
    domain::repository::WorkflowExecutionRepository,
    domain::tenant::TenantId,
    infrastructure::event_bus::EventBus,
    infrastructure::temporal_proto::temporal::api::{
        common::v1::WorkflowExecution as TemporalWorkflowExecution,
        enums::v1::WorkflowExecutionStatus,
        workflowservice::v1::{
            DeleteWorkflowExecutionRequest, DescribeWorkflowExecutionRequest,
            DescribeWorkflowExecutionResponse, RequestCancelWorkflowExecutionRequest,
        },
    },
};

use super::temporal_helpers::{connect_temporal_workflow_client, temporal_namespace};

/// Adapts the daemon's Temporal connectivity into the
/// `WorkflowExecutionControlPort` expected by `ToolInvocationService`.
pub(crate) struct DaemonWorkflowExecutionControl {
    pub(crate) config: NodeConfigManifest,
    pub(crate) temporal_client_container: Arc<
        tokio::sync::RwLock<
            Option<Arc<aegis_orchestrator_core::infrastructure::temporal_client::TemporalClient>>,
        >,
    >,
    /// The record a cancel ends when Temporal no longer runs the execution.
    pub(crate) workflow_execution_repo: Arc<dyn WorkflowExecutionRepository>,
    pub(crate) event_bus: Arc<EventBus>,
}

/// What Temporal holds for a workflow execution's run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemporalRun {
    /// The run is open (running or paused): a cancel is Temporal's to deliver.
    Open,
    /// No open run: Temporal has none by that id, or the run has closed.
    /// Temporal answers a cancel of a closed run with success and does
    /// nothing, so nothing would ever end the orchestrator's record.
    Gone,
}

/// The Temporal calls a workflow cancel makes.
#[async_trait::async_trait]
pub(crate) trait TemporalRunControl: Send + Sync {
    async fn run(&self, execution_id: ExecutionId) -> Result<TemporalRun>;
    async fn request_cancel(&self, execution_id: ExecutionId, reason: &str) -> Result<()>;
}

/// [`TemporalRunControl`] over the Temporal frontend named in the node config.
pub(crate) struct TemporalGrpcRunControl<'a> {
    pub(crate) config: &'a NodeConfigManifest,
}

#[async_trait::async_trait]
impl TemporalRunControl for TemporalGrpcRunControl<'_> {
    async fn run(&self, execution_id: ExecutionId) -> Result<TemporalRun> {
        let namespace = temporal_namespace(self.config)?;
        let mut client = connect_temporal_workflow_client(self.config).await?;
        let described = client
            .describe_workflow_execution(DescribeWorkflowExecutionRequest {
                namespace,
                execution: Some(TemporalWorkflowExecution {
                    workflow_id: execution_id.0.to_string(),
                    run_id: String::new(),
                }),
            })
            .await
            .map(tonic::Response::into_inner);
        temporal_run_from_describe(described)
    }

    async fn request_cancel(&self, execution_id: ExecutionId, reason: &str) -> Result<()> {
        let namespace = temporal_namespace(self.config)?;
        let mut client = connect_temporal_workflow_client(self.config).await?;
        client
            .request_cancel_workflow_execution(RequestCancelWorkflowExecutionRequest {
                namespace,
                workflow_execution: Some(TemporalWorkflowExecution {
                    workflow_id: execution_id.0.to_string(),
                    run_id: String::new(),
                }),
                identity: "aegis-daemon".to_string(),
                request_id: Uuid::new_v4().to_string(),
                first_execution_run_id: String::new(),
                reason: reason.to_string(),
                links: Vec::new(),
            })
            .await?;
        Ok(())
    }
}

/// Reads a DescribeWorkflowExecution answer: an open run (running or
/// paused) is [`TemporalRun::Open`]; a closed run, or none by that id
/// (`NOT_FOUND`), is [`TemporalRun::Gone`]; any other error is an error.
pub(crate) fn temporal_run_from_describe(
    described: std::result::Result<DescribeWorkflowExecutionResponse, tonic::Status>,
) -> Result<TemporalRun> {
    match described {
        Ok(response) => {
            let status = response
                .workflow_execution_info
                .map(|info| info.status)
                .unwrap_or_default();
            if status == WorkflowExecutionStatus::Running as i32
                || status == WorkflowExecutionStatus::Paused as i32
            {
                Ok(TemporalRun::Open)
            } else {
                Ok(TemporalRun::Gone)
            }
        }
        Err(status) if status.code() == tonic::Code::NotFound => Ok(TemporalRun::Gone),
        Err(status) => Err(anyhow::anyhow!(
            "Failed to describe the workflow's Temporal run: {status}"
        )),
    }
}

/// What a workflow cancel did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelOutcome {
    /// Temporal was asked to cancel the open run.
    CancelRequested,
    /// Temporal runs nothing for the execution, so its record was ended as
    /// cancelled (or was already terminal and is left as it is).
    Ended,
}

/// Cancel a workflow execution (the `aegis.workflow.cancel` tool and
/// `POST /v1/workflows/executions/:id/cancel`).
pub(crate) async fn cancel_workflow_run(
    run_control: &dyn TemporalRunControl,
    workflow_execution_repo: &Arc<dyn WorkflowExecutionRepository>,
    event_bus: &Arc<EventBus>,
    tenant_id: &TenantId,
    execution_id: ExecutionId,
    reason: &str,
) -> Result<CancelOutcome> {
    if run_control.run(execution_id).await? == TemporalRun::Open {
        run_control.request_cancel(execution_id, reason).await?;
        return Ok(CancelOutcome::CancelRequested);
    }

    // No open run will ever report an end, so the cancel ends the record.
    let execution = workflow_execution_repo
        .find_by_id_for_tenant(tenant_id, execution_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Workflow execution not found: {}", execution_id.0))?;
    if matches!(
        execution.status,
        ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled
    ) {
        return Ok(CancelOutcome::Ended);
    }
    StandardCompleteWorkflowExecutionUseCase::new(
        workflow_execution_repo.clone(),
        event_bus.clone(),
    )
    .complete_execution_for_tenant(
        tenant_id,
        CompleteWorkflowExecutionRequest {
            execution_id: execution_id.to_string(),
            status: CompletionStatus::Cancelled,
            final_blackboard: None,
            final_output: None,
            error_reason: None,
            artifacts: None,
            final_state: None,
        },
    )
    .await?;
    Ok(CancelOutcome::Ended)
}

#[async_trait::async_trait]
impl aegis_orchestrator_core::application::ports::WorkflowExecutionControlPort
    for DaemonWorkflowExecutionControl
{
    async fn cancel_workflow_execution(
        &self,
        tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
        execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Tenant validation is performed in the handler via
        // `WorkflowExecutionRepository::find_by_id_for_tenant` before this port
        // is invoked; Temporal's workflow id does not encode the tenant, and
        // the record a cancel may end is read for this tenant only.
        cancel_workflow_run(
            &TemporalGrpcRunControl {
                config: &self.config,
            },
            &self.workflow_execution_repo,
            &self.event_bus,
            tenant_id,
            execution_id,
            "Cancelled via aegis.workflow.cancel tool",
        )
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;
        Ok(())
    }

    async fn signal_workflow_execution(
        &self,
        _tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
        execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
        response: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let guard = self.temporal_client_container.read().await;
        let client = guard
            .as_ref()
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                "Temporal client not yet connected".into()
            })?
            .clone();
        drop(guard);
        client
            .send_human_signal(&execution_id.0.to_string(), response.to_string())
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;
        Ok(())
    }

    async fn remove_workflow_execution(
        &self,
        _tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
        execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let namespace = temporal_namespace(&self.config)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;
        let mut client = connect_temporal_workflow_client(&self.config)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;
        let request = DeleteWorkflowExecutionRequest {
            namespace,
            workflow_execution: Some(TemporalWorkflowExecution {
                workflow_id: execution_id.0.to_string(),
                run_id: String::new(),
            }),
        };
        client
            .delete_workflow_execution(request)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;

        // Also clean up the database row if available
        if let Some(database) = &self.config.spec.database {
            if let Ok(database_url) = resolve_env_value(database.url.expose()) {
                if let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&database_url)
                    .await
                {
                    let _ = sqlx::query("DELETE FROM workflow_executions WHERE id = $1")
                        .bind(execution_id.0)
                        .execute(&pool)
                        .await;
                }
            }
        }
        Ok(())
    }
}

/// Adapts the daemon's execution repository into the `AgentActivityPort`
/// expected by `ToolInvocationService` for `aegis.agent.logs`.
pub(crate) struct DaemonAgentActivity {
    pub(crate) execution_repo:
        Arc<dyn aegis_orchestrator_core::domain::repository::ExecutionRepository>,
}

#[async_trait::async_trait]
impl aegis_orchestrator_core::application::ports::AgentActivityPort for DaemonAgentActivity {
    async fn agent_logs_snapshot(
        &self,
        tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
        agent_id: uuid::Uuid,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
        let agent_id = aegis_orchestrator_core::domain::agent::AgentId(agent_id);
        let executions = self
            .execution_repo
            .find_by_agent_for_tenant(tenant_id, agent_id, limit + offset)
            .await
            .map_err(|e: aegis_orchestrator_core::domain::repository::RepositoryError| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;

        let entries: Vec<serde_json::Value> = executions
            .iter()
            .skip(offset)
            .take(limit)
            .map(|e| {
                serde_json::json!({
                    "execution_id": e.id.0.to_string(),
                    "agent_id": e.agent_id.0.to_string(),
                    "status": format!("{:?}", e.status).to_lowercase(),
                    "started_at": e.started_at,
                    "ended_at": e.ended_at,
                    "iteration_count": e.iterations().len(),
                })
            })
            .collect();
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::events::WorkflowEvent;
    use aegis_orchestrator_core::domain::workflow::{
        StateKind, StateName, Workflow, WorkflowExecution, WorkflowMetadata, WorkflowSpec,
        WorkflowState,
    };
    use aegis_orchestrator_core::infrastructure::event_bus::DomainEvent;
    use aegis_orchestrator_core::infrastructure::repositories::InMemoryWorkflowExecutionRepository;
    use aegis_orchestrator_core::infrastructure::temporal_proto::temporal::api::workflow::v1::WorkflowExecutionInfo;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Temporal as the cancel sees it: a run that is open or gone, and every
    /// cancel request it was sent. Like Temporal, it answers a cancel of a
    /// gone run with success.
    struct FakeTemporal {
        run: TemporalRun,
        cancels: Mutex<Vec<ExecutionId>>,
    }

    impl FakeTemporal {
        fn new(run: TemporalRun) -> Self {
            Self {
                run,
                cancels: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl TemporalRunControl for FakeTemporal {
        async fn run(&self, _execution_id: ExecutionId) -> Result<TemporalRun> {
            Ok(self.run)
        }

        async fn request_cancel(&self, execution_id: ExecutionId, _reason: &str) -> Result<()> {
            self.cancels.lock().unwrap().push(execution_id);
            Ok(())
        }
    }

    fn one_state_workflow() -> Workflow {
        let mut states = HashMap::new();
        states.insert(
            StateName::new("EXECUTE_CODE").unwrap(),
            WorkflowState {
                kind: StateKind::System {
                    command: "echo".to_string(),
                    env: HashMap::new(),
                    workdir: None,
                },
                transitions: vec![],
                timeout: None,
                max_state_visits: None,
            },
        );
        Workflow::new(
            WorkflowMetadata {
                name: "cancel-test".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                labels: HashMap::new(),
                annotations: HashMap::new(),
                input_schema: None,
                output_schema: None,
                output_template: None,
            },
            WorkflowSpec {
                initial_state: StateName::new("EXECUTE_CODE").unwrap(),
                context: HashMap::new(),
                states,
                storage: Default::default(),
                max_total_transitions: None,
            },
        )
        .unwrap()
    }

    async fn record(
        tenant_id: &TenantId,
        status: ExecutionStatus,
    ) -> (ExecutionId, Arc<dyn WorkflowExecutionRepository>) {
        let workflow = one_state_workflow();
        let execution_id = ExecutionId::new();
        let mut execution = WorkflowExecution::new(&workflow, execution_id, serde_json::json!({}));
        execution.status = status;
        let repo: Arc<dyn WorkflowExecutionRepository> =
            Arc::new(InMemoryWorkflowExecutionRepository::new());
        repo.save_for_tenant(tenant_id, &execution).await.unwrap();
        (execution_id, repo)
    }

    async fn status_of(
        repo: &Arc<dyn WorkflowExecutionRepository>,
        tenant_id: &TenantId,
        execution_id: ExecutionId,
    ) -> ExecutionStatus {
        repo.find_by_id_for_tenant(tenant_id, execution_id)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    /// 6054290c: its Temporal run had ended, the record said running, and a
    /// cancel answered success without ending it. A cancel of an execution
    /// whose Temporal run is gone ends the record as cancelled.
    #[tokio::test]
    async fn cancel_of_an_execution_whose_temporal_run_is_gone_ends_the_record_as_cancelled() {
        let tenant_id = TenantId::consumer();
        let (execution_id, repo) = record(&tenant_id, ExecutionStatus::Running).await;
        let event_bus = Arc::new(EventBus::new(16));
        let mut receiver = event_bus.subscribe();
        let temporal = FakeTemporal::new(TemporalRun::Gone);

        let outcome = cancel_workflow_run(
            &temporal,
            &repo,
            &event_bus,
            &tenant_id,
            execution_id,
            "test",
        )
        .await
        .unwrap();

        assert_eq!(
            status_of(&repo, &tenant_id, execution_id).await,
            ExecutionStatus::Cancelled
        );
        assert_eq!(outcome, CancelOutcome::Ended);
        match receiver.recv().await.unwrap() {
            DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionCancelled {
                execution_id: published,
                ..
            }) => assert_eq!(published, execution_id),
            other => panic!("expected WorkflowExecutionCancelled, got {other:?}"),
        }
    }

    /// An open run is Temporal's to cancel; the record waits for its end.
    #[tokio::test]
    async fn cancel_of_an_open_temporal_run_requests_its_cancellation() {
        let tenant_id = TenantId::consumer();
        let (execution_id, repo) = record(&tenant_id, ExecutionStatus::Running).await;
        let event_bus = Arc::new(EventBus::new(16));
        let temporal = FakeTemporal::new(TemporalRun::Open);

        let outcome = cancel_workflow_run(
            &temporal,
            &repo,
            &event_bus,
            &tenant_id,
            execution_id,
            "test",
        )
        .await
        .unwrap();

        assert_eq!(outcome, CancelOutcome::CancelRequested);
        assert_eq!(*temporal.cancels.lock().unwrap(), vec![execution_id]);
        assert_eq!(
            status_of(&repo, &tenant_id, execution_id).await,
            ExecutionStatus::Running
        );
    }

    /// A record that has already ended is left as it is.
    #[tokio::test]
    async fn cancel_of_a_gone_run_leaves_a_terminal_record_as_it_is() {
        let tenant_id = TenantId::consumer();
        let (execution_id, repo) = record(&tenant_id, ExecutionStatus::Failed).await;
        let event_bus = Arc::new(EventBus::new(16));
        let mut receiver = event_bus.subscribe();
        let temporal = FakeTemporal::new(TemporalRun::Gone);

        cancel_workflow_run(
            &temporal,
            &repo,
            &event_bus,
            &tenant_id,
            execution_id,
            "test",
        )
        .await
        .unwrap();

        assert_eq!(
            status_of(&repo, &tenant_id, execution_id).await,
            ExecutionStatus::Failed
        );
        assert!(receiver.try_recv().is_err(), "no event is published");
    }

    fn described(status: WorkflowExecutionStatus) -> DescribeWorkflowExecutionResponse {
        DescribeWorkflowExecutionResponse {
            workflow_execution_info: Some(WorkflowExecutionInfo {
                status: status as i32,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_describe_answer_names_the_run_open_or_gone() {
        for (status, run) in [
            (WorkflowExecutionStatus::Running, TemporalRun::Open),
            (WorkflowExecutionStatus::Paused, TemporalRun::Open),
            (WorkflowExecutionStatus::Completed, TemporalRun::Gone),
            (WorkflowExecutionStatus::Failed, TemporalRun::Gone),
            (WorkflowExecutionStatus::Canceled, TemporalRun::Gone),
            (WorkflowExecutionStatus::Terminated, TemporalRun::Gone),
            (WorkflowExecutionStatus::TimedOut, TemporalRun::Gone),
        ] {
            assert_eq!(
                temporal_run_from_describe(Ok(described(status))).unwrap(),
                run,
                "{status:?}"
            );
        }
        assert_eq!(
            temporal_run_from_describe(Err(tonic::Status::not_found("workflow not found")))
                .unwrap(),
            TemporalRun::Gone
        );
        assert!(temporal_run_from_describe(Err(tonic::Status::unavailable("down"))).is_err());
    }
}
