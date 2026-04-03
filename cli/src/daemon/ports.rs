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
    domain::node_config::{NodeConfigManifest, resolve_env_value},
    infrastructure::temporal_proto::temporal::api::{
        common::v1::WorkflowExecution as TemporalWorkflowExecution,
        workflowservice::v1::{
            DeleteWorkflowExecutionRequest, RequestCancelWorkflowExecutionRequest,
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
}

#[async_trait::async_trait]
impl aegis_orchestrator_core::application::ports::WorkflowExecutionControlPort
    for DaemonWorkflowExecutionControl
{
    async fn cancel_workflow_execution(
        &self,
        execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let namespace = temporal_namespace(&self.config)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;
        let mut client = connect_temporal_workflow_client(&self.config)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;
        let request = RequestCancelWorkflowExecutionRequest {
            namespace,
            workflow_execution: Some(TemporalWorkflowExecution {
                workflow_id: execution_id.0.to_string(),
                run_id: String::new(),
            }),
            identity: "aegis-daemon".to_string(),
            request_id: Uuid::new_v4().to_string(),
            first_execution_run_id: String::new(),
            reason: "Cancelled via aegis.workflow.cancel tool".to_string(),
            links: Vec::new(),
        };
        client
            .request_cancel_workflow_execution(request)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;
        Ok(())
    }

    async fn signal_workflow_execution(
        &self,
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
            if let Ok(database_url) = resolve_env_value(&database.url) {
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
        agent_id: uuid::Uuid,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
        let agent_id = aegis_orchestrator_core::domain::agent::AgentId(agent_id);
        let executions = self
            .execution_repo
            .find_by_agent(agent_id, limit + offset)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?;

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
