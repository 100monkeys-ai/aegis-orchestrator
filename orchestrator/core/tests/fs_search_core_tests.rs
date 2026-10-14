// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `fs.search`, the built-in that finds code by text and by its parsed
//! structure (AEGIS ADR-145, Trigger T1 (a) to (h) and T2), each test on an
//! FSAL test volume: host-path volumes under a scratch root, registered for
//! one execution at the mount points a run gives them, called through
//! `invoke_fs_tool` as the tool router calls every `fs.*` tool.

use aegis_orchestrator_core::application::nfs_gateway::{NfsVolumeRegistry, VolumeRegistration};
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationResult;
use aegis_orchestrator_core::application::tools::builtin_fsal::invoke_fs_tool;
use aegis_orchestrator_core::domain::events::StorageEvent;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher, FsalAccessPolicy};
use aegis_orchestrator_core::domain::repository::VolumeRepository;
use aegis_orchestrator_core::domain::volume::{
    StorageClass, TenantId, Volume, VolumeBackend, VolumeId, VolumeOwnership,
};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

struct NoStorageEvents;

#[async_trait::async_trait]
impl EventPublisher for NoStorageEvents {
    async fn publish_storage_event(&self, _event: StorageEvent) {}
}

/// One execution's volumes on a scratch root, each registered at its mount
/// point.
struct Run {
    root: tempfile::TempDir,
    execution: ExecutionId,
    fsal: Arc<AegisFSAL>,
    registry: NfsVolumeRegistry,
    mounts: Vec<(String, VolumeId)>,
}

impl Run {
    async fn with_mounts(mounts: &[&str]) -> Run {
        let root = tempfile::tempdir().expect("scratch root");
        let execution = ExecutionId::new();
        let volumes = Arc::new(InMemoryVolumeRepository::new());
        let registry = NfsVolumeRegistry::new();
        let mut registered = Vec::new();
        for mount in mounts {
            let mut volume = Volume::new(
                "volume".to_string(),
                TenantId::consumer(),
                StorageClass::persistent(),
                VolumeBackend::HostPath {
                    path: PathBuf::from("/placeholder"),
                },
                64 * 1024 * 1024,
                VolumeOwnership::execution(execution),
            )
            .expect("volume");
            volume.backend = VolumeBackend::HostPath {
                path: PathBuf::from(format!("/hosts/{}", volume.id)),
            };
            volume.mark_available().expect("available");
            volumes.save(&volume).await.expect("saved");
            std::fs::create_dir_all(root.path().join(format!("hosts/{}", volume.id)))
                .expect("the volume's folder");
            registry.register(VolumeRegistration {
                volume_id: volume.id,
                execution_id: execution,
                workflow_execution_id: None,
                container_uid: 1000,
                container_gid: 1000,
                policy: FsalAccessPolicy {
                    read: vec!["/**".to_string()],
                    write: vec!["/**".to_string()],
                },
                mount_point: PathBuf::from(mount),
                remote_path: String::new(),
            });
            registered.push((mount.to_string(), volume.id));
        }
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(root.path()).expect("storage root")),
            volumes as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoStorageEvents),
        ));
        Run {
            root,
            execution,
            fsal,
            registry,
            mounts: registered,
        }
    }

    /// Write `content` at the container path `path`, in the volume of the
    /// most specific mount above it.
    fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        let (mount, volume) = self
            .mounts
            .iter()
            .filter(|(mount, _)| path.starts_with(&format!("{mount}/")))
            .max_by_key(|(mount, _)| mount.len())
            .expect("a mount above the path");
        let relative = &path[mount.len() + 1..];
        let on_disk = self
            .root
            .path()
            .join(format!("hosts/{volume}"))
            .join(relative);
        std::fs::create_dir_all(on_disk.parent().expect("a parent")).expect("folders");
        std::fs::write(on_disk, content).expect("written");
    }

    /// `fs.search` with `args`: the answer's text, or the refusal.
    async fn search(&self, args: Value) -> Result<String, String> {
        match invoke_fs_tool(
            "fs.search",
            &args,
            self.execution,
            &self.fsal,
            &self.registry,
        )
        .await
        {
            Ok(ToolInvocationResult::Direct(answer)) => Ok(answer["answer"]
                .as_str()
                .unwrap_or_else(|| panic!("an answer with no text: {answer}"))
                .to_string()),
            Ok(_) => panic!("fs.search answers directly"),
            Err(refused) => Err(refused.to_string()),
        }
    }

    async fn answer(&self, args: Value) -> String {
        self.search(args.clone())
            .await
            .unwrap_or_else(|refused| panic!("fs.search {args} was refused: {refused}"))
    }
}

/// T1 (a): a Rust and a Python fixture each answer a two-word conceptual
/// query with a declaration naming its path, line, kind, name and excerpt.
#[tokio::test]
async fn fs_search_core_t1a_a_conceptual_query_finds_a_rust_and_a_python_declaration() {
    let run = Run::with_mounts(&["/workspace", "/workspace/repo"]).await;
    run.write(
        "/workspace/repo/src/clock.rs",
        "/// Keeps the turn's clock.\npub fn refresh_turn_clock() {}\n",
    );
    run.write(
        "/workspace/repo/app/payments.py",
        "class Ledger:\n    # Settles one invoice for the ledger.\n    def settle_invoice(self):\n        pass\n",
    );

    let rust = run.answer(json!({"needle": "turn clock refreshed"})).await;
    assert!(
        rust.contains(
            "/workspace/repo/src/clock.rs\n  2: function refresh_turn_clock — /// Keeps the turn's clock. pub fn refresh_turn_clock() {}"
        ),
        "the Rust declaration is not answered with its path, line, kind, name and excerpt:\n{rust}"
    );

    let python = run
        .answer(json!({"needle": "where is the invoice settled"}))
        .await;
    assert!(
        python.contains(
            "/workspace/repo/app/payments.py\n  3: function settle_invoice in Ledger — # Settles one invoice for the ledger. def settle_invoice(self): pass"
        ),
        "the Python declaration is not answered with its path, line, kind, name and excerpt:\n{python}"
    );
}

/// T1 (b): camelCase, snake_case and spaced forms of one query find the
/// same declaration.
#[tokio::test]
async fn fs_search_core_t1b_camel_snake_and_spaced_forms_find_the_same_declaration() {
    let run = Run::with_mounts(&["/workspace", "/workspace/repo"]).await;
    run.write(
        "/workspace/repo/src/clock.rs",
        "/// Moves the clock on.\npub fn refresh_turn_clock() {}\npub fn unrelated() {}\n",
    );
    let declaration = "/workspace/repo/src/clock.rs\n  2: function refresh_turn_clock — ";
    for needle in [
        "refreshTurnClock",
        "refresh_turn_clock",
        "refresh turn clock",
    ] {
        let answer = run.answer(json!({"needle": needle})).await;
        assert!(
            answer.contains(declaration),
            "{needle} did not find refresh_turn_clock:\n{answer}"
        );
    }
}

/// T1 (c): a repository volume registered at `/workspace/<label>` beneath a
/// `/workspace` volume is reached by a call with `path` `/workspace` and by a
/// call with no `path`, and its README is named
/// `/workspace/<label>/README.md`.
#[tokio::test]
async fn fs_search_core_t1c_the_repository_beneath_workspace_is_reached_and_named_as_mounted() {
    let run = Run::with_mounts(&["/workspace", "/workspace/aegis-examples"]).await;
    run.write("/workspace/notes.txt", "nothing here\n");
    run.write(
        "/workspace/aegis-examples/README.md",
        "# Examples\nmarker-145 is here\n",
    );

    let from_workspace = run
        .answer(json!({"needle": "marker-145", "path": "/workspace"}))
        .await;
    assert!(
        from_workspace.starts_with("Searched /workspace for \"marker-145\""),
        "{from_workspace}"
    );
    assert!(
        from_workspace.contains("\n/workspace/aegis-examples/README.md\n  2: marker-145 is here\n"),
        "a walk from /workspace did not reach the repository volume beneath it:\n{from_workspace}"
    );

    let with_no_path = run.answer(json!({"needle": "marker-145"})).await;
    assert!(
        with_no_path.starts_with("Searched /workspace/aegis-examples for \"marker-145\""),
        "a call with no path did not search the run's one repository:\n{with_no_path}"
    );
    assert!(
        with_no_path.contains("\n/workspace/aegis-examples/README.md\n  2: marker-145 is here\n"),
        "{with_no_path}"
    );

    let by_name = run
        .answer(json!({"needle": "readme", "path": "/workspace"}))
        .await;
    assert!(
        by_name.contains(
            "Files whose names hold \"readme\":\n  /workspace/aegis-examples/README.md\n"
        ),
        "the README is not named by its container path:\n{by_name}"
    );
}

/// T1 (d): `.git`, `node_modules`, a lock file and a `.gitignore`d file are
/// skipped and counted, and searched with `include_ignored`, `.git` still
/// skipped.
#[tokio::test]
async fn fs_search_core_t1d_ignored_dependency_lock_and_git_files_are_skipped_and_counted() {
    let run = Run::with_mounts(&["/workspace", "/workspace/repo"]).await;
    run.write("/workspace/repo/.gitignore", "secret.txt\n");
    run.write("/workspace/repo/.git/config", "zebra-needle\n");
    run.write(
        "/workspace/repo/node_modules/left/index.js",
        "zebra-needle\n",
    );
    run.write("/workspace/repo/Cargo.lock", "zebra-needle\n");
    run.write("/workspace/repo/secret.txt", "zebra-needle\n");
    run.write("/workspace/repo/src/main.rs", "// zebra-needle\n");

    let answer = run.answer(json!({"needle": "zebra-needle"})).await;
    assert!(
        answer.starts_with(
            "Searched /workspace/repo for \"zebra-needle\" in any case: 1 line in 1 file;"
        ),
        "{answer}"
    );
    assert!(
        answer.contains("\n/workspace/repo/src/main.rs\n  1: // zebra-needle\n"),
        "{answer}"
    );
    for counted in [
        "the folders .git, node_modules (build output, dependencies or repository data)",
        "1 file or folder a .gitignore names (such as /workspace/repo/secret.txt)",
        "1 lock or minified file (such as /workspace/repo/Cargo.lock)",
    ] {
        assert!(answer.contains(counted), "not counted: {counted}\n{answer}");
    }

    let everything = run
        .answer(json!({"needle": "zebra-needle", "include_ignored": true}))
        .await;
    for searched in [
        "/workspace/repo/Cargo.lock (generated or ignored)\n  1: zebra-needle\n",
        "/workspace/repo/node_modules/left/index.js (generated or ignored)\n  1: zebra-needle\n",
        "/workspace/repo/secret.txt (generated or ignored)\n  1: zebra-needle\n",
        "/workspace/repo/src/main.rs\n  1: // zebra-needle\n",
    ] {
        assert!(
            everything.contains(searched),
            "not searched: {searched}\n{everything}"
        );
    }
    assert!(
        !everything.contains(".git/config"),
        ".git was searched with include_ignored:\n{everything}"
    );
    assert!(
        everything.contains("the folder .git (build output"),
        "{everything}"
    );
}

/// T1 (e): a file with a syntax error yields no declaration, and its text is
/// still searched.
#[tokio::test]
async fn fs_search_core_t1e_a_file_that_does_not_parse_gives_no_declaration_and_is_still_searched()
{
    let run = Run::with_mounts(&["/workspace", "/workspace/repo"]).await;
    run.write(
        "/workspace/repo/src/broken.rs",
        "/// Settles the invoice.\nfn settle_invoice( {\n",
    );
    let answer = run.answer(json!({"needle": "settle_invoice"})).await;
    assert!(
        answer.contains("\n/workspace/repo/src/broken.rs\n  2: fn settle_invoice( {\n"),
        "the text of a file that does not parse was not searched:\n{answer}"
    );
    assert!(
        !answer.contains("(definition)") && !answer.contains("Declarations whose names"),
        "a file that does not parse was given a declaration:\n{answer}"
    );
}

/// T1 (f): on a fixture with more matching lines than the bounds, the answer
/// is at most 30,720 bytes and shows at most 40 lines, at most 200
/// characters of each, 8 of a file before other files get theirs; two calls
/// are byte-equal.
#[tokio::test]
async fn fs_search_core_t1f_the_answer_is_bounded_and_two_calls_are_byte_equal() {
    let run = Run::with_mounts(&["/workspace", "/workspace/repo"]).await;
    let long_line = format!("needle {}\n", "x".repeat(300));
    for file in 0..30 {
        run.write(
            &format!("/workspace/repo/src/f{file:02}.txt"),
            long_line.repeat(50),
        );
    }
    let first = run.answer(json!({"needle": "needle"})).await;
    let second = run.answer(json!({"needle": "needle"})).await;
    assert_eq!(
        first.as_bytes(),
        second.as_bytes(),
        "two calls answered differently"
    );
    assert!(first.len() <= 30_720, "{} bytes", first.len());
    assert!(
        first.starts_with("Searched /workspace/repo for \"needle\" in any case: 1500 lines in 30 files; 30 files read."),
        "{first}"
    );
    let rows: Vec<&str> = first
        .lines()
        .filter(|line| line.starts_with("  ") && line.contains(": needle "))
        .collect();
    assert_eq!(rows.len(), 40, "{first}");
    for row in &rows {
        assert!(
            row.ends_with(&format!("{}… (107 more characters)", "x".repeat(193))),
            "a line is not cut at 200 characters: {row}"
        );
    }
    for file in 0..5 {
        let heading = format!("\n/workspace/repo/src/f{file:02}.txt\n");
        let at = first
            .find(&heading)
            .unwrap_or_else(|| panic!("{heading} missing:\n{first}"));
        let shown = first[at + heading.len()..]
            .lines()
            .take_while(|line| line.starts_with("  "))
            .count();
        assert_eq!(shown, 8, "f{file:02}.txt shows {shown} lines:\n{first}");
    }
    assert!(
        first.contains("\n1460 more lines are not shown, in 30 of the 30 files that match."),
        "{first}"
    );
}

/// T1 (g): the empty-needle refusal and the unregistered-mount refusal.
#[tokio::test]
async fn fs_search_core_t1g_an_empty_needle_and_an_unregistered_mount_are_refused() {
    let run = Run::with_mounts(&["/workspace", "/workspace/repo"]).await;
    let empty = run.search(json!({"needle": ""})).await;
    let sentence = "the string to search for is empty, which occurs everywhere in every file; fs.search needs something to look for";
    assert!(
        matches!(&empty, Err(refused) if refused.contains(sentence)),
        "{empty:?}"
    );

    let (_, repository) = run.mounts[1];
    run.registry.deregister(repository);
    let unregistered = run
        .search(json!({"needle": "x", "path": "/workspace/repo/src"}))
        .await;
    let sentence =
        "/workspace/repo/src is under /workspace/repo, whose volume is not registered for this execution";
    assert!(
        matches!(&unregistered, Err(refused) if refused.contains(sentence)),
        "{unregistered:?}"
    );

    let nothing = Run::with_mounts(&[]).await;
    let no_volume = nothing.search(json!({"needle": "x"})).await;
    let sentence = format!("No volume registered for execution {}", nothing.execution);
    assert!(
        matches!(&no_volume, Err(refused) if refused.contains(&sentence)),
        "{no_volume:?}"
    );
}

/// T1 (h): the walk stops at the file bound, answers what it found, and
/// says that it stopped and in which folder.
#[tokio::test]
async fn fs_search_core_t1h_the_walk_stops_at_ten_thousand_files_and_says_where() {
    let run = Run::with_mounts(&["/workspace", "/workspace/repo"]).await;
    for at in 0..10_001 {
        let folder = if at < 6_000 { "a" } else { "b" };
        run.write(
            &format!("/workspace/repo/many/{folder}/f{at:05}.txt"),
            "word\n",
        );
    }
    let answer = run.answer(json!({"needle": "word"})).await;
    assert!(
        answer.starts_with("Searched /workspace/repo for \"word\" in any case: 10000 lines in 10000 files; 10000 files read."),
        "{}",
        &answer[..answer.len().min(400)]
    );
    assert!(
        answer.contains("The walk stopped at 10000 files read, in /workspace/repo/many/b;"),
        "the stop is not said:\n{answer}"
    );
    assert!(
        !answer.contains("f10000.txt"),
        "the file past the bound was read"
    );
}

/// T2: the seven grammars are pinned exactly in the core crate's manifest,
/// and the search's modules name no network client.
#[test]
fn fs_search_core_t2_the_grammars_are_pinned_and_the_search_opens_no_network_client() {
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(core.join("Cargo.toml")).expect("the manifest");
    for pin in [
        "tree-sitter = { version = \"=0.25.10\"",
        "tree-sitter-rust = \"=0.24.2\"",
        "tree-sitter-python = \"=0.23.6\"",
        "tree-sitter-javascript = \"=0.23.1\"",
        "tree-sitter-typescript = \"=0.23.2\"",
        "tree-sitter-go = \"=0.23.4\"",
        "tree-sitter-java = \"=0.23.5\"",
    ] {
        assert!(manifest.contains(pin), "the manifest does not pin {pin}");
    }
    for module in ["builtin_search.rs", "codebase.rs"] {
        let source = std::fs::read_to_string(core.join("src/application/tools").join(module))
            .unwrap_or_else(|e| panic!("{module}: {e}"));
        for client in ["reqwest", "hyper", "tonic"] {
            assert!(!source.contains(client), "{module} names {client}");
        }
    }
}
