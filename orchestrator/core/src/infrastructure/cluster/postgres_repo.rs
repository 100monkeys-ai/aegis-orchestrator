// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Cluster Repository
//!
//! Production `NodeClusterRepository`, `NodeChallengeRepository`, and
//! `StimulusIdempotencyRepository` implementations backed by PostgreSQL.

use crate::domain::cluster::{
    NodeClusterRepository, NodeChallengeRepository, NodePeer, NodeId, NodePeerStatus,
    ResourceSnapshot, NodeChallenge, NodeCapabilityAdvertisement, NodeRole,
};
use crate::domain::stimulus::StimulusId;
use crate::domain::cluster::StimulusIdempotencyRepository;
use sqlx::postgres::PgPool;
use async_trait::async_trait;
use uuid::Uuid;
use chrono::Utc;

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
        sqlx::query!(
            r#"
            INSERT INTO cluster_nodes (
                node_id, role, public_key, grpc_address, status,
                gpu_count, vram_gb, cpu_cores, available_mem_gb,
                supported_runtimes, tags, registered_at, last_heartbeat_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
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
                last_heartbeat_at = EXCLUDED.last_heartbeat_at
            "#,
            peer.node_id.0,
            format!("{:?}", peer.role).to_lowercase(),
            peer.public_key,
            peer.grpc_address,
            format!("{:?}", peer.status).to_lowercase(),
            peer.capabilities.gpu_count as i32,
            peer.capabilities.vram_gb as i32,
            peer.capabilities.cpu_cores as i32,
            peer.capabilities.available_memory_gb as i32,
            &peer.capabilities.supported_runtimes,
            &peer.capabilities.tags,
            peer.registered_at,
            peer.last_heartbeat_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_peer(&self, node_id: &NodeId) -> anyhow::Result<Option<NodePeer>> {
        let row = sqlx::query!(
            r#"
            SELECT node_id, role, grpc_address, status, gpu_count, vram_gb, 
                   cpu_cores, available_mem_gb, supported_runtimes, tags,
                   last_heartbeat_at, registered_at
            FROM cluster_nodes
            WHERE node_id = $1
            "#,
            node_id.0
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| NodePeer {
            node_id: NodeId(r.node_id),
            role: match r.role.as_str() {
                "controller" => NodeRole::Controller,
                "worker" => NodeRole::Worker,
                "hybrid" => NodeRole::Hybrid,
                _ => NodeRole::Worker,
            },
            capabilities: NodeCapabilityAdvertisement {
                gpu_count: r.gpu_count as u32,
                vram_gb: r.vram_gb as u32,
                cpu_cores: r.cpu_cores as u32,
                available_memory_gb: r.available_mem_gb as u32,
                supported_runtimes: r.supported_runtimes,
                tags: r.tags,
            },
            grpc_address: r.grpc_address,
            status: match r.status.as_str() {
                "active" => NodePeerStatus::Active,
                "draining" => NodePeerStatus::Draining,
                "unhealthy" => NodePeerStatus::Unhealthy,
                _ => NodePeerStatus::Active,
            },
            last_heartbeat_at: r.last_heartbeat_at,
            registered_at: r.registered_at,
        }))
    }

    async fn list_peers_by_status(&self, status: NodePeerStatus) -> anyhow::Result<Vec<NodePeer>> {
        let status_str = format!("{:?}", status).to_lowercase();
        let rows = sqlx::query!(
            r#"
            SELECT node_id, role, grpc_address, status, gpu_count, vram_gb, 
                   cpu_cores, available_mem_gb, supported_runtimes, tags,
                   last_heartbeat_at, registered_at
            FROM cluster_nodes
            WHERE status = $1
            "#,
            status_str
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| NodePeer {
            node_id: NodeId(r.node_id),
            role: match r.role.as_str() {
                "controller" => NodeRole::Controller,
                "worker" => NodeRole::Worker,
                "hybrid" => NodeRole::Hybrid,
                _ => NodeRole::Worker,
            },
            capabilities: NodeCapabilityAdvertisement {
                gpu_count: r.gpu_count as u32,
                vram_gb: r.vram_gb as u32,
                cpu_cores: r.cpu_cores as u32,
                available_memory_gb: r.available_mem_gb as u32,
                supported_runtimes: r.supported_runtimes,
                tags: r.tags,
            },
            grpc_address: r.grpc_address,
            status: match r.status.as_str() {
                "active" => NodePeerStatus::Active,
                "draining" => NodePeerStatus::Draining,
                "unhealthy" => NodePeerStatus::Unhealthy,
                _ => NodePeerStatus::Active,
            },
            last_heartbeat_at: r.last_heartbeat_at,
            registered_at: r.registered_at,
        }).collect())
    }

    async fn record_heartbeat(&self, node_id: &NodeId, snapshot: ResourceSnapshot) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            UPDATE cluster_nodes
            SET last_heartbeat_at = NOW(),
                cpu_utilization_percent = $2,
                gpu_utilization_percent = $3,
                active_executions = $4
            WHERE node_id = $1
            "#,
            node_id.0,
            snapshot.cpu_utilization,
            snapshot.gpu_utilization,
            snapshot.active_executions as i32,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_unhealthy(&self, node_id: &NodeId) -> anyhow::Result<()> {
        sqlx::query!(
            "UPDATE cluster_nodes SET status = 'unhealthy' WHERE node_id = $1",
            node_id.0
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn deregister(&self, node_id: &NodeId, _reason: &str) -> anyhow::Result<()> {
        sqlx::query!(
            "DELETE FROM cluster_nodes WHERE node_id = $1",
            node_id.0
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_config_version(&self, node_id: &NodeId) -> anyhow::Result<Option<String>> {
        let row = sqlx::query!(
            "SELECT current_config_version FROM cluster_nodes WHERE node_id = $1",
            node_id.0
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|r| r.current_config_version))
    }

    async fn record_config_version(&self, node_id: &NodeId, hash: &str) -> anyhow::Result<()> {
        sqlx::query!(
            "UPDATE cluster_nodes SET current_config_version = $2 WHERE node_id = $1",
            node_id.0,
            hash
        )
        .execute(&self.pool)
        .await?;
        Ok(())
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
        sqlx::query!(
            r#"
            INSERT INTO node_challenges (
                challenge_id, node_id, nonce, public_key, role,
                gpu_count, vram_gb, cpu_cores, available_mem_gb,
                supported_runtimes, tags, grpc_address, created_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            "#,
            challenge.challenge_id,
            challenge.node_id.0,
            challenge.nonce,
            challenge.public_key,
            format!("{:?}", challenge.role).to_lowercase(),
            challenge.capabilities.gpu_count as i32,
            challenge.capabilities.vram_gb as i32,
            challenge.capabilities.cpu_cores as i32,
            challenge.capabilities.available_memory_gb as i32,
            &challenge.capabilities.supported_runtimes,
            &challenge.capabilities.tags,
            challenge.grpc_address,
            challenge.created_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_challenge(&self, challenge_id: &Uuid) -> anyhow::Result<Option<NodeChallenge>> {
        // First, clean up expired challenges (TTL: 5 minutes as per requirements, 
        // though 002_cluster.sql comment says 5 minutes and idx says it's for TTL cleanup)
        sqlx::query!(
            "DELETE FROM node_challenges WHERE created_at < NOW() - INTERVAL '5 minutes'"
        )
        .execute(&self.pool)
        .await?;

        let row = sqlx::query!(
            r#"
            SELECT challenge_id, node_id, nonce, public_key, role,
                   gpu_count, vram_gb, cpu_cores, available_mem_gb,
                   supported_runtimes, tags, grpc_address, created_at
            FROM node_challenges
            WHERE challenge_id = $1
            "#,
            challenge_id
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| NodeChallenge {
            challenge_id: r.challenge_id,
            node_id: NodeId(r.node_id),
            nonce: r.nonce,
            public_key: r.public_key,
            role: match r.role.as_str() {
                "controller" => NodeRole::Controller,
                "worker" => NodeRole::Worker,
                "hybrid" => NodeRole::Hybrid,
                _ => NodeRole::Worker,
            },
            capabilities: NodeCapabilityAdvertisement {
                gpu_count: r.gpu_count as u32,
                vram_gb: r.vram_gb as u32,
                cpu_cores: r.cpu_cores as u32,
                available_memory_gb: r.available_mem_gb as u32,
                supported_runtimes: r.supported_runtimes,
                tags: r.tags,
            },
            grpc_address: r.grpc_address,
            created_at: r.created_at,
        }))
    }

    async fn delete_challenge(&self, challenge_id: &Uuid) -> anyhow::Result<()> {
        sqlx::query!(
            "DELETE FROM node_challenges WHERE challenge_id = $1",
            challenge_id
        )
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
        let res = sqlx::query!(
            "INSERT INTO stimulus_idempotency (id) VALUES ($1) ON CONFLICT (id) DO NOTHING",
            id.0
        )
        .execute(&self.pool)
        .await?;

        Ok(res.rows_affected() > 0)
    }
}
