// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # carbon-forge's own steps on a Carbon Forge tree (AEGIS ADR-147)
//!
//! AEGIS ADR-147 (Proposed, page revision 55541) builds `carbon-forge`, a
//! second built-in beside `the-forge` whose run works on a Carbon Forge
//! code tree instead of a git repository. Its DIFF, COMMIT and LAND are the
//! Action states `tree_diff`, `tree_write` and `tree_land` (C3), answered
//! by the interpreter's `run_repository_action`, as the-forge's are (AEGIS
//! ADR-142). `tree_write` writes the run's change to a lane of its own;
//! `tree_land` lands that lane as one changeset after the person's
//! approval, as `aegis.git.land` is gated (ADR-141 F8), and a conflict ends
//! with Carbon Forge's own sentence, which the template carries into
//! LAND_REFUSED (C5). A draft run's write lands nothing (C6).
//!
//! Carbon Forge is reached where every remote MCP tool is: the SEAL
//! gateway's `InvokeTool` (AEGIS ADR-132 H1 to H8), with the run's person's
//! credential for the binding the run's `contexts` chose. The gateway here
//! is a loopback one over TLS that records every call and answers each tool
//! as Carbon Forge at `76d62e5` answers it
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
//! | C5: a conflict, in Carbon Forge's own words | `a_conflicting_landing_answers_nuclear_notes_own_sentence` |
//! | Update `execution-token-2026-10-17` X3: the child, never the binding | `execution_token_carries_every_carbon_forge_call_of_the_run_never_the_binding` |
//! | X3: a run with no child is refused | `execution_token_absent_refuses_every_carbon_forge_call_of_the_run` |
//! | X5: the lane's owner is the run's person | `execution_token_run_lane_is_created_owned_by_the_runs_person` |

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
        capabilities: ["aegis.tree.*", "aegis.archive.*"]
            .into_iter()
            .map(|pattern| Capability {
                tool_pattern: pattern.to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            })
            .collect(),
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
    /// The person's binding and the run's execution token, as the secret
    /// store keeps it.
    binding: Arc<OneBinding>,
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
/// `contexts` chose the person's Carbon Forge binding.
async fn fixture() -> Fixture {
    fixture_answering(|_, _| {}).await
}

/// The fixture, its Carbon Forge answering as `answers` changes it.
async fn fixture_answering(
    answers: impl FnOnce(uuid::Uuid, &mut HashMap<String, Answer>),
) -> Fixture {
    fixture_of(answers, true).await
}

/// The fixture; with `materialized` false, the run's `trees` entry names
/// the tree and no volume, as a start of a workflow whose every state is an
/// Action needing only the tree's name leaves it (AEGIS ADR-149 C2).
async fn fixture_of(
    answers: impl FnOnce(uuid::Uuid, &mut HashMap<String, Answer>),
    materialized: bool,
) -> Fixture {
    fixture_holding(answers, materialized, true).await
}

/// The fixture; with `child` false, the run holds no execution token, as a
/// run whose start minted none (AEGIS ADR-147 Update
/// `execution-token-2026-10-17` X3).
async fn fixture_holding(
    answers: impl FnOnce(uuid::Uuid, &mut HashMap<String, Answer>),
    materialized: bool,
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
    // The volume's path is its storage provider's, as a local_host volume's
    // is: under the provider's root on disk.
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
        binding.clone(),
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
    let mut named = json!({
        "workspace": WORKSPACE,
        "path": TREE_PATH,
        "revision": REVISION,
        "volume_id": volume.id.0.to_string()
    });
    if !materialized {
        named.as_object_mut().unwrap().remove("volume_id");
    }
    let input = json!({
        "task": "the task",
        "trees": [named],
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
        binding,
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
    /// Each call Carbon Forge received: its tool and its arguments.
    fn calls(&self) -> Vec<(String, Value)> {
        calls_of(&self.calls)
    }

    /// Every call went to Carbon Forge, in the run's workspace, with the
    /// run's execution token, minted from the person's binding the run
    /// chose (AEGIS ADR-147 Update `execution-token-2026-10-17` X3).
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
                Some(CHILD_TOKEN),
                "{} did not carry the run's execution token",
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
            "AEGIS ADR-147 C5: no landing waited at the gate; Carbon Forge received {:?}",
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
        .unwrap_or_else(|e| panic!("AEGIS ADR-147 C5: tree_write never reached Carbon Forge: {e}"));
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
            "AEGIS ADR-147 C5: the run's change was not put on its lane {lane}: Carbon Forge received {tools:?}"
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
// AEGIS ADR-147 Update `execution-token-2026-10-17` X3, X5: the run's own
// token
// ===========================================================================

#[tokio::test]
async fn execution_token_carries_every_carbon_forge_call_of_the_run_never_the_binding() {
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
        .unwrap();
    assert_eq!(answer.sentence, None, "{answer:?}");
    let calls = fx.calls.lock().unwrap().clone();
    assert!(!calls.is_empty(), "tree_write reached no Carbon Forge tool");
    for call in &calls {
        let credential = call.credential.as_ref().map(|c| c.value.as_str());
        assert_eq!(
            credential,
            Some(CHILD_TOKEN),
            "X3: the run's call of {} did not carry its execution token",
            call.tool
        );
        assert_ne!(
            credential,
            Some(TOKEN),
            "X3: the run's call of {} carried the binding",
            call.tool
        );
    }
}

#[tokio::test]
async fn execution_token_absent_refuses_every_carbon_forge_call_of_the_run() {
    let fx = fixture_holding(|_, _| {}, true, false).await;
    for (action, message) in [
        ("tree_diff", None),
        ("tree_write", Some("carbon-forge: the task")),
    ] {
        let answer = fx
            .service
            .run_repository_action(&fx.tenant, fx.run, action, message)
            .await
            .unwrap();
        assert_eq!(
            answer.sentence.as_deref(),
            Some(aegis_orchestrator_core::domain::execution_token::RUN_HOLDS_NO_EXECUTION_TOKEN),
            "X3: {action} of a run holding no execution token was not refused: {answer:?}"
        );
    }
    assert!(
        fx.calls().is_empty(),
        "X3: a run holding no execution token reached Carbon Forge: {:?}",
        fx.calls()
    );
}

#[tokio::test]
async fn execution_token_run_lane_is_created_owned_by_the_runs_person() {
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
        .unwrap();
    assert_eq!(answer.sentence, None, "{answer:?}");
    let created: Vec<Value> = fx
        .calls()
        .into_iter()
        .filter(|(t, _)| t == "lanes.create")
        .map(|(_, a)| a)
        .collect();
    assert_eq!(created.len(), 1, "one lane was created: {created:?}");
    assert_eq!(
        created[0]["owner"].as_str(),
        Some(NN_USER_ID),
        "X5, NN ADR-0193 L7: the run's lane is not owned by its person: {}",
        created[0]
    );
    assert!(fx.binding.holds_child(fx.run));
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
    // Carbon Forge's own refusal (`content/domain/lane.ts`
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
        "AEGIS ADR-147 C5: a conflicting landing does not answer Carbon Forge's own sentence: {answer:?}"
    );
    assert_eq!(
        answer.commit_sha, None,
        "a refused landing answers a changeset"
    );
}

// ===========================================================================
// AEGIS ADR-149: a deploy keys to a cut (C1, C2)
// ===========================================================================

/// The push's gated tool (AEGIS ADR-149 C2), as `tool_router.rs` names it.
const ARCHIVE_PUSH_TOOL: &str = "aegis.archive.push";

/// The snapshot Carbon Forge answers a cut with (`catalog-snapshots.ts`
/// 145 to 162 at `4569806`: the snapshot's header and `cut`).
const SNAPSHOT_ID: &str = "5a9c0e11-0000-4000-8000-000000000004";

fn snapshot_answer(cut: &str) -> Answer {
    Answer::Result(json!({
        "snapshot_id": SNAPSHOT_ID,
        "predecessor_id": null,
        "entry_count": 4,
        "cut": cut
    }))
}

/// A run of `action` as the interpreter runs it, waiting on the gate when
/// the action is gated.
fn run_action(
    fx: &Fixture,
    action: &'static str,
    message: Option<&'static str>,
) -> tokio::task::JoinHandle<RepositoryActionAnswer> {
    let service = fx.service.clone();
    let tenant = fx.tenant.clone();
    let run = fx.run;
    tokio::spawn(async move {
        service
            .run_repository_action(&tenant, run, action, message)
            .await
            .unwrap_or_else(|e| panic!("AEGIS ADR-149: {action} was not answered: {e}"))
    })
}

#[test]
fn the_deploy_actions_are_actions_naming_the_tree_and_only_the_push_touches_the_world() {
    for (literal, touches) in [
        ("tree_cut", false),
        ("archive_render", false),
        ("archive_push", true),
    ] {
        let action = ActionName::from_literal(literal)
            .unwrap_or_else(|e| panic!("AEGIS ADR-149 C1, C2: {literal} is not an action: {e}"));
        assert_eq!(action.as_str(), literal);
        assert!(
            ActionName::ALL.contains(&action),
            "AEGIS ADR-149: {literal} is missing from ActionName::ALL"
        );
        assert_eq!(
            action.touches_the_world(),
            touches,
            "AEGIS ADR-149 C1, C2: {literal} touches the world: {}",
            action.touches_the_world()
        );
        assert_eq!(
            format!("{:?}", action.needs()),
            "RunTreeRef",
            "AEGIS ADR-149 C2: {literal} needs the run's tree materialized, not only named"
        );
    }
    let no_args = HashMap::new();
    let cut = HashMap::from([("cut".to_string(), "v1.2".to_string())]);
    let tree_cut = ActionName::from_literal("tree_cut").unwrap();
    assert!(tree_cut.check_args(&no_args).is_ok() && tree_cut.check_args(&cut).is_ok());
    let render = ActionName::from_literal("archive_render").unwrap();
    assert!(
        render.check_args(&no_args).is_err() && render.check_args(&cut).is_ok(),
        "AEGIS ADR-149 C2: archive_render requires its cut"
    );
    let push = ActionName::from_literal("archive_push").unwrap();
    assert!(push.check_args(&no_args).is_ok() && push.check_args(&cut).is_err());
}

#[tokio::test]
async fn tree_cut_names_the_landed_run_with_the_given_cut_or_forge_and_its_execution_id() {
    // Given: the state's `cut` argument, carried as the step's message.
    let fx = fixture_answering(|_, answers| {
        answers.insert("snapshots.cut".to_string(), snapshot_answer("v1.2"));
    })
    .await;
    let answer = answered(run_action(&fx, "tree_cut", Some("v1.2"))).await;
    assert_eq!(answer.sentence, None, "AEGIS ADR-149 C1: {answer:?}");
    let cuts: Vec<Value> = fx
        .calls()
        .into_iter()
        .filter(|(t, _)| t == "snapshots.cut")
        .map(|(_, a)| a)
        .collect();
    assert_eq!(
        cuts.len(),
        1,
        "AEGIS ADR-149 C1: tree_cut did not cut one snapshot: Carbon Forge received {:?}",
        fx.calls().into_iter().map(|(t, _)| t).collect::<Vec<_>>()
    );
    assert_eq!(cuts[0]["name"].as_str(), Some("v1.2"));
    assert_eq!(
        (answer.git_ref.as_str(), answer.commit_sha.as_deref()),
        ("v1.2", Some(SNAPSHOT_ID)),
        "AEGIS ADR-149 C1: the cut rides `ref` and the snapshot `commit_sha`: {answer:?}"
    );
    fx.assert_each_call_is_the_persons_in_the_workspace();

    // Default: forge-<execution id>, a name Carbon Forge's cut pattern
    // admits (snapshot.ts:240 at 4569806: no slash).
    let fx = fixture_answering(|run, answers| {
        answers.insert(
            "snapshots.cut".to_string(),
            snapshot_answer(&format!("forge-{run}")),
        );
    })
    .await;
    let answer = answered(run_action(&fx, "tree_cut", None)).await;
    let default = format!("forge-{}", fx.run);
    let named = fx
        .calls()
        .into_iter()
        .find(|(t, _)| t == "snapshots.cut")
        .map(|(_, a)| a["name"].clone());
    assert_eq!(
        named,
        Some(json!(default)),
        "AEGIS ADR-149 C1: a cut with no name is not forge-<execution id>"
    );
    assert_eq!(default.len(), 42);
    assert!(default
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')));
    assert_eq!(answer.git_ref, default, "{answer:?}");
}

#[tokio::test]
async fn archive_render_renders_the_cut_of_a_tree_named_and_not_materialized() {
    let fx = fixture_of(
        |_, answers| {
            answers.insert(
                "archives.render".to_string(),
                Answer::Result(json!({
                    "tree_id": "8a1f3b2c-0000-4000-8000-000000000001",
                    "tree_revision_id": 4243,
                    "commit_sha1": "0123456789abcdef0123456789abcdef01234567",
                    "tree_sha1": "89abcdef0123456789abcdef0123456789abcdef",
                    "parent_sha1": null,
                    "cut": "v1.2",
                    "snapshot_id": SNAPSHOT_ID,
                    "ref": { "name": "refs/heads/main", "previous_sha1": null, "moved": true },
                    "tag": { "name": "refs/tags/nn/v1.2", "sha1": "fedcba9876543210fedcba9876543210fedcba98" },
                    "held": { "commit": false, "blob_ids": 1, "blobs": 1 }
                })),
            );
        },
        false,
    )
    .await;
    let answer = answered(run_action(&fx, "archive_render", Some("v1.2"))).await;
    assert_eq!(answer.sentence, None, "AEGIS ADR-149 C2: {answer:?}");
    let calls = fx.calls();
    let tools: Vec<&str> = calls.iter().map(|(t, _)| t.as_str()).collect();
    let renders: Vec<&Value> = calls
        .iter()
        .filter(|(t, _)| t == "archives.render")
        .map(|(_, a)| a)
        .collect();
    assert_eq!(
        renders.len(),
        1,
        "AEGIS ADR-149 C2: archive_render did not render once: Carbon Forge received {tools:?}"
    );
    assert_eq!(renders[0]["tree"].as_str(), Some(TREE_PATH));
    assert_eq!(renders[0]["cut"].as_str(), Some("v1.2"));
    assert!(
        !tools
            .iter()
            .any(|t| *t == "trees.materialize" || t.starts_with("blobs.")),
        "AEGIS ADR-149 C2: the render read the tree's files: {tools:?}"
    );
    assert_eq!(
        answer.commit_sha.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567"),
        "AEGIS ADR-149 C2: the render does not answer its commit_sha1: {answer:?}"
    );
    assert_eq!(answer.git_ref, "refs/tags/nn/v1.2", "the tag rides `ref`");
    assert_eq!(answer.branch, SNAPSHOT_ID, "the snapshot rides `branch`");
    let rendered: Value =
        serde_json::from_str(answer.diff.as_deref().unwrap_or("null")).unwrap_or_default();
    assert_eq!(
        rendered["tree_revision_id"],
        json!(4243),
        "the render's own answer rides `diff`: {answer:?}"
    );
    fx.assert_each_call_is_the_persons_in_the_workspace();
}

#[tokio::test]
async fn the_archive_push_is_gated_and_listed_to_no_agent() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    assert!(
        router.requires_approval(ARCHIVE_PUSH_TOOL),
        "AEGIS ADR-149 C2: {ARCHIVE_PUSH_TOOL} does not wait at the approval gate"
    );
    let listed = router.list_tools().await.unwrap();
    assert!(
        !listed.iter().any(|t| t.name == ARCHIVE_PUSH_TOOL),
        "AEGIS ADR-149 C2: {ARCHIVE_PUSH_TOOL} is listed to agents"
    );
}

#[tokio::test]
async fn archive_push_queues_the_push_only_after_the_persons_approval() {
    let fx = fixture_of(
        |_, answers| {
            answers.insert(
                "archives.push".to_string(),
                Answer::Result(json!({
                    "tree_id": "8a1f3b2c-0000-4000-8000-000000000001",
                    "path": TREE_PATH,
                    "remote": "https://github.com/100monkeys-ai/aegis-platform-deployment.git",
                    "job_id": "7b0b0b0b-0000-4000-8000-000000000005",
                    "queued": true
                })),
            );
        },
        false,
    )
    .await;
    let pushing = run_action(&fx, "archive_push", None);
    let request = fx.pending().await;
    assert_eq!(
        request.tool_name, ARCHIVE_PUSH_TOOL,
        "AEGIS ADR-149 C2: the push waits at the gate as {}",
        request.tool_name
    );
    assert!(
        !fx.calls().iter().any(|(t, _)| t == "archives.push"),
        "AEGIS ADR-149 C2: the archive was pushed before the person answered"
    );
    let decided = fx
        .approvals
        .decide(
            request.id,
            &fx.tenant,
            PERSON,
            ToolApprovalDecision::Once,
            fx.service.as_ref(),
        )
        .await
        .unwrap_or_else(|e| panic!("the approval was not taken: {e}"));
    assert_eq!(decided.error, None, "the approved push was not run");
    let answer = answered(pushing).await;
    assert_eq!(answer.sentence, None, "AEGIS ADR-149 C2: {answer:?}");
    let pushes: Vec<Value> = fx
        .calls()
        .into_iter()
        .filter(|(t, _)| t == "archives.push")
        .map(|(_, a)| a)
        .collect();
    assert_eq!(pushes.len(), 1, "AEGIS ADR-149 C2: one push");
    assert_eq!(pushes[0]["tree"].as_str(), Some(TREE_PATH));
    assert_eq!(
        answer.commit_sha.as_deref(),
        Some("7b0b0b0b-0000-4000-8000-000000000005"),
        "AEGIS ADR-149 C2: the push does not answer its job id: {answer:?}"
    );
    assert_eq!(
        answer.git_ref, "https://github.com/100monkeys-ai/aegis-platform-deployment.git",
        "the remote rides `ref`"
    );
    assert_eq!(answer.branch, "queued", "whether it queued rides `branch`");
    fx.assert_each_call_is_the_persons_in_the_workspace();
}

#[tokio::test]
async fn a_tree_named_and_not_materialized_has_no_files_to_diff_or_write() {
    let fx = fixture_of(|_, _| {}, false).await;
    for action in ["tree_diff", "tree_write"] {
        let answer = answered(run_action(&fx, action, Some("carbon-forge: the task"))).await;
        assert_eq!(
            answer.sentence.as_deref(),
            Some("this run holds no tree"),
            "AEGIS ADR-149 C2: {action} of a run holding no volume: {answer:?}"
        );
    }
    let answer = fx
        .service
        .run_repository_action(&fx.tenant, fx.run, "tree_unknown", None)
        .await;
    assert!(
        matches!(&answer, Ok(a) if a.sentence.is_some()) || answer.is_err(),
        "an unknown tree action landed: {answer:?}"
    );
    assert!(
        !fx.calls().iter().any(|(t, _)| t == "lanes.land"),
        "AEGIS ADR-149 C1: an unknown tree action reached the landing"
    );
}

// ===========================================================================
// AEGIS ADR-149 C2: the deploy run on the remote is waited for
// ===========================================================================

/// The refusal of a JSON `message` the wait cannot read (AEGIS ADR-149
/// Update `deploy-wait-arguments-2026-10-17`).
const DEPLOY_WAIT_ARGUMENTS: &str = "archive_deploy_wait takes a cut name, or a JSON object {\"cut\": ..., \"timeout_seconds\": ..., \"no_run_grace_seconds\": ...}; nothing was waited for";

/// One GitHub Actions run of the pushed tag's commit, as
/// `archives.deploy_status` answers it.
fn deploy_run(id: u64, name: &str, status: &str, conclusion: Option<&str>) -> Value {
    json!({
        "id": id,
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "html_url": format!("https://github.com/100monkeys-ai/aegis-platform-deployment/actions/runs/{id}")
    })
}

/// A fixture whose `archives.deploy_status` answers `runs`.
async fn deploy_status_answering(runs: Value) -> Fixture {
    fixture_of(
        move |_, answers| {
            answers.insert(
                "archives.deploy_status".to_string(),
                Answer::Result(json!({
                    "tree": TREE_PATH,
                    "cut": "v1.2",
                    "commit_sha1": "0123456789abcdef0123456789abcdef01234567",
                    "runs": runs
                })),
            );
        },
        false,
    )
    .await
}

fn deploy_status_calls(fx: &Fixture) -> Vec<Value> {
    fx.calls()
        .into_iter()
        .filter(|(t, _)| t == "archives.deploy_status")
        .map(|(_, a)| a)
        .collect()
}

#[test]
fn archive_deploy_wait_is_an_action_naming_the_tree_that_touches_nothing() {
    let wait = ActionName::from_literal("archive_deploy_wait")
        .unwrap_or_else(|e| panic!("AEGIS ADR-149 C2: archive_deploy_wait is not an action: {e}"));
    assert_eq!(wait.as_str(), "archive_deploy_wait");
    assert!(ActionName::ALL.contains(&wait));
    assert!(
        !wait.touches_the_world(),
        "AEGIS ADR-149 C2: the deploy wait reads; it waits at no gate"
    );
    assert_eq!(
        format!("{:?}", wait.needs()),
        "RunTreeRef",
        "AEGIS ADR-149 C2: archive_deploy_wait needs only the tree's name"
    );
    let no_args = HashMap::new();
    let cut = HashMap::from([("cut".to_string(), "v1.2".to_string())]);
    let all = HashMap::from([
        ("cut".to_string(), "v1.2".to_string()),
        ("timeout_seconds".to_string(), "600".to_string()),
        ("no_run_grace_seconds".to_string(), "60".to_string()),
    ]);
    assert!(
        wait.check_args(&no_args).is_err()
            && wait.check_args(&cut).is_ok()
            && wait.check_args(&all).is_ok(),
        "AEGIS ADR-149 C2: archive_deploy_wait requires its cut and may name its two waits"
    );
}

#[tokio::test]
async fn archive_deploy_wait_answers_success_when_every_run_of_the_commit_concluded_success() {
    let fx = deploy_status_answering(json!([
        deploy_run(9001, "deploy", "completed", Some("success")),
        deploy_run(9002, "smoke", "completed", Some("success")),
    ]))
    .await;
    let answer = answered(run_action(&fx, "archive_deploy_wait", Some("v1.2"))).await;
    assert_eq!(
        answer.sentence, None,
        "AEGIS ADR-149 C2: a deploy whose every run succeeded was refused: {answer:?}"
    );
    let calls = deploy_status_calls(&fx);
    assert_eq!(
        calls.len(),
        1,
        "AEGIS ADR-149 C2: the wait did not read archives.deploy_status once: {:?}",
        fx.calls().into_iter().map(|(t, _)| t).collect::<Vec<_>>()
    );
    assert_eq!(calls[0]["tree"].as_str(), Some(TREE_PATH));
    assert_eq!(calls[0]["cut"].as_str(), Some("v1.2"));
    assert_eq!(
        answer.commit_sha.as_deref(),
        Some("9001"),
        "the run id rides `commit_sha`: {answer:?}"
    );
    assert_eq!(answer.branch, "success", "the conclusion rides `branch`");
    assert_eq!(
        answer.git_ref,
        "https://github.com/100monkeys-ai/aegis-platform-deployment/actions/runs/9001",
        "the run's html_url rides `ref`"
    );
    let whole: Value =
        serde_json::from_str(answer.diff.as_deref().unwrap_or("null")).unwrap_or_default();
    assert_eq!(
        whole["runs"].as_array().map(Vec::len),
        Some(2),
        "the whole answer rides `diff`: {answer:?}"
    );
    fx.assert_each_call_is_the_persons_in_the_workspace();
}

#[tokio::test]
async fn archive_deploy_wait_refuses_naming_the_first_failed_runs_name_and_html_url() {
    let fx = deploy_status_answering(json!([
        deploy_run(9001, "build", "completed", Some("success")),
        deploy_run(9002, "deploy", "completed", Some("failure")),
        deploy_run(9003, "smoke", "completed", Some("cancelled")),
    ]))
    .await;
    let answer = answered(run_action(&fx, "archive_deploy_wait", Some("v1.2"))).await;
    let sentence = answer.sentence.clone().unwrap_or_default();
    assert!(
        sentence.contains("deploy")
            && sentence.contains(
                "https://github.com/100monkeys-ai/aegis-platform-deployment/actions/runs/9002"
            ),
        "AEGIS ADR-149 C2: a failed deploy run was not refused by its name and html_url: {answer:?}"
    );
    assert_eq!(answer.commit_sha.as_deref(), Some("9002"), "{answer:?}");
    assert_eq!(answer.branch, "failure", "{answer:?}");
}

#[tokio::test]
async fn archive_deploy_wait_with_no_run_after_its_grace_answers_no_run() {
    let fx = deploy_status_answering(json!("none")).await;
    let answer = answered(run_action(
        &fx,
        "archive_deploy_wait",
        Some(r#"{"cut": "v1.2", "no_run_grace_seconds": 0}"#),
    ))
    .await;
    assert_eq!(
        (answer.sentence.as_deref(), answer.branch.as_str()),
        (None, "no_run"),
        "AEGIS ADR-149 C2: a remote with no run after the grace is not answered no_run: {answer:?}"
    );
    assert_eq!(deploy_status_calls(&fx)[0]["cut"].as_str(), Some("v1.2"));
}

#[tokio::test]
async fn archive_deploy_wait_refuses_a_run_not_concluded_by_its_timeout() {
    let fx =
        deploy_status_answering(json!([deploy_run(9001, "deploy", "in_progress", None)])).await;
    let answer = answered(run_action(
        &fx,
        "archive_deploy_wait",
        Some(r#"{"cut": "v1.2", "timeout_seconds": 0}"#),
    ))
    .await;
    let sentence = answer.sentence.clone().unwrap_or_default();
    assert!(
        sentence.contains("deploy")
            && sentence.contains(
                "https://github.com/100monkeys-ai/aegis-platform-deployment/actions/runs/9001"
            ),
        "AEGIS ADR-149 C2: a run unconcluded at the timeout was not refused by its name and html_url: {answer:?}"
    );
}

#[tokio::test]
async fn archive_deploy_wait_refuses_a_json_message_it_cannot_read_and_waits_for_nothing() {
    let fx = deploy_status_answering(json!([])).await;
    for message in [r#"{"timeout_seconds": 5}"#, r#"{"cut": "v1.2""#] {
        let answer = answered(run_action(&fx, "archive_deploy_wait", Some(message))).await;
        assert_eq!(
            answer.sentence.as_deref(),
            Some(DEPLOY_WAIT_ARGUMENTS),
            "AEGIS ADR-149 deploy-wait-arguments-2026-10-17: the message {message} was not refused: {answer:?}"
        );
    }
    assert!(
        deploy_status_calls(&fx).is_empty(),
        "AEGIS ADR-149: a message without its cut was waited for"
    );
}
