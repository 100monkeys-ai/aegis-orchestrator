// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Status command for local stack and cluster health.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Report local service and cluster node health

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use aegis_orchestrator_core::domain::node_config::NodeConfigManifest;
use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    node_cluster_service_client::NodeClusterServiceClient, ListPeersRequest, NodeStatus,
};
use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use tonic::Request;

use crate::daemon::{
    check_daemon_running, probe_health_endpoint, DaemonStatus, HealthEndpointStatus,
};
use crate::output::{render_serialized, OutputFormat};

#[derive(Args, Debug, Clone)]
pub struct StatusArgs {
    /// Show cluster node status instead of the local Docker Compose stack
    /// Cluster mode probes each node's orchestrator health on the global
    /// `--port` value because peer metadata currently exposes only gRPC
    /// addresses, not per-node HTTP ports.
    #[arg(long)]
    pub cluster: bool,

    /// Directory where the AEGIS stack files live (default: ~/.aegis)
    #[arg(long, default_value = "~/.aegis")]
    pub dir: String,
}

pub async fn run(
    args: StatusArgs,
    config_path: Option<PathBuf>,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    if args.cluster {
        run_cluster_status(config_path, port, output_format).await
    } else {
        run_local_status(
            &expand_tilde(Path::new(&args.dir)),
            host,
            port,
            output_format,
        )
        .await
    }
}

#[derive(Serialize)]
struct LocalStatusOutput {
    mode: &'static str,
    services: Vec<LocalStatusRow>,
}

#[derive(Serialize)]
struct ClusterStatusOutput {
    mode: &'static str,
    controller_endpoint: String,
    nodes: Vec<ClusterStatusRow>,
}

async fn run_local_status(
    dir: &Path,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let compose_file = dir.join("docker-compose.yml");
    if !compose_file.exists() {
        bail!(
            "No docker-compose.yml found in {}.\nHave you run `aegis init`?",
            dir.display()
        );
    }

    let enabled_services = load_enabled_services(dir)?;
    let container_rows = load_compose_ps_rows(dir)?;
    let container_map = container_rows
        .into_iter()
        .map(|row| (row.service.clone(), row))
        .collect::<BTreeMap<_, _>>();

    let mut rows = Vec::new();
    let mut has_failures = false;

    for service in enabled_services {
        let compose_service = match service.kind {
            LocalStatusKind::Compose { has_healthcheck } => {
                let row = container_map.get(&service.name);
                let lifecycle = row
                    .map(|entry| normalize_container_state(&entry.state))
                    .unwrap_or_else(|| "missing".to_string());
                let health = if has_healthcheck {
                    row.map(|entry| normalize_health(entry.health.as_deref()))
                        .unwrap_or_else(|| "missing".to_string())
                } else {
                    "unknown".to_string()
                };
                let details = row
                    .map(|entry| entry.name.clone())
                    .unwrap_or_else(|| "container not created".to_string());
                let failing = is_local_service_failure(&lifecycle, &health, has_healthcheck);

                LocalStatusRow {
                    name: service.name,
                    lifecycle,
                    health,
                    details,
                    failing,
                }
            }
            LocalStatusKind::Daemon => daemon_row(host, port).await?,
        };

        has_failures |= compose_service.failing;
        rows.push(compose_service);
    }

    if output_format.is_structured() {
        render_serialized(
            output_format,
            &LocalStatusOutput {
                mode: "local",
                services: rows.clone(),
            },
        )?;
    } else {
        print_local_status_table(&rows);
    }

    if has_failures {
        return Err(anyhow!("One or more local services are unhealthy"));
    }

    Ok(())
}

async fn run_cluster_status(
    config_path: Option<PathBuf>,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let config = NodeConfigManifest::load_or_default(config_path)?;
    let cluster_config = config
        .spec
        .cluster
        .as_ref()
        .context("Cluster configuration (spec.cluster) is missing in aegis-config.yaml")?;

    let endpoint = cluster_config
        .controller
        .as_ref()
        .map(|controller| controller.endpoint.clone())
        .context("Controller endpoint not configured in spec.cluster.controller.endpoint")?;

    if !output_format.is_structured() {
        println!(
            "{} Querying cluster peers from {}...",
            "⚙".yellow(),
            endpoint.cyan()
        );
        println!(
            "{} Probing orchestrator health on HTTP port {} for each node.",
            "ℹ".cyan(),
            port.to_string().cyan()
        );
        println!(
            "{} Heartbeat freshness is derived from each node's last_heartbeat_at timestamp.",
            "ℹ".cyan()
        );
    }

    let mut client = NodeClusterServiceClient::connect(endpoint.clone())
        .await
        .context("Failed to connect to cluster controller")?;

    let resp = client
        .list_peers(Request::new(ListPeersRequest::default()))
        .await
        .context("Failed to list peers")?
        .into_inner();

    let mut rows = Vec::new();
    let mut has_failures = false;
    for node in resp.nodes {
        let node_status = node.status();
        let cluster_status = format!("{:?}", node_status).to_ascii_lowercase();
        let health_host = extract_host(&node.grpc_address);
        let role = format!("{:?}", node.role());
        let node_id = node.node_id;
        let grpc_address = node.grpc_address;
        let last_hb = node.last_heartbeat_at.as_ref().map(|ts| {
            chrono::DateTime::from_timestamp(ts.seconds, ts.nanos as u32)
                .map(|dt| {
                    let ago = chrono::Utc::now().signed_duration_since(dt);
                    format!("{}s ago", ago.num_seconds())
                })
                .unwrap_or_else(|| "invalid".to_string())
        });

        let orchestrator = match probe_health_endpoint(&health_host, port).await {
            Ok(HealthEndpointStatus::Healthy { .. }) => "healthy".to_string(),
            Ok(HealthEndpointStatus::Unhealthy { .. }) => {
                has_failures = true;
                "unhealthy".to_string()
            }
            Err(_) => {
                has_failures = true;
                "unreachable".to_string()
            }
        };

        if !matches!(node_status, NodeStatus::Active) {
            has_failures = true;
        }

        rows.push(ClusterStatusRow {
            node_id,
            role,
            cluster_status,
            orchestrator_status: orchestrator,
            grpc_address,
            last_heartbeat_at: last_hb,
        });
    }

    if output_format.is_structured() {
        render_serialized(
            output_format,
            &ClusterStatusOutput {
                mode: "cluster",
                controller_endpoint: endpoint,
                nodes: rows,
            },
        )?;
    } else {
        println!();
        println!(
            "{:<36} {:<12} {:<14} {:<16} {:<12} {}",
            "NODE ID".bold(),
            "ROLE".bold(),
            "CLUSTER".bold(),
            "ORCHESTRATOR".bold(),
            "HEARTBEAT".bold(),
            "ADDRESS".bold()
        );
        println!("{}", "-".repeat(110));

        for row in rows {
            println!(
                "{:<36} {:<12} {:<14} {:<16} {:<12} {}",
                row.node_id.dimmed(),
                row.role,
                render_cluster_state_label(&row.cluster_status),
                render_orchestrator_health_label(&row.orchestrator_status),
                row.last_heartbeat_at.as_deref().unwrap_or("n/a"),
                row.grpc_address.cyan()
            );
        }
        println!();
    }

    if has_failures {
        return Err(anyhow!("One or more cluster nodes are unhealthy"));
    }

    Ok(())
}

async fn daemon_row(host: &str, port: u16) -> Result<LocalStatusRow> {
    let row = match check_daemon_running(host, port).await? {
        DaemonStatus::Running { pid, uptime } => LocalStatusRow {
            name: "aegis-daemon".to_string(),
            lifecycle: "running".to_string(),
            health: "healthy".to_string(),
            details: daemon_details(pid, uptime),
            failing: false,
        },
        DaemonStatus::Stopped => LocalStatusRow {
            name: "aegis-daemon".to_string(),
            lifecycle: "stopped".to_string(),
            health: "missing".to_string(),
            details: format!("http://{host}:{port}/health unavailable"),
            failing: true,
        },
        DaemonStatus::Unhealthy { pid, error } => LocalStatusRow {
            name: "aegis-daemon".to_string(),
            lifecycle: "running".to_string(),
            health: "unhealthy".to_string(),
            details: daemon_error_details(pid, &error),
            failing: true,
        },
    };

    Ok(row)
}

fn daemon_details(pid: u32, uptime: Option<u64>) -> String {
    let pid_detail = if pid == 0 {
        "pid=remote".to_string()
    } else {
        format!("pid={pid}")
    };

    match uptime {
        Some(uptime) => format!("{pid_detail}, uptime={}s", uptime),
        None => pid_detail,
    }
}

fn daemon_error_details(pid: u32, error: &str) -> String {
    if pid == 0 {
        error.to_string()
    } else {
        format!("pid={pid}, {error}")
    }
}

fn print_local_status_table(rows: &[LocalStatusRow]) {
    println!();
    println!(
        "{:<24} {:<12} {:<12} {}",
        "SERVICE".bold(),
        "STATE".bold(),
        "HEALTH".bold(),
        "DETAILS".bold()
    );
    println!("{}", "-".repeat(88));

    for row in rows {
        println!(
            "{:<24} {:<12} {:<12} {}",
            row.name.as_str(),
            color_state(&row.lifecycle),
            color_health(&row.health),
            row.details
        );
    }
    println!();
}

fn color_state(state: &str) -> colored::ColoredString {
    match state {
        "running" => state.green(),
        "exited" | "dead" | "stopped" | "missing" => state.red(),
        _ => state.yellow(),
    }
}

fn color_health(health: &str) -> colored::ColoredString {
    match health {
        "healthy" => health.green(),
        "unknown" => health.yellow(),
        _ => health.red(),
    }
}

fn render_cluster_state_label(status: &str) -> colored::ColoredString {
    match status {
        "active" => "active".green(),
        "draining" => "draining".yellow(),
        "unhealthy" => "unhealthy".red(),
        _ => "unknown".yellow(),
    }
}

fn render_orchestrator_health_label(status: &str) -> colored::ColoredString {
    match status {
        "healthy" => "healthy".green(),
        "unhealthy" => "unhealthy".red(),
        "unreachable" => "unreachable".red(),
        _ => "unknown".yellow(),
    }
}

fn load_enabled_services(dir: &Path) -> Result<Vec<EnabledService>> {
    let compose = fs::read_to_string(dir.join("docker-compose.yml"))
        .context("Failed to read docker-compose.yml")?;
    let compose: ComposeFile =
        serde_yaml::from_str(&compose).context("Failed to parse compose file")?;

    let enabled_profiles = load_enabled_profiles(dir)?;
    let mut services = compose
        .services
        .into_iter()
        .filter_map(|(name, service)| {
            let included = service.profiles.is_empty()
                || service
                    .profiles
                    .iter()
                    .any(|profile| enabled_profiles.contains(profile));
            if included {
                Some(EnabledService {
                    name,
                    kind: LocalStatusKind::Compose {
                        has_healthcheck: service.healthcheck.is_some(),
                    },
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    services.sort_by(|left, right| left.name.cmp(&right.name));
    services.push(EnabledService {
        name: "aegis-daemon".to_string(),
        kind: LocalStatusKind::Daemon,
    });

    Ok(services)
}

fn load_enabled_profiles(dir: &Path) -> Result<BTreeSet<String>> {
    let env_path = dir.join(".env");
    if !env_path.exists() {
        return Ok(BTreeSet::from([String::from("core")]));
    }

    let contents = fs::read_to_string(env_path).context("Failed to read .env")?;
    let profiles = contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.strip_prefix("COMPOSE_PROFILES=").map(|value| {
                value
                    .trim_matches('"')
                    .split(',')
                    .filter(|entry| !entry.trim().is_empty())
                    .map(|entry| entry.trim().to_string())
                    .collect::<BTreeSet<_>>()
            })
        })
        .next()
        .unwrap_or_else(|| BTreeSet::from([String::from("core")]));

    Ok(profiles)
}

fn load_compose_ps_rows(dir: &Path) -> Result<Vec<ComposePsRow>> {
    let output = Command::new("docker")
        .arg("compose")
        .args(["ps", "--all", "--format", "json"])
        .current_dir(dir)
        .output()
        .context("Failed to run `docker compose ps` — is Docker installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "`docker compose ps --all --format json` failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr
        );
    }

    parse_compose_ps_output(&String::from_utf8_lossy(&output.stdout))
}

fn parse_compose_ps_output(output: &str) -> Result<Vec<ComposePsRow>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).context("Failed to parse docker compose ps JSON");
    }

    trimmed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).context("Failed to parse docker compose ps JSON line")
        })
        .collect()
}

fn normalize_container_state(state: &str) -> String {
    state.trim().to_ascii_lowercase()
}

fn normalize_health(health: Option<&str>) -> String {
    let normalized = health.unwrap_or("").trim().to_ascii_lowercase();
    if normalized.is_empty() {
        "unknown".to_string()
    } else {
        normalized
    }
}

fn is_local_service_failure(lifecycle: &str, health: &str, has_healthcheck: bool) -> bool {
    if lifecycle != "running" {
        return true;
    }

    has_healthcheck && health != "healthy"
}

fn extract_host(address: &str) -> String {
    let address = address
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(address);

    if let Some(host) = address
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']').map(|(host, _)| host))
    {
        return host.to_string();
    }

    address
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(address)
        .to_string()
}

fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" || s.starts_with("~/") {
        if let Some(home) = dirs_next::home_dir() {
            if s == "~" {
                return home;
            }
            return home.join(&s[2..]);
        }
    }
    path.to_path_buf()
}

#[derive(Debug, Clone, Serialize)]
struct LocalStatusRow {
    name: String,
    lifecycle: String,
    health: String,
    details: String,
    failing: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ClusterStatusRow {
    node_id: String,
    role: String,
    cluster_status: String,
    orchestrator_status: String,
    grpc_address: String,
    last_heartbeat_at: Option<String>,
}

#[derive(Debug)]
struct EnabledService {
    name: String,
    kind: LocalStatusKind,
}

#[derive(Debug)]
enum LocalStatusKind {
    Compose { has_healthcheck: bool },
    Daemon,
}

#[derive(Debug, Deserialize)]
struct ComposeFile {
    services: BTreeMap<String, ComposeService>,
}

#[derive(Debug, Deserialize)]
struct ComposeService {
    #[serde(default)]
    profiles: Vec<String>,
    #[serde(default)]
    healthcheck: Option<serde_yaml::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct ComposePsRow {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Service")]
    service: String,
    #[serde(rename = "State")]
    state: String,
    #[serde(rename = "Health")]
    health: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Parser, Subcommand};

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommand,
    }

    #[derive(Subcommand)]
    enum TestCommand {
        Status(StatusArgs),
    }

    #[test]
    fn status_args_parse_cluster_flag() {
        let cli = TestCli::try_parse_from(["aegis", "status", "--cluster"]).expect("parse status");
        match cli.command {
            TestCommand::Status(args) => assert!(args.cluster),
        }
    }

    #[test]
    fn parse_compose_ps_output_supports_array_payloads() {
        let rows = parse_compose_ps_output(
            r#"[{"Name":"aegis-runtime","Service":"aegis-runtime","State":"running","Health":"healthy"}]"#,
        )
        .expect("parse json array");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].service, "aegis-runtime");
        assert_eq!(rows[0].health.as_deref(), Some("healthy"));
    }

    #[test]
    fn parse_compose_ps_output_supports_json_lines() {
        let rows = parse_compose_ps_output(
            "{\"Name\":\"aegis-postgres\",\"Service\":\"postgres\",\"State\":\"running\",\"Health\":\"healthy\"}\n\
             {\"Name\":\"aegis-smcp-gateway\",\"Service\":\"aegis-smcp-gateway\",\"State\":\"running\",\"Health\":\"\"}",
        )
        .expect("parse json lines");

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].service, "aegis-smcp-gateway");
    }

    #[test]
    fn unknown_health_does_not_fail_running_service_without_healthcheck() {
        assert!(!is_local_service_failure("running", "unknown", false));
        assert!(is_local_service_failure("running", "unhealthy", true));
        assert!(is_local_service_failure("exited", "unknown", false));
    }

    #[test]
    fn extract_host_handles_bare_hosts_urls_and_ipv6() {
        assert_eq!(extract_host("10.0.0.8:50051"), "10.0.0.8");
        assert_eq!(
            extract_host("https://controller.aegis.internal:50056"),
            "controller.aegis.internal"
        );
        assert_eq!(extract_host("[2001:db8::1]:50051"), "2001:db8::1");
    }
}
