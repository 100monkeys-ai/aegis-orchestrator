// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis edge logout` — remove the entire local edge state directory.
//!
//! Logout means logout: the whole `~/.aegis/edge` directory (or the override
//! supplied via `--state-dir`) is removed, not just a hand-picked subset of
//! credential files. Leaving `aegis-config.yaml` or `node.pub` behind would
//! pin the next enrollment to the previous (orphaned) `spec.node.id`, which
//! defeats the purpose of logging out.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};

#[derive(Debug, Args, Default)]
pub struct LogoutArgs {
    /// Override the local edge state directory (default: `~/.aegis/edge`).
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
}

pub async fn run(args: LogoutArgs) -> Result<()> {
    let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
    let message = logout(&state_dir)?;
    println!("{message}");
    Ok(())
}

fn default_state_dir() -> PathBuf {
    dirs_next::home_dir()
        .map(|h| h.join(".aegis").join("edge"))
        .unwrap_or_else(|| PathBuf::from(".aegis/edge"))
}

/// Remove the entire edge state directory rooted at `state_dir`.
///
/// Returns the human-readable status message that should be surfaced to the
/// caller. The deletion is bounded to `state_dir` — nothing outside that
/// directory is touched.
pub fn logout(state_dir: &Path) -> Result<String> {
    if !state_dir.exists() {
        return Ok(format!(
            "aegis edge logout: no state at {}; nothing to remove",
            state_dir.display()
        ));
    }
    if !state_dir.is_dir() {
        bail!(
            "aegis edge logout: {} exists but is not a directory; refusing to remove",
            state_dir.display()
        );
    }
    std::fs::remove_dir_all(state_dir).with_context(|| {
        format!(
            "failed to remove edge state directory {}",
            state_dir.display()
        )
    })?;
    Ok(format!(
        "aegis edge logout: removed state directory {}",
        state_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn logout_removes_entire_state_dir_including_config_and_pubkey() {
        // Regression: previously logout only removed node.key / node.token /
        // enrollment.jwt and left aegis-config.yaml + node.pub behind. The
        // leftover spec.node.id pinned the next enrollment to the orphaned
        // UUID. This test fails on the old partial-removal implementation.
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("edge");
        fs::create_dir_all(&state_dir).unwrap();

        write(
            &state_dir.join("aegis-config.yaml"),
            "spec:\n  node:\n    id: 00000000-0000-0000-0000-000000000000\n",
        );
        write(&state_dir.join("enrollment.jwt"), "jwt");
        write(&state_dir.join("node.key"), "private-key");
        write(&state_dir.join("node.pub"), "public-key");
        write(&state_dir.join("node.token"), "token");

        let msg = logout(&state_dir).expect("logout should succeed");

        assert!(
            !state_dir.exists(),
            "state directory must be fully removed; found: {:?}",
            fs::read_dir(&state_dir)
                .ok()
                .map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>()),
        );
        assert!(msg.contains("removed state directory"));
        assert!(msg.contains(&state_dir.display().to_string()));
    }

    #[test]
    fn logout_is_noop_when_state_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("does-not-exist");

        assert!(!state_dir.exists());

        let msg = logout(&state_dir).expect("absent state dir is not an error");

        assert!(!state_dir.exists());
        assert!(msg.contains("nothing to remove"));
        assert!(msg.contains(&state_dir.display().to_string()));
    }

    #[test]
    fn logout_errors_when_state_path_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("edge-not-a-dir");
        write(&state_path, "this is a file, not a directory");

        let err = logout(&state_path).expect_err("should refuse to act on a file");
        let msg = format!("{err}");
        assert!(msg.contains("is not a directory"), "got: {msg}");
        // Boundary check: the file is not deleted by the failed logout.
        assert!(state_path.exists());
    }

    #[test]
    fn logout_does_not_touch_siblings_outside_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("edge");
        fs::create_dir_all(&state_dir).unwrap();
        write(&state_dir.join("node.key"), "k");

        let sibling = tmp.path().join("sibling.txt");
        write(&sibling, "do not touch");

        logout(&state_dir).unwrap();

        assert!(!state_dir.exists());
        assert!(sibling.exists(), "sibling outside state_dir must survive");
    }
}
