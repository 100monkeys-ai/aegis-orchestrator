// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Node command implementations for AEGIS CLI
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements node-related commands (clustering, registration)

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use tonic::Request;

use crate::output::{render_serialized, OutputFormat};
use aegis_orchestrator_core::domain::node_config::NodeConfigManifest;
use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    node_cluster_service_client::NodeClusterServiceClient, AttestNodeRequest, ChallengeNodeRequest,
    ListPeersRequest, NodeCapabilities, NodeRole,
};

#[derive(Subcommand)]
pub enum NodeCommand {
    /// Generates Ed25519 keypairs for node identity
    Init {
        /// Use development defaults
        #[arg(long)]
        dev: bool,
    },
    /// Runs the two-step attestation/registration handshake with a controller
    Join {
        /// Controller gRPC endpoint (e.g., https://controller:50056)
        endpoint: String,
    },
    /// Graceful deregistration from the cluster
    Leave,
    /// Queries the controller for the list of registered cluster peers
    Peers,
}

pub async fn handle_command(
    command: NodeCommand,
    config_path: Option<PathBuf>,
    _host: &str,
    _port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let config = NodeConfigManifest::load_or_default(config_path)?;

    match command {
        NodeCommand::Init { dev: _ } => init_node(&config, output_format).await,
        NodeCommand::Join { endpoint } => join_cluster(&config, endpoint, output_format).await,
        NodeCommand::Peers => list_peers(&config, output_format).await,
        NodeCommand::Leave => {
            anyhow::bail!("Node leave is unavailable in the single-node baseline protocol")
        }
    }
}

#[derive(Serialize)]
struct NodeInitOutput {
    created: bool,
    path: String,
}

#[derive(Serialize)]
struct NodeJoinOutput {
    endpoint: String,
    node_id: String,
    token_issued: bool,
}

#[derive(Serialize)]
struct NodePeerOutput {
    node_id: String,
    role: String,
    status: String,
    grpc_address: String,
}

#[derive(Serialize)]
struct NodePeersOutput {
    controller_endpoint: String,
    peers: Vec<NodePeerOutput>,
}

async fn init_node(config: &NodeConfigManifest, output_format: OutputFormat) -> Result<()> {
    let path = config
        .spec
        .cluster
        .as_ref()
        .map(|c| c.node_keypair_path.clone())
        .unwrap_or_else(|| PathBuf::from("~/.aegis/node_keypair.pem"));

    // Resolve home directory if needed
    let path = if path.to_string_lossy().starts_with('~') {
        if let Some(home) = dirs_next::home_dir() {
            home.join(
                path.to_string_lossy()
                    .trim_start_matches("~/")
                    .trim_start_matches('~'),
            )
        } else {
            path
        }
    } else {
        path
    };

    if path.exists() {
        if output_format.is_structured() {
            return render_serialized(
                output_format,
                &NodeInitOutput {
                    created: false,
                    path: path.display().to_string(),
                },
            );
        }
        println!(
            "{} Node identity keypair already exists at {}",
            "ℹ".blue(),
            path.display().to_string().cyan()
        );
        return Ok(());
    }

    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let bytes = signing_key.to_bytes();

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, bytes).context("Failed to write node keypair")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &NodeInitOutput {
                created: true,
                path: path.display().to_string(),
            },
        );
    }

    println!(
        "{} Generated new node identity keypair at {}",
        "✓".green(),
        path.display().to_string().cyan()
    );
    Ok(())
}

async fn join_cluster(
    config: &NodeConfigManifest,
    endpoint: String,
    output_format: OutputFormat,
) -> Result<()> {
    if !output_format.is_structured() {
        println!(
            "{} Attempting to join cluster at {}...",
            "⚙".yellow(),
            endpoint.cyan()
        );
    }

    let mut client = NodeClusterServiceClient::connect(endpoint.clone())
        .await
        .context("Failed to connect to cluster controller")?;

    // Load Identity Keypair
    let key_path = config
        .spec
        .cluster
        .as_ref()
        .map(|c| &c.node_keypair_path)
        .context("Cluster configuration (spec.cluster) is missing in aegis-config.yaml")?;

    // Resolve home directory if needed
    let key_path = if key_path.to_string_lossy().starts_with('~') {
        if let Some(home) = dirs_next::home_dir() {
            home.join(
                key_path
                    .to_string_lossy()
                    .trim_start_matches("~/")
                    .trim_start_matches('~'),
            )
        } else {
            key_path.clone()
        }
    } else {
        key_path.clone()
    };

    let key_bytes = fs::read(&key_path).context(format!(
        "Failed to read node identity keypair at {}. Run 'aegis node init' first.",
        key_path.display()
    ))?;

    let signing_key = SigningKey::from_bytes(
        key_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid keypair format"))?,
    );

    // 1. Step 1: AttestNode (Identity Presentation)
    let attest_req = AttestNodeRequest {
        node_id: config.spec.node.id.clone(),
        role: match config
            .spec
            .cluster
            .as_ref()
            .map(|c| c.role)
            .unwrap_or_default()
        {
            aegis_orchestrator_core::domain::node_config::NodeRole::Controller => {
                NodeRole::Controller.into()
            }
            aegis_orchestrator_core::domain::node_config::NodeRole::Worker => {
                NodeRole::Worker.into()
            }
            aegis_orchestrator_core::domain::node_config::NodeRole::Hybrid => {
                NodeRole::Hybrid.into()
            }
            aegis_orchestrator_core::domain::node_config::NodeRole::Edge => NodeRole::Edge.into(),
            aegis_orchestrator_core::domain::node_config::NodeRole::RelayCoordinator => {
                NodeRole::RelayCoordinator.into()
            }
        },
        public_key: signing_key.verifying_key().to_bytes().to_vec(),
        capabilities: Some(NodeCapabilities {
            gpu_count: config
                .spec
                .node
                .resources
                .as_ref()
                .map(|r| r.gpu_count)
                .unwrap_or(0),
            vram_gb: config
                .spec
                .node
                .resources
                .as_ref()
                .map(|r| r.vram_gb)
                .unwrap_or(0),
            cpu_cores: config
                .spec
                .node
                .resources
                .as_ref()
                .map(|r| r.cpu_cores)
                .unwrap_or(0),
            available_memory_gb: config
                .spec
                .node
                .resources
                .as_ref()
                .map(|r| r.memory_gb)
                .unwrap_or(0),
            supported_runtimes: vec!["docker".to_string()], // Single-node baseline runtime
            tags: config.spec.node.tags.clone(),
        }),
        grpc_address: config
            .spec
            .network
            .as_ref()
            .map(|n| format!("localhost:{}", n.grpc_port))
            .unwrap_or_else(|| "localhost:50051".to_string()),
        // Audit 002 §4.9: cluster admission token. Sourced from
        // spec.cluster.controller.token (env: indirection supported via
        // resolve_env_value). Edge daemons leave this empty and present a
        // bootstrap_proof on ChallengeNode instead.
        enrolment_token: config
            .spec
            .cluster
            .as_ref()
            .and_then(|c| c.controller.as_ref())
            .and_then(|c| c.token.as_deref())
            .map(|t| {
                aegis_orchestrator_core::domain::node_config::resolve_env_value(t)
                    .unwrap_or_else(|_| t.to_string())
            })
            .unwrap_or_default(),
    };

    if !output_format.is_structured() {
        println!("{} Sending AttestNodeRequest (Step 1)...", "➜".blue());
    }
    let attest_resp = client
        .attest_node(Request::new(attest_req))
        .await
        .context("Attestation failed at Step 1 (AttestNode)")?
        .into_inner();

    // 2. Step 2: ChallengeNode (Proof of Possession)
    if !output_format.is_structured() {
        println!("{} Solving challenge nonce (Step 2)...", "➜".blue());
    }
    let signature = signing_key.sign(&attest_resp.challenge_nonce);
    let challenge_req = ChallengeNodeRequest {
        challenge_id: attest_resp.challenge_id,
        node_id: config.spec.node.id.clone(),
        challenge_signature: signature.to_bytes().to_vec(),
        // ADR-117: workers / hybrids / controllers do not supply a bootstrap
        // proof. Edge daemons populate this with `EnrollmentToken`.
        bootstrap_proof: None,
    };

    let _challenge_resp = client
        .challenge_node(Request::new(challenge_req))
        .await
        .context("Attestation failed at Step 2 (ChallengeNode)")?
        .into_inner();

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &NodeJoinOutput {
                endpoint,
                node_id: config.spec.node.id.clone(),
                token_issued: true,
            },
        );
    }

    println!("{} Successfully joined cluster!", "✓".green());
    println!("{} NodeSecurityToken issued (expires in 1h)", "ℹ".blue());

    // Persisting or forwarding the issued token belongs to the daemon/runtime
    // integration path. This CLI command stops after the registration handshake.

    Ok(())
}

async fn list_peers(config: &NodeConfigManifest, output_format: OutputFormat) -> Result<()> {
    let cluster_config = config
        .spec
        .cluster
        .as_ref()
        .context("Cluster configuration (spec.cluster) is missing in aegis-config.yaml")?;

    let endpoint = cluster_config
        .controller
        .as_ref()
        .map(|c| c.endpoint.clone())
        .context("Controller endpoint not configured in spec.cluster.controller.endpoint")?;

    if !output_format.is_structured() {
        println!(
            "{} Querying cluster peers from {}...",
            "⚙".yellow(),
            endpoint.cyan()
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

    let peers = resp
        .nodes
        .into_iter()
        .map(|node| {
            let role = format!("{:?}", node.role());
            let status = format!("{:?}", node.status()).to_ascii_lowercase();
            let node_id = node.node_id;
            let grpc_address = node.grpc_address;
            NodePeerOutput {
                node_id,
                role,
                status,
                grpc_address,
            }
        })
        .collect::<Vec<_>>();

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &NodePeersOutput {
                controller_endpoint: endpoint,
                peers,
            },
        );
    }

    println!(
        "\n{:<36} {:<12} {:<10} {:<15}",
        "NODE ID".bold(),
        "ROLE".bold(),
        "STATUS".bold(),
        "GRPC ADDRESS".bold()
    );
    println!("{}", "-".repeat(85));

    for node in peers {
        let status_color = match node.status.as_str() {
            "active" => "green",
            "draining" => "yellow",
            "unhealthy" => "red",
            _ => "white",
        };

        println!(
            "{:<36} {:<12} {:<10} {:<15}",
            node.node_id.dimmed(),
            node.role,
            node.status.color(status_color),
            node.grpc_address.cyan()
        );
    }
    println!();

    Ok(())
}
