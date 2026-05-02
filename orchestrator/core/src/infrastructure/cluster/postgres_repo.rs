// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Cluster Repository
//!
//! Production `NodeClusterRepository`, `NodeChallengeRepository`, and
//! `StimulusIdempotencyRepository` implementations backed by PostgreSQL.

use crate::domain::cluster::NodeChallenge;
use crate::domain::cluster::{
    NodeCapabilityAdvertisement, NodeChallengeRepository, NodeClusterRepository, NodeId, NodePeer,
    NodePeerStatus, NodeRole, ResourceSnapshot, StimulusIdempotencyRepository,
};
use crate::domain::stimulus::StimulusId;
use anyhow::anyhow;
use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;

/// Parse the lowercased `Debug` form of [`NodeRole`] back into the enum.
///
/// The serializer side uses `format!("{:?}", role).to_lowercase()`, which
/// produces `"controller"`, `"worker"`, `"hybrid"`, `"edge"`, and
/// `"relaycoordinator"` (no underscore — `Debug` prints `RelayCoordinator`
/// and `to_lowercase()` collapses it). Any other input means stale or
/// corrupted data; we return an error rather than silently downgrading
/// to `Worker`, which previously caused Edge daemons to be persisted into
/// `cluster_nodes` (worker tier) instead of `edge_daemons`.
pub(crate) fn parse_role_str(s: &str) -> anyhow::Result<NodeRole> {
    match s {
        "controller" => Ok(NodeRole::Controller),
        "worker" => Ok(NodeRole::Worker),
        "hybrid" => Ok(NodeRole::Hybrid),
        "edge" => Ok(NodeRole::Edge),
        "relaycoordinator" => Ok(NodeRole::RelayCoordinator),
        other => Err(anyhow!(
            "unknown NodeRole serialization in database: {other:?} \
             (expected one of: controller, worker, hybrid, edge, relaycoordinator)"
        )),
    }
}

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

        row.map(|r| row_to_node_peer(&r)).transpose()
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

        rows.iter().map(row_to_node_peer).collect()
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

    async fn start_drain(&self, node_id: &NodeId) -> anyhow::Result<()> {
        sqlx::query("UPDATE cluster_nodes SET status = 'draining' WHERE node_id = $1")
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
            sqlx::query("SELECT current_config_version FROM registered_nodes WHERE node_id = $1")
                .bind(node_id.0)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|r| r.get::<Option<String>, _>("current_config_version")))
    }

    async fn record_config_version(&self, node_id: &NodeId, hash: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE registered_nodes SET current_config_version = $2 WHERE node_id = $1")
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

        rows.iter().map(row_to_node_peer).collect()
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
}

fn row_to_node_peer(r: &sqlx::postgres::PgRow) -> anyhow::Result<NodePeer> {
    let role_str: String = r.get("role");
    let status_str: String = r.get("status");
    Ok(NodePeer {
        node_id: NodeId(r.get("node_id")),
        role: parse_role_str(&role_str)?,
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
            other => {
                return Err(anyhow!(
                    "unknown NodePeerStatus serialization in database: {other:?} \
                     (expected one of: active, draining, unhealthy)"
                ));
            }
        },
        last_heartbeat_at: r.get("last_heartbeat_at"),
        registered_at: r.get("registered_at"),
    })
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

        row.map(|r| {
            let role_str: String = r.get("role");
            Ok(NodeChallenge {
                challenge_id: r.get("challenge_id"),
                node_id: NodeId(r.get("node_id")),
                nonce: r.get("nonce"),
                public_key: r.get("public_key"),
                role: parse_role_str(&role_str)?,
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
            })
        })
        .transpose()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip every `NodeRole` variant through the serializer
    /// (`format!("{:?}", role).to_lowercase()`) and the deserializer
    /// (`parse_role_str`). Pins the bug where `Edge` and `RelayCoordinator`
    /// silently downgraded to `Worker` on load — Edge daemons completed
    /// AttestNode, the challenge persisted with `role = "edge"`, but the
    /// loader's `_ => NodeRole::Worker` fallback bypassed the Edge gate in
    /// `ChallengeNodeUseCase::execute` and dropped the daemon into
    /// `cluster_nodes` instead of `edge_daemons`.
    #[test]
    fn parse_role_str_round_trip_all_variants() {
        for role in [
            NodeRole::Controller,
            NodeRole::Worker,
            NodeRole::Hybrid,
            NodeRole::Edge,
            NodeRole::RelayCoordinator,
        ] {
            let serialized = format!("{:?}", role).to_lowercase();
            let parsed = parse_role_str(&serialized).unwrap_or_else(|e| {
                panic!("round-trip failed for {role:?} -> {serialized:?}: {e}")
            });
            assert_eq!(
                parsed, role,
                "round-trip mismatch for {role:?}: serialized as {serialized:?} but parsed as {parsed:?}"
            );
        }
    }

    /// Edge specifically — this is the variant the orchestrator gate keys on.
    #[test]
    fn parse_role_str_edge_maps_to_edge_not_worker() {
        assert_eq!(parse_role_str("edge").unwrap(), NodeRole::Edge);
    }

    /// `RelayCoordinator` Debug-prints as `"RelayCoordinator"`, which
    /// `.to_lowercase()` collapses to `"relaycoordinator"` — no underscore.
    /// The deserializer arm must match that exact spelling.
    #[test]
    fn parse_role_str_relay_coordinator_serializes_without_underscore() {
        let serialized = format!("{:?}", NodeRole::RelayCoordinator).to_lowercase();
        assert_eq!(serialized, "relaycoordinator");
        assert_eq!(
            parse_role_str(&serialized).unwrap(),
            NodeRole::RelayCoordinator
        );
    }

    /// Unknown role strings used to silently become `Worker`. They now
    /// surface as a hard error so corrupted DB rows get caught instead of
    /// being misrouted into the worker tier.
    #[test]
    fn parse_role_str_unknown_returns_error() {
        let err = parse_role_str("relay_coordinator").expect_err(
            "underscored form is NOT what the serializer emits and must not be accepted",
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("relay_coordinator"),
            "error should quote the offending value, got: {msg}"
        );

        parse_role_str("garbage").expect_err("garbage role string must error");
        parse_role_str("").expect_err("empty role string must error");
    }

    /// Regression test for the bug where the challenge loader downgraded
    /// `Edge` -> `Worker`. We can't construct a real `PgRow` in unit tests,
    /// but we can exercise the exact serialize -> parse path the loader
    /// uses, asserting the role survives the round trip.
    #[test]
    fn edge_node_challenge_role_survives_db_round_trip() {
        // What `save_challenge` writes:
        let serialized = format!("{:?}", NodeRole::Edge).to_lowercase();
        assert_eq!(serialized, "edge");

        // What `get_challenge` reads:
        let role = parse_role_str(&serialized).expect("edge must deserialize");
        assert_eq!(
            role,
            NodeRole::Edge,
            "Edge daemon role must survive DB round-trip; downgrade to Worker bypasses ChallengeNodeUseCase Edge gate"
        );
    }
}
