// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Membership leases and cluster duties against a real PostgreSQL (AEGIS
//! ADR-152 D1, D2; migrations 057 and 058).
//!
//! Every statement runs through PgBouncer in transaction pooling mode, as
//! the deployment runs it (PlanetScale's 6432): nothing in the membership
//! path may need a session. CI runs one PgBouncer in that mode in front of a
//! PostgreSQL and sets `AEGIS_TEST_PGBOUNCER_URL` and
//! `AEGIS_TEST_POSTGRES_URL`; without them the database tests do nothing
//! and say so, and in CI they fail instead. Each test makes its own
//! database, applies every migration this binary ships, and drops it.
//!
//! `pg_strict` (PlanetScale) refuses an UPDATE or DELETE without a WHERE
//! clause. It is not active on the database today; the first test guards
//! against it being enabled, over every statement the membership code and
//! its migrations issue.

use std::sync::Arc;
use std::time::Duration;

use aegis_orchestrator::daemon::migrations::{apply_migrations, MIGRATOR};
use aegis_orchestrator_core::domain::cluster::{NodeClusterRepository, NodeId};
use aegis_orchestrator_core::domain::cluster_membership::{
    ClusterDutyName, ClusterDutyRepository, DutyEpoch, DutyFence, NodeIncarnation, NodeJoin,
    NodeLeaseRepository, NodeProcess,
};
use aegis_orchestrator_core::infrastructure::cluster::membership_repo::STATEMENTS;
use aegis_orchestrator_core::infrastructure::cluster::postgres_repo::{
    mark_unhealthy_fenced_sql, MARK_UNHEALTHY_SQL,
};
use aegis_orchestrator_core::infrastructure::cluster::{
    PgClusterDutyRepository, PgNodeClusterRepository, PgNodeLeaseRepository,
};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

// ---------------------------------------------------------------------------
// pg_strict
// ---------------------------------------------------------------------------

/// The UPDATE or DELETE statements of `sql` that carry no WHERE clause.
/// Comments are not read; an `ON CONFLICT ... DO UPDATE` belongs to an
/// INSERT and is not a statement of its own.
fn unbounded_writes(sql: &str) -> Vec<String> {
    let code: String = sql
        .lines()
        .map(|l| l.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    code.split(';')
        .map(|s| s.split_whitespace().collect::<Vec<_>>())
        .filter(|w| matches!(w.first(), Some(&"update") | Some(&"delete")))
        .filter(|w| !w.contains(&"where"))
        .map(|w| w.join(" "))
        .collect()
}

#[test]
fn the_strict_check_catches_an_update_or_delete_without_where() {
    assert_eq!(unbounded_writes("UPDATE t SET a = 1;").len(), 1);
    assert_eq!(unbounded_writes("delete from t").len(), 1);
    assert!(unbounded_writes("UPDATE t SET a = 1 WHERE id = $1").is_empty());
    assert!(unbounded_writes("-- UPDATE t SET a = 1\nSELECT 1").is_empty());
    assert!(
        unbounded_writes("INSERT INTO t VALUES (1) ON CONFLICT (id) DO UPDATE SET a = 1")
            .is_empty()
    );
}

#[test]
fn every_membership_statement_passes_pg_strict() {
    let mut sources: Vec<(String, String)> = STATEMENTS
        .iter()
        .map(|(name, sql)| (name.to_string(), sql.to_string()))
        .collect();
    sources.push(("mark_unhealthy".into(), MARK_UNHEALTHY_SQL.into()));
    sources.push(("mark_unhealthy_fenced".into(), mark_unhealthy_fenced_sql()));
    for m in MIGRATOR
        .iter()
        .filter(|m| m.version == 57 || m.version == 58)
    {
        sources.push((format!("migration {}", m.version), m.sql.to_string()));
    }
    assert_eq!(
        sources.len(),
        10,
        "the membership statements and migrations 057 and 058"
    );
    let refused: Vec<String> = sources
        .iter()
        .flat_map(|(name, sql)| {
            unbounded_writes(sql)
                .into_iter()
                .map(move |s| format!("{name}: {s}"))
        })
        .collect();
    assert!(refused.is_empty(), "pg_strict refuses these: {refused:?}");
}

// ---------------------------------------------------------------------------
// The database through PgBouncer
// ---------------------------------------------------------------------------

fn url(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("{name} is not set; in CI these tests must reach PostgreSQL through PgBouncer")
        }
        _ => {
            eprintln!("skipped: {name} is not set");
            None
        }
    }
}

/// A migrated database of its own, reached through PgBouncer.
struct Db {
    pool: PgPool,
    server: PgPool,
    name: String,
}

impl Db {
    async fn new() -> Option<Self> {
        let postgres = url("AEGIS_TEST_POSTGRES_URL")?;
        let bouncer = url("AEGIS_TEST_PGBOUNCER_URL")?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&postgres)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_cluster_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options = bouncer
            .parse::<PgConnectOptions>()
            .expect("the PgBouncer URL parses")
            .database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect through PgBouncer");
        apply_migrations(&pool).await.expect("migrations apply");
        Some(Self { pool, server, name })
    }

    async fn drop(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("drop the test database");
    }
}

fn join(node_id: NodeId, ttl: Duration) -> NodeJoin {
    NodeJoin {
        node_id,
        role: "hybrid".to_string(),
        public_key: vec![9; 32],
        grpc_address: "10.120.0.7:50056".to_string(),
        vrack_address: Some("10.120.0.7".to_string()),
        lease_ttl: ttl,
    }
}

const TTL: Duration = Duration::from_secs(15);
/// A lease short enough for a test to watch run out by the database clock.
const SHORT: Duration = Duration::from_millis(400);

async fn process(leases: &PgNodeLeaseRepository) -> NodeProcess {
    let node_id = NodeId::new();
    let incarnation = leases.join(&join(node_id, TTL)).await.unwrap();
    NodeProcess {
        node_id,
        incarnation,
    }
}

/// A process start increments the incarnation; the older process's next
/// renewal finds it superseded, the newer one renews.
#[tokio::test]
async fn each_start_is_a_new_incarnation_and_supersedes_the_last() {
    let Some(db) = Db::new().await else { return };
    let leases = PgNodeLeaseRepository::new(db.pool.clone());
    let node_id = NodeId::new();

    let first = leases.join(&join(node_id, TTL)).await.unwrap();
    let second = leases.join(&join(node_id, TTL)).await.unwrap();
    assert_eq!((first, second), (NodeIncarnation(1), NodeIncarnation(2)));

    let old = NodeProcess {
        node_id,
        incarnation: first,
    };
    let new = NodeProcess {
        node_id,
        incarnation: second,
    };
    assert!(
        leases.renew(old, TTL).await.unwrap().is_err(),
        "the old process is superseded"
    );
    assert!(leases.renew(new, TTL).await.unwrap().is_ok());
    db.drop().await;
}

/// The lease runs out by the database clock, and a renewal brings it back.
#[tokio::test]
async fn a_lease_expires_by_the_database_clock() {
    let Some(db) = Db::new().await else { return };
    let leases = PgNodeLeaseRepository::new(db.pool.clone());
    let node_id = NodeId::new();
    let incarnation = leases.join(&join(node_id, SHORT)).await.unwrap();
    let live = |all: Vec<aegis_orchestrator_core::domain::cluster_membership::NodeLease>| {
        all.into_iter().find(|l| l.node_id == node_id).unwrap().live
    };
    assert!(live(leases.leases().await.unwrap()), "live after the join");
    tokio::time::sleep(SHORT + Duration::from_millis(300)).await;
    assert!(!live(leases.leases().await.unwrap()), "expired, unrenewed");
    leases
        .renew(
            NodeProcess {
                node_id,
                incarnation,
            },
            TTL,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        live(leases.leases().await.unwrap()),
        "live again after a renewal"
    );
    db.drop().await;
}

/// Two nodes race for one duty, many times at once: exactly one holds it
/// each round, every renewal keeps the epoch, and a change of holder moves
/// it on.
#[tokio::test]
async fn two_nodes_racing_for_a_duty_get_exactly_one_holder_per_epoch() {
    let Some(db) = Db::new().await else { return };
    let leases = PgNodeLeaseRepository::new(db.pool.clone());
    let duties = Arc::new(PgClusterDutyRepository::new(db.pool.clone()));
    let (a, b) = (process(&leases).await, process(&leases).await);
    let duty = ClusterDutyName::custom("race-duty");
    duties.ensure(duty).await.unwrap();
    duties.ensure(duty).await.unwrap(); // a second ensure changes nothing

    let mut holders = std::collections::HashMap::new();
    for round in 0..20 {
        let (ra, rb) = tokio::join!(
            duties.try_acquire(duty, a, SHORT),
            duties.try_acquire(duty, b, SHORT)
        );
        let (ra, rb) = (ra.unwrap(), rb.unwrap());
        assert!(
            ra.is_some() != rb.is_some(),
            "round {round}: exactly one holder, a got {ra:?} and b got {rb:?}"
        );
        let (who, epoch) = match (ra, rb) {
            (Some(e), None) => ("a", e),
            (None, Some(e)) => ("b", e),
            _ => unreachable!(),
        };
        if let Some(previous) = holders.insert(epoch, who) {
            assert_eq!(previous, who, "epoch {epoch} had two holders");
        }
        if round % 5 == 4 {
            // Let the lease run out, so the next round is a fresh race.
            tokio::time::sleep(SHORT + Duration::from_millis(200)).await;
        }
    }
    let epochs: Vec<_> = holders.keys().copied().collect();
    assert!(epochs.iter().all(|e| e.0 >= 1));
    db.drop().await;
}

/// A holder that stalls past its lease is replaced at the next epoch, and
/// its fenced write (the health sweep marking a node unhealthy) affects
/// zero rows; the new holder's affects the row.
#[tokio::test]
async fn a_stalled_holders_fenced_write_affects_zero_rows() {
    let Some(db) = Db::new().await else { return };
    let leases = PgNodeLeaseRepository::new(db.pool.clone());
    let duties = PgClusterDutyRepository::new(db.pool.clone());
    let nodes = PgNodeClusterRepository::new(db.pool.clone());
    let duty = ClusterDutyName::CLUSTER_HEALTH_SWEEP;
    duties.ensure(duty).await.unwrap();
    let (a, b) = (process(&leases).await, process(&leases).await);
    let stale = process(&leases).await.node_id;

    let first = duties.try_acquire(duty, a, SHORT).await.unwrap().unwrap();
    assert_eq!(
        duties.try_acquire(duty, b, SHORT).await.unwrap(),
        None,
        "b waits while a's lease lives"
    );
    assert_eq!(
        duties.try_acquire(duty, a, SHORT).await.unwrap(),
        Some(first),
        "a renewal keeps the epoch"
    );

    // a stalls past its lease; b takes the duty.
    tokio::time::sleep(SHORT + Duration::from_millis(300)).await;
    let second = duties.try_acquire(duty, b, TTL).await.unwrap().unwrap();
    assert_eq!(
        second,
        DutyEpoch(first.0 + 1),
        "a change of holder moves the epoch on"
    );
    assert_eq!(
        duties.try_acquire(duty, a, TTL).await.unwrap(),
        None,
        "a has lost the duty"
    );

    let stalled = nodes
        .mark_unhealthy(&stale, DutyFence::Leased { duty, epoch: first })
        .await
        .unwrap();
    assert!(!stalled, "the stalled holder's write affected a row");
    let status: String = sqlx::query_scalar("SELECT status FROM cluster_nodes WHERE node_id = $1")
        .bind(stale.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "active");

    let current = nodes
        .mark_unhealthy(
            &stale,
            DutyFence::Leased {
                duty,
                epoch: second,
            },
        )
        .await
        .unwrap();
    assert!(current, "the holder at the current epoch writes");
    db.drop().await;
}

/// A released duty is taken at once, at the next epoch, without waiting for
/// the lease; a release under a superseded epoch releases nothing.
#[tokio::test]
async fn a_released_duty_is_handed_over_at_once() {
    let Some(db) = Db::new().await else { return };
    let leases = PgNodeLeaseRepository::new(db.pool.clone());
    let duties = PgClusterDutyRepository::new(db.pool.clone());
    let duty = ClusterDutyName::GOAL_EXPIRY;
    duties.ensure(duty).await.unwrap();
    let (a, b) = (process(&leases).await, process(&leases).await);

    let held = duties.try_acquire(duty, a, TTL).await.unwrap().unwrap();
    assert!(!duties
        .release(duty, a, DutyEpoch(held.0 + 7))
        .await
        .unwrap());
    assert!(
        !duties.release(duty, b, held).await.unwrap(),
        "b never held it"
    );
    assert_eq!(duties.try_acquire(duty, b, TTL).await.unwrap(), None);
    assert!(duties.release(duty, a, held).await.unwrap());
    assert_eq!(
        duties.try_acquire(duty, b, TTL).await.unwrap(),
        Some(DutyEpoch(held.0 + 1))
    );
    db.drop().await;
}

/// A node that restarts takes its own duties back from its previous
/// incarnation at once, at a new epoch, instead of waiting out the lease
/// of a process that no longer exists; the previous incarnation can never
/// take a duty back from the newer one.
#[tokio::test]
async fn a_restarted_node_takes_its_duty_from_its_previous_incarnation() {
    let Some(db) = Db::new().await else { return };
    let leases = PgNodeLeaseRepository::new(db.pool.clone());
    let duties = PgClusterDutyRepository::new(db.pool.clone());
    let duty = ClusterDutyName::VOLUME_TTL_CLEANUP;
    duties.ensure(duty).await.unwrap();
    let node_id = NodeId::new();
    let old = NodeProcess {
        node_id,
        incarnation: leases.join(&join(node_id, TTL)).await.unwrap(),
    };
    let held = duties.try_acquire(duty, old, TTL).await.unwrap().unwrap();
    let new = NodeProcess {
        node_id,
        incarnation: leases.join(&join(node_id, TTL)).await.unwrap(),
    };
    assert_eq!(
        duties.try_acquire(duty, new, TTL).await.unwrap(),
        Some(DutyEpoch(held.0 + 1))
    );
    assert_eq!(duties.try_acquire(duty, old, TTL).await.unwrap(), None);
    db.drop().await;
}

/// Migrations 057 and 058 are forward-only: a cluster_nodes row written
/// before them keeps every value and starts at incarnation 0 with no lease,
/// and applying them again changes nothing.
#[tokio::test]
async fn the_lease_columns_keep_every_existing_row() {
    let Some(db) = Db::new().await else { return };
    let node_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO cluster_nodes (node_id, role, public_key, grpc_address) \
         VALUES ($1, 'worker', $2, '10.0.0.9:50056')",
    )
    .bind(node_id)
    .bind(vec![1u8; 32])
    .execute(&db.pool)
    .await
    .unwrap();
    for m in MIGRATOR
        .iter()
        .filter(|m| m.version == 57 || m.version == 58)
    {
        sqlx::raw_sql(&m.sql).execute(&db.pool).await.unwrap();
    }
    let (incarnation, lease, address): (i64, Option<chrono::DateTime<chrono::Utc>>, String) =
        sqlx::query_as(
            "SELECT incarnation, lease_expires_at, grpc_address FROM cluster_nodes WHERE node_id = $1",
        )
        .bind(node_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        (incarnation, lease, address.as_str()),
        (0, None, "10.0.0.9:50056")
    );
    db.drop().await;
}
