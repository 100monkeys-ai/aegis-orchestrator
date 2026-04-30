// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C bootstrap-on-first-enrollment plan.

use anyhow::{Context, Result};
use base64::Engine;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Default)]
pub struct BootstrapPolicy {
    pub non_interactive: bool,
    pub force: bool,
    pub keep_existing: bool,
    pub dry_run: bool,
    pub minimal_config: bool,
}

#[derive(Debug, Clone, Serialize)]
pub enum Action {
    CreateDir { path: PathBuf },
    EnsurePerms { path: PathBuf, mode: u32 },
    GenerateConfig { path: PathBuf, minimal: bool },
    GenerateKeypair { path: PathBuf },
    InstallService { path: PathBuf, kind: ServiceKind },
}

#[derive(Debug, Clone, Serialize)]
pub enum ServiceKind {
    Systemd,
    Launchd,
    Nssm,
}

#[derive(Debug, Clone, Serialize)]
pub struct BootstrapPlan {
    pub state_dir: PathBuf,
    pub actions: Vec<Action>,
    pub controller_endpoint: String,
}

impl BootstrapPlan {
    pub fn detect(
        state_dir: Option<PathBuf>,
        token: &str,
        _policy: &BootstrapPolicy,
    ) -> Result<Self> {
        let dir = state_dir.unwrap_or_else(default_state_dir);
        let cep = extract_cep_claim(token).context("decode enrollment token")?;
        let mut actions = Vec::new();
        if !dir.exists() {
            actions.push(Action::CreateDir { path: dir.clone() });
        }
        actions.push(Action::EnsurePerms {
            path: dir.clone(),
            mode: 0o700,
        });
        let key_path = dir.join("node.key");
        if !key_path.exists() {
            actions.push(Action::GenerateKeypair { path: key_path });
        }
        let cfg_path = dir.join("aegis-config.yaml");
        if !cfg_path.exists() {
            actions.push(Action::GenerateConfig {
                path: cfg_path,
                minimal: false,
            });
        }
        // Note: node.token is intentionally NOT in the bootstrap plan. The
        // bootstrap step only prepares local FS state; the wire-side
        // attest+challenge handshake (see `enroll::run` → `handshake.rs`) is
        // what actually mints the NodeSecurityToken. Persisting `node.token`
        // before the handshake completes would leave a half-written
        // identity on disk that the daemon would happily try to use.
        if cfg!(target_os = "linux") {
            actions.push(Action::InstallService {
                path: systemd_unit_path(),
                kind: ServiceKind::Systemd,
            });
        } else if cfg!(target_os = "macos") {
            actions.push(Action::InstallService {
                path: launchd_plist_path(),
                kind: ServiceKind::Launchd,
            });
        } else if cfg!(target_os = "windows") {
            actions.push(Action::InstallService {
                path: PathBuf::from("aegis-edge.nssm.json"),
                kind: ServiceKind::Nssm,
            });
        }
        Ok(Self {
            state_dir: dir,
            actions,
            controller_endpoint: cep,
        })
    }
}

/// Conflict resolution decision for an existing on-disk artifact during
/// bootstrap. Computed by consulting [`BootstrapPolicy`] before each write
/// site so the operator's `--force` / `--keep-existing` / `--non-interactive`
/// flags actually drive behavior (prior to this wiring the flags were parsed
/// but ignored, so every run silently reused existing artifacts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictResolution {
    /// Use the existing on-disk artifact as-is.
    Reuse,
    /// Replace the existing artifact with a freshly generated one.
    Overwrite,
}

/// Decide what to do when an artifact exists at `path`.
///
/// Decision matrix (per ADR-117 §"Bootstrap on first enrollment"):
///
/// * `--force`           → always Overwrite.
/// * `--keep-existing`   → Reuse if `is_reuse_safe`; error otherwise.
/// * `--non-interactive` (or no flags — interactive TUI is not yet
///   implemented, so the default conservative path is non-interactive) →
///   error: the operator must explicitly choose `--force` or
///   `--keep-existing`. This prevents a silent overwrite or a silent reuse
///   that may not match the operator's intent.
///
/// `--force` and `--keep-existing` are mutually exclusive at the CLI
/// surface; if both are set, `--force` wins (Overwrite is the safer
/// "make-it-match-the-token" outcome).
///
/// TODO(adr-117): interactive prompt path — when the daemon gets an
/// interactive TUI, the no-flags branch will prompt the operator instead
/// of erroring.
pub(crate) fn resolve_conflict(
    policy: &BootstrapPolicy,
    artifact: &str,
    path: &Path,
    is_reuse_safe: impl FnOnce() -> Result<()>,
) -> Result<ConflictResolution> {
    if policy.force {
        return Ok(ConflictResolution::Overwrite);
    }
    if policy.keep_existing {
        is_reuse_safe().with_context(|| {
            format!(
                "--keep-existing requested but reusing {} at {} is unsafe",
                artifact,
                path.display()
            )
        })?;
        return Ok(ConflictResolution::Reuse);
    }
    // Both unset: today we treat this as `--non-interactive` because the
    // interactive prompt path is not implemented (see TODO above). Either
    // way — explicit `--non-interactive` or the implicit default — refuse
    // to silently pick a side. Branch on `non_interactive` so the message
    // names the operator's actual state.
    if policy.non_interactive {
        anyhow::bail!(
            "{} already exists at {}; --non-interactive run cannot prompt — \
             pass --force to overwrite or --keep-existing to reuse",
            artifact,
            path.display()
        )
    } else {
        anyhow::bail!(
            "{} already exists at {}; interactive prompts are not yet implemented \
             — pass --force to overwrite or --keep-existing to reuse",
            artifact,
            path.display()
        )
    }
}

pub async fn run_bootstrap(
    plan: BootstrapPlan,
    policy: &BootstrapPolicy,
    output: OutputFormat,
    token: &str,
) -> Result<()> {
    if policy.dry_run {
        match output {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&plan)?),
            OutputFormat::Text => {
                println!("dry-run: BootstrapPlan");
                println!("  state_dir: {}", plan.state_dir.display());
                println!("  controller_endpoint: {}", plan.controller_endpoint);
                for a in &plan.actions {
                    println!("  - {a:?}");
                }
            }
        }
        return Ok(());
    }

    fs::create_dir_all(&plan.state_dir)
        .with_context(|| format!("create state dir {}", plan.state_dir.display()))?;
    set_dir_perms(&plan.state_dir, 0o700)?;

    let key_path = plan.state_dir.join("node.key");
    if key_path.exists() {
        let key_path_for_check = key_path.clone();
        let resolution = resolve_conflict(policy, "node.key", &key_path, || {
            // Reuse-safety check: the existing key file must be a valid
            // 32-byte Ed25519 seed. A truncated / corrupt file would cause
            // the daemon to fail at every connect — error early instead.
            let bytes = fs::read(&key_path_for_check)
                .with_context(|| format!("read existing {}", key_path_for_check.display()))?;
            if bytes.len() != 32 {
                anyhow::bail!(
                    "existing node.key is {} bytes, expected 32 (Ed25519 seed)",
                    bytes.len()
                );
            }
            Ok(())
        })?;
        if resolution == ConflictResolution::Overwrite {
            generate_ed25519_keypair(&key_path)?;
        }
    } else {
        generate_ed25519_keypair(&key_path)?;
    }

    // Persist the enrollment token.
    let token_path = plan.state_dir.join("enrollment.jwt");
    fs::write(&token_path, token).with_context(|| format!("write {}", token_path.display()))?;
    set_file_perms(&token_path, 0o600)?;

    // Emit a minimal or annotated config if missing — or, if present,
    // consult policy to decide whether to overwrite or reuse.
    let cfg_path = plan.state_dir.join("aegis-config.yaml");
    let should_write_cfg = if cfg_path.exists() {
        let cfg_path_for_check = cfg_path.clone();
        let expected_endpoint = plan.controller_endpoint.clone();
        let resolution = resolve_conflict(policy, "aegis-config.yaml", &cfg_path, || {
            // Reuse-safety check: the existing config's controller endpoint
            // must match the `cep` claim of the new enrollment token.
            // Reusing a config that points at a different controller would
            // mean the daemon connects to one cluster while the operator
            // believes it enrolled into another — silent split-brain.
            let body = fs::read_to_string(&cfg_path_for_check)
                .with_context(|| format!("read {}", cfg_path_for_check.display()))?;
            let doc: serde_yaml::Value = serde_yaml::from_str(&body)
                .with_context(|| format!("parse {}", cfg_path_for_check.display()))?;
            let existing_ep = doc
                .get("spec")
                .and_then(|v| v.get("cluster"))
                .and_then(|v| v.get("controller"))
                .and_then(|v| v.get("endpoint"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if existing_ep != expected_endpoint {
                anyhow::bail!(
                    "existing aegis-config.yaml controller endpoint '{}' does not \
                     match enrollment token endpoint '{}'",
                    existing_ep,
                    expected_endpoint
                );
            }
            Ok(())
        })?;
        resolution == ConflictResolution::Overwrite
    } else {
        true
    };
    if should_write_cfg {
        let body = if policy.minimal_config {
            include_str!("../../../templates/edge-config-minimal.yaml")
        } else {
            include_str!("../../../templates/edge-config-with-examples.yaml")
        };
        let body = body.replace("{{CONTROLLER_ENDPOINT}}", &plan.controller_endpoint);
        fs::write(&cfg_path, body).with_context(|| format!("write {}", cfg_path.display()))?;
        set_file_perms(&cfg_path, 0o600)?;
    }

    match output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "state_dir": plan.state_dir,
                    "controller_endpoint": plan.controller_endpoint,
                })
            );
        }
        OutputFormat::Text => {
            println!("aegis edge: bootstrap complete");
            println!("  state_dir: {}", plan.state_dir.display());
            println!("  controller_endpoint: {}", plan.controller_endpoint);
        }
    }
    Ok(())
}

fn default_state_dir() -> PathBuf {
    if let Some(home) = dirs_next::home_dir() {
        home.join(".aegis").join("edge")
    } else {
        PathBuf::from(".aegis/edge")
    }
}

fn systemd_unit_path() -> PathBuf {
    if let Some(home) = dirs_next::home_dir() {
        home.join(".config")
            .join("systemd")
            .join("user")
            .join("aegis-edge.service")
    } else {
        PathBuf::from("aegis-edge.service")
    }
}

fn launchd_plist_path() -> PathBuf {
    if let Some(home) = dirs_next::home_dir() {
        home.join("Library")
            .join("LaunchAgents")
            .join("io.aegis.edge.plist")
    } else {
        PathBuf::from("io.aegis.edge.plist")
    }
}

fn extract_cep_claim(token: &str) -> Result<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        anyhow::bail!("invalid token format");
    }
    let claims_b = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .context("decode claims")?;
    #[derive(serde::Deserialize)]
    struct C {
        cep: String,
    }
    let c: C = serde_json::from_slice(&claims_b).context("parse claims")?;
    Ok(c.cep)
}

fn generate_ed25519_keypair(path: &Path) -> Result<()> {
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let secret_bytes = signing_key.to_bytes();
    fs::write(path, secret_bytes).with_context(|| format!("write key to {}", path.display()))?;
    set_file_perms(path, 0o600)?;
    let pub_bytes = signing_key.verifying_key().to_bytes();
    let pub_path = path.with_extension("pub");
    fs::write(&pub_path, pub_bytes)
        .with_context(|| format!("write pubkey to {}", pub_path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_file_perms(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(mode);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(unix)]
fn set_dir_perms(path: &Path, mode: u32) -> Result<()> {
    set_file_perms(path, mode)
}

#[cfg(not(unix))]
fn set_file_perms(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn set_dir_perms(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod policy_tests {
    //! Regression coverage for the BootstrapPolicy flag wiring. Prior to this
    //! fix, `non_interactive`, `force`, and `keep_existing` were parsed by
    //! clap into `BootstrapPolicy` but never read by the bootstrap-plan
    //! execution. Operators driving CI / config-management runs silently got
    //! the implicit-reuse default regardless of the flags they passed. These
    //! tests pin each flag's contribution to the conflict-resolution decision.
    use super::*;
    use base64::Engine;
    use std::fs;

    fn make_token(cep: &str) -> String {
        // Build a minimal JWT-shaped string whose claims segment carries the
        // `cep` claim consumed by `extract_cep_claim`. Signature segment is
        // a placeholder — bootstrap never verifies it locally.
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let claims_json = format!("{{\"cep\":\"{cep}\"}}");
        let claims =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims_json.as_bytes());
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header}.{claims}.{sig}")
    }

    fn write_existing_key(dir: &Path, bytes: &[u8]) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("node.key"), bytes).unwrap();
    }

    fn write_existing_config(dir: &Path, endpoint: &str) {
        fs::create_dir_all(dir).unwrap();
        let body = format!("spec:\n  cluster:\n    controller:\n      endpoint: \"{endpoint}\"\n");
        fs::write(dir.join("aegis-config.yaml"), body).unwrap();
    }

    fn run_with_policy(dir: &Path, policy: BootstrapPolicy, token: &str) -> Result<()> {
        let plan = BootstrapPlan::detect(Some(dir.to_path_buf()), token, &policy)?;
        // tokio runtime to drive the async fn — keep the test body
        // synchronous-flavored by blocking here.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_bootstrap(plan, &policy, OutputFormat::Text, token))
    }

    #[test]
    fn force_overwrites_existing_node_key() {
        // Regression: prior code silently reused any node.key on disk because
        // the `--force` flag was never read. With wiring, a 32-byte placeholder
        // key must be replaced by a fresh one.
        let tmp = tempfile::tempdir().unwrap();
        let placeholder = [0u8; 32];
        write_existing_key(tmp.path(), &placeholder);
        let policy = BootstrapPolicy {
            force: true,
            ..Default::default()
        };
        run_with_policy(tmp.path(), policy, &make_token("controller.example:443"))
            .expect("--force must succeed when artifacts exist");
        let after = fs::read(tmp.path().join("node.key")).unwrap();
        assert_eq!(after.len(), 32);
        assert_ne!(after, placeholder, "--force must replace the existing key");
    }

    #[test]
    fn keep_existing_reuses_valid_node_key() {
        // Regression: prior code silently reused any node.key. With wiring,
        // explicit `--keep-existing` is now the path that opts into reuse —
        // and the safety check (32 bytes) must pass.
        let tmp = tempfile::tempdir().unwrap();
        let placeholder = [42u8; 32];
        write_existing_key(tmp.path(), &placeholder);
        let policy = BootstrapPolicy {
            keep_existing: true,
            ..Default::default()
        };
        run_with_policy(tmp.path(), policy, &make_token("controller.example:443"))
            .expect("--keep-existing must accept a valid 32-byte node.key");
        let after = fs::read(tmp.path().join("node.key")).unwrap();
        assert_eq!(
            after, placeholder,
            "--keep-existing must NOT modify the key"
        );
    }

    #[test]
    fn keep_existing_errors_on_unsafe_reuse() {
        // A node.key that is not 32 bytes is not a valid Ed25519 seed.
        // Reusing it would mean the daemon fails at every connect.
        // `--keep-existing` must refuse this rather than silently keep it.
        let tmp = tempfile::tempdir().unwrap();
        let truncated = [1u8; 16]; // wrong length on purpose
        write_existing_key(tmp.path(), &truncated);
        let policy = BootstrapPolicy {
            keep_existing: true,
            ..Default::default()
        };
        let err = run_with_policy(tmp.path(), policy, &make_token("controller.example:443"))
            .expect_err("--keep-existing must reject a corrupt key");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsafe") || msg.contains("32") || msg.contains("Ed25519"),
            "error must surface the unsafe-reuse reason: {msg}"
        );
    }

    #[test]
    fn keep_existing_errors_on_controller_endpoint_mismatch() {
        // Reuse-safety: an existing aegis-config.yaml whose controller
        // endpoint does NOT match the new enrollment token's `cep` claim
        // would silently split-brain the daemon (it would connect to the
        // old cluster while the operator believed enrollment moved it).
        let tmp = tempfile::tempdir().unwrap();
        write_existing_key(tmp.path(), &[7u8; 32]);
        write_existing_config(tmp.path(), "old-controller.example:443");
        let policy = BootstrapPolicy {
            keep_existing: true,
            ..Default::default()
        };
        let err = run_with_policy(
            tmp.path(),
            policy,
            &make_token("new-controller.example:443"),
        )
        .expect_err("--keep-existing must reject a mismatched controller endpoint");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not") && msg.contains("match") && msg.contains("endpoint"),
            "error must name the endpoint mismatch: {msg}"
        );
    }

    #[test]
    fn non_interactive_without_force_or_keep_errors_on_conflict() {
        // Regression: prior code silently reused. With wiring,
        // `--non-interactive` (without `--force` or `--keep-existing`) must
        // error rather than silently picking a side.
        let tmp = tempfile::tempdir().unwrap();
        write_existing_key(tmp.path(), &[0u8; 32]);
        let policy = BootstrapPolicy {
            non_interactive: true,
            ..Default::default()
        };
        let err = run_with_policy(tmp.path(), policy, &make_token("controller.example:443"))
            .expect_err("--non-interactive must refuse to silently resolve a conflict");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-interactive")
                || msg.contains("--force")
                || msg.contains("--keep-existing"),
            "error must guide the operator to a flag: {msg}"
        );
    }

    #[test]
    fn no_flags_errors_on_conflict_until_interactive_tui_lands() {
        // With no flags set at all, the implicit default is conservative:
        // refuse to silently overwrite or reuse. This will change once the
        // interactive TUI is implemented (see TODO(adr-117) in resolve_conflict).
        let tmp = tempfile::tempdir().unwrap();
        write_existing_key(tmp.path(), &[0u8; 32]);
        let policy = BootstrapPolicy::default();
        let err = run_with_policy(tmp.path(), policy, &make_token("controller.example:443"))
            .expect_err("default policy must refuse silent conflict resolution");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("interactive")
                || msg.contains("--force")
                || msg.contains("--keep-existing"),
            "default-flag error must mention available flags: {msg}"
        );
    }

    #[test]
    fn fresh_state_dir_succeeds_with_default_policy() {
        // Sanity: when there is NO conflict, the default policy must still
        // succeed. The flags only kick in when an artifact exists on disk.
        let tmp = tempfile::tempdir().unwrap();
        let policy = BootstrapPolicy::default();
        run_with_policy(tmp.path(), policy, &make_token("controller.example:443"))
            .expect("fresh state_dir must bootstrap with default policy");
        // Generated artifacts must be present.
        assert!(tmp.path().join("node.key").exists());
        assert!(tmp.path().join("aegis-config.yaml").exists());
    }
}
