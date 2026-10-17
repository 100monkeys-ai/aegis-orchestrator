// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # carbon-forge's own steps on a Nuclear Notes tree (AEGIS ADR-147)
//!
//! AEGIS ADR-147 (Proposed, page revision 55541) builds `carbon-forge`, a
//! second built-in beside `the-forge` whose run works on a Nuclear Notes
//! code tree instead of a git repository. Its DIFF, COMMIT and LAND are the
//! Action states `tree_diff`, `tree_write` and `tree_land` (C3), answered
//! by the interpreter's `run_repository_action`, as the-forge's are (AEGIS
//! ADR-142). `tree_write` writes the run's change to a lane of its own;
//! `tree_land` lands that lane as one changeset after the person's
//! approval, as `aegis.git.land` is gated (ADR-141 F8), and a conflict ends
//! with Nuclear Notes' own sentence, which the template carries into
//! LAND_REFUSED (C5). A draft run's write lands nothing (C6).
//!
//! Nuclear Notes is reached where every remote MCP tool is: the SEAL
//! gateway's `InvokeTool` (AEGIS ADR-132 H1 to H8), with the run's person's
//! credential for the binding the run's `contexts` chose. The gateway here
//! is a loopback one over TLS that records every call and answers each tool
//! as Nuclear Notes at `76d62e5` answers it
//! (`apps/web/src/contexts/mcp/application/catalog-substrate.ts`,
//! `catalog.ts` `lanes.*`, `content/domain/lane.ts` `laneConflictRefusal`).
//!
//! The tree write is `trees.put` with `lane` (Zaru ADR-0068 D5 item 9a),
//! never `changesets.apply`, which takes only page and atom targets.
//!
//! | Clause | Test |
//! |---|---|
//! | C3: the tree actions are in the closed set | `the_tree_actions_are_actions_and_only_the_landing_touches_the_world` |
//! | C5: the landing is gated and listed to no agent | `the_tree_landing_is_gated_and_listed_to_no_agent` |
//! | C5, C6: the write goes to a lane of the run's own | `tree_write_puts_the_runs_change_on_a_lane_of_its_own_and_lands_nothing` |
//! | C5: the landing after approval, one changeset | `tree_land_lands_the_lane_as_one_changeset_after_the_persons_approval` |
//! | C5: a conflict, in Nuclear Notes' own words | `a_conflicting_landing_answers_nuclear_notes_own_sentence` |

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
use aegis_orchestrator_core::domain::workflow::{ActionName, WorkflowExecution};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryVolumeRepository, InMemoryWorkflowExecutionRepository,
};
use aegis_orchestrator_core::infrastructure::seal_gateway_proto::InvokeToolRequest;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

use carbon_forge_support::*;

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

// ===========================================================================
// The fixture
// ===========================================================================

struct Fixture {
    service: Arc<ToolInvocationService>,
    approvals: Arc<ToolApprovalService>,
    calls: Arc<Mutex<Vec<InvokeToolRequest>>>,
    tenant: TenantId,
    run: uuid::Uuid,
    /// The run's tree volume, as its materialization left it and its
    /// agents changed it.
    tree: PathBuf,
    _shutdown: oneshot::Sender<()>,
    _dirs: tempfile::TempDir,
}

/// A carbon-forge run of `PERSON`, its tree materialized from `REVISION`
/// into its volume `tree` and its README changed by its agents, as a start
/// leaves it: the run's `trees` entry names the workspace, path and
/// revision (C2) and the volume it was materialized into (C4); its
/// `contexts` chose the person's Nuclear Notes binding.
async fn fixture() -> Fixture {
    fixture_answering(|_, _| {}).await
}

/// The fixture, its Nuclear Notes answering as `answers` changes it.
async fn fixture_answering(
    answers: impl FnOnce(uuid::Uuid, &mut HashMap<String, Answer>),
) -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let run = ExecutionId::new();
    let mut stub = nuclear_notes(run.0);
    answers(run.0, &mut stub.answers);
    let calls = stub.calls.clone();
    let (url, ca, shutdown) = serve_tls(stub, dirs.path()).await;

    let tenant = TenantId::consumer();
    let event_bus = Arc::new(EventBus::new(256));
    let volume_repo = Arc::new(InMemoryVolumeRepository::new());
    let tree = dirs.path().join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("README.md"), README_AFTER).unwrap();
    let volume = Volume::new(
        "tree".to_string(),
        tenant.clone(),
        StorageClass::persistent(),
        VolumeBackend::HostPath { path: tree.clone() },
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
    let service = tool_service(
        url,
        &ca,
        fsal,
        contexts,
        executions.clone(),
        approvals.clone(),
    );

    let workflow = WorkflowParser::parse_yaml(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: carbon-forge-like
spec:
  initial_state: A
  states:
    A:
      kind: System
      command: "true"
      transitions: []
"#,
    )
    .unwrap();
    let input = json!({
        "task": "the task",
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
        tree,
        _shutdown: shutdown,
        _dirs: dirs,
    }
}

impl Fixture {
    /// Each call Nuclear Notes received: its tool and its arguments.
    fn calls(&self) -> Vec<(String, Value)> {
        calls_of(&self.calls)
    }

    /// Every call went to Nuclear Notes, in the run's workspace, with the
    /// person's credential for the binding the run chose.
    fn assert_each_call_is_the_persons_in_the_workspace(&self) {
        for call in self.calls.lock().unwrap().iter() {
            assert_eq!(call.server, SERVER, "{} went to {}", call.tool, call.server);
            let args: Value = serde_json::from_str(&call.arguments_json).unwrap();
            assert_eq!(
                args["workspace"].as_str(),
                Some(WORKSPACE),
                "{} did not name the run's workspace: {args}",
                call.tool
            );
            assert_eq!(
                call.credential.as_ref().map(|c| c.value.as_str()),
                Some(TOKEN),
                "{} did not carry the chosen binding's credential",
                call.tool
            );
        }
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
            "AEGIS ADR-147 C5: no landing waited at the gate; Nuclear Notes received {:?}",
            self.calls().into_iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
    }

    /// `tree_land`, run as the interpreter runs it, waiting on the gate.
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

    async fn approve(&self) -> ToolApprovalRequest {
        let request = self.pending().await;
        assert_eq!(
            request.tool_name, TREE_LAND_TOOL,
            "AEGIS ADR-147 C5: the landing waits at the gate as {}",
            request.tool_name
        );
        assert!(
            !self.calls().iter().any(|(t, _)| t == "lanes.land"),
            "AEGIS ADR-147 C5: the lane landed before the person answered"
        );
        self.approvals
            .decide(
                request.id,
                &self.tenant,
                PERSON,
                ToolApprovalDecision::Once,
                self.service.as_ref(),
            )
            .await
            .unwrap_or_else(|e| panic!("the approval was not taken: {e}"))
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

// ===========================================================================
// C3: the closed set of actions
// ===========================================================================

#[test]
fn the_tree_actions_are_actions_and_only_the_landing_touches_the_world() {
    for (literal, touches) in [
        ("tree_diff", false),
        ("tree_write", false),
        ("tree_land", true),
    ] {
        let action = ActionName::from_literal(literal)
            .unwrap_or_else(|e| panic!("AEGIS ADR-147 C3: {literal} is not an action: {e}"));
        assert_eq!(action.as_str(), literal);
        assert!(
            ActionName::ALL.contains(&action),
            "AEGIS ADR-147 C3: {literal} is missing from ActionName::ALL"
        );
        assert_eq!(
            action.touches_the_world(),
            touches,
            "AEGIS ADR-147 C3, C5: {literal} touches the world: {}",
            action.touches_the_world()
        );
    }
}

// ===========================================================================
// C5: the landing's catalogue mark
// ===========================================================================

#[tokio::test]
async fn the_tree_landing_is_gated_and_listed_to_no_agent() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    assert!(
        router.requires_approval(TREE_LAND_TOOL),
        "AEGIS ADR-147 C5: {TREE_LAND_TOOL} does not wait at the approval gate"
    );
    let listed = router.list_tools().await.unwrap();
    assert!(
        !listed.iter().any(|t| t.name == TREE_LAND_TOOL),
        "AEGIS ADR-147 C5: {TREE_LAND_TOOL} is listed to agents"
    );
}

// ===========================================================================
// C5, C6: the write
// ===========================================================================

#[tokio::test]
async fn tree_write_puts_the_runs_change_on_a_lane_of_its_own_and_lands_nothing() {
    let fx = fixture().await;
    let answer = fx
        .service
        .run_repository_action(
            &fx.tenant,
            fx.run,
            "tree_write",
            Some("carbon-forge: the task"),
        )
        .await
        .unwrap_or_else(|e| {
            panic!("AEGIS ADR-147 C5: tree_write never reached Nuclear Notes: {e}")
        });
    assert_eq!(answer.sentence, None, "AEGIS ADR-147 C5: {answer:?}");

    let calls = fx.calls();
    let tools: Vec<&str> = calls.iter().map(|(t, _)| t.as_str()).collect();
    let lane = lane_of(fx.run);
    let created = calls
        .iter()
        .position(|(t, a)| t == "lanes.create" && a["label"].as_str() == Some(lane.as_str()));
    let put = calls.iter().position(|(t, a)| {
        t == "trees.put"
            && a["lane"].as_str() == Some(lane.as_str())
            && a["path"].as_str() == Some(TREE_PATH)
    });
    let (Some(created), Some(put)) = (created, put) else {
        panic!(
            "AEGIS ADR-147 C5: the run's change was not put on its lane {lane}: Nuclear Notes received {tools:?}"
        );
    };
    assert!(
        created < put,
        "the lane was created after the write: {tools:?}"
    );
    let new_readme = sha256_hex(README_AFTER.as_bytes());
    let uploaded = calls
        .iter()
        .position(|(t, a)| t == "blobs.put" && a["sha256"].as_str() == Some(new_readme.as_str()));
    assert!(
        uploaded.is_some_and(|u| u < put),
        "AEGIS ADR-147 C5: the changed README's bytes were not stored before the tree: {tools:?}"
    );
    let entries = calls[put].1["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        entries,
        vec![json!({ "path": "README.md", "mode": "100644", "sha256": new_readme })],
        "AEGIS ADR-147 C5: the lane's tree is not the run's volume"
    );
    assert_eq!(
        calls[put].1["intent"].as_str(),
        Some("carbon-forge: the task"),
        "the write's intent is the step's message"
    );
    assert!(
        !tools.contains(&"lanes.land") && !tools.contains(&"changesets.apply"),
        "AEGIS ADR-147 C6: a write landed or wrote the trunk: {tools:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|(t, a)| t == "trees.put" && a.get("lane").is_none()),
        "AEGIS ADR-147 C5: the trunk's tree was written: {tools:?}"
    );
    assert_eq!(
        answer.branch, lane,
        "AEGIS ADR-147 C5: the write does not answer its lane"
    );
    assert_eq!(
        std::fs::read_to_string(fx.tree.join("README.md")).unwrap(),
        README_AFTER,
        "the write changed the run's volume"
    );
    fx.assert_each_call_is_the_persons_in_the_workspace();
}

// ===========================================================================
// C5: the landing
// ===========================================================================

#[tokio::test]
async fn tree_land_lands_the_lane_as_one_changeset_after_the_persons_approval() {
    let fx = fixture().await;
    let landing = fx.land();
    let decided = fx.approve().await;
    assert_eq!(decided.error, None, "the approved landing was not run");
    let answer = answered(landing).await;

    assert_eq!(answer.sentence, None, "AEGIS ADR-147 C5: {answer:?}");
    let lands: Vec<Value> = fx
        .calls()
        .into_iter()
        .filter(|(t, _)| t == "lanes.land")
        .map(|(_, a)| a)
        .collect();
    assert_eq!(
        lands.len(),
        1,
        "AEGIS ADR-147 C5: one landing, as one changeset"
    );
    assert_eq!(lands[0]["label"].as_str(), Some(lane_of(fx.run).as_str()));
    assert_eq!(lands[0]["intent"].as_str(), Some("carbon-forge: the task"));
    assert_ne!(
        lands[0]["dry_run"],
        json!(true),
        "the landing was only predicted"
    );
    assert_eq!(
        answer.commit_sha.as_deref(),
        Some("c4a9e5e1-0000-4000-8000-000000000003"),
        "AEGIS ADR-147 C5: the landing does not answer its changeset"
    );
    fx.assert_each_call_is_the_persons_in_the_workspace();
}

#[tokio::test]
async fn a_conflicting_landing_answers_nuclear_notes_own_sentence() {
    // Nuclear Notes' own refusal (`content/domain/lane.ts`
    // `laneConflictRefusal` at `76d62e5`), relayed as the gateway relays a
    // remote tool's error.
    let sentence = |run: uuid::Uuid| {
        format!(
            "Lane {} has 1 conflict with the trunk; the first: tree {TREE_PATH} patch 1 entry \"README.md\": trunk_changed_entry. Nothing was written.",
            lane_of(run)
        )
    };
    let fx = fixture_answering(|run, answers| {
        answers.insert("lanes.land".to_string(), Answer::RemoteError(sentence(run)));
    })
    .await;
    let landing = fx.land();
    fx.approve().await;
    let answer = answered(landing).await;

    assert_eq!(
        answer.sentence.as_deref(),
        Some(sentence(fx.run).as_str()),
        "AEGIS ADR-147 C5: a conflicting landing does not answer Nuclear Notes' own sentence: {answer:?}"
    );
    assert_eq!(
        answer.commit_sha, None,
        "a refused landing answers a changeset"
    );
}
