// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Host-aware bootstrap detection (ADR-117 §"Bootstrap on first enrollment").
//!
//! On first enrollment the bootstrap step persists an `aegis-config.yaml`
//! whose `spec.cluster.edge.capabilities` block reflects the actual host. The
//! prior implementation shipped a static `local_tools: [shell]` and
//! `mount_points: ["/"]` regardless of OS, so the daemon advertised
//! capabilities that bore no relation to what the host could actually run.
//!
//! This module owns the detection logic so it can be unit-tested without
//! depending on what is installed on the CI runner — the `which` lookup is
//! injected as a closure.

use std::path::PathBuf;

/// Canonical list of local tools the bootstrap step probes for via
/// `which`. Entries that resolve are included verbatim in
/// `local_tools`; missing entries are silently skipped.
///
/// `shell` is special-cased — it is always included even if `which shell`
/// returns nothing, because every Unix-ish host has `/bin/sh` and the edge
/// daemon's `cmd.run` builtin assumes the operator wants at least one shell
/// dispatcher available. Operators who genuinely have no shell can edit the
/// generated YAML by hand.
pub const PROBE_TOOLS: &[&str] = &[
    "shell",
    "bash",
    "zsh",
    "sh",
    "docker",
    "podman",
    "kubectl",
    "helm",
    "git",
    "curl",
    "jq",
    "terraform",
    "ansible",
];

/// Snapshot of host-aware values written into `aegis-config.yaml` at
/// bootstrap time. Fields map 1:1 to `EdgeCapabilitiesConfig` slots in
/// `orchestrator/core/src/domain/node_config.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDetection {
    pub os: String,
    pub arch: String,
    pub local_tools: Vec<String>,
    pub mount_points: Vec<String>,
}

impl HostDetection {
    /// Detect the current host via `std::env::consts::OS` /
    /// `std::env::consts::ARCH` and the system `which` lookup. Used by the
    /// bootstrap path; tests use [`detect_with`] with an injected lookup so
    /// they don't depend on what is installed on the CI runner.
    pub fn detect() -> Self {
        Self::detect_with(std::env::consts::OS, std::env::consts::ARCH, |tool| {
            which::which(tool).ok()
        })
    }

    /// Test seam — perform host detection with caller-supplied OS/arch and a
    /// pluggable `which` lookup.
    pub fn detect_with<F>(os: &str, arch: &str, which_lookup: F) -> Self
    where
        F: Fn(&str) -> Option<PathBuf>,
    {
        let mut local_tools: Vec<String> = PROBE_TOOLS
            .iter()
            .filter_map(|t| {
                if which_lookup(t).is_some() {
                    Some((*t).to_string())
                } else {
                    None
                }
            })
            .collect();
        // Always include `shell` even if not detected — every Unix-ish host
        // has `/bin/sh`, and the edge daemon's `cmd.run` builtin expects at
        // least one shell dispatcher. De-duplicate so a host where `which
        // shell` resolves does not produce ["shell", "shell"].
        if !local_tools.iter().any(|t| t == "shell") {
            local_tools.insert(0, "shell".to_string());
        }
        let mount_points = default_mount_points_for(os);
        Self {
            os: canonical_os(os),
            arch: arch.to_string(),
            local_tools,
            mount_points,
        }
    }
}

/// Sensible per-OS mount-point defaults.
///
/// The detection runs once at bootstrap time; operators are expected to edit
/// the generated YAML when they want to expose additional paths.
pub fn default_mount_points_for(os: &str) -> Vec<String> {
    match os {
        "linux" => vec![
            "/".to_string(),
            "/home".to_string(),
            "/etc".to_string(),
            "/var/log".to_string(),
        ],
        "macos" => vec![
            "/".to_string(),
            "/Users".to_string(),
            "/Library".to_string(),
            "/var/log".to_string(),
        ],
        "windows" => vec!["C:\\".to_string()],
        // Unknown OS — be conservative and just expose root. The operator
        // will see this in the YAML and adjust.
        _ => vec!["/".to_string()],
    }
}

/// Map `std::env::consts::OS` values to the canonical strings the proto
/// `EdgeCapabilities.os` field documents ("linux", "darwin", "windows"). Rust
/// reports macOS as "macos"; ADR-117 §A documents the wire value as "darwin"
/// — translate here so the operator-visible YAML uses the platform-friendly
/// "macos" but the wire value stays canonical via [`HostDetection::os`].
///
/// We persist the Rust-reported value in YAML (operator-facing) and let the
/// daemon-side `EdgeCapabilities` proto wiring translate at the wire boundary
/// if desired. This keeps the YAML readable; the wire format is internal.
fn canonical_os(os: &str) -> String {
    os.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Helper: produce a `which` lookup that resolves only the tools listed
    /// in `present`. Mirrors how the real `which::which` returns an Ok path.
    fn fake_which(present: &[&str]) -> impl Fn(&str) -> Option<PathBuf> {
        let owned: Vec<String> = present.iter().map(|s| s.to_string()).collect();
        move |tool: &str| {
            if owned.iter().any(|t| t == tool) {
                Some(PathBuf::from(format!("/usr/bin/{tool}")))
            } else {
                None
            }
        }
    }

    #[test]
    fn detect_with_includes_only_present_tools_plus_shell() {
        // Regression for BUG 3: the bootstrap shipped `local_tools: [shell]`
        // unconditionally regardless of host. The fix probes a canonical
        // list via `which` and includes only resolved entries. `shell` is
        // always included (special-cased) even when `which` returns nothing.
        let det = HostDetection::detect_with("linux", "x86_64", fake_which(&["docker", "git"]));
        assert!(det.local_tools.contains(&"docker".to_string()));
        assert!(det.local_tools.contains(&"git".to_string()));
        assert!(det.local_tools.contains(&"shell".to_string()));
        assert!(!det.local_tools.contains(&"kubectl".to_string()));
        assert!(!det.local_tools.contains(&"podman".to_string()));
    }

    #[test]
    fn detect_with_always_includes_shell_when_absent() {
        // `shell` must appear even if `which shell` returns nothing — every
        // Unix-ish host has /bin/sh and the daemon assumes at least one
        // shell dispatcher.
        let det = HostDetection::detect_with("linux", "x86_64", fake_which(&[]));
        assert_eq!(det.local_tools, vec!["shell".to_string()]);
    }

    #[test]
    fn detect_with_does_not_duplicate_shell() {
        // When `which shell` does resolve, the special-case insert must not
        // produce a duplicate entry.
        let det = HostDetection::detect_with("linux", "x86_64", fake_which(&["shell"]));
        let count = det.local_tools.iter().filter(|t| *t == "shell").count();
        assert_eq!(
            count, 1,
            "shell must appear exactly once: {:?}",
            det.local_tools
        );
    }

    #[test]
    fn detect_with_records_os_and_arch_verbatim() {
        let det = HostDetection::detect_with("macos", "aarch64", fake_which(&[]));
        assert_eq!(det.os, "macos");
        assert_eq!(det.arch, "aarch64");
    }

    #[test]
    fn mount_points_linux_defaults() {
        assert_eq!(
            default_mount_points_for("linux"),
            vec![
                "/".to_string(),
                "/home".to_string(),
                "/etc".to_string(),
                "/var/log".to_string(),
            ]
        );
    }

    #[test]
    fn mount_points_macos_defaults() {
        assert_eq!(
            default_mount_points_for("macos"),
            vec![
                "/".to_string(),
                "/Users".to_string(),
                "/Library".to_string(),
                "/var/log".to_string(),
            ]
        );
    }

    #[test]
    fn mount_points_windows_defaults() {
        assert_eq!(
            default_mount_points_for("windows"),
            vec!["C:\\".to_string()]
        );
    }

    #[test]
    fn mount_points_unknown_falls_back_to_root() {
        // An unknown OS string must produce a conservative default rather
        // than panicking — we never want bootstrap to fail just because the
        // operator is on something exotic.
        assert_eq!(default_mount_points_for("haiku"), vec!["/".to_string()]);
    }

    #[test]
    fn probe_tools_includes_canonical_set() {
        // Pin the documented tool set so future additions are deliberate. If
        // this test fails, update both PROBE_TOOLS and this assertion.
        let expected = [
            "shell",
            "bash",
            "zsh",
            "sh",
            "docker",
            "podman",
            "kubectl",
            "helm",
            "git",
            "curl",
            "jq",
            "terraform",
            "ansible",
        ];
        assert_eq!(PROBE_TOOLS, &expected);
    }

    #[test]
    fn detect_with_preserves_probe_order_for_resolved_tools() {
        // The output order should follow PROBE_TOOLS, not the order present
        // in the lookup. This makes the YAML deterministic across runs.
        let det =
            HostDetection::detect_with("linux", "x86_64", fake_which(&["jq", "docker", "git"]));
        // shell is special-cased to position 0 when absent; here `which
        // shell` returns nothing, so it is inserted at index 0. The rest
        // follow PROBE_TOOLS order: docker (index 4), git (8), jq (10).
        let pos = |s: &str| det.local_tools.iter().position(|t| t == s).expect(s);
        assert!(pos("shell") < pos("docker"));
        assert!(pos("docker") < pos("git"));
        assert!(pos("git") < pos("jq"));
    }
}
