// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A new schedule naming an archived version is refused (AEGIS ADR-148,
//! A10): its target is resolved as a run's is, by UUID or by name and
//! version, and an archived row is refused with the name's active version
//! before anything is stored or sent to Temporal. By name alone the
//! schedule names the active version and is created.

use std::sync::{Arc, Mutex};

use aegis_orchestrator_core::application::ports::{
    ScheduleEnginePort, StartWorkflowParams, TemporalScheduleDescription, TemporalScheduleSpec,
    WorkflowEnginePort,
};
use aegis_orchestrator_core::application::register_workflow::{
    RegisterWorkflowUseCase, StandardRegisterWorkflowUseCase,
};
use aegis_orchestrator_core::application::schedule_service::{
    ScheduleError, ScheduleService, ScheduledRunPort,
};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;
use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionStatus};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::repository::ArchivedTarget;
use aegis_orchestrator_core::domain::schedule::{
    Schedule, ScheduleDraft, ScheduleRepository, TargetKind,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{WorkflowId, WorkflowScope};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::InMemoryScheduleRepository;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use async_trait::async_trait;

struct AcceptingEngine;

#[async_trait]
impl WorkflowEnginePort for AcceptingEngine {
    async fn register_workflow(&self, _def: &TemporalWorkflowDefinition) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_workflow(&self, _params: StartWorkflowParams<'_>) -> anyhow::Result<String> {
        Ok("run".to_string())
    }
}

fn engine() -> Arc<tokio::sync::RwLock<Option<Arc<dyn WorkflowEnginePort>>>> {
    Arc::new(tokio::sync::RwLock::new(Some(
        Arc::new(AcceptingEngine) as Arc<dyn WorkflowEnginePort>
    )))
}

/// A Temporal stand-in that counts the schedules it is asked to create.
#[derive(Default)]
struct CountingScheduleEngine {
    created: Mutex<usize>,
}

#[async_trait]
impl ScheduleEnginePort for CountingScheduleEngine {
    async fn create_schedule(&self, _spec: &TemporalScheduleSpec) -> anyhow::Result<()> {
        *self.created.lock().unwrap() += 1;
        Ok(())
    }
    async fn update_schedule(&self, _spec: &TemporalScheduleSpec) -> anyhow::Result<()> {
        Ok(())
    }
    async fn set_schedule_paused(&self, _id: &str, _paused: bool) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_schedule(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn describe_schedule(
        &self,
        _id: &str,
    ) -> anyhow::Result<Option<TemporalScheduleDescription>> {
        Ok(None)
    }
}

/// The run port a schedule's create asks whether its target is archived; it
/// answers through the workflow start use case, which resolves the target
/// as a run's.
struct ResolvingRuns {
    start: Arc<dyn StartWorkflowExecutionUseCase>,
}

#[async_trait]
impl ScheduledRunPort for ResolvingRuns {
    async fn start(&self, _: &Schedule, _: &UserIdentity) -> Result<ExecutionId, String> {
        Err("no run is started here".to_string())
    }
    async fn run_status(
        &self,
        _: TargetKind,
        _: &TenantId,
        _: ExecutionId,
    ) -> anyhow::Result<Option<ExecutionStatus>> {
        Ok(None)
    }
    async fn archived_target(&self, schedule: &Schedule) -> anyhow::Result<Option<ArchivedTarget>> {
        self.start
            .archived_target(
                &schedule.tenant_id,
                &schedule.target,
                schedule.target_version.as_deref(),
            )
            .await
    }
}

fn workflow_yaml(name: &str, version: &str) -> String {
    format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: {name}
  version: "{version}"
spec:
  initial_state: START
  states:
    START:
      kind: System
      command: echo "start"
      transitions:
        - condition: always
          target: END
    END:
      kind: System
      command: echo "done"
      transitions: []
"#
    )
}

async fn register_global(repo: &Arc<InMemoryWorkflowRepository>, version: &str) -> WorkflowId {
    let use_case = StandardRegisterWorkflowUseCase::new(
        repo.clone(),
        engine(),
        Arc::new(EventBus::new(16)),
        Arc::new(InMemoryAgentRepository::new()),
    );
    let registered = use_case
        .register_workflow_for_tenant(
            &TenantId::system(),
            &workflow_yaml("builtin-flow", version),
            true,
            WorkflowScope::Global,
        )
        .await
        .expect("the workflow registers");
    WorkflowId(uuid::Uuid::parse_str(&registered.workflow_id).unwrap())
}

fn alice() -> UserIdentity {
    UserIdentity {
        sub: "alice".into(),
        realm_slug: "zaru-consumer".into(),
        email: Some("alice@example.test".into()),
        email_verified: true,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Pro,
            tenant_id: TenantId::for_consumer_user("alice").unwrap(),
        },
    }
}

fn draft(target: &str, version: Option<&str>) -> ScheduleDraft {
    ScheduleDraft {
        name: Some("nightly".into()),
        target_kind: Some("workflow".into()),
        target: Some(target.into()),
        version: version.map(str::to_string),
        at: Some("2030-01-01T00:00:00Z".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_new_schedule_naming_an_archived_workflow_is_refused_with_the_active_version() {
    let workflows = Arc::new(InMemoryWorkflowRepository::new());
    let old = register_global(&workflows, "1.0.0").await;
    register_global(&workflows, "1.1.0").await;
    let start: Arc<dyn StartWorkflowExecutionUseCase> =
        Arc::new(StandardStartWorkflowExecutionUseCase::new(
            workflows.clone(),
            Arc::new(InMemoryWorkflowExecutionRepository::new()),
            engine(),
            Arc::new(EventBus::new(16)),
        ));
    let store = Arc::new(InMemoryScheduleRepository::new());
    let temporal = Arc::new(CountingScheduleEngine::default());
    let service = ScheduleService::new(
        store.clone(),
        temporal.clone(),
        Arc::new(ResolvingRuns { start }),
    );
    let owner = alice();
    let tenant = TenantId::for_consumer_user("alice").unwrap();

    for refused in [
        draft(&old.0.to_string(), None),
        draft("builtin-flow", Some("1.0.0")),
    ] {
        let err = service
            .create(&owner, &tenant, refused)
            .await
            .expect_err("a schedule of an archived row is refused");
        let ScheduleError::Archived(target) = err else {
            panic!("expected the archived refusal, got: {err}");
        };
        assert_eq!(target.active_version.as_deref(), Some("1.1.0"));
        assert_eq!(
            target.to_string(),
            "workflow 'builtin-flow' version '1.0.0' is archived; the active version is '1.1.0'"
        );
    }
    assert!(
        store.list_for_owner("alice").await.unwrap().is_empty(),
        "a refused schedule stores nothing"
    );
    assert_eq!(
        *temporal.created.lock().unwrap(),
        0,
        "a refused schedule creates no Temporal Schedule"
    );

    service
        .create(&owner, &tenant, draft("builtin-flow", None))
        .await
        .expect("by name alone the schedule names the active version");
    assert_eq!(*temporal.created.lock().unwrap(), 1);
}
