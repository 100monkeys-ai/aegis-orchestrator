// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis edge logout` — revoke server-side and remove the entire local
//! edge state directory.
//!
//! Logout means logout: the whole `~/.aegis/edge` directory (or the override
//! supplied via `--state-dir`) is removed, not just a hand-picked subset of
//! credential files. Leaving `aegis-config.yaml` or `node.pub` behind would
//! pin the next enrollment to the previous (orphaned) `spec.node.id`, which
//! defeats the purpose of logging out.
//!
//! Server-side revocation is best-effort: if an operator profile is
//! configured (i.e. `EdgeApiClient::from_env` resolves), this command
//! first issues `DELETE /v1/edge/hosts/{node_id}` before clearing local
//! state. When no operator credentials are available, or the request
//! fails for a reason other than 404, the user must opt-in (`--force`)
//! to skip the server hop and proceed with local-only cleanup. The
//! orchestrator's REST surface requires a `UserIdentity` (Keycloak JWT)
//! for `/v1/edge/*`, so a daemon authenticating only with its own
//! NodeSecurityToken cannot self-revoke today — see the docs for the
//! operator-side path.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};

use super::client::EdgeApiClient;
use super::grpc;
use super::service::{self as svc, ServiceManager, UninstallOutcome};

#[derive(Debug, Args, Default)]
pub struct LogoutArgs {
    /// Override the local edge state directory (default: `~/.aegis/edge`).
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// Skip the server-side revoke hop and only clear local state.
    #[arg(long)]
    pub no_server: bool,
    /// Proceed with local cleanup even when the server-side revoke fails
    /// for a reason other than 404 (already revoked).
    #[arg(long)]
    pub force: bool,
    /// Skip the OS service uninstall step. By default `aegis edge logout`
    /// stops + removes the supervisor unit so the daemon does not get
    /// relaunched against the now-empty state directory.
    #[arg(long)]
    pub no_service: bool,
}

pub async fn run(args: LogoutArgs) -> Result<()> {
    let state_dir = args.state_dir.clone().unwrap_or_else(default_state_dir);

    // Server-side revoke (best-effort; gated by --no-server and --force).
    if !args.no_server && state_dir.exists() {
        match revoke_server_side(&state_dir).await {
            ServerRevokeOutcome::Skipped(reason) => {
                println!("aegis edge logout: skipping server revoke ({reason})");
            }
            ServerRevokeOutcome::Succeeded(node_id) => {
                println!(
                    "aegis edge logout: server revoke succeeded for node {node_id}; \
                     proceeding to local cleanup"
                );
            }
            ServerRevokeOutcome::AlreadyGone(node_id) => {
                println!(
                    "aegis edge logout: server already has no record of node \
                     {node_id}; proceeding to local cleanup"
                );
            }
            ServerRevokeOutcome::Failed { node_id, error } => {
                eprintln!("aegis edge logout: server revoke for node {node_id} failed: {error}");
                if !args.force {
                    bail!(
                        "refusing to clear local state while server still believes \
                         the daemon is enrolled. Re-run with --force to skip the \
                         server hop, or revoke from Zaru's hosts list and retry."
                    );
                }
                eprintln!(
                    "aegis edge logout: --force supplied; proceeding with local-only cleanup"
                );
            }
        }
    }

    // Stop + uninstall the supervisor unit BEFORE wiping local state, so the
    // service doesn't restart against half-deleted state. Failure of the
    // uninstall is informational — it must not block local cleanup
    // (logout means logout). Honor `--no-service` for users who manage the
    // supervisor outside of the `aegis` CLI.
    if !args.no_service {
        match svc::detect_service_manager() {
            Ok(mgr) => match uninstall_service_for_logout(&*mgr) {
                Ok(UninstallOutcome::Removed { unit_path }) => {
                    println!(
                        "aegis edge logout: removed service unit at {}",
                        unit_path.display()
                    );
                }
                Ok(UninstallOutcome::NotInstalled) => {
                    println!("aegis edge logout: no service unit installed; skipping uninstall");
                }
                Err(e) => {
                    eprintln!(
                        "aegis edge logout: service uninstall failed ({e:#}); \
                         continuing with local cleanup"
                    );
                }
            },
            Err(e) => {
                eprintln!(
                    "aegis edge logout: no supported service supervisor on this host \
                     ({e:#}); skipping uninstall"
                );
            }
        }
    }

    let message = logout(&state_dir)?;
    println!("{message}");
    Ok(())
}

/// Test-friendly seam: drives the service uninstall against the supplied
/// `ServiceManager`. Tests pass a mock; production invokes via
/// `svc::detect_service_manager`.
pub(crate) fn uninstall_service_for_logout(mgr: &dyn ServiceManager) -> Result<UninstallOutcome> {
    svc::uninstall(mgr)
}

/// Outcome of the optional server-side revoke hop.
#[derive(Debug)]
pub(crate) enum ServerRevokeOutcome {
    /// No server hop attempted (no node id, no credentials, etc.).
    Skipped(String),
    Succeeded(String),
    AlreadyGone(String),
    Failed {
        node_id: String,
        error: String,
    },
}

async fn revoke_server_side(state_dir: &Path) -> ServerRevokeOutcome {
    // Resolve node id from the persisted aegis-config.yaml. A missing /
    // malformed config means we have nothing to revoke against the
    // orchestrator — proceed straight to local cleanup.
    let node_id = match grpc::load_node_id_from_config(state_dir) {
        Ok(id) => id,
        Err(e) => {
            return ServerRevokeOutcome::Skipped(format!(
                "no usable node id in aegis-config.yaml: {e}"
            ));
        }
    };

    // The orchestrator REST surface authenticates via Keycloak operator
    // identity; the daemon's NodeSecurityToken is NOT accepted here. We
    // reuse the standard operator-profile resolution (auth.json + env
    // vars) so a logged-in operator on the same machine can drive the
    // revoke. When that resolution fails, surface the reason and skip.
    let client = match EdgeApiClient::from_env() {
        Ok(c) => c,
        Err(e) => {
            return ServerRevokeOutcome::Skipped(format!(
                "no operator credentials available ({e}); revoke from Zaru's \
                 hosts list to disable the daemon server-side"
            ));
        }
    };

    revoke_via_client(&client, &node_id).await
}

/// Test-friendly seam: drives the actual REST DELETE against the supplied
/// `EdgeApiClient`. Test code provides a client pointing at an in-process
/// mock orchestrator (see `logout_calls_delete_against_orchestrator` in the test module).
pub(crate) async fn revoke_via_client(
    client: &EdgeApiClient,
    node_id: &str,
) -> ServerRevokeOutcome {
    match client.delete(&format!("/v1/edge/hosts/{node_id}")).await {
        Ok(()) => ServerRevokeOutcome::Succeeded(node_id.to_string()),
        Err(e) => {
            let s = e.to_string();
            // EdgeApiClient::delete surfaces the upstream status code in
            // the error message; a 404 means the row is already gone.
            if s.contains(" 404 ") || s.starts_with("404") {
                ServerRevokeOutcome::AlreadyGone(node_id.to_string())
            } else {
                ServerRevokeOutcome::Failed {
                    node_id: node_id.to_string(),
                    error: s,
                }
            }
        }
    }
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

    /// Regression: `aegis edge logout` must call
    /// `DELETE /v1/edge/hosts/{node_id}` against the orchestrator before
    /// clearing local state. Prior to this fix the command was local-only
    /// and the orchestrator was left believing the daemon was still
    /// enrolled — which kept dispatching to a host that no longer existed
    /// and required an out-of-band revoke from Zaru's hosts list.
    #[tokio::test]
    async fn logout_calls_delete_against_orchestrator() {
        let mut server = mockito::Server::new_async().await;
        let node_uuid = uuid::Uuid::new_v4().to_string();
        let m = server
            .mock("DELETE", format!("/v1/edge/hosts/{node_uuid}").as_str())
            .with_status(204)
            .expect(1)
            .create_async()
            .await;

        let client = EdgeApiClient::new_for_test(server.url());
        let outcome = revoke_via_client(&client, &node_uuid).await;
        assert!(
            matches!(outcome, ServerRevokeOutcome::Succeeded(ref n) if n == &node_uuid),
            "expected Succeeded({node_uuid}), got {outcome:?}"
        );
        m.assert_async().await;
    }

    /// Regression: a 404 from the orchestrator (the row is already gone)
    /// is NOT a logout failure — the local state should still be cleared.
    #[tokio::test]
    async fn logout_treats_404_as_already_gone() {
        let mut server = mockito::Server::new_async().await;
        let node_uuid = uuid::Uuid::new_v4().to_string();
        let m = server
            .mock("DELETE", format!("/v1/edge/hosts/{node_uuid}").as_str())
            .with_status(404)
            .with_body(r#"{"error":"not_found"}"#)
            .create_async()
            .await;

        let client = EdgeApiClient::new_for_test(server.url());
        let outcome = revoke_via_client(&client, &node_uuid).await;
        assert!(
            matches!(outcome, ServerRevokeOutcome::AlreadyGone(ref n) if n == &node_uuid),
            "expected AlreadyGone({node_uuid}), got {outcome:?}"
        );
        m.assert_async().await;
    }

    /// Regression: any non-404 server error must surface as Failed so the
    /// outer `run` can refuse to proceed unless --force is supplied.
    #[tokio::test]
    async fn logout_surfaces_non_404_server_error_as_failed() {
        let mut server = mockito::Server::new_async().await;
        let node_uuid = uuid::Uuid::new_v4().to_string();
        let _m = server
            .mock("DELETE", format!("/v1/edge/hosts/{node_uuid}").as_str())
            .with_status(500)
            .with_body(r#"{"error":"internal"}"#)
            .create_async()
            .await;

        let client = EdgeApiClient::new_for_test(server.url());
        let outcome = revoke_via_client(&client, &node_uuid).await;
        match outcome {
            ServerRevokeOutcome::Failed { node_id, error } => {
                assert_eq!(node_id, node_uuid);
                assert!(
                    error.contains("500"),
                    "error should mention status: {error}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Service uninstall on logout (regression for the missing wiring).
    // -----------------------------------------------------------------

    use super::svc::{
        InstallOutcome, ServiceKind, ServiceManager as SvcMgr, ServiceState, ServiceStatus,
        UninstallOutcome,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    struct CountingMgr {
        installed: Rc<RefCell<bool>>,
        uninstall_calls: Rc<RefCell<usize>>,
        unit_path: PathBuf,
    }

    impl CountingMgr {
        fn new(installed: bool, unit_path: PathBuf) -> Self {
            Self {
                installed: Rc::new(RefCell::new(installed)),
                uninstall_calls: Rc::new(RefCell::new(0)),
                unit_path,
            }
        }
    }

    impl SvcMgr for CountingMgr {
        fn kind(&self) -> ServiceKind {
            ServiceKind::SystemdUser
        }
        fn unit_name(&self) -> &str {
            "test"
        }
        fn unit_path(&self) -> PathBuf {
            self.unit_path.clone()
        }
        fn install(&self, _content: &str, _f: bool, _k: bool) -> Result<InstallOutcome> {
            *self.installed.borrow_mut() = true;
            Ok(InstallOutcome::Installed {
                unit_path: self.unit_path.clone(),
            })
        }
        fn uninstall(&self) -> Result<UninstallOutcome> {
            *self.uninstall_calls.borrow_mut() += 1;
            if !*self.installed.borrow() {
                return Ok(UninstallOutcome::NotInstalled);
            }
            *self.installed.borrow_mut() = false;
            Ok(UninstallOutcome::Removed {
                unit_path: self.unit_path.clone(),
            })
        }
        fn status(&self) -> Result<ServiceStatus> {
            Ok(ServiceStatus {
                state: ServiceState::Active,
                pid: None,
                uptime: None,
                recent_logs: Vec::new(),
            })
        }
        fn restart(&self) -> Result<()> {
            Ok(())
        }
        fn logs(&self, _f: bool, _l: usize) -> Result<()> {
            Ok(())
        }
    }

    /// Regression: prior to the wiring, `aegis edge logout` left the
    /// supervisor unit installed and running. The supervisor would then try
    /// to restart against the now-empty state directory and emit error spam.
    /// `uninstall_service_for_logout` is the seam the logout flow drives;
    /// asserting it removes when installed and is idempotent when not.
    #[test]
    fn logout_invokes_service_uninstall_when_installed() {
        let mgr = CountingMgr::new(
            /*installed=*/ true,
            PathBuf::from("/tmp/aegis-edge.service"),
        );
        let outcome =
            uninstall_service_for_logout(&mgr).expect("uninstall must not error in happy path");
        assert!(matches!(outcome, UninstallOutcome::Removed { .. }));
        assert_eq!(*mgr.uninstall_calls.borrow(), 1);
        assert!(!*mgr.installed.borrow());
    }

    #[test]
    fn logout_service_uninstall_is_idempotent_when_not_installed() {
        let mgr = CountingMgr::new(
            /*installed=*/ false,
            PathBuf::from("/tmp/aegis-edge.service"),
        );
        let outcome = uninstall_service_for_logout(&mgr).expect("not-installed must not error");
        assert_eq!(outcome, UninstallOutcome::NotInstalled);
        assert_eq!(*mgr.uninstall_calls.borrow(), 1);
    }
}
