// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A run's git tools on the tool path (AEGIS ADR-136 G7, G7a to G7c, G8,
//! G8a, G13a, G13b): the four tools listed and offered, the label resolved
//! against the run's repositories, the run's own commit, status and push on
//! the binding it holds, the answers, the rows published, and no part of the
//! credential in any of them.
//!
//! The run's repository is a binding of a SeaweedFS-shaped volume, cloned
//! and worked by the git step's own script run by the host's shell
//! (`HostShellRunner`), from a git server that demands a password
//! (`GitTestServer`); the password sits in the binding's URL, where the
//! clone path takes it as the credential.
//!
//! | Scenario | Test |
//! |---|---|
//! | The tools and their schemas | `the_four_git_tools_are_listed_with_schemas_declaring_repository_and_binding_id` |
//! | Offered to an agent naming them | `an_agent_naming_the_git_tools_with_an_admitting_context_is_offered_them` |
//! | A label the run did not mount | `a_label_the_run_did_not_mount_is_refused_and_no_binding_id_is_read` |
//! | Commit, and the clean tree | `the_run_commits_on_the_binding_it_holds_and_a_clean_tree_answers_its_sentence` |
//! | Status | `status_answers_the_work_branch_clean_or_changed_and_its_head` |
//! | Push, and the credential | `the_run_pushes_only_its_work_branch_and_no_part_of_the_token_is_anywhere` |
//! | A non-fast-forward | `a_push_the_remote_refuses_as_not_a_fast_forward_answers_its_sentence_and_pushes_nothing` |
//! | The person as author (G5d) | `a_commit_inside_a_run_started_by_a_person_with_a_name_and_email_is_authored_as_them` |
//! | No person's name, as before (G5d) | `a_run_started_with_no_identity_commits_as_before` |

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock as StdRwLock};

use async_trait::async_trait;
use futures::Stream;
use serde_json::{json, Value};
use std::pin::Pin;

use crate::application::agent::AgentLifecycleService;
use crate::application::correlated_activity_stream::normalize_domain_event;
use crate::application::execution::ExecutionService;
use crate::application::git_clone_executor::{
    EphemeralCliEngine, EphemeralCliPaths, GitCloneExecutor,
};
use crate::application::git_repo_service::{GitRepoService, PersonProfiles, RunRepositories};
use crate::application::git_test_server::{holds_any_part_of, GitTestServer, HostShellRunner};
use crate::application::nfs_gateway::NfsVolumeRegistry;
use crate::application::tool_invocation_service::{ToolInvocationResult, ToolInvocationService};
use crate::application::user_volume_service::UserVolumeService;
use crate::application::volume_manager::VolumeService;
use crate::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus, VolumeSpec};
use crate::domain::events::{ExecutionEvent, StorageEvent};
use crate::domain::execution::{Execution, ExecutionId, ExecutionInput, Iteration};
use crate::domain::fsal::{AegisFSAL, EventPublisher};
use crate::domain::git_repo::{
    default_work_branch, CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId,
    GitRepoBindingRepository, RunRepository,
};
use crate::domain::repository::{AgentVersion, RepositoryError, VolumeRepository};
use crate::domain::runtime::{ContainerStepRunner, InstanceId};
use crate::domain::seal_session::{CallerAnswer, SealSessionError};
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::security_context::SecurityContext;
use crate::domain::shared_kernel::{TenantId, VolumeId};
use crate::domain::volume::{
    AccessMode, FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership,
};
use crate::infrastructure::event_bus::{DomainEvent, EventBus, EventReceiver};
use crate::infrastructure::repositories::InMemoryVolumeRepository;
use crate::infrastructure::seal::middleware::SealMiddleware;
use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use crate::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use crate::infrastructure::security_context::InMemorySecurityContextRepository;
use crate::infrastructure::storage::LocalHostStorageProvider;
use crate::infrastructure::tool_router::ToolRouter;

const CONTEXT: &str = "git-tools-test-context";
const USER: &str = "git-tools-user";
const GIT_USER: &str = "x-access-token";
const LABEL: &str = "app";
const NOT_ONE: &str = "repository 'other' is not one of this run's repositories";
const CLEAN: &str = "nothing to commit: working tree is clean";

/// A password no other value in the test resembles.
fn token() -> String {
    format!("Zt9{}", uuid::Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------
// The catalogue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_four_git_tools_are_listed_with_schemas_declaring_repository_and_binding_id() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let tools = router.list_tools().await.unwrap();
    let mut wrong = Vec::new();
    for (name, extra, required) in [
        ("aegis.git.status", None, Vec::<&str>::new()),
        ("aegis.git.diff", Some("staged"), vec![]),
        ("aegis.git.commit", Some("message"), vec!["message"]),
        ("aegis.git.push", None, vec![]),
    ] {
        let Some(tool) = tools.iter().find(|t| t.name == name) else {
            wrong.push(format!("{name} is not listed"));
            continue;
        };
        let properties = &tool.input_schema["properties"];
        for key in ["repository", "binding_id"] {
            if !properties[key].is_object() {
                wrong.push(format!("{name} does not declare `{key}`"));
            }
        }
        if let Some(extra) = extra {
            if !properties[extra].is_object() {
                wrong.push(format!("{name} does not declare `{extra}`"));
            }
        }
        for forbidden in ["ref", "remote"] {
            if properties.get(forbidden).is_some() {
                wrong.push(format!("{name} declares `{forbidden}`"));
            }
        }
        let declared: Vec<&str> = tool.input_schema["required"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if declared != required {
            wrong.push(format!("{name} requires {declared:?}, not {required:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("; "));
}

#[tokio::test]
async fn an_agent_naming_the_git_tools_with_an_admitting_context_is_offered_them() {
    let harness = Harness::new(None).await;
    let offered = harness
        .service
        .get_available_tools_for_agent_run(
            &harness.tenant,
            harness.agent_id,
            harness.execution,
            CONTEXT,
        )
        .await
        .expect("the agent's tools are listed");
    let names: Vec<&str> = offered.iter().map(|t| t.name.as_str()).collect();
    let missing: Vec<&str> = [
        "aegis.git.status",
        "aegis.git.diff",
        "aegis.git.commit",
        "aegis.git.push",
    ]
    .into_iter()
    .filter(|name| !names.contains(name))
    .collect();
    assert!(
        missing.is_empty(),
        "the agent was not offered {missing:?}; it was offered {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Inside a run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_label_the_run_did_not_mount_is_refused_and_no_binding_id_is_read() {
    let harness = Harness::new(None).await;
    let mut wrong = Vec::new();
    for tool in [
        "aegis.git.status",
        "aegis.git.diff",
        "aegis.git.commit",
        "aegis.git.push",
    ] {
        let other = harness
            .call(
                tool,
                json!({"repository": "other", "message": "m", "binding_id": harness.binding.0.to_string()}),
            )
            .await;
        if told(&other) != NOT_ONE {
            wrong.push(format!("{tool} with label `other`: {}", told(&other)));
        }
        let by_id = harness
            .call(
                tool,
                json!({"binding_id": harness.binding.0.to_string(), "message": "m"}),
            )
            .await;
        let empty = "repository '' is not one of this run's repositories";
        if told(&by_id) != empty {
            wrong.push(format!("{tool} by binding_id alone: {}", told(&by_id)));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[tokio::test]
async fn the_run_commits_on_the_binding_it_holds_and_a_clean_tree_answers_its_sentence() {
    let mut harness = Harness::new(None).await;
    std::fs::write(harness.tree().join("CHANGE.md"), "a change\n").unwrap();
    let committed = harness
        .call(
            "aegis.git.commit",
            json!({"repository": LABEL, "message": "the run's change"}),
        )
        .await;
    let sha = match &committed {
        Ok(ToolInvocationResult::Direct(v)) => v["commit_sha"].as_str().unwrap_or("").to_string(),
        other => panic!(
            "the run's commit on the binding it holds failed: {}",
            told(other)
        ),
    };
    assert_eq!(sha.len(), 40, "the commit answered {committed:?}");
    assert_eq!(
        git(&harness.tree(), &["rev-parse", "HEAD"]),
        sha,
        "HEAD is not the commit answered"
    );
    assert_eq!(
        git(&harness.tree(), &["symbolic-ref", "--short", "HEAD"]),
        harness.branch,
        "the commit is not on the work branch"
    );
    let rows = harness.rows();
    let expected = format!(
        "Committed {sha} on branch {} of repository {LABEL} as User",
        harness.branch
    );
    assert!(
        rows.iter()
            .any(|(kind, line)| kind == "repository_committed" && *line == expected),
        "no row `{expected}` among {rows:?}"
    );

    let again = harness
        .call(
            "aegis.git.commit",
            json!({"repository": LABEL, "message": "nothing"}),
        )
        .await;
    match &again {
        Err(SealSessionError::Answered {
            answer: CallerAnswer::Conflict(sentence),
            ..
        }) if sentence == CLEAN => {}
        other => panic!("a clean tree answered {}, not `{CLEAN}`", told(other)),
    }
}

/// AEGIS ADR-136 G5d: a commit inside a run started by a person whose
/// profile has a name and an email is authored "name <email>", on a volume
/// (the git step) and on a host directory (libgit2), and its row names them.
#[tokio::test]
async fn a_commit_inside_a_run_started_by_a_person_with_a_name_and_email_is_authored_as_them() {
    let mut wrong = Vec::new();
    for host in [false, true] {
        let arm = if host { "host directory" } else { "volume" };
        let mut harness =
            Harness::new_with(None, host, Some(("Ada Lovelace", "ada@example.com"))).await;
        std::fs::write(harness.tree().join("CHANGE.md"), "a change\n").unwrap();
        let committed = harness
            .call(
                "aegis.git.commit",
                json!({"repository": LABEL, "message": "the person's change"}),
            )
            .await;
        let sha = direct(&committed)["commit_sha"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let author = git(&harness.tree(), &["log", "-1", "--format=%an <%ae>"]);
        if author != "Ada Lovelace <ada@example.com>" {
            wrong.push(format!("on a {arm} the commit is authored `{author}`"));
        }
        let expected = format!(
            "Committed {sha} on branch {} of repository {LABEL} as Ada Lovelace",
            harness.branch
        );
        let rows = harness.rows();
        if !rows
            .iter()
            .any(|(kind, line)| kind == "repository_committed" && *line == expected)
        {
            wrong.push(format!("on a {arm} no row `{expected}` among {rows:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "the run's commit is not authored as its person: {wrong:?}"
    );
}

/// AEGIS ADR-136 G5d: a run whose person has no name and email to read
/// commits as before, "User <user@aegis.local>".
#[tokio::test]
async fn a_run_started_with_no_identity_commits_as_before() {
    let mut harness = Harness::new(None).await;
    std::fs::write(harness.tree().join("CHANGE.md"), "a change\n").unwrap();
    let committed = harness
        .call(
            "aegis.git.commit",
            json!({"repository": LABEL, "message": "the run's change"}),
        )
        .await;
    let sha = direct(&committed)["commit_sha"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        git(&harness.tree(), &["log", "-1", "--format=%an <%ae>"]),
        "User <user@aegis.local>",
        "a run with no person's name and email does not commit as before"
    );
    let expected = format!(
        "Committed {sha} on branch {} of repository {LABEL} as User",
        harness.branch
    );
    let rows = harness.rows();
    assert!(
        rows.iter()
            .any(|(kind, line)| kind == "repository_committed" && *line == expected),
        "no row `{expected}` among {rows:?}"
    );
}

#[tokio::test]
async fn status_answers_the_work_branch_clean_or_changed_and_its_head() {
    let harness = Harness::new(None).await;
    let head = git(&harness.tree(), &["rev-parse", "HEAD"]);
    let clean = harness
        .call("aegis.git.status", json!({"repository": LABEL}))
        .await;
    assert_eq!(
        direct(&clean),
        json!({"repository": LABEL, "branch": harness.branch, "status": "clean", "last_commit_sha": head}),
        "the status of a clean tree"
    );
    std::fs::write(harness.tree().join("NEW.md"), "new\n").unwrap();
    let changed = harness
        .call("aegis.git.status", json!({"repository": LABEL}))
        .await;
    assert_eq!(
        direct(&changed)["status"],
        json!("changed"),
        "the status of a changed tree: {changed:?}"
    );
}

#[tokio::test]
async fn the_run_pushes_only_its_work_branch_and_no_part_of_the_token_is_anywhere() {
    let secret = token();
    let server = GitTestServer::start(GIT_USER, &secret);
    let mut harness = Harness::new(Some((&server, &secret))).await;
    // A second local branch the push must not send.
    git(&harness.tree(), &["branch", "not-for-the-remote"]);
    std::fs::write(harness.tree().join("CHANGE.md"), "a change\n").unwrap();
    let committed = harness
        .call(
            "aegis.git.commit",
            json!({"repository": LABEL, "message": "the run's change"}),
        )
        .await;
    let sha = direct(&committed)["commit_sha"]
        .as_str()
        .unwrap()
        .to_string();
    let pushed = harness
        .call(
            "aegis.git.push",
            json!({"repository": LABEL, "ref": "main", "remote": "elsewhere"}),
        )
        .await;
    assert_eq!(
        direct(&pushed),
        json!({"branch": harness.branch, "remote_url": server.url()}),
        "the push's answer"
    );
    let remote = remote_heads(&server, &secret);
    assert_eq!(
        remote.get(&format!("refs/heads/{}", harness.branch)),
        Some(&sha),
        "the work branch is not on the remote at the run's commit: {remote:?}"
    );
    assert_eq!(
        remote.get("refs/heads/main"),
        Some(&server.head()),
        "main moved"
    );
    assert!(
        !remote.contains_key("refs/heads/not-for-the-remote"),
        "a branch other than the work branch was pushed: {remote:?}"
    );

    let events = harness.events();
    let branch_url = format!(
        "{}/tree/{}",
        server.url().trim_end_matches(".git"),
        harness.branch
    );
    let expected = format!(
        "Pushed branch {} of repository {LABEL} to {branch_url}",
        harness.branch
    );
    let lines: Vec<String> = events
        .iter()
        .map(|e| normalize_domain_event(e, None).message)
        .collect();
    assert!(
        lines.contains(&expected),
        "no row `{expected}` among {lines:?}"
    );
    let mut holding = Vec::new();
    for (what, text) in [
        ("the commit's result", format!("{committed:?}")),
        ("the push's result", format!("{pushed:?}")),
        ("the events", format!("{events:?}")),
        (
            "the events' rows",
            serde_json::to_string(
                &events
                    .iter()
                    .map(|e| normalize_domain_event(e, None))
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
        ),
    ] {
        if holds_any_part_of(&text, &secret) {
            holding.push(what);
        }
    }
    assert!(holding.is_empty(), "the token is in {holding:?}");
}

#[tokio::test]
async fn a_push_the_remote_refuses_as_not_a_fast_forward_answers_its_sentence_and_pushes_nothing() {
    for host in [false, true] {
        refused_push_on(host).await;
    }
}

/// A non-fast-forward over HTTP, on a volume (the step) or on a host
/// directory (libgit2, whose remote answers the refusal per reference).
async fn refused_push_on(host: bool) {
    let arm = if host { "host directory" } else { "volume" };
    let secret = token();
    let server = GitTestServer::start(GIT_USER, &secret);
    let harness = Harness::new_on(Some((&server, &secret)), host).await;
    // The remote's copy of the work branch gains a commit the run does not
    // have.
    let other = tempfile::tempdir().unwrap();
    let url = with_user_info(&server.url(), &secret);
    git(other.path(), &["clone", "--quiet", &url, "c"]);
    let clone = other.path().join("c");
    git(&clone, &["checkout", "-b", &harness.branch]);
    std::fs::write(clone.join("THEIRS.md"), "theirs\n").unwrap();
    git(&clone, &["add", "THEIRS.md"]);
    git(&clone, &["commit", "-m", "theirs"]);
    git(&clone, &["push", "--quiet", "origin", &harness.branch]);
    let theirs = git(&clone, &["rev-parse", "HEAD"]);

    std::fs::write(harness.tree().join("OURS.md"), "ours\n").unwrap();
    harness
        .call(
            "aegis.git.commit",
            json!({"repository": LABEL, "message": "ours"}),
        )
        .await
        .unwrap_or_else(|e| panic!("{arm}: the run's commit: {e:?}"));
    let pushed = harness
        .call("aegis.git.push", json!({"repository": LABEL}))
        .await;
    let sentence = format!(
        "the remote branch '{}' has commits this run does not have; nothing was pushed",
        harness.branch
    );
    match &pushed {
        Err(SealSessionError::Answered {
            answer: CallerAnswer::Conflict(told),
            ..
        }) if *told == sentence => {}
        other => panic!(
            "{arm}: the refused push answered {}, not `{sentence}`",
            told(other)
        ),
    }
    assert_eq!(
        remote_heads(&server, &secret).get(&format!("refs/heads/{}", harness.branch)),
        Some(&theirs),
        "{arm}: the remote's branch moved"
    );
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

struct Harness {
    service: ToolInvocationService,
    tenant: TenantId,
    agent_id: AgentId,
    execution: ExecutionId,
    binding: GitRepoBindingId,
    branch: String,
    tree: PathBuf,
    events: EventReceiver,
    _dirs: tempfile::TempDir,
    _local: Option<(PathBuf, PathBuf)>,
}

impl Harness {
    /// A run (a root agent execution of `USER`) given one repository,
    /// labelled `app`, on a SeaweedFS-shaped volume, prepared and held for
    /// the run. Cloned from `server` (the password in the binding's URL), or
    /// from a bare repository on the local disk.
    async fn new(server: Option<(&GitTestServer, &str)>) -> Self {
        Self::new_on(server, false).await
    }

    /// As [`Self::new`], the repository on a host directory (libgit2) when
    /// `host`, its `origin` the server's URL with no user info.
    async fn new_on(server: Option<(&GitTestServer, &str)>, host: bool) -> Self {
        Self::new_with(server, host, None).await
    }

    /// As [`Self::new_on`], the run's person's profile answering `profile`
    /// as their name and email (AEGIS ADR-136 G5d).
    async fn new_with(
        server: Option<(&GitTestServer, &str)>,
        host: bool,
        profile: Option<(&str, &str)>,
    ) -> Self {
        let dirs = tempfile::tempdir().unwrap();
        let workspace = dirs.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let scratch = dirs.path().join("scratch").join("aegis-git");
        std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
        let (url, local) = match server {
            Some((server, secret)) => (with_user_info(&server.url(), secret), None),
            None => {
                let (bare, seed) = local_upstream(dirs.path());
                (format!("file://{}", bare.display()), Some((bare, seed)))
            }
        };

        let tenant = TenantId::default();
        let event_bus = Arc::new(EventBus::new(1024));
        let events = event_bus.subscribe();
        let volume_repo = Arc::new(InMemoryVolumeRepository::new());
        let user_volume_service = Arc::new(UserVolumeService::new(
            volume_repo.clone() as Arc<dyn VolumeRepository>,
            Arc::new(NoVolumes) as Arc<dyn VolumeService>,
            event_bus.clone(),
            crate::domain::volume::StorageTierLimits::default(),
        ));
        let secrets_manager = Arc::new(SecretsManager::from_store(
            Arc::new(TestSecretStore::new()),
            event_bus.clone(),
        ));
        let storage_root = dirs.path().join("fsal");
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
            volume_repo.clone() as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoStorageEvents),
        ));
        let engine = EphemeralCliEngine::new(
            Arc::new(HostShellRunner::new()) as Arc<dyn ContainerStepRunner>,
            Arc::new(NfsVolumeRegistry::new()),
        )
        .with_paths(EphemeralCliPaths {
            workspace: workspace.display().to_string(),
            scratch: scratch.display().to_string(),
        });
        let clone_executor = Arc::new(GitCloneExecutor::new(
            secrets_manager.clone(),
            fsal.clone(),
            Some(Arc::new(engine)),
        ));
        let bindings = Arc::new(Bindings::default());
        let git_repos = GitRepoService::new(
            bindings.clone() as Arc<dyn GitRepoBindingRepository>,
            user_volume_service,
            clone_executor,
            secrets_manager,
            event_bus.clone(),
        );
        let git_repos = Arc::new(match profile {
            Some((name, email)) => git_repos
                .with_person_profiles(Arc::new(Profile(name.to_string(), email.to_string()))),
            None => git_repos,
        });

        let host_tree = dirs.path().join("host-tree");
        let volume = Volume::new(
            "git-app".to_string(),
            tenant.clone(),
            StorageClass::persistent(),
            if host {
                VolumeBackend::HostPath {
                    path: host_tree.clone(),
                }
            } else {
                VolumeBackend::SeaweedFS {
                    filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
                    remote_path: "/aegis/seaweedfs/git-tools".to_string(),
                }
            },
            64 * 1024 * 1024,
            VolumeOwnership::persistent(USER),
        )
        .unwrap();
        volume_repo.save(&volume).await.unwrap();
        let mut binding = GitRepoBinding::new(
            tenant.clone(),
            None,
            url.clone(),
            GitRef::Branch("main".to_string()),
            None,
            volume.id,
            LABEL.to_string(),
            CloneStrategy::EphemeralCli {
                reason: "SeaweedFS volume requires FUSE-mounted container".to_string(),
            },
            false,
            None,
            None,
            None,
        );
        binding.clone_strategy = CloneStrategy::EphemeralCli {
            reason: "SeaweedFS volume requires FUSE-mounted container".to_string(),
        };
        if host {
            git(dirs.path(), &["clone", "--quiet", &url, "host-tree"]);
            let plain = crate::domain::secrets::RedactedUrl::new(&url)
                .as_str()
                .to_string();
            git(&host_tree, &["remote", "set-url", "origin", &plain]);
            binding.clone_strategy = CloneStrategy::Libgit2;
            binding.complete_clone(git(&host_tree, &["rev-parse", "HEAD"]), 0);
            bindings.save(&binding).await.unwrap();
        } else {
            bindings.save(&binding).await.unwrap();
            git_repos
                .clone_repo(&binding.id)
                .await
                .expect("the clone step clones the repository");
        }

        let agent = agent();
        let agent_id = agent.id;
        let execution_id = ExecutionId::new();
        let run = execution_id.0;
        let prepared = git_repos
            .prepare_for_run(
                &tenant,
                Some(USER),
                run,
                &[RunRepository {
                    binding_id: binding.id,
                    branch: None,
                    author: None,
                }],
            )
            .await
            .expect("the run's repository is prepared");
        let branch = default_work_branch(run);
        let mut execution = Execution::new_with_id(
            execution_id,
            agent_id,
            ExecutionInput {
                intent: None,
                input: json!({ "repositories": serde_json::to_value(&prepared).unwrap() }),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            5,
            CONTEXT.to_string(),
        );
        execution.tenant_id = tenant.clone();
        execution.initiating_user_sub = Some(USER.to_string());

        let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
        security_context_repo
            .save(security_context())
            .await
            .unwrap();
        let service = ToolInvocationService::new(
            Arc::new(InMemorySealSessionRepository::new()),
            security_context_repo,
            Arc::new(SealMiddleware::new()),
            Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
            fsal,
            NfsVolumeRegistry::new(),
            Arc::new(OneAgent(agent)),
            Arc::new(Executions(HashMap::from([(execution_id, execution)]))),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
            event_bus,
            None,
        )
        .with_git_repo_service(git_repos);

        Self {
            service,
            tenant,
            agent_id,
            execution: execution_id,
            binding: binding.id,
            branch,
            tree: if host {
                host_tree
            } else {
                workspace.join("repo")
            },
            events,
            _dirs: dirs,
            _local: local,
        }
    }

    /// The working tree: `repo` under the step's workspace, or the host
    /// directory.
    fn tree(&self) -> PathBuf {
        self.tree.clone()
    }

    async fn call(
        &self,
        tool: &str,
        args: Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        self.service
            .invoke_tool_internal(
                &self.agent_id,
                self.execution,
                self.tenant.clone(),
                0,
                Vec::new(),
                tool.to_string(),
                args,
            )
            .await
    }

    /// Every event published so far, and not read before.
    fn events(&mut self) -> Vec<DomainEvent> {
        let mut seen = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            seen.push(event);
        }
        seen
    }

    /// The type name and narrative line of every event published so far.
    fn rows(&mut self) -> Vec<(String, String)> {
        self.events()
            .iter()
            .map(|e| {
                let row = normalize_domain_event(e, None);
                (row.event_type, row.message)
            })
            .collect()
    }
}

/// A person's profile with a name and an email, as the identity provider
/// answers it for the run's person.
struct Profile(String, String);

#[async_trait]
impl PersonProfiles for Profile {
    async fn name_and_email(&self, sub: &str) -> Result<Option<(String, String)>, String> {
        assert_eq!(sub, USER, "the profile was read for another subject");
        Ok(Some((self.0.clone(), self.1.clone())))
    }
}

/// The sentence a call was answered with, or what it answered.
fn told(result: &Result<ToolInvocationResult, SealSessionError>) -> String {
    match result {
        Ok(ToolInvocationResult::Direct(v)) => format!("Ok({v})"),
        Ok(_) => "Ok(..)".to_string(),
        Err(SealSessionError::InvalidArguments(message)) => message.clone(),
        Err(SealSessionError::Answered {
            answer: CallerAnswer::InvalidArguments(message),
            ..
        })
        | Err(SealSessionError::Answered {
            answer: CallerAnswer::Conflict(message),
            ..
        }) => message.clone(),
        Err(other) => format!("{other:?}"),
    }
}

fn direct(result: &Result<ToolInvocationResult, SealSessionError>) -> Value {
    match result {
        Ok(ToolInvocationResult::Direct(v)) => v.clone(),
        other => panic!("the call answered {}", told(other)),
    }
}

/// `git` in `dir`, with no configuration of the machine's.
fn git(dir: &Path, args: &[&str]) -> String {
    let home = dir.join(".git-test-home");
    let _ = std::fs::create_dir_all(&home);
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository on the local disk holding one commit on `main`.
fn local_upstream(root: &Path) -> (PathBuf, PathBuf) {
    let bare = root.join("remote.git");
    let seed = root.join("seed");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(&seed).unwrap();
    git(&bare, &["init", "--bare", "--initial-branch=main"]);
    git(&seed, &["init", "--initial-branch=main"]);
    std::fs::write(seed.join("README.md"), "first line\n").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-m", "first"]);
    git(&seed, &["push", bare.to_str().unwrap(), "main"]);
    (bare, seed)
}

fn with_user_info(url: &str, password: &str) -> String {
    url.replacen("http://", &format!("http://{GIT_USER}:{password}@"), 1)
}

/// The remote's branches and their commits, read with the password.
fn remote_heads(server: &GitTestServer, secret: &str) -> HashMap<String, String> {
    let dir = tempfile::tempdir().unwrap();
    git(
        dir.path(),
        &[
            "ls-remote",
            "--heads",
            &with_user_info(&server.url(), secret),
        ],
    )
    .lines()
    .filter_map(|line| {
        let (sha, name) = line.split_once('\t')?;
        Some((name.to_string(), sha.to_string()))
    })
    .collect()
}

fn agent() -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: git-tools-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: ["aegis.git.status", "aegis.git.diff", "aegis.git.commit", "aegis.git.push"]
"#,
    )
    .unwrap();
    Agent {
        id: AgentId::new(),
        tenant_id: TenantId::default(),
        scope: crate::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn security_context() -> SecurityContext {
    SecurityContext {
        name: CONTEXT.to_string(),
        description: "git tools test".to_string(),
        capabilities: vec![crate::domain::security_context::Capability {
            tool_pattern: "aegis.git.*".to_string(),
            path_allowlist: None,
            command_allowlist: None,
            subcommand_allowlist: None,
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        }],
        deny_list: vec![],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

#[derive(Default)]
struct Bindings {
    bindings: StdRwLock<HashMap<GitRepoBindingId, GitRepoBinding>>,
}

#[async_trait]
impl GitRepoBindingRepository for Bindings {
    async fn save(&self, binding: &GitRepoBinding) -> Result<(), RepositoryError> {
        self.bindings
            .write()
            .unwrap()
            .insert(binding.id, binding.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &GitRepoBindingId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(self.bindings.read().unwrap().get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<Vec<GitRepoBinding>, RepositoryError> {
        Ok(self
            .bindings
            .read()
            .unwrap()
            .values()
            .filter(|b| &b.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    async fn find_by_volume_id(
        &self,
        volume_id: &VolumeId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(self
            .bindings
            .read()
            .unwrap()
            .values()
            .find(|b| &b.volume_id == volume_id)
            .cloned())
    }
    async fn find_by_webhook_lookup_hash(
        &self,
        _hash: &str,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(None)
    }
    async fn count_by_owner(
        &self,
        _tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<u32, RepositoryError> {
        Ok(0)
    }
    async fn delete(&self, id: &GitRepoBindingId) -> Result<(), RepositoryError> {
        self.bindings.write().unwrap().remove(id);
        Ok(())
    }
}

struct NoVolumes;

#[async_trait]
impl VolumeService for NoVolumes {
    async fn create_volume(
        &self,
        _name: String,
        _tenant_id: TenantId,
        _storage_class: StorageClass,
        _size_limit_mb: u64,
        _ownership: VolumeOwnership,
    ) -> anyhow::Result<VolumeId> {
        anyhow::bail!("not exercised")
    }
    async fn get_volume(&self, _id: VolumeId) -> anyhow::Result<Volume> {
        anyhow::bail!("not exercised")
    }
    async fn list_volumes_by_tenant(&self, _tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
        Ok(vec![])
    }
    async fn list_volumes_by_ownership(
        &self,
        _ownership: &VolumeOwnership,
    ) -> anyhow::Result<Vec<Volume>> {
        Ok(vec![])
    }
    async fn attach_volume(
        &self,
        _volume_id: VolumeId,
        _instance_id: InstanceId,
        _mount_point: PathBuf,
        _access_mode: AccessMode,
    ) -> anyhow::Result<VolumeMount> {
        anyhow::bail!("not exercised")
    }
    async fn detach_volume(
        &self,
        _volume_id: VolumeId,
        _instance_id: InstanceId,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_volume(&self, _volume_id: VolumeId) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get_volume_usage(&self, _volume_id: VolumeId) -> anyhow::Result<u64> {
        Ok(0)
    }
    async fn cleanup_expired_volumes(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn create_volumes_for_execution(
        &self,
        _execution_id: ExecutionId,
        _tenant_id: TenantId,
        _volume_specs: &[VolumeSpec],
        _storage_mode: &str,
    ) -> anyhow::Result<Vec<Volume>> {
        Ok(vec![])
    }
    async fn persist_external_volume(
        &self,
        _volume_id: VolumeId,
        _name: String,
        _tenant_id: TenantId,
        _remote_path: String,
        _size_limit_bytes: u64,
        _ownership: VolumeOwnership,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

struct NoStorageEvents;

#[async_trait]
impl EventPublisher for NoStorageEvents {
    async fn publish_storage_event(&self, _event: StorageEvent) {}
}

/// Serves the executions that call tools.
struct Executions(HashMap<ExecutionId, Execution>);

#[async_trait]
impl ExecutionService for Executions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        Ok(execution_id)
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> anyhow::Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_for_tenant(
        &self,
        _: &TenantId,
        id: ExecutionId,
    ) -> anyhow::Result<Execution> {
        self.get_execution_unscoped(id).await
    }
    async fn get_execution_unscoped(&self, id: ExecutionId) -> anyhow::Result<Execution> {
        self.0
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("execution not found"))
    }
    async fn get_iterations_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> anyhow::Result<Vec<Iteration>> {
        anyhow::bail!("not exercised")
    }
    async fn cancel_execution_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> anyhow::Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn stream_execution(
        &self,
        _: ExecutionId,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<DomainEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn list_executions_for_tenant(
        &self,
        _: &TenantId,
        _: Option<AgentId>,
        _: Option<crate::domain::workflow::WorkflowId>,
        _: usize,
    ) -> anyhow::Result<Vec<Execution>> {
        anyhow::bail!("not exercised")
    }
    async fn delete_execution_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> anyhow::Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: crate::domain::execution::LlmInteraction,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<crate::domain::execution::TrajectoryStep>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Resolves every agent to one agent with no `tool_validation`, so the
/// inner-loop judge does not run.
struct OneAgent(Agent);

#[async_trait]
impl AgentLifecycleService for OneAgent {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: crate::domain::agent::AgentScope,
        _: Option<&crate::domain::iam::UserIdentity>,
    ) -> anyhow::Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> anyhow::Result<Agent> {
        Ok(self.0.clone())
    }
    async fn update_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: AgentManifest,
    ) -> anyhow::Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> anyhow::Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_for_tenant(&self, _: &TenantId) -> anyhow::Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn lookup_agent_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> anyhow::Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> anyhow::Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}
