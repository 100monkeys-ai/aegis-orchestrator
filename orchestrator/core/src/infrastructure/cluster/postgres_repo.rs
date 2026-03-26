// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Cluster Repository
//!
//! Production `NodeClusterRepository`, `NodeChallengeRepository`, and
//! `StimulusIdempotencyRepository` implementations backed by PostgreSQL.

use crate::domain::cluster::NodeChallenge;
use crate::domain::cluster::{
    NodeCapabilityAdvertisement, NodeChallengeRepository, NodeClusterRepository, NodeId, NodePeer,
    NodePeerStatus, NodeRole, RegisteredNode, ResourceSnapshot, StimulusIdempotencyRepository,
};
use crate::domain::stimulus::StimulusId;
use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;

pub struct PgNodeClusterRepository {
    pool: PgPool,
}

impl PgNodeClusterRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl NodeClusterRepository for PgNodeClusterRepository {
    async fn upsert_peer(&self, peer: &NodePeer) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            INSERT INTO cluster_nodes (
                node_id, role, public_key, grpc_address, status,
                gpu_count, vram_gb, cpu_cores, available_mem_gb,
                supported_runtimes, tags, registered_at, last_heartbeat_at,
                hostname, software_version, metadata
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
            ON CONFLICT (node_id) DO UPDATE SET
                role = EXCLUDED.role,
                public_key = EXCLUDED.public_key,
                grpc_address = EXCLUDED.grpc_address,
                status = EXCLUDED.status,
                gpu_count = EXCLUDED.gpu_count,
                vram_gb = EXCLUDED.vram_gb,
                cpu_cores = EXCLUDED.cpu_cores,
                available_mem_gb = EXCLUDED.available_mem_gb,
                supported_runtimes = EXCLUDED.supported_runtimes,
                tags = EXCLUDED.tags,
                last_heartbeat_at = EXCLUDED.last_heartbeat_at,
                hostname = EXCLUDED.hostname,
                software_version = EXCLUDED.software_version,
                metadata = EXCLUDED.metadata
            "#,
        )
        .bind(peer.node_id.0)
        .bind(format!("{:?}", peer.role).to_lowercase())
        .bind(&peer.public_key)
        .bind(&peer.grpc_address)
        .bind(format!("{:?}", peer.status).to_lowercase())
        .bind(peer.capabilities.gpu_count as i32)
        .bind(peer.capabilities.vram_gb as i32)
        .bind(peer.capabilities.cpu_cores as i32)
        .bind(peer.capabilities.available_memory_gb as i32)
        .bind(&peer.capabilities.supported_runtimes)
        .bind(&peer.capabilities.tags)
        .bind(peer.registered_at)
        .bind(peer.last_heartbeat_at)
        .bind("") // hostname default
        .bind("") // software_version default
        .bind(serde_json::json!({})) // metadata default
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_peer(&self, node_id: &NodeId) -> anyhow::Result<Option<NodePeer>> {
        let row = sqlx::query(
            r#"
            SELECT node_id, role, public_key, grpc_address, status, gpu_count, vram_gb,
                   cpu_cores, available_mem_gb, supported_runtimes, tags,
                   last_heartbeat_at, registered_at
            FROM cluster_nodes
            WHERE node_id = $1
            "#,
        )
        .bind(node_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_node_peer(&r)))
    }

    async fn list_peers_by_status(&self, status: NodePeerStatus) -> anyhow::Result<Vec<NodePeer>> {
        let status_str = format!("{:?}", status).to_lowercase();
        let rows = sqlx::query(
            r#"
            SELECT node_id, role, public_key, grpc_address, status, gpu_count, vram_gb,
                   cpu_cores, available_mem_gb, supported_runtimes, tags,
                   last_heartbeat_at, registered_at
            FROM cluster_nodes
            WHERE status = $1
            "#,
        )
        .bind(status_str)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_node_peer).collect())
    }

    async fn record_heartbeat(
        &self,
        node_id: &NodeId,
        snapshot: ResourceSnapshot,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            UPDATE cluster_nodes
            SET last_heartbeat_at = NOW(),
                cpu_utilization_percent = $2,
                gpu_utilization_percent = $3,
                active_executions = $4
            WHERE node_id = $1
            "#,
        )
        .bind(node_id.0)
        .bind(snapshot.cpu_utilization)
        .bind(snapshot.gpu_utilization)
        .bind(snapshot.active_executions as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_unhealthy(&self, node_id: &NodeId) -> anyhow::Result<()> {
        sqlx::query("UPDATE cluster_nodes SET status = 'unhealthy' WHERE node_id = $1")
            .bind(node_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn deregister(&self, node_id: &NodeId, _reason: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM cluster_nodes WHERE node_id = $1")
            .bind(node_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_config_version(&self, node_id: &NodeId) -> anyhow::Result<Option<String>> {
        let row =
            sqlx::query("SELECT current_config_version FROM cluster_nodes WHERE node_id = $1")
                .bind(node_id.0)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("current_config_version")))
    }

    async fn record_config_version(&self, node_id: &NodeId, hash: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE cluster_nodes SET current_config_version = $2 WHERE node_id = $1")
            .bind(node_id.0)
            .bind(hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_all_peers(&self) -> anyhow::Result<Vec<NodePeer>> {
        let rows = sqlx::query(
            r#"
            SELECT node_id, role, public_key, grpc_address, status, gpu_count, vram_gb,
                   cpu_cores, available_mem_gb, supported_runtimes, tags,
                   last_heartbeat_at, registered_at
            FROM cluster_nodes
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_node_peer).collect())
    }

    async fn count_by_status(&self) -> anyhow::Result<HashMap<NodePeerStatus, usize>> {
        let rows = sqlx::query(
            r#"
            SELECT status, COUNT(*) as count
            FROM cluster_nodes
            GROUP BY status
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut map = HashMap::new();
        for row in &rows {
            let status_str: String = row.get("status");
            let count: i64 = row.get("count");
            let status = match status_str.as_str() {
                "active" => NodePeerStatus::Active,
                "draining" => NodePeerStatus::Draining,
                "unhealthy" => NodePeerStatus::Unhealthy,
                _ => continue,
            };
            map.insert(status, count as usize);
        }
        Ok(map)
    }

    async fn find_registered_node(
        &self,
        node_id: &NodeId,
    ) -> anyhow::Result<Option<RegisteredNode>> {
        let row = sqlx::query(
            r#"
            SELECT node_id, role, public_key, grpc_address, status, gpu_count, vram_gb,
                   cpu_cores, available_mem_gb, supported_runtimes, tags,
                   last_heartbeat_at, registered_at,
                   hostname, software_version, metadata, current_config_version
            FROM cluster_nodes
            WHERE node_id = $1
            "#,
        )
        .bind(node_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            let peer = row_to_node_peer(&r);
            let hostname: String = r.get("hostname");
            let software_version: String = r.get("software_version");
            let metadata_json: serde_json::Value = r.get("metadata");
            let metadata: HashMap<String, String> = metadata_json
                .as_object()
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let config_version: Option<String> = r.get("current_config_version");
            RegisteredNode::from_peer(&peer, hostname, software_version, metadata, config_version)
        }))
    }
}

fn row_to_node_peer(r: &sqlx::postgres::PgRow) -> NodePeer {
    let role_str: String = r.get("role");
    let status_str: String = r.get("status");
    NodePeer {
        node_id: NodeId(r.get("node_id")),
        role: match role_str.as_str() {
            "controller" => NodeRole::Controller,
            "worker" => NodeRole::Worker,
            "hybrid" => NodeRole::Hybrid,
            _ => NodeRole::Worker,
        },
        public_key: r.get("public_key"),
        capabilities: NodeCapabilityAdvertisement {
            gpu_count: r.get::<i32, _>("gpu_count") as u32,
            vram_gb: r.get::<i32, _>("vram_gb") as u32,
            cpu_cores: r.get::<i32, _>("cpu_cores") as u32,
            available_memory_gb: r.get::<i32, _>("available_mem_gb") as u32,
            supported_runtimes: r.get("supported_runtimes"),
            tags: r.get("tags"),
        },
        grpc_address: r.get("grpc_address"),
        status: match status_str.as_str() {
            "active" => NodePeerStatus::Active,
            "draining" => NodePeerStatus::Draining,
            "unhealthy" => NodePeerStatus::Unhealthy,
            _ => NodePeerStatus::Active,
        },
        last_heartbeat_at: r.get("last_heartbeat_at"),
        registered_at: r.get("registered_at"),
    }
}

pub struct PgNodeChallengeRepository {
    pool: PgPool,
}

impl PgNodeChallengeRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl NodeChallengeRepository for PgNodeChallengeRepository {
    async fn save_challenge(&self, challenge: &NodeChallenge) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            INSERT INTO node_challenges (
                challenge_id, node_id, nonce, public_key, role,
                gpu_count, vram_gb, cpu_cores, available_mem_gb,
                supported_runtimes, tags, grpc_address, created_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            "#,
        )
        .bind(challenge.challenge_id)
        .bind(challenge.node_id.0)
        .bind(&challenge.nonce)
        .bind(&challenge.public_key)
        .bind(format!("{:?}", challenge.role).to_lowercase())
        .bind(challenge.capabilities.gpu_count as i32)
        .bind(challenge.capabilities.vram_gb as i32)
        .bind(challenge.capabilities.cpu_cores as i32)
        .bind(challenge.capabilities.available_memory_gb as i32)
        .bind(&challenge.capabilities.supported_runtimes)
        .bind(&challenge.capabilities.tags)
        .bind(&challenge.grpc_address)
        .bind(challenge.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_challenge(&self, challenge_id: &Uuid) -> anyhow::Result<Option<NodeChallenge>> {
        // Clean up expired challenges (TTL: 5 minutes)
        sqlx::query("DELETE FROM node_challenges WHERE created_at < NOW() - INTERVAL '5 minutes'")
            .execute(&self.pool)
            .await?;

        let row = sqlx::query(
            r#"
            SELECT challenge_id, node_id, nonce, public_key, role,
                   gpu_count, vram_gb, cpu_cores, available_mem_gb,
                   supported_runtimes, tags, grpc_address, created_at
            FROM node_challenges
            WHERE challenge_id = $1
            "#,
        )
        .bind(challenge_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            let role_str: String = r.get("role");
            NodeChallenge {
                challenge_id: r.get("challenge_id"),
                node_id: NodeId(r.get("node_id")),
                nonce: r.get("nonce"),
                public_key: r.get("public_key"),
                role: match role_str.as_str() {
                    "controller" => NodeRole::Controller,
                    "worker" => NodeRole::Worker,
                    "hybrid" => NodeRole::Hybrid,
                    _ => NodeRole::Worker,
                },
                capabilities: NodeCapabilityAdvertisement {
                    gpu_count: r.get::<i32, _>("gpu_count") as u32,
                    vram_gb: r.get::<i32, _>("vram_gb") as u32,
                    cpu_cores: r.get::<i32, _>("cpu_cores") as u32,
                    available_memory_gb: r.get::<i32, _>("available_mem_gb") as u32,
                    supported_runtimes: r.get("supported_runtimes"),
                    tags: r.get("tags"),
                },
                grpc_address: r.get("grpc_address"),
                created_at: r.get("created_at"),
            }
        }))
    }

    async fn delete_challenge(&self, challenge_id: &Uuid) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM node_challenges WHERE challenge_id = $1")
            .bind(challenge_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

pub struct PgStimulusIdempotencyRepository {
    pool: PgPool,
}

impl PgStimulusIdempotencyRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl StimulusIdempotencyRepository for PgStimulusIdempotencyRepository {
    async fn check_and_insert(&self, id: &StimulusId) -> anyhow::Result<bool> {
        let res = sqlx::query(
            "INSERT INTO stimulus_idempotency (id) VALUES ($1) ON CONFLICT (id) DO NOTHING",
        )
        .bind(id.0)
        .execute(&self.pool)
        .await?;

        Ok(res.rows_affected() > 0)
    }
}
