// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # The drift report at carbon-forge's gate (AEGIS ADR-147 Update
//! `drift-report-at-the-gate-2026-10-23`)
//!
//! carbon-forge gains the Action state DRIFT (`drift_report`) after COMMIT,
//! on the draft and the landing paths alike: it reads Carbon Forge's
//! `symbols.drift` (Carbon Forge ADR-0207) on the run's lane with the run's
//! tree, through the run's own execution token, as `tree_land` calls
//! `lanes.land`, and answers in the proto's existing fields: the report's
//! hash as `commit_sha`, its finding counts by class as `branch`, the whole
//! report as `diff`. LAND's approval request carries `drift` (the counts by
//! class and the first ten findings) beside `tree`, `lane` and `intent`, so
//! the person sees the tension at the Human gate before the lane replays
//! onto the trunk. A refused, failed or pending report is recorded and never
//! ends the run: the report advises, and the gate's verdict stays the
//! person's.
//!
//! Carbon Forge is the loopback SEAL gateway of `carbon_forge_support`,
//! answering `symbols.drift` as `catalog-semantics.ts` 404 to 440 answers it
//! at carbon-forge `ba9c2f33`.
//!
//! | Clause | Test |
//! |---|---|
//! | `drift_report` is an action of the closed set that touches nothing | `drift_report_is_an_action_taking_no_arguments_that_touches_nothing` |
//! | DRIFT reads the run's lane and tree, answering the hash and the counts | `drift_report_reads_the_runs_lane_and_tree_and_answers_the_hash_and_the_counts` |
//! | DRIFT after COMMIT; a draft run ends at COMPLETE with the report | `carbon_forge_reads_the_drift_after_commit_and_a_draft_run_completes_with_it` |
//! | LAND's approval carries `drift` beside tree, lane and intent | `the_landings_approval_request_carries_the_drift_beside_tree_lane_and_intent` |
//! | The gate shows the first ten findings and every count | `the_gate_shows_the_first_ten_findings_and_every_count` |
//! | A refused report is recorded and the landing goes on | `a_refused_drift_report_is_recorded_and_the_landing_goes_on` |
//! | A pending report is counted and refuses nothing | `a_pending_drift_report_counts_its_pending_files_and_refuses_nothing` |
//! | A failed read is a sentence, never an error ending the run | `a_drift_report_that_could_not_be_read_answers_its_sentence_never_an_error` |

mod carbon_forge_support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::oneshot;

use aegis_orchestrator_core::application::git_repo_service::RepositoryActionAnswer;
use aegis_orchestrator_core::application::tool_approval_service::ToolApprovalService;
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::fsal::AegisFSAL;
use aegis_orchestrator_core::domain::repository::{VolumeRepository, WorkflowExecutionRepository};
use aegis_orchestrator_core::domain::security_context::{
    Capability, SecurityContext, SecurityContextMetadata, SecurityContextRepository,
};
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use aegis_orchestrator_core::domain::tool_approval::{
    ToolApprovalDecision, ToolApprovalRequest, ToolApprovalStatus,
};
use aegis_orchestrator_core::domain::volume::{
    StorageClass, Volume, VolumeBackend, VolumeOwnership,
};
use aegis_orchestrator_core::domain::workflow::{ActionName, ActionNeed, WorkflowExecution};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryVolumeRepository, InMemoryWorkflowExecutionRepository,
};
use aegis_orchestrator_core::infrastructure::seal_gateway_proto::InvokeToolRequest;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

use carbon_forge_support::*;

/// The Update's anchor, named by every failure.
const UPDATE: &str = "AEGIS ADR-147 Update drift-report-at-the-gate-2026-10-23";

/// carbon-forge as the daemon registers it.
const CARBON_FORGE: &str = include_str!("../../../cli/templates/workflows/carbon-forge.yaml");

/// Carbon Forge's refusal of a lane whose workspace's seams page does not
/// parse (Carbon Forge ADR-0207; `governed-seams.ts` at `ba9c2f33`).
const MALFORMED_SEAMS: &str = "The governed seams page .governed-seams is malformed at line 3: a row needs four cells, tree | glob | symbols | records.";

// ===========================================================================
// The fixture
// ===========================================================================

struct Fixture {
    service: Arc<ToolInvocationService>,
    approvals: Arc<ToolApprovalService>,
    calls: Arc<Mutex<Vec<InvokeToolRequest>>>,
    tenant: TenantId,
    run: uuid::Uuid,
    _shutdown: oneshot::Sender<()>,
    _dirs: tempfile::TempDir,
}

fn landing_context() -> SecurityContext {
    SecurityContext {
        name: LANDING_CONTEXT.to_string(),
        description: "the landing's context".to_string(),
        capabilities: vec![Capability {
            tool_pattern: "aegis.tree.*".to_string(),
            path_allowlist: None,
            command_allowlist: None,
            subcommand_allowlist: None,
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        }],
        deny_list: vec![],
        metadata: SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

/// A carbon-forge run of `PERSON` whose COMMIT wrote its change to its
/// lane: its tree materialized from `REVISION` into its volume `tree`, its
/// `contexts` choosing the person's binding, and, with `child`, holding the
/// execution token its start minted. Carbon Forge answers as `answers`
/// changes it.
async fn fixture_holding(
    answers: impl FnOnce(uuid::Uuid, &mut HashMap<String, Answer>),
    child: bool,
) -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let run = ExecutionId::new();
    let binding = Arc::new(if child {
        OneBinding::holding_child(run.0)
    } else {
        OneBinding::default()
    });
    let mut stub = nuclear_notes(run.0);
    answers(run.0, &mut stub.answers);
    let calls = stub.calls.clone();
    let (url, ca, shutdown) = serve_tls(stub, dirs.path()).await;

    let tenant = TenantId::consumer();
    let event_bus = Arc::new(EventBus::new(256));
    let volume_repo = Arc::new(InMemoryVolumeRepository::new());
    let volume_path = PathBuf::from(format!("/aegis/volumes/{}/tree-of-the-run", tenant));
    let tree = dirs
        .path()
        .join("fsal")
        .join(volume_path.strip_prefix("/").unwrap());
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("README.md"), README_AFTER).unwrap();
    let volume = Volume::new(
        "tree".to_string(),
        tenant.clone(),
        StorageClass::persistent(),
        VolumeBackend::HostPath { path: volume_path },
        64 * 1024 * 1024,
        VolumeOwnership::persistent(PERSON),
    )
    .unwrap();
    volume_repo.save(&volume).await.unwrap();
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(
            aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider::new(
                dirs.path().join("fsal"),
            )
            .unwrap(),
        ),
        volume_repo.clone() as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    ));
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(landing_context()).await.unwrap();
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let approvals = Arc::new(ToolApprovalService::new(
        Arc::new(InMemoryToolApprovalRepository::new()),
        event_bus.clone(),
    ));
    let service = tool_service_with(
        url,
        &ca,
        fsal,
        contexts,
        executions.clone(),
        approvals.clone(),
        binding,
    );

    let workflow = WorkflowParser::parse_yaml(CARBON_FORGE).unwrap();
    let input = json!({
        "task": "the task",
        "draft_only": false,
        "trees": [{
            "workspace": WORKSPACE,
            "path": TREE_PATH,
            "revision": REVISION,
            "volume_id": volume.id.0.to_string()
        }],
        "contexts": { SERVER: BINDING }
    });
    let mut execution = WorkflowExecution::new(&workflow, run, input);
    execution.tenant_id = tenant.clone();
    execution.initiating_user_sub = Some(PERSON.to_string());
    executions
        .save_for_tenant(&tenant, &execution)
        .await
        .unwrap();

    Fixture {
        service,
        approvals,
        calls,
        tenant,
        run: run.0,
        _shutdown: shutdown,
        _dirs: dirs,
    }
}

async fn fixture_answering(
    answers: impl FnOnce(uuid::Uuid, &mut HashMap<String, Answer>),
) -> Fixture {
    fixture_holding(answers, true).await
}

async fn fixture() -> Fixture {
    fixture_answering(|_, _| {}).await
}

impl Fixture {
    fn calls(&self) -> Vec<(String, Value)> {
        calls_of(&self.calls)
    }

    /// The `symbols.drift` calls Carbon Forge received.
    fn drift_calls(&self) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|(t, _)| t == "symbols.drift")
            .map(|(_, a)| a)
            .collect()
    }

    /// The drift step, run as the interpreter runs it.
    async fn drift(&self) -> RepositoryActionAnswer {
        self.service
            .run_repository_action(&self.tenant, self.run, "drift_report", None)
            .await
            .unwrap_or_else(|e| panic!("{UPDATE}: drift_report was not answered: {e}"))
    }

    /// The landing, run as the interpreter runs it, waiting on the gate.
    fn land(&self) -> tokio::task::JoinHandle<RepositoryActionAnswer> {
        let service = self.service.clone();
        let tenant = self.tenant.clone();
        let run = self.run;
        tokio::spawn(async move {
            service
                .run_repository_action(&tenant, run, "tree_land", Some("carbon-forge: the task"))
                .await
                .unwrap_or_else(|e| panic!("AEGIS ADR-147 C5: tree_land was not answered: {e}"))
        })
    }

    /// The one pending approval of `PERSON`, waited for.
    async fn pending(&self) -> ToolApprovalRequest {
        for _ in 0..100 {
            let pending = self
                .approvals
                .list_for_user(&self.tenant, PERSON, Some(ToolApprovalStatus::Pending))
                .await
                .unwrap();
            if let Some(request) = pending.into_iter().next() {
                return request;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!(
            "AEGIS ADR-147 C5: no landing waited at the gate; Carbon Forge received {:?}",
            self.calls().into_iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
    }

    async fn approve(&self, request: &ToolApprovalRequest) {
        self.approvals
            .decide(
                request.id,
                &self.tenant,
                PERSON,
                ToolApprovalDecision::Once,
                self.service.as_ref(),
            )
            .await
            .unwrap_or_else(|e| panic!("the approval was not taken: {e}"));
    }
}

async fn answered(
    landing: tokio::task::JoinHandle<RepositoryActionAnswer>,
) -> RepositoryActionAnswer {
    tokio::time::timeout(std::time::Duration::from_secs(30), landing)
        .await
        .expect("the landing did not answer after the approval")
        .expect("the landing's task did not panic")
}

/// The approval request's `drift`, after its tree, lane and intent were
/// checked to be the landing's own.
fn drift_at_the_gate(fx: &Fixture, request: &ToolApprovalRequest) -> Value {
    assert_eq!(request.tool_name, TREE_LAND_TOOL, "{UPDATE}: {request:?}");
    let arguments = &request.arguments;
    assert_eq!(arguments["tree"].as_str(), Some(TREE_PATH), "{arguments}");
    assert_eq!(
        arguments["lane"].as_str(),
        Some(lane_of(fx.run).as_str()),
        "{arguments}"
    );
    assert_eq!(
        arguments["intent"].as_str(),
        Some("carbon-forge: the task"),
        "{arguments}"
    );
    arguments.get("drift").cloned().unwrap_or_else(|| {
        panic!("{UPDATE}: the landing's approval request carries no drift: {arguments}")
    })
}

// ===========================================================================
// The action
// ===========================================================================

#[test]
fn drift_report_is_an_action_taking_no_arguments_that_touches_nothing() {
    let action = ActionName::from_literal("drift_report")
        .unwrap_or_else(|e| panic!("{UPDATE}: drift_report is not an action: {e}"));
    assert_eq!(action.as_str(), "drift_report");
    assert!(
        ActionName::ALL.contains(&action),
        "{UPDATE}: drift_report is missing from ActionName::ALL"
    );
    assert!(
        !action.touches_the_world(),
        "{UPDATE}: the drift report advises; it does not wait at the gate"
    );
    assert_eq!(
        action.needs(),
        ActionNeed::RunTreeRef,
        "{UPDATE}: the drift report reads the run's lane by its tree's name, never its files"
    );
    assert_eq!(action.check_args(&HashMap::new()), Ok(()));
    let message = HashMap::from([("message".to_string(), "m".to_string())]);
    assert!(
        action.check_args(&message).is_err(),
        "{UPDATE}: drift_report takes no arguments"
    );
}

#[tokio::test]
async fn drift_report_reads_the_runs_lane_and_tree_and_answers_the_hash_and_the_counts() {
    let fx = fixture().await;
    let answer = fx.drift().await;

    assert_eq!(answer.sentence, None, "{UPDATE}: {answer:?}");
    let drifts = fx.drift_calls();
    assert_eq!(
        drifts.len(),
        1,
        "{UPDATE}: one read of the report: {drifts:?}"
    );
    assert_eq!(
        drifts[0],
        json!({ "lane": lane_of(fx.run), "tree": TREE_PATH, "workspace": WORKSPACE }),
        "{UPDATE}: symbols.drift was not asked of the run's lane and tree"
    );
    assert_eq!(
        answer.commit_sha.as_deref(),
        Some(DRIFT_REPORT_SHA256),
        "{UPDATE}: the report's hash does not ride commit_sha"
    );
    assert_eq!(
        answer.branch, "seam_changed_without_record=1 seam_changed_with_record=0 pending=0",
        "{UPDATE}: the counts by class do not ride branch"
    );
    let report: Value = serde_json::from_str(answer.diff.as_deref().unwrap_or("null")).unwrap();
    assert_eq!(
        report,
        drift_report(fx.run),
        "{UPDATE}: the whole report does not ride diff"
    );
    assert_eq!(answer.git_ref, TREE_PATH);
    assert!(
        !fx.calls().iter().any(|(t, _)| t == "lanes.land"),
        "{UPDATE}: the drift step landed the lane"
    );
    for call in fx.calls.lock().unwrap().iter() {
        assert_eq!(
            call.credential.as_ref().map(|c| c.value.as_str()),
            Some(CHILD_TOKEN),
            "{UPDATE}: {} did not carry the run's execution token",
            call.tool
        );
    }
}

// ===========================================================================
// The template
// ===========================================================================

#[test]
fn carbon_forge_reads_the_drift_after_commit_and_a_draft_run_completes_with_it() {
    WorkflowParser::parse_yaml(CARBON_FORGE)
        .unwrap_or_else(|e| panic!("{UPDATE}: carbon-forge does not parse: {e}"));
    let carbon: serde_yaml::Value = serde_yaml::from_str(CARBON_FORGE).unwrap();
    let states = &carbon["spec"]["states"];
    let routes = |state: &str| -> Vec<(String, String)> {
        states[state]["transitions"]
            .as_sequence()
            .map(|ts| {
                ts.iter()
                    .map(|t| {
                        (
                            format!(
                                "{} {}",
                                t["condition"].as_str().unwrap_or(""),
                                t["expression"].as_str().unwrap_or("")
                            )
                            .trim()
                            .to_string(),
                            t["target"].as_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut complaints = Vec::new();
    if carbon["metadata"]["version"].as_str() != Some("1.2.0") {
        complaints.push(format!(
            "the version is {:?}, not 1.2.0",
            carbon["metadata"]["version"]
        ));
    }
    if states["DRIFT"]["kind"].as_str() != Some("Action")
        || states["DRIFT"]["action"].as_str() != Some("drift_report")
    {
        complaints.push(format!(
            "DRIFT is kind {:?} action {:?}, not the Action drift_report",
            states["DRIFT"]["kind"], states["DRIFT"]["action"]
        ));
    }
    let commit = routes("COMMIT");
    if !commit.contains(&("on_success".to_string(), "DRIFT".to_string()))
        || commit.iter().any(|(_, t)| t == "LAND" || t == "COMPLETE")
    {
        complaints.push(format!(
            "COMMIT does not go on to DRIFT on both paths: {commit:?}"
        ));
    }
    let drift = routes("DRIFT");
    let expected = [
        (
            "custom (default input.draft_only true)".to_string(),
            "COMPLETE".to_string(),
        ),
        ("always".to_string(), "LAND".to_string()),
    ];
    if drift != expected {
        complaints.push(format!(
            "DRIFT's every outcome does not go on, a draft to COMPLETE and else LAND: {drift:?}"
        ));
    }
    for state in ["COMPLETE", "LAND_REFUSED", "CUT_REFUSED"] {
        let kept = states[state]["args"]["drift"].as_str();
        if kept != Some("{{{DRIFT.output.diff}}}{{{DRIFT.output.sentence}}}") {
            complaints.push(format!(
                "{state} does not keep the report, or its sentence, on the blackboard as drift: {kept:?}"
            ));
        }
    }
    assert!(
        complaints.is_empty(),
        "{UPDATE}: carbon-forge is not the template the Update names:\n{}",
        complaints.join("\n")
    );
}

// ===========================================================================
// The gate
// ===========================================================================

#[tokio::test]
async fn the_landings_approval_request_carries_the_drift_beside_tree_lane_and_intent() {
    let fx = fixture().await;
    let landing = fx.land();
    let request = fx.pending().await;
    let drift = drift_at_the_gate(&fx, &request);

    assert_eq!(
        drift,
        json!({
            "report_sha256": DRIFT_REPORT_SHA256,
            "counts": {
                "seam_changed_without_record": 1,
                "seam_changed_with_record": 0,
                "pending": 0
            },
            "findings": [drift_finding("seam_changed_without_record", "deploy")]
        }),
        "{UPDATE}: the gate does not show the report's counts and findings"
    );
    assert!(
        !fx.calls().iter().any(|(t, _)| t == "lanes.land"),
        "AEGIS ADR-147 C5: the lane landed before the person answered"
    );
    fx.approve(&request).await;
    let answer = answered(landing).await;
    assert_eq!(answer.sentence, None, "{UPDATE}: {answer:?}");
    assert_eq!(
        answer.commit_sha.as_deref(),
        Some("c4a9e5e1-0000-4000-8000-000000000003"),
        "{UPDATE}: the advised landing did not land as one changeset"
    );
    let tools: Vec<String> = fx.calls().into_iter().map(|(t, _)| t).collect();
    let read = tools.iter().position(|t| t == "symbols.drift");
    let landed = tools.iter().position(|t| t == "lanes.land");
    assert!(
        matches!((read, landed), (Some(r), Some(l)) if r < l),
        "{UPDATE}: the report was not read before the lane landed: {tools:?}"
    );
}

#[tokio::test]
async fn the_gate_shows_the_first_ten_findings_and_every_count() {
    let findings: Vec<Value> = (0..12)
        .map(|i| {
            let class = if i % 3 == 0 {
                "seam_changed_with_record"
            } else {
                "seam_changed_without_record"
            };
            drift_finding(class, &format!("deploy{i:02}"))
        })
        .collect();
    let first_ten: Vec<Value> = findings.iter().take(10).cloned().collect();
    let fx = fixture_answering(move |run, answers| {
        answers.insert(
            "symbols.drift".to_string(),
            Answer::Result(drift_report_of(run, findings, vec![])),
        );
    })
    .await;
    let landing = fx.land();
    let request = fx.pending().await;
    let drift = drift_at_the_gate(&fx, &request);

    assert_eq!(
        drift["counts"],
        json!({
            "seam_changed_without_record": 8,
            "seam_changed_with_record": 4,
            "pending": 0
        }),
        "{UPDATE}: the counts are not every finding's, by class"
    );
    assert_eq!(
        drift["findings"],
        Value::Array(first_ten),
        "{UPDATE}: the gate does not show the first ten findings"
    );
    fx.approve(&request).await;
    answered(landing).await;
}

// ===========================================================================
// Advice, never a refusal
// ===========================================================================

#[tokio::test]
async fn a_refused_drift_report_is_recorded_and_the_landing_goes_on() {
    let fx = fixture_answering(|_, answers| {
        answers.insert(
            "symbols.drift".to_string(),
            Answer::RemoteError(MALFORMED_SEAMS.to_string()),
        );
    })
    .await;
    let answer = fx.drift().await;
    assert_eq!(
        answer.sentence.as_deref(),
        Some(MALFORMED_SEAMS),
        "{UPDATE}: a refused report does not answer Carbon Forge's own sentence: {answer:?}"
    );
    assert_eq!(answer.commit_sha, None, "a refused report answers a hash");
    assert_eq!(answer.diff, None, "a refused report answers a report");

    let landing = fx.land();
    let request = fx.pending().await;
    assert_eq!(
        drift_at_the_gate(&fx, &request),
        json!({ "sentence": MALFORMED_SEAMS }),
        "{UPDATE}: the gate does not show the refusal's sentence"
    );
    fx.approve(&request).await;
    let landed = answered(landing).await;
    assert_eq!(
        landed.sentence, None,
        "{UPDATE}: a refused report refused the landing: {landed:?}"
    );
    assert_eq!(
        landed.commit_sha.as_deref(),
        Some("c4a9e5e1-0000-4000-8000-000000000003"),
        "{UPDATE}: the landing did not go on after a refused report"
    );
}

#[tokio::test]
async fn a_pending_drift_report_counts_its_pending_files_and_refuses_nothing() {
    let fx = fixture_answering(|run, answers| {
        answers.insert(
            "symbols.drift".to_string(),
            Answer::Result(drift_report_of(
                run,
                vec![],
                vec![json!({ "tree": TREE_PATH, "path": "src/deploy.ts", "status": "pending" })],
            )),
        );
    })
    .await;
    let answer = fx.drift().await;
    assert_eq!(answer.sentence, None, "{UPDATE}: {answer:?}");
    assert_eq!(
        answer.branch, "seam_changed_without_record=0 seam_changed_with_record=0 pending=1",
        "{UPDATE}: a pending file is not counted"
    );
    assert_eq!(answer.commit_sha.as_deref(), Some(DRIFT_REPORT_SHA256));
}

#[tokio::test]
async fn a_drift_report_that_could_not_be_read_answers_its_sentence_never_an_error() {
    // A run holding no execution token: Carbon Forge is never reached, and
    // the step answers why instead of failing the run.
    let fx = fixture_holding(|_, _| {}, false).await;
    let answer = fx.drift().await;
    assert_eq!(
        answer.sentence.as_deref(),
        Some(aegis_orchestrator_core::domain::execution_token::RUN_HOLDS_NO_EXECUTION_TOKEN),
        "{UPDATE}: an unread report does not answer its sentence: {answer:?}"
    );
    assert!(
        fx.drift_calls().is_empty(),
        "a run holding no execution token reached Carbon Forge"
    );
}
