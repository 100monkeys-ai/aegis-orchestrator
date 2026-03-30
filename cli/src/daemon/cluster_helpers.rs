// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Cluster node aggregation and DTOs.

use std::collections::HashSet;

use aegis_orchestrator_core::domain::{
    cluster::{ClusterSummaryStatus, NodePeer, NodePeerStatus, NodeRole},
    node_config::NodeConfigManifest,
};

use crate::daemon::state::AppState;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ClusterNodeView {
    pub(crate) node_id: String,
    pub(crate) role: String,
    pub(crate) status: String,
    pub(crate) grpc_address: String,
    pub(crate) gpu_count: u32,
    pub(crate) vram_gb: u32,
    pub(crate) cpu_cores: u32,
    pub(crate) available_memory_gb: u32,
    pub(crate) supported_runtimes: Vec<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) last_heartbeat_at: chrono::DateTime<chrono::Utc>,
    pub(crate) registered_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ClusterStatusView {
    pub(crate) source: String,
    pub(crate) controller_node_id: Option<String>,
    pub(crate) total_nodes: usize,
    pub(crate) active_nodes: usize,
    pub(crate) draining_nodes: usize,
    pub(crate) unhealthy_nodes: usize,
    pub(crate) cluster_health: String,
    pub(crate) nodes: Vec<ClusterNodeView>,
}

pub(crate) fn cluster_role_to_string(role: &NodeRole) -> String {
    format!("{role:?}").to_lowercase()
}

pub(crate) fn node_status_to_string(status: NodePeerStatus) -> String {
    format!("{status:?}").to_lowercase()
}

pub(crate) fn cluster_node_view(peer: &NodePeer) -> ClusterNodeView {
    ClusterNodeView {
        node_id: peer.node_id.0.to_string(),
        role: cluster_role_to_string(&peer.role),
        status: node_status_to_string(peer.status),
        grpc_address: peer.grpc_address.clone(),
        gpu_count: peer.capabilities.gpu_count,
        vram_gb: peer.capabilities.vram_gb,
        cpu_cores: peer.capabilities.cpu_cores,
        available_memory_gb: peer.capabilities.available_memory_gb,
        supported_runtimes: peer.capabilities.supported_runtimes.clone(),
        tags: peer.capabilities.tags.clone(),
        last_heartbeat_at: peer.last_heartbeat_at,
        registered_at: peer.registered_at,
    }
}

pub(crate) fn fallback_cluster_node(config: &NodeConfigManifest) -> NodePeer {
    use aegis_orchestrator_core::domain::cluster::{NodeCapabilityAdvertisement, NodeId};

    let node_id =
        uuid::Uuid::parse_str(&config.spec.node.id).unwrap_or_else(|_| uuid::Uuid::new_v4());
    let role = config
        .spec
        .cluster
        .as_ref()
        .map(|cluster| cluster.role)
        .unwrap_or_default();
    let (gpu_count, vram_gb, cpu_cores, available_memory_gb, tags) = config
        .spec
        .node
        .resources
        .as_ref()
        .map(|resources| {
            (
                resources.gpu_count,
                resources.vram_gb,
                resources.cpu_cores,
                resources.memory_gb,
                config.spec.node.tags.clone(),
            )
        })
        .unwrap_or((0, 0, 0, 0, config.spec.node.tags.clone()));
    let grpc_address = config
        .spec
        .cluster
        .as_ref()
        .and_then(|cluster| {
            cluster
                .controller
                .as_ref()
                .map(|controller| controller.endpoint.clone())
        })
        .unwrap_or_else(|| {
            config
                .spec
                .network
                .as_ref()
                .map(|network| {
                    format!(
                        "{}:{}",
                        network.bind_address,
                        config
                            .spec
                            .cluster
                            .as_ref()
                            .map(|cluster| cluster.cluster_grpc_port)
                            .unwrap_or(0)
                    )
                })
                .unwrap_or_else(|| {
                    format!(
                        "127.0.0.1:{}",
                        config
                            .spec
                            .cluster
                            .as_ref()
                            .map(|cluster| cluster.cluster_grpc_port)
                            .unwrap_or(0)
                    )
                })
        });

    NodePeer {
        node_id: NodeId(node_id),
        role,
        public_key: Vec::new(),
        capabilities: NodeCapabilityAdvertisement {
            gpu_count,
            vram_gb,
            cpu_cores,
            available_memory_gb,
            supported_runtimes: vec![],
            tags,
        },
        grpc_address,
        status: NodePeerStatus::Active,
        last_heartbeat_at: chrono::Utc::now(),
        registered_at: chrono::Utc::now(),
    }
}

pub(crate) async fn cluster_status_view(state: &AppState) -> ClusterStatusView {
    let nodes = load_cluster_nodes(state).await;
    let active_nodes = nodes.iter().filter(|node| node.status == "active").count();
    let draining_nodes = nodes
        .iter()
        .filter(|node| node.status == "draining")
        .count();
    let unhealthy_nodes = nodes
        .iter()
        .filter(|node| node.status == "unhealthy")
        .count();

    let health = ClusterSummaryStatus::from_counts(active_nodes, draining_nodes, unhealthy_nodes);

    ClusterStatusView {
        source: if state.cluster_repo.is_some() {
            "cluster_repository".to_string()
        } else {
            "local_fallback".to_string()
        },
        controller_node_id: nodes
            .iter()
            .find(|node| node.role == "controller")
            .or_else(|| nodes.first())
            .map(|node| node.node_id.clone()),
        total_nodes: nodes.len(),
        active_nodes,
        draining_nodes,
        unhealthy_nodes,
        cluster_health: health.to_string(),
        nodes,
    }
}

pub(crate) async fn load_cluster_nodes(state: &AppState) -> Vec<ClusterNodeView> {
    if let Some(repo) = &state.cluster_repo {
        let mut peers = Vec::new();
        for status in [
            NodePeerStatus::Active,
            NodePeerStatus::Draining,
            NodePeerStatus::Unhealthy,
        ] {
            if let Ok(mut items) = repo.list_peers_by_status(status).await {
                peers.append(&mut items);
            }
        }

        let mut seen = HashSet::new();
        peers
            .into_iter()
            .filter(|peer| seen.insert(peer.node_id))
            .map(|peer| cluster_node_view(&peer))
            .collect()
    } else {
        vec![cluster_node_view(&fallback_cluster_node(&state.config))]
    }
}
