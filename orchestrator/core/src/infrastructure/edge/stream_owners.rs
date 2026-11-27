// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Which cluster member holds each edge daemon's stream (AEGIS ADR-152 D8).

use anyhow::Result;
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::shared_kernel::NodeId;

/// The record of which member's Relay Coordinator holds an edge's stream.
#[async_trait]
pub trait EdgeStreamOwners: Send + Sync {
    /// A stream opened on `member`'s replica.
    async fn record(&self, edge: NodeId, member: NodeId, stream: Uuid) -> Result<()>;
    /// That stream closed: its row goes, unless a newer stream replaced it.
    async fn release(&self, edge: NodeId, stream: Uuid) -> Result<()>;
    /// The member holding the edge's stream, if any.
    async fn owner_of(&self, edge: NodeId) -> Result<Option<NodeId>>;
    /// A dead member's rows go (its streams died with it).
    /// Every edge with a recorded stream (ADR-152 D8).
    async fn connected_edges(&self) -> Result<Vec<NodeId>>;

    async fn release_member(&self, member: NodeId) -> Result<u64>;
}

pub const RECORD_STREAM_SQL: &str = r#"
INSERT INTO edge_stream_owners (edge_node_id, member_node_id, stream_id, connected_at)
VALUES ($1, $2, $3, now())
ON CONFLICT (edge_node_id) DO UPDATE
SET member_node_id = EXCLUDED.member_node_id, stream_id = EXCLUDED.stream_id, connected_at = now()
"#;
pub const RELEASE_STREAM_SQL: &str =
    "DELETE FROM edge_stream_owners WHERE edge_node_id = $1 AND stream_id = $2";
pub const CONNECTED_EDGES_SQL: &str = "SELECT edge_node_id FROM edge_stream_owners";
pub const RELEASE_MEMBER_SQL: &str = "DELETE FROM edge_stream_owners WHERE member_node_id = $1";
const OWNER_OF_SQL: &str = "SELECT member_node_id FROM edge_stream_owners WHERE edge_node_id = $1";

pub struct PgEdgeStreamOwners {
    pool: PgPool,
}

impl PgEdgeStreamOwners {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl EdgeStreamOwners for PgEdgeStreamOwners {
    async fn record(&self, edge: NodeId, member: NodeId, stream: Uuid) -> Result<()> {
        sqlx::query(RECORD_STREAM_SQL)
            .bind(edge.0)
            .bind(member.0)
            .bind(stream)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn release(&self, edge: NodeId, stream: Uuid) -> Result<()> {
        sqlx::query(RELEASE_STREAM_SQL)
            .bind(edge.0)
            .bind(stream)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn owner_of(&self, edge: NodeId) -> Result<Option<NodeId>> {
        let member: Option<Uuid> = sqlx::query_scalar(OWNER_OF_SQL)
            .bind(edge.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(member.map(NodeId))
    }

    async fn connected_edges(&self) -> Result<Vec<NodeId>> {
        let ids: Vec<uuid::Uuid> = sqlx::query_scalar(CONNECTED_EDGES_SQL)
            .fetch_all(&self.pool)
            .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    async fn release_member(&self, member: NodeId) -> Result<u64> {
        Ok(sqlx::query(RELEASE_MEMBER_SQL)
            .bind(member.0)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}
