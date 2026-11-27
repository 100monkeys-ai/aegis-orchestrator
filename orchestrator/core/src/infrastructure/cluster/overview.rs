// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The operator's view of the cluster (ADR-152): members with their leases,
//! load and edge streams, the duties and who holds them, and executions by
//! owner. Read-only, one round of queries per call.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Row};

pub const MEMBERS_SQL: &str = r#"
SELECT n.node_id, n.display_name, n.role, n.status, n.incarnation, n.vrack_address,
       n.grpc_address, n.max_executions, n.lease_expires_at, n.last_renewed_at,
       COALESCE(n.lease_expires_at > now(), false) AS live,
       (SELECT count(*) FROM executions e
          WHERE e.owner_node_id = n.node_id AND e.status IN ('pending', 'running')) AS running,
       (SELECT count(*) FROM executions e
          WHERE e.owner_node_id = n.node_id AND e.status = 'completed'
            AND e.started_at > now() - interval '24 hours') AS completed_24h,
       (SELECT count(*) FROM executions e
          WHERE e.owner_node_id = n.node_id AND e.status IN ('failed', 'cancelled')
            AND e.started_at > now() - interval '24 hours') AS failed_24h,
       (SELECT count(*) FROM edge_stream_owners s WHERE s.member_node_id = n.node_id) AS edge_streams
FROM cluster_nodes n
ORDER BY n.vrack_address NULLS LAST, n.node_id
"#;

pub const DUTIES_SQL: &str = r#"
SELECT d.duty, d.holder_node_id, d.epoch, d.lease_expires_at,
       COALESCE(d.lease_expires_at > now(), false) AS held
FROM cluster_duties d
ORDER BY d.duty
"#;

pub const EXECUTIONS_SQL: &str = r#"
SELECT status, count(*) AS n FROM executions
WHERE started_at > now() - interval '24 hours'
GROUP BY status
"#;

#[derive(Debug, Clone, Serialize)]
pub struct MemberView {
    pub node_id: String,
    pub name: Option<String>,
    pub role: String,
    pub status: String,
    pub live: bool,
    pub incarnation: i64,
    pub vrack_address: Option<String>,
    pub grpc_address: String,
    pub max_executions: Option<i32>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub last_renewed_at: Option<DateTime<Utc>>,
    pub running_executions: i64,
    pub completed_24h: i64,
    pub failed_24h: i64,
    pub edge_streams: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DutyView {
    pub duty: String,
    pub holder_node_id: Option<String>,
    pub epoch: i64,
    pub held: bool,
    pub lease_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterOverview {
    pub generated_at: DateTime<Utc>,
    pub members: Vec<MemberView>,
    pub duties: Vec<DutyView>,
    /// Executions started in the last 24 h, by status.
    pub executions_24h: std::collections::BTreeMap<String, i64>,
}

pub struct PgClusterOverview {
    pool: PgPool,
}

impl PgClusterOverview {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn overview(&self) -> Result<ClusterOverview> {
        let members = sqlx::query(MEMBERS_SQL)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| MemberView {
                node_id: r.get::<uuid::Uuid, _>("node_id").to_string(),
                name: r.get("display_name"),
                role: r.get("role"),
                status: r.get("status"),
                live: r.get("live"),
                incarnation: r.get("incarnation"),
                vrack_address: r.get("vrack_address"),
                grpc_address: r.get("grpc_address"),
                max_executions: r.get("max_executions"),
                lease_expires_at: r.get("lease_expires_at"),
                last_renewed_at: r.get("last_renewed_at"),
                running_executions: r.get("running"),
                completed_24h: r.get("completed_24h"),
                failed_24h: r.get("failed_24h"),
                edge_streams: r.get("edge_streams"),
            })
            .collect();
        let duties = sqlx::query(DUTIES_SQL)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| DutyView {
                duty: r.get("duty"),
                holder_node_id: r
                    .get::<Option<uuid::Uuid>, _>("holder_node_id")
                    .map(|u| u.to_string()),
                epoch: r.get("epoch"),
                held: r.get("held"),
                lease_expires_at: r.get("lease_expires_at"),
            })
            .collect();
        let executions_24h = sqlx::query(EXECUTIONS_SQL)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| (r.get::<String, _>("status"), r.get::<i64, _>("n")))
            .collect();
        Ok(ClusterOverview {
            generated_at: Utc::now(),
            members,
            duties,
            executions_24h,
        })
    }
}

/// The graphs the operator views draw (ADR-152): each a fixed PromQL
/// expression over the cluster's Prometheus, grouped by member. Callers
/// name a series; no caller-supplied PromQL is ever evaluated.
pub const METRIC_SERIES: &[(&str, &str)] = &[
    (
        "executions_active",
        "sum by (member) (aegis_execution_active)",
    ),
    (
        "executions_rate",
        "sum by (member) (rate(aegis_executions_total[5m]))",
    ),
    (
        "workflows_active",
        "sum by (member) (aegis_workflow_executions_active)",
    ),
    (
        "forwarded_rate",
        "sum by (member) (rate(aegis_cluster_executions_forwarded_total[5m]))",
    ),
    (
        "relayed_events_rate",
        "sum by (member) (rate(aegis_cluster_events_relayed_in_total[5m]))",
    ),
    (
        "seal_sessions",
        "sum by (member) (aegis_seal_sessions_active)",
    ),
    (
        "lease_renewal_failures_rate",
        "sum by (member) (rate(aegis_cluster_lease_renewal_failures_total[5m]))",
    ),
];

/// The status matrix: every scrape target's `up`, by job and member.
pub const TARGETS_UP_QUERY: &str = "up";

pub fn series_query(name: &str) -> Option<&'static str> {
    METRIC_SERIES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, q)| *q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_named_series_are_evaluated() {
        assert_eq!(
            series_query("executions_active"),
            Some("sum by (member) (aegis_execution_active)")
        );
        assert_eq!(
            series_query("up or vector(1)"),
            None,
            "caller PromQL is never evaluated"
        );
        for (name, promql) in METRIC_SERIES {
            assert!(
                promql.contains("by (member)"),
                "{name} is grouped by member"
            );
        }
    }
}
