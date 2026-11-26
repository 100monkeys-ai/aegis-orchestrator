// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Execution ownership against a real PostgreSQL through PgBouncer in
//! transaction mode (AEGIS ADR-152 D5.1, D5.2, D5.6; migration 059).
//!
//! A cluster node claims what it creates in the creating INSERT; a write by
//! an owner whose execution was adopted affects zero rows and reports the
//! execution fenced; the restart pass of a new incarnation takes and ends
//! only what its own earlier incarnations left, never another node's; a node
//! outside a cluster writes as it always has. Without
//! `AEGIS_TEST_POSTGRES_URL` and `AEGIS_TEST_PGBOUNCER_URL` the database
//! tests do nothing and say so; in CI they fail instead.

use std::sync::{Arc, Mutex};

use aegis_orchestrator::daemon::migrations::{apply_migrations, MIGRATOR};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::cluster::NodeId;
use aegis_orchestrator_core::domain::cluster_membership::{
    ExecutionOwnership, NodeIncarnation, NodeProcess,
};
use aegis_orchestrator_core::domain::execution::{Execution, ExecutionId, ExecutionInput};
use aegis_orchestrator_core::domain::repository::{ExecutionRepository, RepositoryError};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_execution::{
    owned_save_sql, unowned_save_sql, PostgresExecutionRepository, TAKE_OWN_UNFINISHED_SQL,
};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

// ---------------------------------------------------------------------------
// pg_strict
// ---------------------------------------------------------------------------

/// The UPDATE or DELETE statements of `sql` that carry no WHERE clause, as
/// cluster_duties.rs reads them.
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
fn every_ownership_statement_and_migration_059_passes_pg_strict() {
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 59)
        .expect("migration 059 ships");
    for (name, sql) in [
        ("owned save", owned_save_sql()),
        ("unowned save", unowned_save_sql()),
        ("take own unfinished", TAKE_OWN_UNFINISHED_SQL.to_string()),
        ("migration 059", migration.sql.to_string()),
    ] {
        assert!(
            unbounded_writes(&sql).is_empty(),
            "{name}: pg_strict refuses {:?}",
            unbounded_writes(&sql)
        );
    }
    assert!(
        owned_save_sql().contains("WHERE executions.owner_node_id = $19"),
        "the owner's update is conditional on owning the row"
    );
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
        let name = format!("aegis_ownership_{}", uuid::Uuid::new_v4().simple());
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

fn process(node_id: NodeId, incarnation: i64) -> NodeProcess {
    NodeProcess {
        node_id,
        incarnation: NodeIncarnation(incarnation),
    }
}

/// A repository claiming for `process`, and the ids it reports fenced.
fn owned(
    pool: &PgPool,
    process: NodeProcess,
) -> (PostgresExecutionRepository, Arc<Mutex<Vec<ExecutionId>>>) {
    let ownership = Arc::new(ExecutionOwnership::default());
    ownership.claim_for(process).unwrap();
    let fenced = Arc::new(Mutex::new(Vec::new()));
    let seen = fenced.clone();
    ownership
        .on_fenced(move |id| seen.lock().unwrap().push(id))
        .unwrap();
    (
        PostgresExecutionRepository::new(pool.clone()).with_ownership(ownership),
        fenced,
    )
}

async fn insert_agent(pool: &PgPool, tenant: &TenantId) -> AgentId {
    sqlx::query(
        "INSERT INTO tenants (slug, display_name, keycloak_realm, openbao_namespace) \
         VALUES ($1, $1, 'zaru-consumer', $1) ON CONFLICT DO NOTHING",
    )
    .bind(tenant.as_str())
    .execute(pool)
    .await
    .expect("insert the tenant");
    let id = AgentId::new();
    sqlx::query(
        "INSERT INTO agents (id, tenant_id, name, manifest_yaml, manifest_json, runtime, security_policy) \
         VALUES ($1, $2, $3, '', '{}'::jsonb, 'python:3.11', '{}'::jsonb)",
    )
    .bind(id.0)
    .bind(tenant.as_str())
    .bind(format!("ownership-agent-{}", &id.0.simple().to_string()[..8]))
    .execute(pool)
    .await
    .expect("insert an agent");
    id
}

async fn running(pool: &PgPool, tenant: &TenantId) -> Execution {
    let agent_id = insert_agent(pool, tenant).await;
    let input = ExecutionInput {
        intent: Some("work".to_string()),
        input: serde_json::json!({}),
        workspace_volume_id: None,
        workspace_volume_mount_path: None,
        workspace_remote_path: None,
        workflow_execution_id: None,
        attachments: Vec::new(),
    };
    let mut e = Execution::new(agent_id, input, 3, "aegis-system-operator".to_string());
    e.tenant_id = tenant.clone();
    e.start();
    e
}

async fn owner_of(
    pool: &PgPool,
    id: ExecutionId,
) -> (Option<uuid::Uuid>, Option<i64>, i64, String) {
    sqlx::query_as(
        "SELECT owner_node_id, owner_incarnation, owner_epoch, status FROM executions WHERE id = $1",
    )
    .bind(id.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The execution-adoption duty's takeover (D5.7), as it will write it: the
/// epoch moves on and the adopter is the owner.
async fn adopt(pool: &PgPool, id: ExecutionId, adopter: NodeProcess) {
    sqlx::query(
        "UPDATE executions SET owner_epoch = owner_epoch + 1, owner_node_id = $2, \
         owner_incarnation = $3, status = 'failed', error_message = 'owner_lost' WHERE id = $1",
    )
    .bind(id.0)
    .bind(adopter.node_id.0)
    .bind(adopter.incarnation.0)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_cluster_node_claims_what_it_creates_and_keeps_writing_it() {
    let Some(db) = Db::new().await else { return };
    let tenant = TenantId::for_consumer_user("owner-claims").unwrap();
    let me = process(NodeId::new(), 3);
    let (repo, fenced) = owned(&db.pool, me);
    let mut execution = running(&db.pool, &tenant).await;
    repo.save_for_tenant(&tenant, &execution).await.unwrap();
    assert_eq!(
        owner_of(&db.pool, execution.id).await,
        (Some(me.node_id.0), Some(3), 0, "running".to_string()),
        "claimed in the creating INSERT"
    );
    execution.complete();
    repo.save_for_tenant(&tenant, &execution).await.unwrap();
    assert_eq!(owner_of(&db.pool, execution.id).await.3, "completed");
    assert!(fenced.lock().unwrap().is_empty());
    db.drop().await;
}

#[tokio::test]
async fn a_write_by_a_fenced_owner_affects_no_row_and_reports_the_execution() {
    let Some(db) = Db::new().await else { return };
    let tenant = TenantId::for_consumer_user("owner-fenced").unwrap();
    let me = process(NodeId::new(), 1);
    let adopter = process(NodeId::new(), 1);
    let (repo, fenced) = owned(&db.pool, me);
    let mut execution = running(&db.pool, &tenant).await;
    repo.save_for_tenant(&tenant, &execution).await.unwrap();

    adopt(&db.pool, execution.id, adopter).await;

    execution.complete();
    let err = repo.save_for_tenant(&tenant, &execution).await.unwrap_err();
    assert!(matches!(err, RepositoryError::Fenced(_)), "{err}");
    assert_eq!(
        *fenced.lock().unwrap(),
        vec![execution.id],
        "the fence handler ran"
    );
    assert_eq!(
        owner_of(&db.pool, execution.id).await,
        (Some(adopter.node_id.0), Some(1), 1, "failed".to_string()),
        "the adopter's ending stands"
    );

    // The same node's next incarnation does not own it either.
    let (later, _) = owned(&db.pool, process(me.node_id, 2));
    assert!(matches!(
        later.save_for_tenant(&tenant, &execution).await,
        Err(RepositoryError::Fenced(_))
    ));
    db.drop().await;
}

#[tokio::test]
async fn the_restart_pass_takes_only_what_this_nodes_earlier_incarnations_left() {
    let Some(db) = Db::new().await else { return };
    let tenant = TenantId::for_consumer_user("owner-restart").unwrap();
    let node = NodeId::new();
    let (before, _) = owned(&db.pool, process(node, 1));
    let (peer, _) = owned(&db.pool, process(NodeId::new(), 1));
    let mine = running(&db.pool, &tenant).await;
    let theirs = running(&db.pool, &tenant).await;
    let mut done = running(&db.pool, &tenant).await;
    before.save_for_tenant(&tenant, &mine).await.unwrap();
    done.complete();
    before.save_for_tenant(&tenant, &done).await.unwrap();
    peer.save_for_tenant(&tenant, &theirs).await.unwrap();

    let (now, fenced) = owned(&db.pool, process(node, 2));
    let mut taken = now.take_unfinished_left_by_earlier_process().await.unwrap();
    assert_eq!(
        taken.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![mine.id],
        "only this node's unfinished execution, never a live peer's"
    );
    let mut cut = taken.pop().unwrap();
    cut.fail_cut_by_restart();
    now.save_for_tenant(&tenant, &cut).await.unwrap();
    assert_eq!(
        owner_of(&db.pool, mine.id).await,
        (Some(node.0), Some(2), 0, "failed".to_string())
    );
    assert_eq!(
        owner_of(&db.pool, theirs.id).await.3,
        "running",
        "the peer's run is untouched"
    );
    assert!(fenced.lock().unwrap().is_empty());
    assert!(
        now.take_unfinished_left_by_earlier_process()
            .await
            .unwrap()
            .is_empty(),
        "nothing is left a second time"
    );
    db.drop().await;
}

#[tokio::test]
async fn a_node_outside_a_cluster_writes_unconditionally_and_claims_nothing() {
    let Some(db) = Db::new().await else { return };
    let tenant = TenantId::for_consumer_user("standalone").unwrap();
    let repo = PostgresExecutionRepository::new(db.pool.clone())
        .with_ownership(Arc::new(ExecutionOwnership::default()));
    let mut execution = running(&db.pool, &tenant).await;
    repo.save_for_tenant(&tenant, &execution).await.unwrap();
    execution.complete();
    repo.save_for_tenant(&tenant, &execution).await.unwrap();
    assert_eq!(
        owner_of(&db.pool, execution.id).await,
        (None, None, 0, "completed".to_string())
    );

    let left = running(&db.pool, &tenant).await;
    repo.save_for_tenant(&tenant, &left).await.unwrap();
    let taken = repo
        .take_unfinished_left_by_earlier_process()
        .await
        .unwrap();
    assert_eq!(
        taken.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![left.id]
    );
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Adoption (D5.7)
// ---------------------------------------------------------------------------

use aegis_orchestrator_core::domain::cluster_membership::{
    ClusterDutyName, ClusterDutyRepository, DutyFence, ExecutionAdoptionRepository, NodeJoin,
    NodeLeaseRepository,
};
use aegis_orchestrator_core::domain::execution::OWNER_LOST_FAILURE_REASON;
use aegis_orchestrator_core::infrastructure::cluster::adoption_repo::{
    adopt_sql, UNFINISHED_OF_SQL,
};
use aegis_orchestrator_core::infrastructure::cluster::{
    PgClusterDutyRepository, PgExecutionAdoptionRepository, PgNodeLeaseRepository,
};

#[test]
fn the_adoption_statements_pass_pg_strict() {
    for (name, sql) in [
        ("adopt", adopt_sql()),
        ("unfinished of", UNFINISHED_OF_SQL.to_string()),
    ] {
        assert!(
            unbounded_writes(&sql).is_empty(),
            "{name}: {:?}",
            unbounded_writes(&sql)
        );
    }
}

async fn joined(pool: &PgPool) -> NodeProcess {
    let leases = PgNodeLeaseRepository::new(pool.clone());
    let node_id = NodeId::new();
    let incarnation = leases
        .join(&NodeJoin {
            node_id,
            role: "hybrid".to_string(),
            public_key: vec![9; 32],
            grpc_address: "10.120.0.7:50056".to_string(),
            vrack_address: Some("10.120.0.7".to_string()),
            lease_ttl: std::time::Duration::from_secs(15),
        })
        .await
        .unwrap();
    NodeProcess {
        node_id,
        incarnation,
    }
}

async fn holding_adoption(pool: &PgPool, holder: NodeProcess) -> DutyFence {
    let duties = PgClusterDutyRepository::new(pool.clone());
    duties
        .ensure(ClusterDutyName::EXECUTION_ADOPTION)
        .await
        .unwrap();
    let epoch = duties
        .try_acquire(
            ClusterDutyName::EXECUTION_ADOPTION,
            holder,
            std::time::Duration::from_secs(15),
        )
        .await
        .unwrap()
        .expect("the duty is free");
    DutyFence::Leased {
        duty: ClusterDutyName::EXECUTION_ADOPTION,
        epoch,
    }
}

#[tokio::test]
async fn the_adopter_ends_a_dead_nodes_run_and_the_dead_owner_is_fenced() {
    let Some(db) = Db::new().await else { return };
    let tenant = TenantId::for_consumer_user("adoption").unwrap();
    let dead = joined(&db.pool).await;
    let adopter = joined(&db.pool).await;
    let (dead_repo, dead_fenced) = owned(&db.pool, dead);
    let run = running(&db.pool, &tenant).await;
    let mut cancelled = running(&db.pool, &tenant).await;
    dead_repo.save_for_tenant(&tenant, &run).await.unwrap();
    dead_repo
        .save_for_tenant(&tenant, &cancelled)
        .await
        .unwrap();
    sqlx::query("UPDATE executions SET cancel_requested_at = now() WHERE id = $1")
        .bind(cancelled.id.0)
        .execute(&db.pool)
        .await
        .unwrap();

    let fence = holding_adoption(&db.pool, adopter).await;
    let adoption = PgExecutionAdoptionRepository::new(db.pool.clone());
    let unfinished = adoption.unfinished_of(dead.node_id).await.unwrap();
    assert_eq!(unfinished.len(), 2);
    for (mut execution, cancel_requested) in unfinished {
        assert_eq!(cancel_requested, execution.id == cancelled.id);
        execution.end_owner_lost(cancel_requested);
        assert!(adoption
            .adopt(&execution, dead.node_id, adopter, fence)
            .await
            .unwrap());
        // A second adoption of the same run takes nothing.
        assert!(!adoption
            .adopt(&execution, dead.node_id, adopter, fence)
            .await
            .unwrap());
    }

    assert_eq!(
        owner_of(&db.pool, run.id).await,
        (
            Some(adopter.node_id.0),
            Some(adopter.incarnation.0),
            1,
            "failed".to_string()
        )
    );
    let error: Option<String> =
        sqlx::query_scalar("SELECT error_message FROM executions WHERE id = $1")
            .bind(run.id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(error.as_deref(), Some(OWNER_LOST_FAILURE_REASON));
    assert!(OWNER_LOST_FAILURE_REASON.starts_with("owner_lost"));
    assert_eq!(
        owner_of(&db.pool, cancelled.id).await.3,
        "cancelled",
        "a requested cancel is honoured"
    );

    // The dead owner's late write affects no row and reports the run fenced.
    cancelled.complete();
    assert!(matches!(
        dead_repo.save_for_tenant(&tenant, &cancelled).await,
        Err(RepositoryError::Fenced(_))
    ));
    assert_eq!(*dead_fenced.lock().unwrap(), vec![cancelled.id]);
    assert_eq!(owner_of(&db.pool, cancelled.id).await.3, "cancelled");
    db.drop().await;
}

#[tokio::test]
async fn a_holder_whose_duty_epoch_moved_on_adopts_nothing() {
    let Some(db) = Db::new().await else { return };
    let tenant = TenantId::for_consumer_user("adoption-stale").unwrap();
    let dead = joined(&db.pool).await;
    let stalled = joined(&db.pool).await;
    let (dead_repo, _) = owned(&db.pool, dead);
    let run = running(&db.pool, &tenant).await;
    dead_repo.save_for_tenant(&tenant, &run).await.unwrap();

    let stale_fence = holding_adoption(&db.pool, stalled).await;
    // The stalled holder's lease runs out; another holder takes the duty at
    // the next epoch.
    sqlx::query(
        "UPDATE cluster_duties SET lease_expires_at = now() - interval '1 second' WHERE duty = $1",
    )
    .bind(ClusterDutyName::EXECUTION_ADOPTION.as_str())
    .execute(&db.pool)
    .await
    .unwrap();
    let next = joined(&db.pool).await;
    let _ = holding_adoption(&db.pool, next).await;

    let adoption = PgExecutionAdoptionRepository::new(db.pool.clone());
    let (mut execution, _) = adoption
        .unfinished_of(dead.node_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    execution.end_owner_lost(false);
    assert!(!adoption
        .adopt(&execution, dead.node_id, stalled, stale_fence)
        .await
        .unwrap());
    assert_eq!(
        owner_of(&db.pool, run.id).await,
        (
            Some(dead.node_id.0),
            Some(dead.incarnation.0),
            0,
            "running".to_string()
        )
    );
    // A node running its duties alone never adopts.
    assert!(!adoption
        .adopt(&execution, dead.node_id, stalled, DutyFence::Solo)
        .await
        .unwrap());
    db.drop().await;
}
