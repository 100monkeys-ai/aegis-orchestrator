// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A workflow start keeps the dispatch's binding choices for its agent
//! states and nowhere else (Zaru ADR-0055 D14): the workflow's input schema
//! and the Temporal input never see the reserved key `contexts`; the
//! persisted workflow execution keeps it, where an agent state reads it.

use aegis_orchestrator_core::application::ports::{StartWorkflowParams, WorkflowEnginePort};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::repository::{
    WorkflowExecutionRepository, WorkflowRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowMetadata,
    WorkflowSpec, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const BINDING: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";

/// Records the input each workflow start hands Temporal.
#[derive(Default)]
struct RecordingEngine {
    inputs: Mutex<Vec<HashMap<String, Value>>>,
}

#[async_trait]
impl WorkflowEnginePort for RecordingEngine {
    async fn register_workflow(&self, _def: &TemporalWorkflowDefinition) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_workflow(&self, params: StartWorkflowParams<'_>) -> anyhow::Result<String> {
        self.inputs.lock().unwrap().push(params.input.clone());
        Ok("recorded-run-id".to_string())
    }
}

/// A one-state workflow whose input schema refuses any key but `topic`.
fn closed_schema_workflow() -> Workflow {
    let mut states = HashMap::new();
    states.insert(
        StateName::new("START").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo ok".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::Always,
                target: StateName::new("END").unwrap(),
                feedback: None,
            }],
            timeout: None,
            max_state_visits: None,
        },
    );
    states.insert(
        StateName::new("END").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo done".to_string(),
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
            name: "contexts-closed-schema".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: Some(json!({
                "type": "object",
                "required": ["topic"],
                "properties": { "topic": { "type": "string" } },
                "additionalProperties": false
            })),
            output_schema: None,
            output_template: None,
        },
        WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
            max_total_transitions: None,
            default_schedule: None,
        },
    )
    .unwrap()
}

#[tokio::test]
async fn the_contexts_reach_the_persisted_execution_and_never_the_schema_or_temporal() {
    let workflow = closed_schema_workflow();
    let tenant = TenantId::consumer();
    let workflows = Arc::new(InMemoryWorkflowRepository::new());
    workflows.save_for_tenant(&tenant, &workflow).await.unwrap();
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let engine = Arc::new(RecordingEngine::default());
    let use_case = StandardStartWorkflowExecutionUseCase::new(
        workflows,
        executions.clone(),
        Arc::new(tokio::sync::RwLock::new(Some(
            engine.clone() as Arc<dyn WorkflowEnginePort>
        ))),
        Arc::new(EventBus::new(8)),
    );

    let contexts = json!({ "nuclear-notes": BINDING });
    let started = use_case
        .start_execution(StartWorkflowExecutionRequest {
            workflow_id: workflow.metadata.name.clone(),
            input: json!({ "topic": "units", "contexts": contexts }),
            blackboard: None,
            version: None,
            tenant_id: Some(tenant.clone()),
            security_context_name: None,
            intent: None,
        })
        .await
        .unwrap_or_else(|e| panic!("the workflow's schema saw the contexts: {e:#}"));

    let temporal_inputs = engine.inputs.lock().unwrap().clone();
    assert_eq!(temporal_inputs.len(), 1, "one start reached Temporal");
    assert!(
        !temporal_inputs[0].contains_key("contexts"),
        "Temporal's input carried the contexts: {:?}",
        temporal_inputs[0]
    );
    assert_eq!(temporal_inputs[0].get("topic"), Some(&json!("units")));

    let id = ExecutionId(uuid::Uuid::parse_str(&started.execution_id).unwrap());
    let persisted = executions
        .find_by_id_for_tenant(&tenant, id)
        .await
        .unwrap()
        .expect("the workflow execution was persisted");
    assert_eq!(
        persisted.input.get("contexts"),
        Some(&contexts),
        "the persisted workflow execution lost the contexts its agent states inherit"
    );
}

/// AEGIS ADR-126, Update of 2026-10-07 (2), clause 3: the conversation a run
/// was started from reaches the persisted workflow execution, where its
/// agent states inherit it, and never the workflow's input schema (closed
/// here) or the Temporal input.
#[tokio::test]
async fn the_conversation_reaches_the_persisted_execution_and_never_the_schema_or_temporal() {
    const CONVERSATION: &str = "6c1f0b52-8a3e-4d7b-9f21-0e5d4c3b2a19";
    let workflow = closed_schema_workflow();
    let tenant = TenantId::consumer();
    let workflows = Arc::new(InMemoryWorkflowRepository::new());
    workflows.save_for_tenant(&tenant, &workflow).await.unwrap();
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let engine = Arc::new(RecordingEngine::default());
    let use_case = StandardStartWorkflowExecutionUseCase::new(
        workflows,
        executions.clone(),
        Arc::new(tokio::sync::RwLock::new(Some(
            engine.clone() as Arc<dyn WorkflowEnginePort>
        ))),
        Arc::new(EventBus::new(8)),
    );

    let started = use_case
        .start_execution(StartWorkflowExecutionRequest {
            workflow_id: workflow.metadata.name.clone(),
            input: json!({ "topic": "units", "conversation_id": CONVERSATION }),
            blackboard: None,
            version: None,
            tenant_id: Some(tenant.clone()),
            security_context_name: None,
            intent: None,
        })
        .await
        .unwrap_or_else(|e| panic!("the workflow's schema saw the conversation: {e:#}"));

    let temporal_inputs = engine.inputs.lock().unwrap().clone();
    assert_eq!(temporal_inputs.len(), 1, "one start reached Temporal");
    assert!(
        !temporal_inputs[0].contains_key("conversation_id"),
        "Temporal's input carried the conversation: {:?}",
        temporal_inputs[0]
    );

    let id = ExecutionId(uuid::Uuid::parse_str(&started.execution_id).unwrap());
    let persisted = executions
        .find_by_id_for_tenant(&tenant, id)
        .await
        .unwrap()
        .expect("the workflow execution was persisted");
    assert_eq!(
        persisted.input.get("conversation_id"),
        Some(&json!(CONVERSATION)),
        "the persisted workflow execution lost the conversation its agent states inherit"
    );
}

/// One profile of `PERSON`'s over one mailbox, for the starts of AEGIS
/// ADR-140 D6.
struct OneProfile(uuid::Uuid);

const PERSON: &str = "profile-person";

#[async_trait]
impl aegis_orchestrator_core::application::profile_service::RunProfiles for OneProfile {
    async fn contexts_of(
        &self,
        _: &TenantId,
        person: Option<&str>,
        id: uuid::Uuid,
    ) -> Result<
        serde_json::Map<String, Value>,
        aegis_orchestrator_core::application::profile_service::ProfileError,
    > {
        if id == self.0 && person == Some(PERSON) {
            Ok(json!({ "imap": [BINDING] }).as_object().unwrap().clone())
        } else {
            Err(aegis_orchestrator_core::application::profile_service::ProfileError::NotFound)
        }
    }
    async fn admission_of(
        &self,
        _: &TenantId,
        _: Option<&str>,
        _: uuid::Uuid,
    ) -> Result<
        aegis_orchestrator_core::application::profile_service::ProfileAdmission,
        aegis_orchestrator_core::application::profile_service::ProfileError,
    > {
        Err(aegis_orchestrator_core::application::profile_service::ProfileError::NotFound)
    }
}

/// AEGIS ADR-140 D6, D10: a workflow started on a profile reaches the
/// persisted execution with the profile and the profile's bindings written
/// into `contexts` for its agent states, and neither reaches the schema or
/// Temporal; a profile not the person's, and one beside chosen contexts,
/// refuse the start with their sentences.
#[tokio::test]
async fn a_workflow_on_a_profile_keeps_it_and_its_bindings_for_its_agent_states() {
    use aegis_orchestrator_core::domain::execution::{PROFILE_GONE, PROFILE_WITH_CONTEXTS};
    use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
    let workflow = closed_schema_workflow();
    let tenant = TenantId::consumer();
    let workflows = Arc::new(InMemoryWorkflowRepository::new());
    workflows.save_for_tenant(&tenant, &workflow).await.unwrap();
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let engine = Arc::new(RecordingEngine::default());
    let use_case = StandardStartWorkflowExecutionUseCase::new(
        workflows,
        executions.clone(),
        Arc::new(tokio::sync::RwLock::new(Some(
            engine.clone() as Arc<dyn WorkflowEnginePort>
        ))),
        Arc::new(EventBus::new(8)),
    );
    let profile = uuid::Uuid::new_v4();
    use_case.set_profiles(Arc::new(OneProfile(profile)));
    let person = |sub: &str| UserIdentity {
        sub: sub.to_string(),
        realm_slug: "zaru-consumer".to_string(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Pro,
            tenant_id: tenant.clone(),
        },
    };
    let start = |input: Value, sub: &'static str| {
        let use_case = &use_case;
        let tenant = tenant.clone();
        let name = workflow.metadata.name.clone();
        let identity = person(sub);
        async move {
            use_case
                .start_execution_for_tenant(
                    &tenant,
                    StartWorkflowExecutionRequest {
                        workflow_id: name,
                        input,
                        blackboard: None,
                        version: None,
                        tenant_id: Some(tenant.clone()),
                        security_context_name: None,
                        intent: None,
                    },
                    Some(&identity),
                )
                .await
        }
    };
    let mut complaints = Vec::new();
    match start(
        json!({ "topic": "units", "profile": profile.to_string() }),
        PERSON,
    )
    .await
    {
        Ok(started) => {
            let id = ExecutionId(uuid::Uuid::parse_str(&started.execution_id).unwrap());
            let persisted = executions
                .find_by_id_for_tenant(&tenant, id)
                .await
                .unwrap()
                .expect("persisted");
            if persisted.input.get("contexts") != Some(&json!({ "imap": [BINDING] }))
                || persisted.input.get("profile") != Some(&json!(profile.to_string()))
            {
                complaints.push(format!("the persisted input was {}", persisted.input));
            }
            let temporal = engine.inputs.lock().unwrap().clone();
            if temporal
                .iter()
                .any(|i| i.contains_key("profile") || i.contains_key("contexts"))
            {
                complaints.push(format!("Temporal's input carried them: {temporal:?}"));
            }
        }
        Err(e) => complaints.push(format!("the person's own profile was refused: {e:#}")),
    }
    for (case, input, sub, sentence) in [
        (
            "another person's profile",
            json!({ "topic": "x", "profile": profile.to_string() }),
            "someone-else",
            PROFILE_GONE,
        ),
        (
            "a profile beside chosen contexts",
            json!({ "topic": "x", "profile": profile.to_string(), "contexts": { "imap": BINDING } }),
            PERSON,
            PROFILE_WITH_CONTEXTS,
        ),
    ] {
        match start(input, sub).await {
            Ok(_) => complaints.push(format!("{case}: started")),
            Err(e) if format!("{e:#}").contains(sentence) => {}
            Err(e) => complaints.push(format!("{case}: answered {e:#}")),
        }
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
