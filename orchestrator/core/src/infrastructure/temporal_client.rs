// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Temporal.io gRPC Client
//!
//! Provides low-level gRPC client for interacting with Temporal.io workflow engine.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** gRPC communication with Temporal workflow service
//! - **Integration:** AEGIS Workflow Engine → Temporal.io gRPC API
//!
//! # Client Features
//!
//! - **Workflow Execution**: Start workflows with the Generic Interpreter pattern
//! - **Connection Management**: Persistent gRPC channel with timeout handling
//! - **JSON Payload Encoding**: Standard encoding for workflow inputs
//! - **Namespace Isolation**: Multi-tenant workflow execution support
//!
//! # Generic Interpreter Pattern
//!
//! This client uses a generic workflow pattern where:
//! 1. All AEGIS workflows execute via a single TypeScript workflow function (`aegis_workflow`)
//! 2. The workflow name and input are passed as payload parameters
//! 3. The TypeScript worker interprets the workflow definition at runtime
//!
//! Input structure:
//! ```json
//! {
//!   "workflow_id": "550e8400-e29b-41d4-a716-446655440000",
//!   "input": { "query": "..." }
//! }
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use temporal_client::TemporalClient;
//!
//! let client = TemporalClient::new(
//!     "localhost:7233",
//!     "default",
//!     "aegis-task-queue",
//!     "http://temporal-worker:3000"
//! ).await?;
//!
//! let run_id = client.start_workflow(
//!     "my-workflow",
//!     execution_id,
//!     "tenant-slug",
//!     input_params
//! ).await?;
//! ```
//!
//! # Configuration
//!
//! - **Address**: Temporal server endpoint (e.g., `localhost:7233`)
//! - **Namespace**: Logical isolation boundary for workflows
//! - **Task Queue**: Worker registration and task routing identifier

use crate::application::ports::{
    ScheduleEnginePort, TemporalScheduleDescription, TemporalScheduleSpec, WorkflowEnginePort,
};
use crate::domain::schedule::{Timing, CATCHUP_WINDOW_SECONDS, FIRE_WORKFLOW_TYPE};
use crate::domain::secrets::SensitiveUrl;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client as HttpClient;
use std::collections::HashMap;
use std::time::Duration;
use tonic::transport::Channel;
use uuid::Uuid;

// Import generated protos
use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::workflow_service_client::WorkflowServiceClient;
use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::StartWorkflowExecutionRequest;
use crate::infrastructure::temporal_proto::temporal::api::common::v1::{WorkflowType, Payloads, Payload};

#[derive(Clone)]
pub struct TemporalClient {
    client: WorkflowServiceClient<Channel>,
    http_client: HttpClient,
    namespace: String,
    task_queue: String,
    /// Original Temporal server address (used for diagnostics/reconnection)
    temporal_endpoint: String,
    worker_http_endpoint: String,
}

impl TemporalClient {
    pub async fn new(
        address: &str,
        namespace: &str,
        task_queue: &str,
        worker_http_endpoint: &str,
    ) -> Result<Self> {
        // Ensure address has scheme
        let addr = if address.contains("://") {
            address.to_string()
        } else {
            format!("http://{address}")
        };

        let endpoint = Channel::from_shared(addr.clone())
            .context("Invalid Temporal address")?
            .timeout(Duration::from_secs(10));

        let channel = endpoint
            .connect()
            .await
            .context("Failed to connect to Temporal server")?;

        let client = WorkflowServiceClient::new(channel);
        let http_client = HttpClient::new();

        Ok(Self {
            client,
            http_client,
            namespace: namespace.to_string(),
            task_queue: task_queue.to_string(),
            temporal_endpoint: address.to_string(),
            worker_http_endpoint: worker_http_endpoint.to_string(),
        })
    }

    /// Start a workflow execution using the Generic Interpreter pattern
    pub async fn start_workflow(
        &self,
        params: crate::application::ports::StartWorkflowParams<'_>,
    ) -> Result<String> {
        let execution_workflow_id = params.execution_id.0.to_string();

        // Generic workflow type that the worker registers
        let workflow_type_name = "aegis_workflow";

        // Construct input payload matching GenericWorkflowInput interface in TS
        // interface GenericWorkflowInput { workflow_id: string; input: Record<string, any>; intent?: string; }
        let mut input_obj = serde_json::json!({
            "workflow_id": params.workflow_id,
            "input": params.input,
            "blackboard": params.blackboard,
            "security_context_name": params.security_context_name,
            "tenant_id": params.tenant_id,
        });
        if let Some(intent_val) = params.intent {
            input_obj["intent"] = serde_json::Value::String(intent_val);
        }

        // Serialize to JSON payload
        let json_bytes = serde_json::to_vec(&input_obj)?;

        // Metadata for JSON encoding
        let mut metadata = HashMap::new();
        metadata.insert("encoding".to_string(), "json/plain".as_bytes().to_vec());

        let payload = Payload {
            metadata,
            data: json_bytes,
            // Any additional fields generated from the Temporal proto definition
            // (for example, `external_payloads` in newer API versions) are left at
            // their default values via `..Default::default()`. This matches the
            // expected encoding for a single JSON payload; see the Temporal
            // Payloads documentation for details.
            ..Default::default()
        };

        let payloads = Payloads {
            payloads: vec![payload],
        };

        let request_id = Uuid::new_v4().to_string();

        let request = StartWorkflowExecutionRequest {
            namespace: self.namespace.clone(),
            workflow_id: execution_workflow_id.clone(),
            workflow_type: Some(WorkflowType {
                name: workflow_type_name.to_string(),
            }),
            task_queue: Some(
                crate::infrastructure::temporal_proto::temporal::api::taskqueue::v1::TaskQueue {
                    name: self.task_queue.clone(),
                    kind: 0, // Normal
                    ..Default::default()
                },
            ),
            input: Some(payloads),
            request_id,
            ..Default::default()
        };

        let mut client = self.client.clone();
        let response = client
            .start_workflow_execution(request)
            .await
            .context(format!(
                "Failed to start workflow execution via gRPC (Temporal: {})",
                SensitiveUrl::new(self.temporal_endpoint.as_str())
            ))?;

        Ok(response.into_inner().run_id)
    }
    /// Get workflow execution history
    pub async fn get_workflow_history(
        &self,
        execution_id: String,
        run_id: Option<String>,
    ) -> Result<Vec<crate::infrastructure::temporal_proto::temporal::api::history::v1::HistoryEvent>>
    {
        use crate::infrastructure::temporal_proto::temporal::api::common::v1::WorkflowExecution;
        use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::GetWorkflowExecutionHistoryRequest;

        let request = GetWorkflowExecutionHistoryRequest {
            namespace: self.namespace.clone(),
            execution: Some(WorkflowExecution {
                workflow_id: execution_id,
                run_id: run_id.unwrap_or_default(),
            }),
            maximum_page_size: 1000,
            next_page_token: Vec::new(),
            wait_new_event: false,
            history_event_filter_type: 0, // All events
            ..Default::default()
        };

        let mut client = self.client.clone();
        let response = client
            .get_workflow_execution_history(request)
            .await
            .context("Failed to get workflow history")?;

        Ok(response
            .into_inner()
            .history
            .map(|h| h.events)
            .unwrap_or_default())
    }

    /// Send a `humanInput` Temporal signal to a workflow paused at a Human state.
    ///
    /// Workflows using `StateKind::Human` call `defineSignal('humanInput')` and
    /// `await condition(...)` — they resume only when this signal arrives.
    /// The `response` string is JSON-encoded and forwarded as the signal payload.
    ///
    /// # gRPC Endpoint
    ///
    /// `WorkflowService.SignalWorkflowExecution` — the signal is sent directly to
    /// Temporal Server without an extra HTTP hop through the TypeScript worker.
    pub async fn send_human_signal(&self, execution_id: &str, response: String) -> Result<()> {
        use crate::infrastructure::temporal_proto::temporal::api::common::v1::WorkflowExecution;
        use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::SignalWorkflowExecutionRequest;

        let json_bytes = serde_json::to_vec(&response)?;
        let mut metadata = HashMap::new();
        metadata.insert("encoding".to_string(), "json/plain".as_bytes().to_vec());

        let payload = Payload {
            metadata,
            data: json_bytes,
            ..Default::default()
        };

        let request = SignalWorkflowExecutionRequest {
            namespace: self.namespace.clone(),
            workflow_execution: Some(WorkflowExecution {
                workflow_id: execution_id.to_string(),
                run_id: String::new(),
            }),
            signal_name: "humanInput".to_string(),
            input: Some(Payloads {
                payloads: vec![payload],
            }),
            identity: "aegis-orchestrator".to_string(),
            request_id: Uuid::new_v4().to_string(),
            ..Default::default()
        };

        let mut client = self.client.clone();
        client
            .signal_workflow_execution(request)
            .await
            .context("Failed to send humanInput signal to workflow execution")?;

        Ok(())
    }

    /// Register a workflow definition with the Temporal worker
    ///
    /// This calls the TypeScript worker HTTP API to register a new workflow definition.
    /// The worker stores the definition in PostgreSQL for dynamic runtime interpretation.
    ///
    /// # HTTP Endpoint
    ///
    /// POST /{worker_http_endpoint}/register-workflow
    /// Body: JSON serialized TemporalWorkflowDefinition
    /// Response: 200 OK {status: "registered"} or error
    pub async fn register_temporal_workflow(
        &self,
        definition: &crate::application::temporal_mapper::TemporalWorkflowDefinition,
    ) -> Result<()> {
        let url = format!("{}/register-workflow", self.worker_http_endpoint);

        let response = self
            .http_client
            .post(&url)
            .json(definition)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("Failed to send workflow registration request to Temporal worker")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "(no body)".to_string());
            anyhow::bail!("Failed to register workflow with Temporal worker: {status} - {body}");
        }

        Ok(())
    }
}

#[async_trait]
impl WorkflowEnginePort for TemporalClient {
    async fn register_workflow(
        &self,
        definition: &crate::application::temporal_mapper::TemporalWorkflowDefinition,
    ) -> Result<()> {
        self.register_temporal_workflow(definition).await
    }

    async fn start_workflow(
        &self,
        params: crate::application::ports::StartWorkflowParams<'_>,
    ) -> Result<String> {
        TemporalClient::start_workflow(self, params).await
    }
}

// ── Temporal Schedules (AEGIS ADR-139 N5) ───────────────────────────────────

use crate::infrastructure::temporal_proto::temporal::api::enums::v1::ScheduleOverlapPolicy;
use crate::infrastructure::temporal_proto::temporal::api::schedule::v1::{
    CalendarSpec, Schedule as TemporalSchedule, ScheduleAction, SchedulePatch, SchedulePolicies,
    ScheduleSpec, ScheduleState as TemporalScheduleState,
};
use crate::infrastructure::temporal_proto::temporal::api::taskqueue::v1::TaskQueue;
use crate::infrastructure::temporal_proto::temporal::api::workflow::v1::NewWorkflowExecutionInfo;
use crate::infrastructure::temporal_proto::temporal::api::workflowservice::v1::{
    CreateScheduleRequest, DeleteScheduleRequest, DescribeScheduleRequest, PatchScheduleRequest,
    UpdateScheduleRequest,
};
use chrono::{Datelike, Timelike};

/// The identity this client gives Temporal on schedule calls.
const SCHEDULE_IDENTITY: &str = "aegis-orchestrator";

/// The Temporal Schedule for `spec` (N5): a recurrence's `cron_string`,
/// `timezone_name` and `jitter`, or one `calendar` entry at `at` limited to
/// one action; the SKIP overlap policy, the catch-up window and no pause on
/// failure; an action that starts the worker's fire workflow on `task_queue`
/// with `{schedule_id, tenant_id}` as its only input.
pub fn temporal_schedule(spec: &TemporalScheduleSpec, task_queue: &str) -> TemporalSchedule {
    let (schedule_spec, limited_actions, remaining_actions) = match &spec.timing {
        Timing::Recurrence(recurrence) => (
            ScheduleSpec {
                cron_string: vec![recurrence.cron.clone()],
                timezone_name: recurrence.timezone.clone(),
                jitter: Some(prost_types::Duration {
                    seconds: i64::from(recurrence.jitter_seconds),
                    nanos: 0,
                }),
                ..Default::default()
            },
            false,
            0,
        ),
        Timing::Once { at } => (
            ScheduleSpec {
                calendar: vec![CalendarSpec {
                    second: at.second().to_string(),
                    minute: at.minute().to_string(),
                    hour: at.hour().to_string(),
                    day_of_month: at.day().to_string(),
                    month: at.month().to_string(),
                    year: at.year().to_string(),
                    day_of_week: "*".to_string(),
                    comment: String::new(),
                }],
                timezone_name: "UTC".to_string(),
                ..Default::default()
            },
            true,
            1,
        ),
    };
    let input = serde_json::json!({
        "schedule_id": spec.schedule_id,
        "tenant_id": spec.tenant_id,
    });
    let mut metadata = HashMap::new();
    metadata.insert("encoding".to_string(), "json/plain".as_bytes().to_vec());
    let payload = Payload {
        metadata,
        data: serde_json::to_vec(&input).unwrap_or_default(),
        ..Default::default()
    };
    TemporalSchedule {
        spec: Some(schedule_spec),
        action: Some(ScheduleAction {
            action: Some(
                crate::infrastructure::temporal_proto::temporal::api::schedule::v1::schedule_action::Action::StartWorkflow(
                    NewWorkflowExecutionInfo {
                        workflow_id: format!("aegis-schedule-fire-{}", spec.schedule_id),
                        workflow_type: Some(WorkflowType {
                            name: FIRE_WORKFLOW_TYPE.to_string(),
                        }),
                        task_queue: Some(TaskQueue {
                            name: task_queue.to_string(),
                            kind: 0,
                            ..Default::default()
                        }),
                        input: Some(Payloads {
                            payloads: vec![payload],
                        }),
                        ..Default::default()
                    },
                ),
            ),
        }),
        policies: Some(SchedulePolicies {
            overlap_policy: ScheduleOverlapPolicy::Skip as i32,
            catchup_window: Some(prost_types::Duration {
                seconds: CATCHUP_WINDOW_SECONDS as i64,
                nanos: 0,
            }),
            pause_on_failure: false,
            ..Default::default()
        }),
        state: Some(TemporalScheduleState {
            paused: spec.paused,
            limited_actions,
            remaining_actions,
            ..Default::default()
        }),
    }
}

fn timestamp_to_utc(ts: &prost_types::Timestamp) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(ts.seconds, u32::try_from(ts.nanos).ok()?)
}

#[async_trait]
impl ScheduleEnginePort for TemporalClient {
    async fn create_schedule(&self, spec: &TemporalScheduleSpec) -> Result<()> {
        let request = CreateScheduleRequest {
            namespace: self.namespace.clone(),
            schedule_id: spec.temporal_schedule_id.clone(),
            schedule: Some(temporal_schedule(spec, &self.task_queue)),
            identity: SCHEDULE_IDENTITY.to_string(),
            request_id: Uuid::new_v4().to_string(),
            ..Default::default()
        };
        self.client
            .clone()
            .create_schedule(request)
            .await
            .context("Temporal CreateSchedule failed")?;
        Ok(())
    }

    async fn update_schedule(&self, spec: &TemporalScheduleSpec) -> Result<()> {
        let request = UpdateScheduleRequest {
            namespace: self.namespace.clone(),
            schedule_id: spec.temporal_schedule_id.clone(),
            schedule: Some(temporal_schedule(spec, &self.task_queue)),
            identity: SCHEDULE_IDENTITY.to_string(),
            request_id: Uuid::new_v4().to_string(),
            ..Default::default()
        };
        self.client
            .clone()
            .update_schedule(request)
            .await
            .context("Temporal UpdateSchedule failed")?;
        Ok(())
    }

    async fn set_schedule_paused(&self, temporal_schedule_id: &str, paused: bool) -> Result<()> {
        let note = "paused by its owner or by the orchestrator".to_string();
        let patch = if paused {
            SchedulePatch {
                pause: note,
                ..Default::default()
            }
        } else {
            SchedulePatch {
                unpause: note,
                ..Default::default()
            }
        };
        let request = PatchScheduleRequest {
            namespace: self.namespace.clone(),
            schedule_id: temporal_schedule_id.to_string(),
            patch: Some(patch),
            identity: SCHEDULE_IDENTITY.to_string(),
            request_id: Uuid::new_v4().to_string(),
        };
        self.client
            .clone()
            .patch_schedule(request)
            .await
            .context("Temporal PatchSchedule failed")?;
        Ok(())
    }

    async fn delete_schedule(&self, temporal_schedule_id: &str) -> Result<()> {
        let request = DeleteScheduleRequest {
            namespace: self.namespace.clone(),
            schedule_id: temporal_schedule_id.to_string(),
            identity: SCHEDULE_IDENTITY.to_string(),
        };
        match self.client.clone().delete_schedule(request).await {
            Ok(_) => Ok(()),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(()),
            Err(status) => Err(anyhow::anyhow!("Temporal DeleteSchedule failed: {status}")),
        }
    }

    async fn describe_schedule(
        &self,
        temporal_schedule_id: &str,
    ) -> Result<Option<TemporalScheduleDescription>> {
        let request = DescribeScheduleRequest {
            namespace: self.namespace.clone(),
            schedule_id: temporal_schedule_id.to_string(),
        };
        match self.client.clone().describe_schedule(request).await {
            Ok(response) => {
                let response = response.into_inner();
                let paused = response
                    .schedule
                    .as_ref()
                    .and_then(|s| s.state.as_ref())
                    .is_some_and(|state| state.paused);
                let next_action_times = response
                    .info
                    .map(|info| {
                        info.future_action_times
                            .iter()
                            .filter_map(timestamp_to_utc)
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Some(TemporalScheduleDescription {
                    paused,
                    next_action_times,
                }))
            }
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(status) => Err(anyhow::anyhow!(
                "Temporal DescribeSchedule failed: {status}"
            )),
        }
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    use crate::domain::schedule::Recurrence;
    use crate::infrastructure::temporal_proto::temporal::api::schedule::v1::schedule_action::Action;

    fn spec(timing: Timing) -> TemporalScheduleSpec {
        TemporalScheduleSpec {
            temporal_schedule_id: "aegis-schedule-s-1".into(),
            schedule_id: "s-1".into(),
            tenant_id: "u-owner".into(),
            timing,
            paused: false,
        }
    }

    fn assert_common(schedule: &TemporalSchedule) {
        let policies = schedule.policies.as_ref().expect("policies");
        let mut wrong = Vec::new();
        if policies.overlap_policy != ScheduleOverlapPolicy::Skip as i32 {
            wrong.push(format!("overlap_policy {}", policies.overlap_policy));
        }
        if policies.catchup_window.as_ref().map(|d| d.seconds) != Some(600) {
            wrong.push(format!("catchup_window {:?}", policies.catchup_window));
        }
        if policies.pause_on_failure {
            wrong.push("pause_on_failure true".into());
        }
        match schedule.action.as_ref().and_then(|a| a.action.as_ref()) {
            Some(Action::StartWorkflow(start)) => {
                let workflow = start.workflow_type.as_ref().map(|t| t.name.as_str());
                if workflow != Some("aegis_schedule_fire") {
                    wrong.push(format!("workflow type {workflow:?}"));
                }
                let queue = start.task_queue.as_ref().map(|q| q.name.as_str());
                if queue != Some("aegis-queue") {
                    wrong.push(format!("task queue {queue:?}"));
                }
                let input: serde_json::Value =
                    serde_json::from_slice(&start.input.as_ref().expect("input").payloads[0].data)
                        .expect("json input");
                if input != serde_json::json!({"schedule_id": "s-1", "tenant_id": "u-owner"}) {
                    wrong.push(format!("input {input}"));
                }
            }
            other => wrong.push(format!("action {other:?}")),
        }
        assert!(
            wrong.is_empty(),
            "the schedule's policies and action: {wrong:?}"
        );
    }

    /// N5: a recurrence's spec, the SKIP policy, the 600 s window, and an
    /// action that starts the fire workflow with only the two ids.
    #[test]
    fn a_recurrence_is_held_as_cron_timezone_and_jitter() {
        let schedule = temporal_schedule(
            &spec(Timing::Recurrence(Recurrence {
                cron: "0 15 * * 1-5".into(),
                timezone: "Europe/Berlin".into(),
                jitter_seconds: 1_800,
            })),
            "aegis-queue",
        );
        let s = schedule.spec.as_ref().expect("spec");
        assert_eq!(
            (
                s.cron_string.clone(),
                s.timezone_name.as_str(),
                s.jitter.as_ref().map(|d| d.seconds),
                s.calendar.len()
            ),
            (
                vec!["0 15 * * 1-5".to_string()],
                "Europe/Berlin",
                Some(1_800),
                0
            ),
            "the recurrence's spec"
        );
        let state = schedule.state.as_ref().expect("state");
        assert!(!state.limited_actions && !state.paused, "{state:?}");
        assert_common(&schedule);
    }

    /// N5: `at` is one calendar entry, limited to one action.
    #[test]
    fn at_is_held_as_one_calendar_entry_with_one_remaining_action() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-02T15:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let schedule = temporal_schedule(&spec(Timing::Once { at }), "aegis-queue");
        let s = schedule.spec.as_ref().expect("spec");
        let c = &s.calendar;
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(
            (
                c[0].year.as_str(),
                c[0].month.as_str(),
                c[0].day_of_month.as_str(),
                c[0].hour.as_str(),
                c[0].minute.as_str(),
                c[0].second.as_str(),
                s.cron_string.len(),
                s.timezone_name.as_str()
            ),
            ("2026", "11", "2", "15", "4", "5", 0, "UTC")
        );
        let state = schedule.state.as_ref().expect("state");
        assert!(
            state.limited_actions && state.remaining_actions == 1,
            "{state:?}"
        );
        assert_common(&schedule);
    }
}
