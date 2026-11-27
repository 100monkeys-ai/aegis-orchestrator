// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Membership leases and duty leases in PostgreSQL (ADR-152 D1, D2)
//!
//! Every statement here is one statement, so none needs a session: all of
//! them run through PgBouncer in transaction pooling mode. Every `UPDATE`
//! carries a `WHERE` clause, so `pg_strict` accepts them. All lease
//! arithmetic is the database's (`now()`); no node clock is compared with
//! another. The statements are public so the regression tests can check
//! the whole set, not a copy of it.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;

use crate::domain::cluster::NodeId;
use crate::domain::cluster_membership::{
    ClusterDutyName, ClusterDutyRepository, DutyEpoch, LeaseSuperseded, NodeIncarnation, NodeJoin,
    NodeLease, NodeLeaseRepository, NodeProcess,
};

/// Insert this node's row, or increment the incarnation of the row it has;
/// either way grant the first lease. A process starting is active.
pub const JOIN_SQL: &str = r#"
INSERT INTO cluster_nodes (
    node_id, role, public_key, grpc_address, status, vrack_address,
    incarnation, lease_expires_at, last_renewed_at, last_heartbeat_at,
    max_executions
) VALUES (
    $1, $2, $3, $4, 'active', $5,
    1, now() + make_interval(secs => $6), now(), now(),
    $7
)
ON CONFLICT (node_id) DO UPDATE SET
    role              = EXCLUDED.role,
    public_key        = EXCLUDED.public_key,
    grpc_address      = EXCLUDED.grpc_address,
    vrack_address     = EXCLUDED.vrack_address,
    status            = 'active',
    incarnation       = cluster_nodes.incarnation + 1,
    lease_expires_at  = EXCLUDED.lease_expires_at,
    last_renewed_at   = EXCLUDED.last_renewed_at,
    last_heartbeat_at = EXCLUDED.last_heartbeat_at,
    max_executions    = EXCLUDED.max_executions
RETURNING incarnation
"#;

/// Extend this process's lease. A renewal is proof the node reaches the
/// database, so a node the health sweep marked unhealthy is active again;
/// a draining node stays draining.
pub const RENEW_SQL: &str = r#"
UPDATE cluster_nodes SET
    lease_expires_at  = now() + make_interval(secs => $3),
    last_renewed_at   = now(),
    last_heartbeat_at = now(),
    status            = CASE WHEN status = 'unhealthy' THEN 'active' ELSE status END
WHERE node_id = $1 AND incarnation = $2
RETURNING last_renewed_at
"#;

/// Every node's lease, judged by the database clock. Edge daemons live in
/// `edge_daemons`, never here, so every row is a peer.
pub const LEASES_SQL: &str = r#"
SELECT node_id, incarnation, lease_expires_at, last_renewed_at, vrack_address,
       grpc_address, COALESCE(lease_expires_at > now(), false) AS live
FROM cluster_nodes
ORDER BY node_id
"#;

/// Create a duty's row if it has none.
pub const ENSURE_DUTY_SQL: &str = r#"
INSERT INTO cluster_duties (duty) VALUES ($1) ON CONFLICT (duty) DO NOTHING
"#;

/// Acquire or renew (ADR-152 D2.2). The duty is taken when it is unheld,
/// when its lease has run out, or when it is held by an older incarnation of
/// this same node (a process that has already been replaced). The epoch
/// moves on every change of holder and never on a renewal; `SET`
/// expressions read the row as it was.
pub const ACQUIRE_DUTY_SQL: &str = r#"
UPDATE cluster_duties SET
    epoch = CASE
        WHEN holder_node_id = $2 AND holder_incarnation = $3 THEN epoch
        ELSE epoch + 1
    END,
    holder_node_id     = $2,
    holder_incarnation = $3,
    lease_expires_at   = now() + make_interval(secs => $4)
WHERE duty = $1
  AND (
        holder_node_id IS NULL
     OR lease_expires_at IS NULL
     OR lease_expires_at < now()
     OR (holder_node_id = $2 AND holder_incarnation <= $3)
  )
RETURNING epoch
"#;

/// Give a duty up, only under the epoch it was held at.
pub const RELEASE_DUTY_SQL: &str = r#"
UPDATE cluster_duties SET
    holder_node_id     = NULL,
    holder_incarnation = NULL,
    lease_expires_at   = NULL
WHERE duty = $1 AND holder_node_id = $2 AND holder_incarnation = $3 AND epoch = $4
"#;

/// Every statement of this module, for the `pg_strict` regression test.
pub const STATEMENTS: [(&str, &str); 6] = [
    ("join", JOIN_SQL),
    ("renew", RENEW_SQL),
    ("leases", LEASES_SQL),
    ("ensure_duty", ENSURE_DUTY_SQL),
    ("acquire_duty", ACQUIRE_DUTY_SQL),
    ("release_duty", RELEASE_DUTY_SQL),
];

/// The fence a duty's write carries (ADR-152 D2.3): true only while the
/// duty is still at the epoch the writer was granted. `duty` and `epoch`
/// are the statement's parameter numbers.
pub fn under_duty_epoch(duty: usize, epoch: usize) -> String {
    format!("(SELECT epoch FROM cluster_duties WHERE duty = ${duty}) = ${epoch}")
}

fn seconds(ttl: Duration) -> f64 {
    ttl.as_secs_f64()
}

pub struct PgNodeLeaseRepository {
    pool: PgPool,
}

impl PgNodeLeaseRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl NodeLeaseRepository for PgNodeLeaseRepository {
    async fn join(&self, join: &NodeJoin) -> anyhow::Result<NodeIncarnation> {
        let incarnation: i64 = sqlx::query_scalar(JOIN_SQL)
            .bind(join.node_id.0)
            .bind(&join.role)
            .bind(&join.public_key)
            .bind(&join.grpc_address)
            .bind(&join.vrack_address)
            .bind(seconds(join.lease_ttl))
            .bind(join.max_executions.map(|n| n as i32))
            .fetch_one(&self.pool)
            .await?;
        Ok(NodeIncarnation(incarnation))
    }

    async fn renew(
        &self,
        process: NodeProcess,
        ttl: Duration,
    ) -> anyhow::Result<Result<DateTime<Utc>, LeaseSuperseded>> {
        let renewed: Option<DateTime<Utc>> = sqlx::query_scalar(RENEW_SQL)
            .bind(process.node_id.0)
            .bind(process.incarnation.0)
            .bind(seconds(ttl))
            .fetch_optional(&self.pool)
            .await?;
        Ok(renewed.ok_or(LeaseSuperseded {
            node_id: process.node_id,
            incarnation: process.incarnation,
        }))
    }

    async fn leases(&self) -> anyhow::Result<Vec<NodeLease>> {
        let rows = sqlx::query(LEASES_SQL).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok(NodeLease {
                    node_id: NodeId(row.try_get("node_id")?),
                    incarnation: NodeIncarnation(row.try_get("incarnation")?),
                    live: row.try_get("live")?,
                    lease_expires_at: row.try_get("lease_expires_at")?,
                    last_renewed_at: row.try_get("last_renewed_at")?,
                    vrack_address: row.try_get("vrack_address")?,
                    grpc_address: row.try_get("grpc_address")?,
                })
            })
            .collect()
    }
}

pub struct PgClusterDutyRepository {
    pool: PgPool,
}

impl PgClusterDutyRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ClusterDutyRepository for PgClusterDutyRepository {
    async fn ensure(&self, duty: ClusterDutyName) -> anyhow::Result<()> {
        sqlx::query(ENSURE_DUTY_SQL)
            .bind(duty.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn try_acquire(
        &self,
        duty: ClusterDutyName,
        holder: NodeProcess,
        ttl: Duration,
    ) -> anyhow::Result<Option<DutyEpoch>> {
        let epoch: Option<i64> = sqlx::query_scalar(ACQUIRE_DUTY_SQL)
            .bind(duty.as_str())
            .bind(holder.node_id.0)
            .bind(holder.incarnation.0)
            .bind(seconds(ttl))
            .fetch_optional(&self.pool)
            .await?;
        Ok(epoch.map(DutyEpoch))
    }

    async fn release(
        &self,
        duty: ClusterDutyName,
        holder: NodeProcess,
        epoch: DutyEpoch,
    ) -> anyhow::Result<bool> {
        let done = sqlx::query(RELEASE_DUTY_SQL)
            .bind(duty.as_str())
            .bind(holder.node_id.0)
            .bind(holder.incarnation.0)
            .bind(epoch.0)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() == 1)
    }
}

/// How many unfinished executions a node owns (ADR-152 D5.3).
pub const LOAD_OF_SQL: &str = r#"
SELECT count(*) FROM executions
WHERE owner_node_id = $1 AND status IN ('pending', 'running')
"#;

/// The live, active peers with headroom, most first (ADR-152 D5.3): a lease
/// valid by the database clock, an advertised capacity, and fewer
/// unfinished owned executions than that capacity.
pub const PLACEMENT_CANDIDATES_SQL: &str = r#"
SELECT n.node_id, n.grpc_address,
       n.max_executions - count(e.id) AS headroom
FROM cluster_nodes n
LEFT JOIN executions e
  ON e.owner_node_id = n.node_id AND e.status IN ('pending', 'running')
WHERE n.node_id <> $1
  AND n.status = 'active'
  AND n.lease_expires_at > now()
  AND n.max_executions IS NOT NULL
GROUP BY n.node_id, n.grpc_address, n.max_executions
HAVING n.max_executions - count(e.id) > 0
ORDER BY headroom DESC, n.node_id
"#;

pub struct PgPlacementRepository {
    pool: PgPool,
}

impl PgPlacementRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl crate::domain::cluster_membership::PlacementRepository for PgPlacementRepository {
    async fn load_of(&self, node: crate::domain::cluster::NodeId) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(LOAD_OF_SQL)
            .bind(node.0)
            .fetch_one(&self.pool)
            .await?)
    }

    async fn candidates(
        &self,
        me: crate::domain::cluster::NodeId,
    ) -> anyhow::Result<Vec<crate::domain::cluster_membership::PlacementCandidate>> {
        let rows: Vec<(uuid::Uuid, String, i64)> = sqlx::query_as(PLACEMENT_CANDIDATES_SQL)
            .bind(me.0)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id, grpc_address, headroom)| {
                crate::domain::cluster_membership::PlacementCandidate {
                    node_id: crate::domain::cluster::NodeId(id),
                    grpc_address,
                    headroom,
                }
            })
            .collect())
    }
}
