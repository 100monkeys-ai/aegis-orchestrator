// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # git through a FUSE mount of a SeaweedFS volume, end to end
//!
//! The clone step of a git repository binding runs `git clone` in a container
//! whose `/workspace` is a FUSE mount of the binding's volume, served by the
//! FSAL over the SeaweedFS filer. This harness runs real `git` through a real
//! mount of a volume on a real filer: the clone exactly as the step runs it
//! (`application/git_clone_executor.rs`, `clone_step`), then `git status`, a
//! commit and `git log` in the clone.
//!
//! It needs a SeaweedFS filer, `/dev/fuse` and `fusermount3`, so it runs only
//! when `AEGIS_SEAWEEDFS_E2E=1` and is skipped otherwise. The filer is
//! `AEGIS_SEAWEEDFS_E2E_FILER` (default `http://localhost:8888`); a filer
//! started as production starts it (`chrislusf/seaweedfs`, `master`, `volume`
//! and `filer` with their production arguments) serves it.
//!
//! The daemon mounts with `allow_other`, which `fusermount3` refuses to a
//! user unless `/etc/fuse.conf` sets `user_allow_other`. Where it does not,
//! the harness runs in a user and mount namespace of its own, where the
//! mount is made directly:
//!
//! ```text
//! AEGIS_SEAWEEDFS_E2E=1 CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='unshare -Urm' \
//!     cargo test -p aegis-orchestrator-core --test seaweedfs_git_e2e -- --nocapture
//! ```
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** End-to-end harness for git on the SeaweedFS storage adapter
//!   through the FUSE daemon

use aegis_orchestrator_core::application::nfs_gateway::{NfsVolumeRegistry, VolumeRegistration};
use aegis_orchestrator_core::domain::events::StorageEvent;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher, FsalAccessPolicy};
use aegis_orchestrator_core::domain::repository::VolumeRepository;
use aegis_orchestrator_core::domain::volume::{
    FilerEndpoint, StorageClass, TenantId, Volume, VolumeBackend, VolumeOwnership,
};
use aegis_orchestrator_core::infrastructure::fuse::daemon::{FuseFsalDaemon, FuseVolumeContext};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::storage::seaweedfs::SeaweedFSAdapter;
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const ENABLE: &str = "AEGIS_SEAWEEDFS_E2E";
const FILER: &str = "AEGIS_SEAWEEDFS_E2E_FILER";

struct NoEvents;

#[async_trait]
impl EventPublisher for NoEvents {
    async fn publish_storage_event(&self, _: StorageEvent) {}
}

/// One git command's outcome, as the harness prints it.
struct Ran {
    line: String,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn describe(&self) -> String {
        format!(
            "$ {}\nexit {:?}\nstdout: {}\nstderr: {}",
            self.line,
            self.code,
            self.stdout.trim_end(),
            self.stderr.trim_end()
        )
    }
}

/// `git` with no configuration of the machine's or the user's, as the step's
/// image has none, and a fixed author.
fn git(home: &Path, cwd: &Path, args: &[&str]) -> Ran {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "AEGIS e2e")
        .env("GIT_AUTHOR_EMAIL", "e2e@100monkeys.ai")
        .env("GIT_COMMITTER_NAME", "AEGIS e2e")
        .env("GIT_COMMITTER_EMAIL", "e2e@100monkeys.ai")
        .output()
        .expect("git runs");
    let ran = Ran {
        line: format!("git {}", args.join(" ")),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    println!("{}", ran.describe());
    ran
}

/// The step's `g`: git trusting the mounted tree and running none of its
/// hooks or file-system monitor (`step_prelude`).
fn g(home: &Path, cwd: &Path, args: &[&str]) -> Ran {
    let mut all = vec![
        "-c",
        "safe.directory=*",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
    ];
    all.extend_from_slice(args);
    git(home, cwd, &all)
}

/// A small bare repository on the local disk: a README, a nested source
/// tree, an executable script, and a directory of 150 files.
fn source_repository(scratch: &Path, home: &Path) -> PathBuf {
    let work = scratch.join("work");
    let bare = scratch.join("source.git");
    std::fs::create_dir_all(work.join("src/lib")).unwrap();
    std::fs::create_dir_all(work.join("data")).unwrap();
    std::fs::write(
        work.join("README.md"),
        "# e2e\n\nA repository for the harness.\n",
    )
    .unwrap();
    std::fs::write(
        work.join("src/lib/mod.rs"),
        "pub fn answer() -> u32 {\n    42\n}\n",
    )
    .unwrap();
    std::fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
    let script = work.join("run.sh");
    std::fs::write(&script, "#!/bin/sh\necho ok\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    for i in 0..150 {
        std::fs::write(work.join(format!("data/{i:03}.txt")), format!("{i}\n")).unwrap();
    }
    for args in [
        vec!["init", "--quiet", "--initial-branch=main"],
        vec!["add", "-A"],
        vec!["commit", "--quiet", "-m", "first"],
    ] {
        assert!(
            git(home, &work, &args).ok(),
            "the source repository is made"
        );
    }
    std::fs::write(work.join("CHANGES.md"), "- second\n").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "--quiet", "-m", "second"]] {
        assert!(
            git(home, &work, &args).ok(),
            "the source repository is made"
        );
    }
    assert!(
        git(
            home,
            scratch,
            &[
                "clone",
                "--quiet",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap()
            ]
        )
        .ok(),
        "the bare repository is made"
    );
    bare
}

/// git's clone, status, commit and log through a FUSE mount of a SeaweedFS
/// volume all succeed, and the clone's tree is clean.
#[test]
fn git_clones_commits_and_logs_through_a_fuse_mount_of_a_seaweedfs_volume() {
    if std::env::var(ENABLE).as_deref() != Ok("1") {
        println!(
            "skipped: set {ENABLE}=1 with a SeaweedFS filer at {FILER} (default http://localhost:8888), /dev/fuse and fusermount3"
        );
        return;
    }
    let filer_url = std::env::var(FILER).unwrap_or_else(|_| "http://localhost:8888".to_string());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    // A person's persistent volume, as a git repository binding's is, with
    // the clone step's execution registered for it (`run_git_step`).
    let step = ExecutionId::new();
    let mut volume = Volume::new(
        "git-repo-binding".to_string(),
        TenantId::default(),
        StorageClass::persistent(),
        VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new(&filer_url).unwrap(),
            remote_path: String::new(),
        },
        500 * 1024 * 1024,
        VolumeOwnership::persistent("e2e"),
    )
    .unwrap();
    let remote_path = format!("/aegis/volumes/u-e2e/{}", volume.id);
    volume.backend = VolumeBackend::SeaweedFS {
        filer_endpoint: FilerEndpoint::new(&filer_url).unwrap(),
        remote_path: remote_path.clone(),
    };
    volume.mark_available().unwrap();
    let volume_id = volume.id;

    let repository = Arc::new(InMemoryVolumeRepository::new());
    rt.block_on(repository.save(&volume)).unwrap();
    let registry = NfsVolumeRegistry::new();
    registry.register(VolumeRegistration {
        volume_id,
        execution_id: step,
        workflow_execution_id: None,
        container_uid: 0,
        container_gid: 0,
        policy: FsalAccessPolicy::default(),
        mount_point: PathBuf::from("/workspace"),
        remote_path: remote_path.clone(),
    });
    let fsal = AegisFSAL::new(
        Arc::new(SeaweedFSAdapter::new(&filer_url)),
        repository as Arc<dyn VolumeRepository>,
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    )
    .with_volume_context_lookup(Arc::new(registry.clone()));

    let scratch = tempfile::tempdir_in("/tmp").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let bare = source_repository(scratch.path(), &home);
    let url = format!("file://{}", bare.display());

    // The step's mount of the volume, as the container step runner asks for
    // it: the step's execution, the container's root, every path readable
    // and writable.
    let mountpoint = scratch.path().join("workspace");
    let daemon = FuseFsalDaemon::new(Arc::new(fsal));
    let mount = {
        let _entered = rt.enter();
        daemon
            .mount(
                &mountpoint,
                FuseVolumeContext {
                    execution_id: step,
                    volume_id,
                    workflow_execution_id: None,
                    container_uid: 0,
                    container_gid: 0,
                    policy: FsalAccessPolicy {
                        read: vec!["/*".to_string()],
                        write: vec!["/*".to_string()],
                    },
                },
            )
            .expect("the FUSE daemon mounts the volume")
    };
    println!("mounted {remote_path} at {}", mountpoint.display());

    let tree = mountpoint.join("repo");
    let tree_arg = tree.to_str().unwrap().to_string();
    let mut runs = Vec::new();

    // The clone step's script (`clone_step`).
    runs.push(g(
        &home,
        &mountpoint,
        &[
            "clone",
            "--depth=1",
            "--filter=blob:limit=10M",
            "--branch",
            "main",
            "--",
            &url,
            &tree_arg,
        ],
    ));
    runs.push(g(
        &home,
        &mountpoint,
        &["-C", &tree_arg, "rev-parse", "HEAD"],
    ));

    // The tree as a run then works in it.
    runs.push(g(&home, &tree, &["status"]));
    let porcelain = g(
        &home,
        &tree,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let clean = porcelain.ok() && porcelain.stdout.trim().is_empty();
    runs.push(porcelain);
    let readme = std::fs::read_to_string(tree.join("README.md"));
    println!("README.md through the mount: {readme:?}");
    std::fs::write(tree.join("NOTES.md"), "written through the mount\n")
        .expect("NOTES.md is written through the mount");
    std::fs::write(
        tree.join("src/main.rs"),
        "fn main() {\n    println!(\"e2e\");\n}\n",
    )
    .expect("src/main.rs is written through the mount");
    runs.push(g(&home, &tree, &["add", "-A"]));
    runs.push(g(
        &home,
        &tree,
        &["commit", "-m", "e2e: a commit through the mount"],
    ));

    // A file removed from a directory that keeps others: git removes the
    // file, then tries `rmdir` of its directory, which must refuse.
    runs.push(g(&home, &tree, &["rm", "--quiet", "data/000.txt"]));
    runs.push(g(
        &home,
        &tree,
        &["commit", "--quiet", "-m", "e2e: remove one data file"],
    ));
    let kept = std::fs::read_to_string(tree.join("data/001.txt"));
    println!("data/001.txt through the mount after git rm data/000.txt: {kept:?}");
    // A directory removed whole: git removes its files and the directory.
    runs.push(g(&home, &tree, &["rm", "-r", "--quiet", "src/lib"]));
    runs.push(g(
        &home,
        &tree,
        &["commit", "--quiet", "-m", "e2e: remove src/lib"],
    ));
    let removed = std::fs::symlink_metadata(tree.join("src/lib"));
    println!("src/lib through the mount after git rm -r src/lib: {removed:?}");

    runs.push(g(&home, &tree, &["log", "--oneline"]));
    let after = g(
        &home,
        &tree,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let clean_after = after.ok() && after.stdout.trim().is_empty();
    runs.push(after);

    drop(mount);

    let failed: Vec<String> = runs.iter().filter(|r| !r.ok()).map(Ran::describe).collect();
    assert!(
        failed.is_empty(),
        "git failed through the mount:\n{}",
        failed.join("\n\n")
    );
    assert_eq!(
        readme.as_deref().ok(),
        Some("# e2e\n\nA repository for the harness.\n"),
        "README.md did not read back through the mount"
    );
    assert!(clean, "the clone's tree is not clean through the mount");
    assert_eq!(
        kept.as_deref().ok(),
        Some("1\n"),
        "git rm of one file took the rest of its directory with it"
    );
    assert!(
        removed.is_err(),
        "src/lib is still there after git rm -r src/lib: {removed:?}"
    );
    assert!(clean_after, "the tree is not clean after the commit");
}
