// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Runs the Python tests of `assets/bootstrap.py` (ADR-040), the script the
//! orchestrator copies into every StandardRuntime agent container, so they
//! gate the pipeline as the Rust tests do.
//!
//! The production failures they pin (2026-10-01): every built-in executor's
//! `cmd.run` with cwd `/workspace` raised `FileNotFoundError` out of
//! `run_dispatch` and killed the bootstrap, so the model never saw the error;
//! and the code validator's generate request, timed out after 60 s on a
//! delivered request, was re-sent to two host names that do not resolve and
//! reported as "Failed to reach orchestrator".

use std::path::Path;
use std::process::Command;

#[test]
fn bootstrap_python_tests_pass() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/bootstrap_py/test_bootstrap.py");
    let output = Command::new("python3")
        .arg(&tests)
        .arg("-v")
        .env("AEGIS_MODEL_ALIAS", "default")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("python3 must be on PATH to run the bootstrap's tests");

    assert!(
        output.status.success(),
        "bootstrap.py tests failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
