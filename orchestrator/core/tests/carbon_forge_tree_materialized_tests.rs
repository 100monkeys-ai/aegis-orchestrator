// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A carbon-forge run's tree, materialized before its first agent (AEGIS ADR-147 C4)
//!
//! AEGIS ADR-147 (Proposed, page revision 55541) C4: `trees.materialize`
//! puts the named revision into the run's volume `tree`, mounted at
//! `/workspace`, before the first agent; each file is verified against its
//! SHA-256, and the run's record quotes the blob refs. The start is given
//! its tree service as it is given its repositories (`set_repositories`):
//! the interpreter's tool invocation service, which reaches Nuclear Notes
//! through the SEAL gateway with the person's credential for the binding
//! the run's `contexts` chose (AEGIS ADR-132 H1 to H8). Here that gateway
//! is the loopback one of `carbon_forge_support`, answering the recorded
//! `trees.materialize` and `blobs.get` of Nuclear Notes at `76d62e5`.
//!
//! Until the start can be given a tree service this file does not compile:
//! its red is `no method named set_trees found for struct
//! StandardStartWorkflowExecutionUseCase`, the assertion that no start can
//! materialize a tree.
//!
//! | Clause | Test |
//! |---|---|
//! | C4: the revision, verified, before ANALYZE | `a_run_reaches_analyze_with_its_tree_materialized_from_the_recorded_revision` |
//! | C4: a blob that does not hash to its entry | `a_blob_that_does_not_hash_to_its_entry_refuses_the_start` |
//! | ADR-147 Update `execution-token-2026-10-17` X2, X3: minted with the binding before the tree, carrying its calls | `execution_token_is_minted_with_the_binding_before_the_tree_and_carries_its_calls` |
//! | X2: a refused mint refuses the run before it is persisted | `execution_token_refused_mint_refuses_the_run_before_it_is_persisted` |
//! | X4: the terminal event revokes the child and deletes its secret | `execution_token_terminal_event_revokes_the_child_and_deletes_its_secret` |
//! | X3: the child's secret is in no record or event | `execution_token_secret_is_in_no_serialized_execution_record_or_event` |

mod carbon_forge_support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};

use aegis_orchestrator_core::application::ports::{StartWorkflowParams, WorkflowEnginePort};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;
use aegis_orchestrator_core::application::tool_approval_service::ToolApprovalService;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::fsal::AegisFSAL;
use aegis_orchestrator_core::domain::repository::{
    VolumeRepository, WorkflowExecutionRepository, WorkflowRepository,
};
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryVolumeRepository, InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

use carbon_forge_support::*;

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

/// carbon-forge's start, its schema closed to all but `task`: the schema
/// never sees `trees` or `contexts`, which are the platform's.
fn workflow() -> aegis_orchestrator_core::domain::workflow::Workflow {
    WorkflowParser::parse_yaml(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: carbon-forge-start
  input_schema:
    type: object
    required: [task]
    additionalProperties: false
    properties:
      task: { type: string }
spec:
  initial_state: ANALYZE
  states:
    ANALYZE:
      kind: System
      command: "true"
      transitions: []
"#,
    )
    .unwrap()
}

struct Started {
    answer: anyhow::Result<String>,
    service:
        Arc<aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService>,
    binding: Arc<OneBinding>,
    event_bus: Arc<EventBus>,
    events: aegis_orchestrator_core::infrastructure::event_bus::EventReceiver,
    temporal: Vec<HashMap<String, Value>>,
    calls: Arc<
        Mutex<Vec<aegis_orchestrator_core::infrastructure::seal_gateway_proto::InvokeToolRequest>>,
    >,
    executions: Arc<InMemoryWorkflowExecutionRepository>,
    volumes: Arc<InMemoryVolumeRepository>,
    tenant: TenantId,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dirs: tempfile::TempDir,
}

/// Start carbon-forge on `TREE_PATH` at `REVISION`, Nuclear Notes answering
/// `blobs.get` with `bytes`.
async fn start(bytes: &str) -> Started {
    start_answering(bytes, |_| {}).await
}

/// [`start`], Nuclear Notes' answers changed by `answers`.
async fn start_answering(
    bytes: &str,
    answers: impl FnOnce(&mut HashMap<String, Answer>),
) -> Started {
    let dirs = tempfile::tempdir().unwrap();
    let mut stub = nuclear_notes(uuid::Uuid::nil());
    stub.answers.insert(
        "blobs.get".to_string(),
        Answer::Result(json!({
            "sha256": sha256_hex(README_BEFORE.as_bytes()),
            "byte_size": bytes.len(),
            "offset": 0,
            "length": bytes.len(),
            "eof": true,
            "content_base64": base64::engine::general_purpose::STANDARD.encode(bytes)
        })),
    );
    answers(&mut stub.answers);
    let calls = stub.calls.clone();
    let (url, ca, shutdown) = serve_tls(stub, dirs.path()).await;

    let tenant = TenantId::consumer();
    let event_bus = Arc::new(EventBus::new(256));
    let events = event_bus.subscribe();
    let volumes = Arc::new(InMemoryVolumeRepository::new());
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(
            aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider::new(
                dirs.path().join("fsal"),
            )
            .unwrap(),
        ),
        volumes.clone() as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    ));
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let binding = Arc::new(OneBinding::default());
    let service = tool_service_with(
        url,
        &ca,
        fsal,
        Arc::new(InMemorySecurityContextRepository::new()),
        executions.clone(),
        Arc::new(ToolApprovalService::new(
            Arc::new(InMemoryToolApprovalRepository::new()),
            event_bus.clone(),
        )),
        binding.clone(),
    );

    let workflow = workflow();
    let workflows = Arc::new(InMemoryWorkflowRepository::new());
    workflows.save_for_tenant(&tenant, &workflow).await.unwrap();
    let engine = Arc::new(RecordingEngine::default());
    let use_case = StandardStartWorkflowExecutionUseCase::new(
        workflows,
        executions.clone(),
        Arc::new(tokio::sync::RwLock::new(Some(
            engine.clone() as Arc<dyn WorkflowEnginePort>
        ))),
        event_bus.clone(),
    );
    // AEGIS ADR-147 C4: the start is given its tree service.
    use_case.set_trees(service.clone());

    // The run's person starts it: the gateway acts for them alone.
    let person = aegis_orchestrator_core::domain::iam::UserIdentity {
        sub: PERSON.to_string(),
        realm_slug: "zaru-consumer".to_string(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: aegis_orchestrator_core::domain::iam::IdentityKind::ConsumerUser {
            zaru_tier: aegis_orchestrator_core::domain::iam::ZaruTier::Free,
            tenant_id: tenant.clone(),
        },
    };
    let answer = use_case
        .start_execution_for_tenant(
            &tenant,
            StartWorkflowExecutionRequest {
                workflow_id: workflow.metadata.name.clone(),
                input: json!({
                    "task": "the task",
                    "trees": [{ "workspace": WORKSPACE, "path": TREE_PATH, "revision": REVISION }],
                    "contexts": { SERVER: BINDING }
                }),
                blackboard: None,
                version: None,
                tenant_id: Some(tenant.clone()),
                security_context_name: None,
                intent: None,
            },
            Some(&person),
        )
        .await
        .map(|s| s.execution_id);
    let temporal = engine.inputs.lock().unwrap().clone();
    Started {
        answer,
        service,
        binding,
        event_bus,
        events,
        temporal,
        calls,
        executions,
        volumes,
        tenant,
        _shutdown: shutdown,
        _dirs: dirs,
    }
}

#[tokio::test]
async fn a_run_reaches_analyze_with_its_tree_materialized_from_the_recorded_revision() {
    let started = start(README_BEFORE).await;
    let id = started
        .answer
        .as_ref()
        .unwrap_or_else(|e| panic!("AEGIS ADR-147 C4: the run did not start: {e:#}"));

    assert_eq!(started.temporal.len(), 1, "one start reached Temporal");
    for key in ["trees", "contexts"] {
        assert!(
            !started.temporal[0].contains_key(key),
            "AEGIS ADR-147 C2: Temporal was handed the platform's `{key}`"
        );
    }

    let calls = calls_of(&started.calls);
    let tools: Vec<&str> = calls.iter().map(|(t, _)| t.as_str()).collect();
    let materialized = calls.iter().position(|(t, a)| {
        t == "trees.materialize"
            && a["workspace"].as_str() == Some(WORKSPACE)
            && a["path_or_id"].as_str() == Some(TREE_PATH)
            && a["revision_id"].as_i64() == Some(REVISION)
    });
    let read = calls.iter().position(|(t, a)| {
        t == "blobs.get"
            && a["sha256"].as_str() == Some(sha256_hex(README_BEFORE.as_bytes()).as_str())
    });
    assert!(
        matches!((materialized, read), (Some(m), Some(r)) if m < r),
        "AEGIS ADR-147 C4: the revision was not materialized and its blob read: {tools:?}"
    );

    let run = ExecutionId(uuid::Uuid::parse_str(id).unwrap());
    let persisted = started
        .executions
        .find_by_id_for_tenant(&started.tenant, run)
        .await
        .unwrap()
        .expect("the run was persisted");
    let tree = &persisted.input["trees"][0];
    assert_eq!(tree["workspace"].as_str(), Some(WORKSPACE));
    assert_eq!(tree["path"].as_str(), Some(TREE_PATH));
    assert_eq!(tree["revision"].as_i64(), Some(REVISION));
    assert_eq!(
        tree["blobs"],
        json!([{ "path": "README.md", "sha256": sha256_hex(README_BEFORE.as_bytes()) }]),
        "AEGIS ADR-147 C4: the run's record does not quote the blob refs it materialized"
    );
    let volume = tree["volume_id"]
        .as_str()
        .and_then(|v| uuid::Uuid::parse_str(v).ok())
        .map(VolumeId)
        .unwrap_or_else(|| panic!("AEGIS ADR-147 C4: the run's tree names no volume: {tree}"));
    let volume = started
        .volumes
        .find_by_id(volume)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("AEGIS ADR-147 C4: the run's tree volume does not exist"));
    assert_eq!(
        volume.name, "tree",
        "AEGIS ADR-147 C4: the run's volume is not `tree`"
    );
}

#[tokio::test]
async fn a_blob_that_does_not_hash_to_its_entry_refuses_the_start() {
    let started = start("not the README\n").await;
    let Err(refusal) = &started.answer else {
        panic!(
            "AEGIS ADR-147 C4: a blob that does not hash to its entry was materialized; Temporal was handed {:?}",
            started.temporal
        );
    };
    let text = refusal.to_string();
    assert!(
        text.contains("README.md") && text.contains(&sha256_hex(README_BEFORE.as_bytes())),
        "AEGIS ADR-147 C4: the refusal does not name the entry and its SHA-256: {text}"
    );
    assert!(
        started.temporal.is_empty(),
        "a refused start reached Temporal"
    );
}

// ===========================================================================
// AEGIS ADR-147 Update `execution-token-2026-10-17`: the run's own token
// ===========================================================================

/// The run a start answered.
fn run_of(started: &Started) -> uuid::Uuid {
    let id = started
        .answer
        .as_ref()
        .unwrap_or_else(|e| panic!("the run did not start: {e:#}"));
    uuid::Uuid::parse_str(id).unwrap()
}

/// The run's terminal event, as the worker sends it.
async fn complete(started: &Started, run: uuid::Uuid) {
    let listener = aegis_orchestrator_core::infrastructure::TemporalEventListener::new(
        started.event_bus.clone(),
        started.executions.clone(),
    );
    listener.set_execution_tokens(started.service.clone());
    listener
        .handle_event(
            aegis_orchestrator_core::infrastructure::TemporalEventPayload {
                event_type: "WorkflowExecutionCompleted".to_string(),
                execution_id: run.to_string(),
                temporal_sequence_number: 100,
                final_blackboard: Some(json!({ "result": "done" })),
                timestamp: "2026-10-17T12:00:00Z".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn execution_token_is_minted_with_the_binding_before_the_tree_and_carries_its_calls() {
    let started = start(README_BEFORE).await;
    let run = run_of(&started);
    let calls = started.calls.lock().unwrap().clone();
    let tools: Vec<&str> = calls.iter().map(|c| c.tool.as_str()).collect();
    assert_eq!(
        tools,
        vec![
            "cortex.ground",
            "tokens.mint_execution",
            "trees.materialize",
            "blobs.get"
        ],
        "X2: the run's token was not minted once, after its workspace was grounded and before its tree"
    );
    let credential = |i: usize| calls[i].credential.as_ref().map(|c| c.value.clone());
    assert_eq!(
        credential(0).as_deref(),
        Some(TOKEN),
        "X2: the grounding is the binding's"
    );
    assert_eq!(
        credential(1).as_deref(),
        Some(TOKEN),
        "X2: the mint is the binding's"
    );
    for call in &calls[2..] {
        assert_eq!(
            call.credential.as_ref().map(|c| c.value.as_str()),
            Some(CHILD_TOKEN),
            "X3: {} carried the binding, not the run's execution token",
            call.tool
        );
    }
    let mint: Value = serde_json::from_str(&calls[1].arguments_json).unwrap();
    assert_eq!(
        mint,
        json!({
            "execution_id": run,
            "instance_id": INSTANCE_ID,
            "workspace_id": WORKSPACE,
            "tree_path": TREE_PATH,
            "lane_label": lane_of(run),
            "tools": ["blobs.get", "cortex.ground", "trees.materialize"],
            "lifetime_seconds": 86400
        }),
        "X2: the mint does not name the run, its tree, its lane, its declared tools and a day"
    );
    assert!(
        started.binding.holds_child(run),
        "X3: the child's secret was not kept"
    );
}

#[tokio::test]
async fn execution_token_refused_mint_refuses_the_run_before_it_is_persisted() {
    const REFUSAL: &str =
        "This token may not mint execution tokens: its owner has not allowed it in the token form.";
    let started = start_answering(README_BEFORE, |answers| {
        answers.insert(
            "tokens.mint_execution".to_string(),
            Answer::RemoteError(REFUSAL.to_string()),
        );
    })
    .await;
    let Err(refusal) = &started.answer else {
        panic!("X2: a refused mint started the run");
    };
    assert!(
        refusal.to_string().contains(REFUSAL),
        "X2: the run was not refused with Carbon Forge's sentence: {refusal:#}"
    );
    assert!(
        started
            .executions
            .find_active_for_tenant(&started.tenant)
            .await
            .unwrap()
            .is_empty(),
        "X2: a run whose mint was refused was persisted"
    );
    assert!(
        started.temporal.is_empty(),
        "X2: a refused run reached Temporal"
    );
    let tools: Vec<String> = calls_of(&started.calls)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(
        tools,
        vec!["cortex.ground", "tokens.mint_execution"],
        "X2: a run whose mint was refused reached its tree on the binding"
    );
}

#[tokio::test]
async fn execution_token_terminal_event_revokes_the_child_and_deletes_its_secret() {
    let started = start(README_BEFORE).await;
    let run = run_of(&started);
    assert!(started.binding.holds_child(run));
    complete(&started, run).await;
    let calls = started.calls.lock().unwrap().clone();
    let revokes: Vec<_> = calls
        .iter()
        .filter(|c| c.tool == "tokens.revoke_execution")
        .collect();
    assert_eq!(
        revokes.len(),
        1,
        "X4: the terminal event did not revoke the child once"
    );
    let args: Value = serde_json::from_str(&revokes[0].arguments_json).unwrap();
    assert_eq!(
        args,
        json!({ "token_id": CHILD_TOKEN_ID }),
        "X4: the revoke names another token"
    );
    assert_eq!(
        revokes[0].credential.as_ref().map(|c| c.value.as_str()),
        Some(TOKEN),
        "X4: the revoke is the binding's, the child's parent"
    );
    assert!(
        !started.binding.holds_child(run),
        "X4: the child's secret was not deleted on the terminal event"
    );
    // A second terminal event finds nothing to release.
    complete(&started, run).await;
    assert_eq!(
        calls_of(&started.calls)
            .iter()
            .filter(|(t, _)| t == "tokens.revoke_execution")
            .count(),
        1
    );
}

#[tokio::test]
async fn execution_token_secret_is_in_no_serialized_execution_record_or_event() {
    let mut started = start(README_BEFORE).await;
    let run = run_of(&started);
    let record = started
        .executions
        .find_by_id_for_tenant(&started.tenant, ExecutionId(run))
        .await
        .unwrap()
        .expect("the run was persisted");
    let serialized = serde_json::to_string(&record).unwrap();
    assert!(
        !serialized.contains(CHILD_TOKEN) && !format!("{record:?}").contains(CHILD_TOKEN),
        "X3: the execution record carries the child's secret"
    );
    complete(&started, run).await;
    let record = started
        .executions
        .find_by_id_for_tenant(&started.tenant, ExecutionId(run))
        .await
        .unwrap()
        .unwrap();
    assert!(!serde_json::to_string(&record)
        .unwrap()
        .contains(CHILD_TOKEN));
    let mut seen = 0;
    while let Ok(event) = started.events.try_recv() {
        seen += 1;
        assert!(
            !format!("{event:?}").contains(CHILD_TOKEN),
            "X3: an event carries the child's secret: {event:?}"
        );
    }
    assert!(seen > 0, "the run published no event");
    for (tool, args) in calls_of(&started.calls) {
        assert!(
            !args.to_string().contains(CHILD_TOKEN),
            "X3: {tool}'s arguments carry the child's secret"
        );
    }
}
