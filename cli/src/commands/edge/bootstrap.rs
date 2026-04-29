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
    WriteToken { path: PathBuf },
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
        actions.push(Action::WriteToken {
            path: dir.join("node.token"),
        });
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
    if !key_path.exists() {
        generate_ed25519_keypair(&key_path)?;
    }

    // Persist the enrollment token.
    let token_path = plan.state_dir.join("enrollment.jwt");
    fs::write(&token_path, token).with_context(|| format!("write {}", token_path.display()))?;
    set_file_perms(&token_path, 0o600)?;

    // Emit a minimal or annotated config if missing.
    let cfg_path = plan.state_dir.join("aegis-config.yaml");
    if !cfg_path.exists() {
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
