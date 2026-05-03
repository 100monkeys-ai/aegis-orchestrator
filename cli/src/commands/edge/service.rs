// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis edge service` — install / uninstall / status / restart / logs for the
//! OS-level service unit that supervises the long-lived `aegis edge daemon`.
//!
//! The same install logic is invoked by `aegis edge enroll` after a successful
//! handshake (see `enroll::install_service_after_enroll`). Driving install
//! through this module keeps the persistence step in one place — the bootstrap
//! plan only writes filesystem state, while *enabling and starting* the
//! supervisor lives here.
//!
//! ## Trait-based seam
//!
//! `ServiceManager` is the seam unit tests poke. Production binds to one of
//! `SystemdUserManager` / `LaunchdManager` / `NssmManager`; tests bind
//! to a `MockServiceManager` that records invocations without spawning shell
//! commands. This is how we cover install / uninstall / status without
//! requiring systemd, launchd, or NSSM to be present in CI.

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use std::path::PathBuf;
use std::process::Command;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Stdio;

const SYSTEMD_SERVICE_TEMPLATE: &str = include_str!("../../../templates/aegis-edge.service");
const LAUNCHD_PLIST_TEMPLATE: &str = include_str!("../../../templates/io.aegis.edge.plist");
const NSSM_TEMPLATE: &str = include_str!("../../../templates/aegis-edge.nssm.json");

/// systemd unit name used for the user-scoped `aegis-edge` service.
pub const SYSTEMD_UNIT: &str = "aegis-edge.service";
/// launchd label used for the macOS LaunchAgent. Referenced by the
/// platform-gated `LaunchctlServiceManager` only — unused on Linux/Windows.
#[allow(dead_code)]
pub const LAUNCHD_LABEL: &str = "io.aegis.edge";
/// NSSM service name used on Windows. Referenced by the platform-gated
/// `NssmServiceManager` only — unused on Linux/macOS.
#[allow(dead_code)]
pub const NSSM_SERVICE_NAME: &str = "AegisEdge";

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Install + enable + start the OS service unit. Idempotent.
    Install(InstallArgs),
    /// Stop, disable, and remove the OS service unit. Idempotent.
    Uninstall(UninstallArgs),
    /// Print active/inactive/failed state, PID, uptime, and a recent log tail.
    Status(StatusArgs),
    /// Restart the OS service unit.
    Restart(RestartArgs),
    /// Tail or stream the OS service unit's logs.
    Logs(LogsArgs),
}

#[derive(Debug, Args, Default)]
pub struct InstallArgs {
    /// Replace any existing service unit on disk before enabling.
    #[arg(long)]
    pub force: bool,
    /// Skip the install if the service unit is already on disk.
    #[arg(long, conflicts_with = "force")]
    pub keep_existing: bool,
    /// Disable interactive prompts for conflict resolution.
    #[arg(long)]
    pub non_interactive: bool,
}

#[derive(Debug, Args, Default)]
pub struct UninstallArgs {}

#[derive(Debug, Args, Default)]
pub struct StatusArgs {}

#[derive(Debug, Args, Default)]
pub struct RestartArgs {}

#[derive(Debug, Args, Default)]
pub struct LogsArgs {
    /// Stream logs as they arrive (like `tail -f` / `journalctl -f`).
    #[arg(long)]
    pub follow: bool,
    /// Number of recent lines to show before any tail-follow output starts.
    #[arg(long, default_value_t = 200)]
    pub lines: usize,
}

pub async fn run(cmd: ServiceCommand) -> Result<()> {
    let mgr = detect_service_manager()?;
    match cmd {
        ServiceCommand::Install(args) => {
            let outcome = install(&*mgr, &args)?;
            print_install_outcome(mgr.kind(), &outcome);
            Ok(())
        }
        ServiceCommand::Uninstall(_) => {
            let outcome = uninstall(&*mgr)?;
            print_uninstall_outcome(mgr.kind(), &outcome);
            Ok(())
        }
        ServiceCommand::Status(_) => {
            let s = mgr.status().context("query service status")?;
            print_status(mgr.kind(), &s);
            Ok(())
        }
        ServiceCommand::Restart(_) => {
            mgr.restart().context("restart service")?;
            println!("aegis edge service: restarted {}", mgr.unit_name());
            Ok(())
        }
        ServiceCommand::Logs(args) => mgr.logs(args.follow, args.lines).context("stream logs"),
    }
}

// ---------------------------------------------------------------------------
// ServiceManager trait + outcomes
// ---------------------------------------------------------------------------

/// Service unit kind detected at runtime. The `Launchd` and `Nssm` variants
/// are constructed only on macOS and Windows respectively; on other targets
/// they are dead but must remain present so the same `ServiceKind` type
/// covers all platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ServiceKind {
    SystemdUser,
    Launchd,
    Nssm,
}

impl ServiceKind {
    pub fn label(self) -> &'static str {
        match self {
            ServiceKind::SystemdUser => "systemd --user",
            ServiceKind::Launchd => "launchd",
            ServiceKind::Nssm => "nssm",
        }
    }
}

/// Lifecycle states a service unit can report. The aim is a small, OS-agnostic
/// surface so callers (and tests) can reason without parsing platform-specific
/// strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    /// Service is installed AND running.
    Active,
    /// Service is installed AND stopped (cleanly).
    Inactive,
    /// Service is installed AND failed (non-zero exit / restart loop).
    Failed,
    /// Service unit is not installed at all.
    NotInstalled,
}

#[derive(Debug, Clone)]
pub struct ServiceStatus {
    pub state: ServiceState,
    pub pid: Option<u32>,
    /// Free-form uptime string (e.g. "1h 22min" from systemd) — opaque to the caller.
    pub uptime: Option<String>,
    /// Recent log tail (best-effort; may be empty on platforms where the
    /// status query and the log query are entirely disjoint).
    pub recent_logs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Installed (or replaced) and started.
    Installed { unit_path: PathBuf },
    /// Already on disk, identical content — no-op.
    AlreadyInstalled { unit_path: PathBuf },
    /// Already on disk; refused to overwrite without `--force`.
    Conflict { unit_path: PathBuf, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    Removed { unit_path: PathBuf },
    NotInstalled,
}

/// OS-agnostic surface for service unit installation / lifecycle / log access.
///
/// Each platform impl wraps a single supervisor (systemd-user, launchd, NSSM).
/// The trait is the seam tests poke — `MockServiceManager` records calls
/// without spawning subprocesses. See `tests` module below.
#[allow(dead_code)]
pub trait ServiceManager {
    fn kind(&self) -> ServiceKind;
    fn unit_name(&self) -> &str;
    fn unit_path(&self) -> PathBuf;
    /// Install (write file, daemon-reload, enable, start). Returns the
    /// outcome so the CLI can surface whether anything changed on disk.
    fn install(&self, content: &str, force: bool, keep_existing: bool) -> Result<InstallOutcome>;
    fn uninstall(&self) -> Result<UninstallOutcome>;
    fn status(&self) -> Result<ServiceStatus>;
    fn restart(&self) -> Result<()>;
    /// Stream/tail logs to stdout. `follow` requests a tail-follow stream;
    /// otherwise prints `lines` of recent history and returns.
    fn logs(&self, follow: bool, lines: usize) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Platform-detection entry point
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub fn detect_service_manager() -> Result<Box<dyn ServiceManager>> {
    Ok(Box::new(SystemdUserManager::new()?))
}

#[cfg(target_os = "macos")]
pub fn detect_service_manager() -> Result<Box<dyn ServiceManager>> {
    Ok(Box::new(LaunchdManager::new()?))
}

#[cfg(target_os = "windows")]
pub fn detect_service_manager() -> Result<Box<dyn ServiceManager>> {
    Ok(Box::new(NssmManager::new()?))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn detect_service_manager() -> Result<Box<dyn ServiceManager>> {
    bail!("aegis edge service: unsupported platform — no service manager known for this OS")
}

/// Alias kept for the post-enroll install path so changes to detection logic
/// are confined to this module. The enroll path uses this name to make the
/// call site self-documenting.
pub fn detect_service_manager_for_install() -> Result<Box<dyn ServiceManager>> {
    detect_service_manager()
}

// ---------------------------------------------------------------------------
// Template substitution
// ---------------------------------------------------------------------------

/// Substitute `{{BINARY_PATH}}` (and `{{HOME}}` for the launchd plist) in the
/// service unit template, using the currently-running `aegis` binary path so
/// the unit launches the right binary across reinstalls.
///
/// For JSON-bodied templates (NSSM), naive string substitution would inject
/// raw Windows paths like `C:\Program Files\Aegis\aegis.exe` whose backslashes
/// the JSON parser then rejects. We detect a JSON template by parsing it and,
/// on success, rewrite the placeholder fields via `serde_json` so the binary
/// path is emitted as a properly-escaped JSON string. Non-JSON templates
/// (systemd unit, launchd plist) keep the literal string substitution path.
pub fn render_unit_template(template: &str, binary_path: &str, home_dir: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(template) {
        substitute_json_placeholders(&mut value, binary_path, home_dir);
        // `serde_json::to_string_pretty` escapes embedded backslashes/quotes
        // in `binary_path` automatically, so the rendered JSON is always
        // round-trippable regardless of the OS path conventions.
        return serde_json::to_string_pretty(&value)
            .expect("serializing a serde_json::Value cannot fail");
    }
    template
        .replace("{{BINARY_PATH}}", binary_path)
        .replace("{{HOME}}", home_dir)
}

/// Walk a `serde_json::Value` tree and replace `{{BINARY_PATH}}` / `{{HOME}}`
/// occurrences inside any string leaf with the resolved values. Only string
/// leaves are touched — keys, numbers, booleans, etc. are passed through.
fn substitute_json_placeholders(value: &mut serde_json::Value, binary_path: &str, home_dir: &str) {
    match value {
        serde_json::Value::String(s) => {
            // Whole-string replacements (the common case for `binary_path`)
            // and embedded substitutions both work — `String::replace` is a
            // substring rewrite.
            *s = s
                .replace("{{BINARY_PATH}}", binary_path)
                .replace("{{HOME}}", home_dir);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                substitute_json_placeholders(item, binary_path, home_dir);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                substitute_json_placeholders(v, binary_path, home_dir);
            }
        }
        _ => {}
    }
}

/// Resolve the path to the currently-running `aegis` binary so the service
/// unit launches the same binary that ran `aegis edge enroll`. Falls back to
/// the bare name `"aegis"` (resolved by PATH at service start) when the
/// running-executable path is unavailable.
pub fn current_binary_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "aegis".to_string())
}

fn home_dir_string() -> String {
    dirs_next::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Templates by ServiceKind
// ---------------------------------------------------------------------------

/// Pick the correct unit template for a `ServiceKind`. Public so the enroll
/// path can render the same template into the same on-disk location.
pub fn template_for(kind: ServiceKind) -> &'static str {
    match kind {
        ServiceKind::SystemdUser => SYSTEMD_SERVICE_TEMPLATE,
        ServiceKind::Launchd => LAUNCHD_PLIST_TEMPLATE,
        ServiceKind::Nssm => NSSM_TEMPLATE,
    }
}

// ---------------------------------------------------------------------------
// Top-level install / uninstall (used by both `aegis edge service install`
// and the post-enroll auto-install path).
// ---------------------------------------------------------------------------

/// Install the service unit using the supplied manager. Renders the platform
/// template against the current `aegis` binary path and the user's home dir.
///
/// Belt-and-suspenders to the systemd template's `ReadWritePaths=-` prefix:
/// before handing the rendered unit to the manager (which on Linux issues
/// `systemctl --user enable --now`), we ensure the daemon's state directory
/// exists so the very first start picks up the bind-mount cleanly. The `-`
/// prefix on `ReadWritePaths` makes systemd tolerate a *missing* path without
/// failing the unit, but it does NOT auto-rebind once the path appears later
/// — a restart is required. By creating the dir here we avoid that restart on
/// the post-enroll install path. The daemon itself also creates the dir on
/// startup (see `commands::edge::daemon::run`), so a manual `systemctl start`
/// before enrollment still self-heals on the next start.
pub fn install(mgr: &dyn ServiceManager, args: &InstallArgs) -> Result<InstallOutcome> {
    if let Some(state_dir) = edge_state_dir() {
        std::fs::create_dir_all(&state_dir).with_context(|| {
            format!(
                "create edge state dir {} prior to service install",
                state_dir.display()
            )
        })?;
    }
    let binary = current_binary_path();
    let home = home_dir_string();
    let content = render_unit_template(template_for(mgr.kind()), &binary, &home);
    mgr.install(&content, args.force, args.keep_existing)
}

/// Resolve the on-disk edge state directory (`$HOME/.aegis/edge`). Returns
/// `None` when `$HOME` cannot be determined — in that case the install path
/// proceeds without pre-creating the dir (the manager's downstream calls will
/// surface the missing-home failure with a clearer error).
fn edge_state_dir() -> Option<PathBuf> {
    dirs_next::home_dir().map(|h| h.join(".aegis").join("edge"))
}

/// Uninstall the service unit using the supplied manager. Idempotent.
pub fn uninstall(mgr: &dyn ServiceManager) -> Result<UninstallOutcome> {
    mgr.uninstall()
}

// ---------------------------------------------------------------------------
// CLI output helpers
// ---------------------------------------------------------------------------

fn print_install_outcome(kind: ServiceKind, outcome: &InstallOutcome) {
    match outcome {
        InstallOutcome::Installed { unit_path } => {
            println!(
                "aegis edge service: installed {} unit at {}",
                kind.label(),
                unit_path.display()
            );
        }
        InstallOutcome::AlreadyInstalled { unit_path } => {
            println!(
                "aegis edge service: {} unit already installed at {} (no changes)",
                kind.label(),
                unit_path.display()
            );
        }
        InstallOutcome::Conflict { unit_path, reason } => {
            eprintln!(
                "aegis edge service: refusing to replace existing {} unit at {} ({}). \
                 Re-run with --force to overwrite or --keep-existing to skip.",
                kind.label(),
                unit_path.display(),
                reason
            );
        }
    }
}

fn print_uninstall_outcome(kind: ServiceKind, outcome: &UninstallOutcome) {
    match outcome {
        UninstallOutcome::Removed { unit_path } => {
            println!(
                "aegis edge service: removed {} unit at {}",
                kind.label(),
                unit_path.display()
            );
        }
        UninstallOutcome::NotInstalled => {
            println!(
                "aegis edge service: no {} unit installed; nothing to remove",
                kind.label()
            );
        }
    }
}

fn print_status(kind: ServiceKind, s: &ServiceStatus) {
    let state = match s.state {
        ServiceState::Active => "active",
        ServiceState::Inactive => "inactive",
        ServiceState::Failed => "failed",
        ServiceState::NotInstalled => "not-installed",
    };
    println!("aegis edge service ({}):", kind.label());
    println!("  state: {state}");
    if let Some(pid) = s.pid {
        println!("  pid:   {pid}");
    }
    if let Some(up) = &s.uptime {
        println!("  uptime: {up}");
    }
    if !s.recent_logs.is_empty() {
        println!("  recent logs:");
        for line in &s.recent_logs {
            println!("    {line}");
        }
    }
}

// ---------------------------------------------------------------------------
// Status output parsers (table-driven, OS-agnostic seams).
//
// Extracted so unit tests can pin the parser logic without spawning systemctl,
// launchctl, or sc.exe.
// ---------------------------------------------------------------------------

/// Parse the property=value output of `systemctl --user show aegis-edge.service
/// --property=LoadState,ActiveState,SubState,MainPID,ActiveEnterTimestamp` into
/// a `ServiceStatus`.
pub fn parse_systemd_show(output: &str) -> ServiceStatus {
    let mut load_state = String::new();
    let mut active_state = String::new();
    let mut sub_state = String::new();
    let mut main_pid: Option<u32> = None;
    let mut active_enter: Option<String> = None;
    for line in output.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k {
                "LoadState" => load_state = v.to_string(),
                "ActiveState" => active_state = v.to_string(),
                "SubState" => sub_state = v.to_string(),
                "MainPID" => main_pid = v.trim().parse().ok().filter(|p: &u32| *p != 0),
                "ActiveEnterTimestamp" if !v.trim().is_empty() => {
                    active_enter = Some(v.trim().to_string());
                }
                _ => {}
            }
        }
    }
    let state = if load_state == "not-found" || load_state == "masked" || load_state.is_empty() {
        ServiceState::NotInstalled
    } else {
        match active_state.as_str() {
            "active" => ServiceState::Active,
            "failed" => ServiceState::Failed,
            "inactive" | "deactivating" => {
                if sub_state == "failed" {
                    ServiceState::Failed
                } else {
                    ServiceState::Inactive
                }
            }
            // activating / reloading: treat as active for operator UX
            "activating" | "reloading" => ServiceState::Active,
            _ => ServiceState::Inactive,
        }
    };
    ServiceStatus {
        state,
        pid: main_pid,
        uptime: active_enter,
        recent_logs: Vec::new(),
    }
}

/// Parse `launchctl list io.aegis.edge` output. The command prints a small
/// property-list-shaped record on success and a non-zero exit on
/// "not-installed". Caller passes `None` for the latter.
///
/// Used by the macOS `LaunchctlServiceManager`; flagged unused on Linux/Windows.
#[allow(dead_code)]
pub fn parse_launchctl_list(output: Option<&str>) -> ServiceStatus {
    let Some(text) = output else {
        return ServiceStatus {
            state: ServiceState::NotInstalled,
            pid: None,
            uptime: None,
            recent_logs: Vec::new(),
        };
    };
    let mut pid: Option<u32> = None;
    let mut last_exit: Option<i64> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("\"PID\" = ") {
            pid = rest.trim_end_matches(';').trim().parse().ok();
        } else if let Some(rest) = trimmed.strip_prefix("\"LastExitStatus\" = ") {
            last_exit = rest.trim_end_matches(';').trim().parse().ok();
        }
    }
    let state = match (pid, last_exit) {
        (Some(_), _) => ServiceState::Active,
        (None, Some(0)) => ServiceState::Inactive,
        (None, Some(_)) => ServiceState::Failed,
        (None, None) => ServiceState::Inactive,
    };
    ServiceStatus {
        state,
        pid,
        uptime: None,
        recent_logs: Vec::new(),
    }
}

/// Parse `sc.exe query AegisEdge` output. Output is `STATE : <code> <name>`
/// where running == 4, stopped == 1, paused == 7. The CLI returns a non-zero
/// exit and "service does not exist" message when not installed.
///
/// Used by the Windows `NssmServiceManager`; flagged unused on Linux/macOS.
#[allow(dead_code)]
pub fn parse_sc_query(output: Option<&str>) -> ServiceStatus {
    let Some(text) = output else {
        return ServiceStatus {
            state: ServiceState::NotInstalled,
            pid: None,
            uptime: None,
            recent_logs: Vec::new(),
        };
    };
    if text.contains("does not exist") || text.contains("1060") {
        return ServiceStatus {
            state: ServiceState::NotInstalled,
            pid: None,
            uptime: None,
            recent_logs: Vec::new(),
        };
    }
    let mut state = ServiceState::Inactive;
    for line in text.lines() {
        let upper = line.to_ascii_uppercase();
        if upper.contains("STATE") {
            if upper.contains("RUNNING") {
                state = ServiceState::Active;
            } else if upper.contains("STOPPED") {
                state = ServiceState::Inactive;
            } else if upper.contains("PAUSED") {
                state = ServiceState::Failed;
            }
        }
    }
    ServiceStatus {
        state,
        pid: None,
        uptime: None,
        recent_logs: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Linux: systemd --user
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub struct SystemdUserManager {
    unit_path: PathBuf,
}

#[cfg(target_os = "linux")]
impl SystemdUserManager {
    pub fn new() -> Result<Self> {
        let home = dirs_next::home_dir()
            .context("resolve $HOME for ~/.config/systemd/user/aegis-edge.service")?;
        let unit_path = home
            .join(".config")
            .join("systemd")
            .join("user")
            .join(SYSTEMD_UNIT);
        Ok(Self { unit_path })
    }
}

#[cfg(target_os = "linux")]
impl ServiceManager for SystemdUserManager {
    fn kind(&self) -> ServiceKind {
        ServiceKind::SystemdUser
    }
    fn unit_name(&self) -> &str {
        SYSTEMD_UNIT
    }
    fn unit_path(&self) -> PathBuf {
        self.unit_path.clone()
    }

    fn install(&self, content: &str, force: bool, keep_existing: bool) -> Result<InstallOutcome> {
        if let Some(outcome) = check_existing(&self.unit_path, content, force, keep_existing)? {
            return Ok(outcome);
        }
        if let Some(parent) = self.unit_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create systemd user dir {}", parent.display()))?;
        }
        std::fs::write(&self.unit_path, content)
            .with_context(|| format!("write systemd unit {}", self.unit_path.display()))?;
        run_systemctl_user(&["daemon-reload"]).context("systemctl --user daemon-reload")?;
        run_systemctl_user(&["enable", "--now", SYSTEMD_UNIT])
            .with_context(|| format!("systemctl --user enable --now {SYSTEMD_UNIT}"))?;
        Ok(InstallOutcome::Installed {
            unit_path: self.unit_path.clone(),
        })
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        if !self.unit_path.exists() {
            return Ok(UninstallOutcome::NotInstalled);
        }
        // best-effort stop + disable; ignore failures (might already be stopped)
        let _ = run_systemctl_user(&["disable", "--now", SYSTEMD_UNIT]);
        std::fs::remove_file(&self.unit_path)
            .with_context(|| format!("remove systemd unit {}", self.unit_path.display()))?;
        let _ = run_systemctl_user(&["daemon-reload"]);
        Ok(UninstallOutcome::Removed {
            unit_path: self.unit_path.clone(),
        })
    }

    fn status(&self) -> Result<ServiceStatus> {
        if !self.unit_path.exists() {
            return Ok(ServiceStatus {
                state: ServiceState::NotInstalled,
                pid: None,
                uptime: None,
                recent_logs: Vec::new(),
            });
        }
        let out = Command::new("systemctl")
            .args([
                "--user",
                "show",
                SYSTEMD_UNIT,
                "--property=LoadState,ActiveState,SubState,MainPID,ActiveEnterTimestamp",
            ])
            .output()
            .context("invoke systemctl --user show")?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut status = parse_systemd_show(&text);
        // Best-effort tail of the journal for the operator. Failure is silent.
        if matches!(
            status.state,
            ServiceState::Active | ServiceState::Failed | ServiceState::Inactive
        ) {
            if let Ok(j) = Command::new("journalctl")
                .args(["--user", "-u", SYSTEMD_UNIT, "-n", "10", "--no-pager"])
                .output()
            {
                let lines: Vec<String> = String::from_utf8_lossy(&j.stdout)
                    .lines()
                    .map(|l| l.to_string())
                    .collect();
                status.recent_logs = lines;
            }
        }
        Ok(status)
    }

    fn restart(&self) -> Result<()> {
        run_systemctl_user(&["restart", SYSTEMD_UNIT])
            .with_context(|| format!("systemctl --user restart {SYSTEMD_UNIT}"))
    }

    fn logs(&self, follow: bool, lines: usize) -> Result<()> {
        let lines_str = lines.to_string();
        let mut argv: Vec<&str> = vec!["--user", "-u", SYSTEMD_UNIT, "-n", &lines_str];
        if follow {
            argv.push("-f");
        } else {
            argv.push("--no-pager");
        }
        let status = Command::new("journalctl")
            .args(&argv)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("invoke journalctl --user")?;
        if !status.success() {
            bail!("journalctl --user exited with status {}", status);
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn run_systemctl_user(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("spawn systemctl")?;
    if !out.status.success() {
        bail!(
            "systemctl --user {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// macOS: launchd
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub struct LaunchdManager {
    plist_path: PathBuf,
}

#[cfg(target_os = "macos")]
impl LaunchdManager {
    pub fn new() -> Result<Self> {
        let home = dirs_next::home_dir().context("resolve $HOME for ~/Library/LaunchAgents")?;
        let plist_path = home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"));
        Ok(Self { plist_path })
    }
}

#[cfg(target_os = "macos")]
impl ServiceManager for LaunchdManager {
    fn kind(&self) -> ServiceKind {
        ServiceKind::Launchd
    }
    fn unit_name(&self) -> &str {
        LAUNCHD_LABEL
    }
    fn unit_path(&self) -> PathBuf {
        self.plist_path.clone()
    }

    fn install(&self, content: &str, force: bool, keep_existing: bool) -> Result<InstallOutcome> {
        if let Some(outcome) = check_existing(&self.plist_path, content, force, keep_existing)? {
            return Ok(outcome);
        }
        if let Some(parent) = self.plist_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create LaunchAgents dir {}", parent.display()))?;
        }
        std::fs::write(&self.plist_path, content)
            .with_context(|| format!("write launchd plist {}", self.plist_path.display()))?;
        let out = Command::new("launchctl")
            .args(["load", "-w"])
            .arg(&self.plist_path)
            .output()
            .context("invoke launchctl load")?;
        if !out.status.success() {
            bail!(
                "launchctl load -w {} failed: {}",
                self.plist_path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(InstallOutcome::Installed {
            unit_path: self.plist_path.clone(),
        })
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        if !self.plist_path.exists() {
            return Ok(UninstallOutcome::NotInstalled);
        }
        let _ = Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&self.plist_path)
            .output();
        std::fs::remove_file(&self.plist_path)
            .with_context(|| format!("remove launchd plist {}", self.plist_path.display()))?;
        Ok(UninstallOutcome::Removed {
            unit_path: self.plist_path.clone(),
        })
    }

    fn status(&self) -> Result<ServiceStatus> {
        if !self.plist_path.exists() {
            return Ok(parse_launchctl_list(None));
        }
        let out = Command::new("launchctl")
            .args(["list", LAUNCHD_LABEL])
            .output()
            .context("invoke launchctl list")?;
        let text = if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        };
        Ok(parse_launchctl_list(text.as_deref()))
    }

    fn restart(&self) -> Result<()> {
        let _ = Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&self.plist_path)
            .output();
        let out = Command::new("launchctl")
            .args(["load", "-w"])
            .arg(&self.plist_path)
            .output()
            .context("invoke launchctl load")?;
        if !out.status.success() {
            bail!(
                "launchctl restart failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn logs(&self, follow: bool, lines: usize) -> Result<()> {
        // `log show` does not support a tail-follow over a subsystem
        // predicate without `log stream`. Use `log stream` for follow mode.
        if follow {
            let status = Command::new("log")
                .args([
                    "stream",
                    "--predicate",
                    &format!("subsystem == \"{LAUNCHD_LABEL}\""),
                ])
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .context("invoke log stream")?;
            if !status.success() {
                bail!("log stream exited with status {}", status);
            }
            return Ok(());
        }
        // Non-follow: tail the last N lines from `log show`.
        let out = Command::new("log")
            .args([
                "show",
                "--predicate",
                &format!("subsystem == \"{LAUNCHD_LABEL}\""),
                "--last",
                "1h",
            ])
            .output()
            .context("invoke log show")?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text
            .lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .iter()
            .rev()
        {
            println!("{line}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Windows: NSSM
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
pub struct NssmManager {
    /// NSSM stores config in the registry; the on-disk artifact we manage is
    /// the rendered nssm.json *spec* under `~/.aegis/edge/`. This is how we
    /// detect "already installed" without round-tripping the registry.
    spec_path: PathBuf,
}

#[cfg(target_os = "windows")]
impl NssmManager {
    pub fn new() -> Result<Self> {
        let home = dirs_next::home_dir().context("resolve $HOME for nssm spec")?;
        let spec_path = home
            .join(".aegis")
            .join("edge")
            .join("aegis-edge.nssm.json");
        Ok(Self { spec_path })
    }

    fn require_nssm() -> Result<()> {
        which::which("nssm")
            .map(|_| ())
            .context("nssm executable not found in PATH; install NSSM and re-run")
    }
}

#[cfg(target_os = "windows")]
impl ServiceManager for NssmManager {
    fn kind(&self) -> ServiceKind {
        ServiceKind::Nssm
    }
    fn unit_name(&self) -> &str {
        NSSM_SERVICE_NAME
    }
    fn unit_path(&self) -> PathBuf {
        self.spec_path.clone()
    }

    fn install(&self, content: &str, force: bool, keep_existing: bool) -> Result<InstallOutcome> {
        Self::require_nssm()?;
        if let Some(outcome) = check_existing(&self.spec_path, content, force, keep_existing)? {
            return Ok(outcome);
        }
        if let Some(parent) = self.spec_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create nssm spec dir {}", parent.display()))?;
        }
        std::fs::write(&self.spec_path, content)
            .with_context(|| format!("write nssm spec {}", self.spec_path.display()))?;
        let spec: serde_json::Value = serde_json::from_str(content)
            .with_context(|| format!("parse nssm spec {}", self.spec_path.display()))?;
        let bin = spec
            .get("binary_path")
            .and_then(|v| v.as_str())
            .context("nssm spec missing binary_path")?;
        let args: Vec<&str> = spec
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        // Idempotent: nssm install fails if the service already exists.
        // Try to remove first, then install. Failure of remove is fine.
        let _ = Command::new("nssm")
            .args(["remove", NSSM_SERVICE_NAME, "confirm"])
            .output();
        let mut install_cmd = Command::new("nssm");
        install_cmd.args(["install", NSSM_SERVICE_NAME, bin]);
        install_cmd.args(&args);
        let out = install_cmd.output().context("invoke nssm install")?;
        if !out.status.success() {
            bail!(
                "nssm install failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let _ = Command::new("nssm")
            .args(["start", NSSM_SERVICE_NAME])
            .output();
        Ok(InstallOutcome::Installed {
            unit_path: self.spec_path.clone(),
        })
    }

    fn uninstall(&self) -> Result<UninstallOutcome> {
        if !self.spec_path.exists() {
            return Ok(UninstallOutcome::NotInstalled);
        }
        let _ = Command::new("nssm")
            .args(["stop", NSSM_SERVICE_NAME])
            .output();
        let _ = Command::new("nssm")
            .args(["remove", NSSM_SERVICE_NAME, "confirm"])
            .output();
        std::fs::remove_file(&self.spec_path)
            .with_context(|| format!("remove nssm spec {}", self.spec_path.display()))?;
        Ok(UninstallOutcome::Removed {
            unit_path: self.spec_path.clone(),
        })
    }

    fn status(&self) -> Result<ServiceStatus> {
        if !self.spec_path.exists() {
            return Ok(parse_sc_query(None));
        }
        let out = Command::new("sc")
            .args(["query", NSSM_SERVICE_NAME])
            .output()
            .context("invoke sc query")?;
        let text = if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        };
        Ok(parse_sc_query(text.as_deref()))
    }

    fn restart(&self) -> Result<()> {
        Self::require_nssm()?;
        let out = Command::new("nssm")
            .args(["restart", NSSM_SERVICE_NAME])
            .output()
            .context("invoke nssm restart")?;
        if !out.status.success() {
            bail!(
                "nssm restart failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn logs(&self, _follow: bool, _lines: usize) -> Result<()> {
        // NSSM does not stream logs natively; the spec configures stdout/stderr
        // redirect paths. Surface those paths so the operator can `Get-Content
        // -Wait` themselves.
        if !self.spec_path.exists() {
            bail!("aegis edge service not installed; nothing to tail");
        }
        let body = std::fs::read_to_string(&self.spec_path).with_context(|| {
            format!("read nssm spec for log paths {}", self.spec_path.display())
        })?;
        let spec: serde_json::Value = serde_json::from_str(&body)?;
        let stdout = spec
            .get("stdout_path")
            .and_then(|v| v.as_str())
            .unwrap_or("(not configured)");
        let stderr = spec
            .get("stderr_path")
            .and_then(|v| v.as_str())
            .unwrap_or("(not configured)");
        println!(
            "aegis edge service: NSSM does not stream logs through this CLI on Windows.\n\
             Tail manually:\n  Get-Content -Wait \"{stdout}\"\n  Get-Content -Wait \"{stderr}\""
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Conflict-resolution helper shared by every platform install path.
// ---------------------------------------------------------------------------

fn check_existing(
    path: &std::path::Path,
    new_content: &str,
    force: bool,
    keep_existing: bool,
) -> Result<Option<InstallOutcome>> {
    if !path.exists() {
        return Ok(None);
    }
    let on_disk = std::fs::read_to_string(path).unwrap_or_default();
    if on_disk == new_content {
        return Ok(Some(InstallOutcome::AlreadyInstalled {
            unit_path: path.to_path_buf(),
        }));
    }
    if force {
        return Ok(None);
    }
    if keep_existing {
        return Ok(Some(InstallOutcome::AlreadyInstalled {
            unit_path: path.to_path_buf(),
        }));
    }
    Ok(Some(InstallOutcome::Conflict {
        unit_path: path.to_path_buf(),
        reason: "existing unit content differs from rendered template".to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// In-process recording mock — drives the unit-test suite without touching
    /// systemd / launchd / nssm.
    struct MockServiceManager {
        kind: ServiceKind,
        unit_path: PathBuf,
        installed: Rc<RefCell<bool>>,
        last_install_content: Rc<RefCell<Option<String>>>,
        next_status: Rc<RefCell<ServiceStatus>>,
        restart_calls: Rc<RefCell<usize>>,
    }

    impl MockServiceManager {
        fn new(kind: ServiceKind, unit_path: PathBuf) -> Self {
            Self {
                kind,
                unit_path,
                installed: Rc::new(RefCell::new(false)),
                last_install_content: Rc::new(RefCell::new(None)),
                next_status: Rc::new(RefCell::new(ServiceStatus {
                    state: ServiceState::NotInstalled,
                    pid: None,
                    uptime: None,
                    recent_logs: Vec::new(),
                })),
                restart_calls: Rc::new(RefCell::new(0)),
            }
        }
    }

    impl ServiceManager for MockServiceManager {
        fn kind(&self) -> ServiceKind {
            self.kind
        }
        fn unit_name(&self) -> &str {
            "mock"
        }
        fn unit_path(&self) -> PathBuf {
            self.unit_path.clone()
        }
        fn install(
            &self,
            content: &str,
            _force: bool,
            _keep_existing: bool,
        ) -> Result<InstallOutcome> {
            *self.last_install_content.borrow_mut() = Some(content.to_string());
            *self.installed.borrow_mut() = true;
            Ok(InstallOutcome::Installed {
                unit_path: self.unit_path.clone(),
            })
        }
        fn uninstall(&self) -> Result<UninstallOutcome> {
            if !*self.installed.borrow() {
                return Ok(UninstallOutcome::NotInstalled);
            }
            *self.installed.borrow_mut() = false;
            Ok(UninstallOutcome::Removed {
                unit_path: self.unit_path.clone(),
            })
        }
        fn status(&self) -> Result<ServiceStatus> {
            Ok(self.next_status.borrow().clone())
        }
        fn restart(&self) -> Result<()> {
            *self.restart_calls.borrow_mut() += 1;
            Ok(())
        }
        fn logs(&self, _follow: bool, _lines: usize) -> Result<()> {
            Ok(())
        }
    }

    // -------- template substitution ----------

    #[test]
    fn render_unit_template_substitutes_binary_and_home() {
        let tpl = "{{BINARY_PATH}} edge daemon --state-dir {{HOME}}/.aegis/edge";
        let out = render_unit_template(tpl, "/usr/local/bin/aegis", "/home/jeshua");
        assert_eq!(
            out,
            "/usr/local/bin/aegis edge daemon --state-dir /home/jeshua/.aegis/edge"
        );
    }

    #[test]
    fn systemd_template_marks_state_dir_bind_mount_tolerant_of_missing_path() {
        // Regression: production was crash-looping with
        //   aegis-edge.service: Failed to set up mount namespacing:
        //   /home/<user>/.aegis/edge: No such file or directory
        //   Main process exited, code=exited, status=226/NAMESPACE
        // because `ReadWritePaths=%h/.aegis/edge` (no `-` prefix) hard-fails
        // namespace setup if the path doesn't exist when the unit starts —
        // even though the daemon creates the dir on startup. The `-` prefix
        // makes systemd silently skip the bind when the path is missing.
        let body = render_unit_template(
            template_for(ServiceKind::SystemdUser),
            "/usr/local/bin/aegis",
            "/home/jeshua",
        );
        assert!(
            body.contains("ReadWritePaths=-%h/.aegis/edge"),
            "ReadWritePaths must use the `-` prefix to tolerate a missing \
             state dir at unit-start time; got: {body}"
        );
        // Negative: the unprefixed form must NOT appear (otherwise the
        // tolerant entry above could coexist with a hard-fail entry and
        // namespace setup would still fail).
        assert!(
            !body.contains("\nReadWritePaths=%h/.aegis/edge"),
            "no untolerated `ReadWritePaths=%h/.aegis/edge` may remain in the \
             unit; only the `-`-prefixed form is permitted: {body}"
        );
    }

    #[test]
    fn install_creates_edge_state_dir_before_handing_to_manager() {
        // Regression: the install path must materialize `$HOME/.aegis/edge`
        // before the systemd unit is enabled+started, otherwise the very
        // first start hits the `ReadWritePaths=-` skip and the daemon needs
        // an extra restart to pick up the bind. We point `$HOME` at a temp
        // dir, run install via the mock manager, and assert the directory
        // exists afterwards.
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("HOME");
        // SAFETY: tests in this module are not run concurrently with other
        // tests that mutate $HOME; cargo test serializes within a process
        // for `#[test]` fns sharing process-global env at our scale.
        std::env::set_var("HOME", tmp.path());
        let mock = MockServiceManager::new(
            ServiceKind::SystemdUser,
            tmp.path().join("aegis-edge.service"),
        );
        let outcome = install(&mock, &InstallArgs::default()).expect("install ok");
        assert!(matches!(outcome, InstallOutcome::Installed { .. }));
        let state_dir = tmp.path().join(".aegis").join("edge");
        assert!(
            state_dir.is_dir(),
            "install must pre-create {} so the first systemd start \
             binds the path cleanly",
            state_dir.display()
        );
        // Restore $HOME for any sibling tests.
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn systemd_template_uses_resolved_binary_path() {
        // Regression: prior code never rendered `{{BINARY_PATH}}` so the
        // `aegis-edge.service` unit shipped with literal `{{BINARY_PATH}}` in
        // ExecStart and refused to start. The render must produce a unit with
        // an absolute path, no leftover placeholder, and the `edge daemon`
        // subcommand.
        let body = render_unit_template(
            template_for(ServiceKind::SystemdUser),
            "/usr/local/bin/aegis",
            "/home/jeshua",
        );
        assert!(
            !body.contains("{{BINARY_PATH}}"),
            "rendered unit must not contain the placeholder: {body}"
        );
        assert!(
            body.contains("ExecStart=/usr/local/bin/aegis edge daemon"),
            "ExecStart must invoke `edge daemon`: {body}"
        );
    }

    #[test]
    fn launchd_template_uses_resolved_binary_and_home() {
        // Regression: launchd plist had two placeholders. Both must be
        // substituted so launchctl can load the plist.
        let body = render_unit_template(
            template_for(ServiceKind::Launchd),
            "/usr/local/bin/aegis",
            "/Users/jeshua",
        );
        assert!(!body.contains("{{BINARY_PATH}}"));
        assert!(!body.contains("{{HOME}}"));
        assert!(body.contains("/usr/local/bin/aegis"));
        assert!(body.contains("/Users/jeshua/.aegis/edge"));
        // The ProgramArguments must invoke `edge daemon`, not the legacy
        // `--daemon --config` path which targets the wrong subsystem.
        assert!(body.contains("<string>edge</string>"));
        assert!(body.contains("<string>daemon</string>"));
    }

    #[test]
    fn nssm_template_uses_resolved_binary_and_edge_daemon_args() {
        let body = render_unit_template(
            template_for(ServiceKind::Nssm),
            r"C:\Program Files\Aegis\aegis.exe",
            "%USERPROFILE%",
        );
        assert!(!body.contains("{{BINARY_PATH}}"));
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v.get("binary_path").and_then(|v| v.as_str()),
            Some(r"C:\Program Files\Aegis\aegis.exe")
        );
        let args = v.get("args").and_then(|v| v.as_array()).unwrap();
        let arg_strs: Vec<&str> = args.iter().filter_map(|a| a.as_str()).collect();
        assert!(arg_strs.contains(&"edge"));
        assert!(arg_strs.contains(&"daemon"));
        assert!(arg_strs.contains(&"--state-dir"));
    }

    // -------- install / uninstall via MockServiceManager ----------

    #[test]
    fn install_records_rendered_content() {
        let tmp = tempfile::tempdir().unwrap();
        let mock = MockServiceManager::new(
            ServiceKind::SystemdUser,
            tmp.path().join("aegis-edge.service"),
        );
        let outcome = install(&mock, &InstallArgs::default()).unwrap();
        assert!(matches!(outcome, InstallOutcome::Installed { .. }));
        let recorded = mock.last_install_content.borrow().clone().unwrap();
        assert!(
            recorded.contains("ExecStart=") && recorded.contains("edge daemon"),
            "installer must pass the rendered systemd unit body, got: {recorded}"
        );
        assert!(
            !recorded.contains("{{BINARY_PATH}}"),
            "installer must substitute placeholders before handing to the manager"
        );
    }

    #[test]
    fn uninstall_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let mock = MockServiceManager::new(
            ServiceKind::SystemdUser,
            tmp.path().join("aegis-edge.service"),
        );
        // Not installed yet → NotInstalled, no error.
        let outcome = uninstall(&mock).unwrap();
        assert_eq!(outcome, UninstallOutcome::NotInstalled);
        // Install + uninstall round-trip.
        install(&mock, &InstallArgs::default()).unwrap();
        let outcome = uninstall(&mock).unwrap();
        assert!(matches!(outcome, UninstallOutcome::Removed { .. }));
        // Second uninstall is a no-op, not an error.
        let outcome = uninstall(&mock).unwrap();
        assert_eq!(outcome, UninstallOutcome::NotInstalled);
    }

    // -------- check_existing conflict policy ----------

    #[test]
    fn check_existing_returns_none_when_path_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("absent.service");
        let r = check_existing(&path, "x", false, false).unwrap();
        assert!(r.is_none(), "absent path must allow install");
    }

    #[test]
    fn check_existing_no_op_when_content_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("match.service");
        std::fs::write(&path, "same").unwrap();
        let r = check_existing(&path, "same", false, false).unwrap();
        assert!(matches!(r, Some(InstallOutcome::AlreadyInstalled { .. })));
    }

    #[test]
    fn check_existing_force_overwrites_diff_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diff.service");
        std::fs::write(&path, "old").unwrap();
        let r = check_existing(&path, "new", true, false).unwrap();
        assert!(r.is_none(), "--force must allow install through");
    }

    #[test]
    fn check_existing_keep_existing_skips_diff_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diff.service");
        std::fs::write(&path, "old").unwrap();
        let r = check_existing(&path, "new", false, true).unwrap();
        assert!(matches!(r, Some(InstallOutcome::AlreadyInstalled { .. })));
    }

    #[test]
    fn check_existing_conflicts_diff_content_without_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diff.service");
        std::fs::write(&path, "old").unwrap();
        let r = check_existing(&path, "new", false, false).unwrap();
        match r {
            Some(InstallOutcome::Conflict { unit_path, reason }) => {
                assert_eq!(unit_path, path);
                assert!(reason.contains("differs"));
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    // -------- status parsers ----------

    #[test]
    fn parse_systemd_show_active_with_pid_and_uptime() {
        let out = "LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=12345\nActiveEnterTimestamp=Tue 2026-04-29 10:00:00 UTC\n";
        let s = parse_systemd_show(out);
        assert_eq!(s.state, ServiceState::Active);
        assert_eq!(s.pid, Some(12345));
        assert!(s.uptime.unwrap().contains("Tue 2026-04-29"));
    }

    #[test]
    fn parse_systemd_show_inactive_no_pid() {
        let out =
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nMainPID=0\nActiveEnterTimestamp=\n";
        let s = parse_systemd_show(out);
        assert_eq!(s.state, ServiceState::Inactive);
        assert_eq!(s.pid, None);
        assert!(s.uptime.is_none());
    }

    #[test]
    fn parse_systemd_show_failed() {
        let out = "LoadState=loaded\nActiveState=failed\nSubState=failed\nMainPID=0\nActiveEnterTimestamp=\n";
        let s = parse_systemd_show(out);
        assert_eq!(s.state, ServiceState::Failed);
    }

    #[test]
    fn parse_systemd_show_not_installed_when_load_state_is_not_found() {
        let out = "LoadState=not-found\nActiveState=inactive\nSubState=dead\nMainPID=0\n";
        let s = parse_systemd_show(out);
        assert_eq!(s.state, ServiceState::NotInstalled);
    }

    #[test]
    fn parse_launchctl_list_active_when_pid_present() {
        let out = "{
    \"LimitLoadToSessionType\" = \"Aqua\";
    \"Label\" = \"io.aegis.edge\";
    \"OnDemand\" = false;
    \"LastExitStatus\" = 0;
    \"PID\" = 4242;
};
";
        let s = parse_launchctl_list(Some(out));
        assert_eq!(s.state, ServiceState::Active);
        assert_eq!(s.pid, Some(4242));
    }

    #[test]
    fn parse_launchctl_list_inactive_when_no_pid_clean_exit() {
        let out = "{
    \"Label\" = \"io.aegis.edge\";
    \"LastExitStatus\" = 0;
};
";
        let s = parse_launchctl_list(Some(out));
        assert_eq!(s.state, ServiceState::Inactive);
        assert_eq!(s.pid, None);
    }

    #[test]
    fn parse_launchctl_list_failed_when_no_pid_nonzero_exit() {
        let out = "{
    \"Label\" = \"io.aegis.edge\";
    \"LastExitStatus\" = 1;
};
";
        let s = parse_launchctl_list(Some(out));
        assert_eq!(s.state, ServiceState::Failed);
    }

    #[test]
    fn parse_launchctl_list_not_installed_when_command_failed() {
        let s = parse_launchctl_list(None);
        assert_eq!(s.state, ServiceState::NotInstalled);
    }

    #[test]
    fn parse_sc_query_running() {
        let out = "SERVICE_NAME: AegisEdge\n        TYPE               : 10  WIN32_OWN_PROCESS\n        STATE              : 4  RUNNING\n";
        let s = parse_sc_query(Some(out));
        assert_eq!(s.state, ServiceState::Active);
    }

    #[test]
    fn parse_sc_query_stopped() {
        let out = "STATE              : 1  STOPPED\n";
        let s = parse_sc_query(Some(out));
        assert_eq!(s.state, ServiceState::Inactive);
    }

    #[test]
    fn parse_sc_query_not_installed_when_does_not_exist() {
        let out = "[SC] EnumQueryServicesStatus:OpenService FAILED 1060:\n\nThe specified service does not exist as an installed service.\n";
        let s = parse_sc_query(Some(out));
        assert_eq!(s.state, ServiceState::NotInstalled);
    }

    #[test]
    fn parse_sc_query_not_installed_when_command_failed() {
        let s = parse_sc_query(None);
        assert_eq!(s.state, ServiceState::NotInstalled);
    }

    // -------- restart wiring ----------

    #[test]
    fn restart_invokes_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let mock = MockServiceManager::new(
            ServiceKind::SystemdUser,
            tmp.path().join("aegis-edge.service"),
        );
        mock.restart().unwrap();
        mock.restart().unwrap();
        assert_eq!(*mock.restart_calls.borrow(), 2);
    }
}
