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
            max_executions: None,
            display_name: None,
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

// ---------------------------------------------------------------------------
// The recorded cancel (D5.4)
// ---------------------------------------------------------------------------

#[test]
fn the_cancel_statement_passes_pg_strict() {
    use aegis_orchestrator_core::infrastructure::repositories::postgres_execution::REQUEST_CANCEL_SQL;
    assert!(unbounded_writes(REQUEST_CANCEL_SQL).is_empty());
}

#[tokio::test]
async fn a_cancel_is_recorded_on_an_unfinished_run_once_and_answers_its_owner() {
    let Some(db) = Db::new().await else { return };
    let tenant = TenantId::for_consumer_user("cancel-record").unwrap();
    let other = TenantId::for_consumer_user("cancel-other").unwrap();
    let owner = process(NodeId::new(), 1);
    let (repo, _) = owned(&db.pool, owner);
    let run = running(&db.pool, &tenant).await;
    repo.save_for_tenant(&tenant, &run).await.unwrap();
    assert!(!repo.cancel_requested(run.id).await.unwrap());

    assert_eq!(
        repo.request_cancel(&other, run.id).await.unwrap(),
        None,
        "another tenant records nothing"
    );
    assert_eq!(
        repo.request_cancel(&tenant, run.id).await.unwrap(),
        Some(Some(owner.node_id)),
        "recorded, and its owner answered"
    );
    let first: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT cancel_requested_at FROM executions WHERE id = $1")
            .bind(run.id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    repo.request_cancel(&tenant, run.id).await.unwrap();
    let again: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT cancel_requested_at FROM executions WHERE id = $1")
            .bind(run.id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(first, again, "the first request's time is kept");
    assert!(repo.cancel_requested(run.id).await.unwrap());

    let mut done = running(&db.pool, &tenant).await;
    done.complete();
    repo.save_for_tenant(&tenant, &done).await.unwrap();
    assert_eq!(
        repo.request_cancel(&tenant, done.id).await.unwrap(),
        None,
        "an ended run records no cancel"
    );
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Placement (D5.3)
// ---------------------------------------------------------------------------

#[test]
fn the_placement_statements_and_migration_060_pass_pg_strict() {
    use aegis_orchestrator_core::infrastructure::cluster::membership_repo::{
        LOAD_OF_SQL, PLACEMENT_CANDIDATES_SQL,
    };
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 60)
        .expect("migration 060 ships");
    for sql in [
        LOAD_OF_SQL,
        PLACEMENT_CANDIDATES_SQL,
        migration.sql.as_ref(),
    ] {
        assert!(unbounded_writes(sql).is_empty(), "{sql}");
    }
}

/// A node joined with `capacity`, owning `unfinished` unfinished executions.
async fn node_with(pool: &PgPool, capacity: Option<u32>, unfinished: usize) -> NodeProcess {
    let leases = PgNodeLeaseRepository::new(pool.clone());
    let node_id = NodeId::new();
    let incarnation = leases
        .join(&NodeJoin {
            node_id,
            role: "hybrid".to_string(),
            public_key: vec![9; 32],
            grpc_address: format!("10.121.0.{}:50056", (node_id.0.as_u128() % 200) + 20),
            vrack_address: None,
            lease_ttl: std::time::Duration::from_secs(15),
            max_executions: capacity,
            display_name: None,
        })
        .await
        .unwrap();
    let me = NodeProcess {
        node_id,
        incarnation,
    };
    let (repo, _) = owned(pool, me);
    let tenant = TenantId::for_consumer_user("placement").unwrap();
    for _ in 0..unfinished {
        let run = running(pool, &tenant).await;
        repo.save_for_tenant(&tenant, &run).await.unwrap();
    }
    me
}

#[tokio::test]
async fn a_new_execution_goes_to_the_live_peer_with_the_most_headroom() {
    use aegis_orchestrator_core::domain::cluster_membership::PlacementRepository;
    use aegis_orchestrator_core::infrastructure::cluster::PgPlacementRepository;
    let Some(db) = Db::new().await else { return };
    let me = node_with(&db.pool, Some(2), 2).await;
    let roomy = node_with(&db.pool, Some(5), 1).await;
    let tight = node_with(&db.pool, Some(3), 2).await;
    let full = node_with(&db.pool, Some(2), 2).await;
    let uncapped = node_with(&db.pool, None, 0).await;
    let dead = node_with(&db.pool, Some(10), 0).await;
    sqlx::query("UPDATE cluster_nodes SET lease_expires_at = now() - interval '1 second' WHERE node_id = $1")
        .bind(dead.node_id.0)
        .execute(&db.pool)
        .await
        .unwrap();

    let placement = PgPlacementRepository::new(db.pool.clone());
    assert_eq!(placement.load_of(me.node_id).await.unwrap(), 2);
    let candidates = placement.candidates(me.node_id).await.unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|c| (c.node_id, c.headroom))
            .collect::<Vec<_>>(),
        vec![(roomy.node_id, 4), (tight.node_id, 1)],
        "most headroom first; this node, a full, an uncapped and a dead peer are never chosen"
    );
    let _ = (full, uncapped);
    db.drop().await;
}

// ---------------------------------------------------------------------------
// SEAL replay nonces shared by the cluster (ADR-152 D4.2)
// ---------------------------------------------------------------------------

#[test]
fn the_nonce_statements_and_migration_061_pass_pg_strict() {
    use aegis_orchestrator_core::infrastructure::seal::nonce_store::{
        DELETE_EXPIRED_NONCES_SQL, RECORD_NONCE_SQL,
    };
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 61)
        .expect("migration 061 ships");
    for sql in [
        RECORD_NONCE_SQL,
        DELETE_EXPIRED_NONCES_SQL,
        migration.sql.as_ref(),
    ] {
        assert!(unbounded_writes(sql).is_empty(), "{sql}");
    }
}

#[tokio::test]
async fn an_envelope_replayed_to_another_node_is_a_replay() {
    use aegis_orchestrator_core::infrastructure::seal::nonce_store::{
        NonceOutcome, NonceStore, PostgresNonceStore,
    };
    let Some(db) = Db::new().await else { return };
    let (node_a, node_b) = (
        PostgresNonceStore::new(db.pool.clone()),
        PostgresNonceStore::new(db.pool.clone()),
    );
    assert_eq!(node_a.record("sig-1").await.unwrap(), NonceOutcome::Fresh);
    assert_eq!(
        node_a.record("sig-1").await.unwrap(),
        NonceOutcome::Replay,
        "seen here"
    );
    assert_eq!(
        node_b.record("sig-1").await.unwrap(),
        NonceOutcome::Replay,
        "seen on the other node"
    );
    assert_eq!(node_b.record("sig-2").await.unwrap(), NonceOutcome::Fresh);

    sqlx::query("UPDATE seal_replay_nonces SET expires_at = now() - interval '1 second' WHERE nonce = 'sig-1'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        PostgresNonceStore::delete_expired(&db.pool).await.unwrap(),
        1,
        "only the expired one"
    );
    let left: Vec<String> = sqlx::query_scalar("SELECT nonce FROM seal_replay_nonces")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(left, vec!["sig-2".to_string()]);
    db.drop().await;
}

// ---------------------------------------------------------------------------
// SEAL sessions shared by the cluster (ADR-152 D4.1)
// ---------------------------------------------------------------------------

#[test]
fn the_session_statements_and_migration_062_pass_pg_strict() {
    use aegis_orchestrator_core::infrastructure::seal::session_repository::{
        DELETE_STALE_SESSIONS_SQL, SAVE_SESSION_SQL,
    };
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 62)
        .expect("migration 062 ships");
    for sql in [
        SAVE_SESSION_SQL,
        DELETE_STALE_SESSIONS_SQL,
        migration.sql.as_ref(),
    ] {
        assert!(unbounded_writes(sql).is_empty(), "{sql}");
    }
}

#[tokio::test]
async fn a_session_attested_on_one_node_is_known_on_another_and_its_token_is_never_stored() {
    use aegis_orchestrator_core::domain::seal_session::{SealSession, SessionStatus};
    use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
    use aegis_orchestrator_core::domain::secrets::SensitiveString;
    use aegis_orchestrator_core::domain::security_context::{
        SecurityContext, SecurityContextMetadata,
    };
    use aegis_orchestrator_core::domain::shared_kernel::{AgentId, ExecutionId};
    use aegis_orchestrator_core::infrastructure::seal::session_repository::PostgresSealSessionRepository;
    let Some(db) = Db::new().await else { return };
    let context = SecurityContext {
        name: "zaru-pro".to_string(),
        description: "test".to_string(),
        capabilities: vec![],
        deny_list: vec![],
        metadata: SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    };
    let token = "header.payload.signature-of-a-seal-token";
    let session = SealSession::new(
        AgentId::new(),
        ExecutionId::new(),
        vec![7; 32],
        token.to_string(),
        context,
        TenantId::for_consumer_user("seal-shared").unwrap(),
    );
    let (node_a, node_b) = (
        PostgresSealSessionRepository::new(db.pool.clone()),
        PostgresSealSessionRepository::new(db.pool.clone()),
    );
    node_a.save(session.clone()).await.unwrap();

    let found = node_b
        .find_active_by_security_token(&SensitiveString::new(token))
        .await
        .unwrap()
        .expect("the other node finds the session by its token");
    assert_eq!(found.id, session.id);
    assert_eq!(found.agent_public_key, session.agent_public_key);
    assert_eq!(found.tenant_id, session.tenant_id);
    assert_eq!(found.security_token_raw, SensitiveString::new(token));
    assert!(node_b
        .find_active_by_security_token(&SensitiveString::new("another-token"))
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        node_b
            .find_active_by_agent(&session.agent_id)
            .await
            .unwrap()
            .map(|s| s.id),
        Some(session.id)
    );

    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM seal_sessions WHERE body::text LIKE '%' || $1 || '%' OR encode(token_hash, 'escape') LIKE '%' || $1 || '%'",
    )
    .bind(token)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        stored, 0,
        "the bearer token is stored nowhere, only its hash"
    );

    node_b
        .revoke_for_agent(&session.agent_id, "execution ended".to_string())
        .await
        .unwrap();
    assert!(
        node_a
            .find_active_by_security_token(&SensitiveString::new(token))
            .await
            .unwrap()
            .is_none(),
        "revoked on one node, gone on the other"
    );
    let revoked = node_a.find_by_id(&session.id).await.unwrap().unwrap();
    assert!(matches!(revoked.status, SessionStatus::Revoked { .. }));
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Edge stream owners (ADR-152 D8)
// ---------------------------------------------------------------------------

#[test]
fn the_edge_stream_statements_and_migration_063_pass_pg_strict() {
    use aegis_orchestrator_core::infrastructure::edge::stream_owners::{
        CONNECTED_EDGES_SQL, RECORD_STREAM_SQL, RELEASE_MEMBER_SQL, RELEASE_STREAM_SQL,
    };
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 63)
        .expect("migration 063 ships");
    for sql in [
        RECORD_STREAM_SQL,
        RELEASE_STREAM_SQL,
        RELEASE_MEMBER_SQL,
        CONNECTED_EDGES_SQL,
        migration.sql.as_ref(),
    ] {
        assert!(unbounded_writes(sql).is_empty(), "{sql}");
    }
}

#[tokio::test]
async fn a_late_close_never_removes_a_newer_streams_record() {
    use aegis_orchestrator_core::infrastructure::edge::{EdgeStreamOwners, PgEdgeStreamOwners};
    let Some(db) = Db::new().await else { return };
    let owners = PgEdgeStreamOwners::new(db.pool.clone());
    let (edge, member_a, member_b) = (NodeId::new(), NodeId::new(), NodeId::new());
    let (first, second) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

    owners.record(edge, member_a, first).await.unwrap();
    assert_eq!(owners.owner_of(edge).await.unwrap(), Some(member_a));
    // The edge reconnects to another member's replica before the first
    // stream's close is seen.
    owners.record(edge, member_b, second).await.unwrap();
    owners.release(edge, first).await.unwrap();
    assert_eq!(
        owners.owner_of(edge).await.unwrap(),
        Some(member_b),
        "the first stream's late close leaves the newer record"
    );
    owners.release(edge, second).await.unwrap();
    assert_eq!(owners.owner_of(edge).await.unwrap(), None);

    owners.record(edge, member_a, first).await.unwrap();
    assert_eq!(
        owners.release_member(member_a).await.unwrap(),
        1,
        "a dead member's streams go"
    );
    assert_eq!(owners.owner_of(edge).await.unwrap(), None);
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Fleet command owners (ADR-152 D4.7)
// ---------------------------------------------------------------------------

#[test]
fn the_fleet_owner_statements_and_migration_064_pass_pg_strict() {
    use aegis_orchestrator_core::infrastructure::edge::fleet_owners::{
        RECORD_FLEET_SQL, RELEASE_FLEET_SQL,
    };
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 64)
        .expect("migration 064 ships");
    for sql in [RECORD_FLEET_SQL, RELEASE_FLEET_SQL, migration.sql.as_ref()] {
        assert!(unbounded_writes(sql).is_empty(), "{sql}");
    }
}

#[tokio::test]
async fn a_fleet_command_is_found_on_its_member_until_it_ends() {
    use aegis_orchestrator_core::domain::cluster::FleetCommandId;
    use aegis_orchestrator_core::domain::shared_kernel::TenantId;
    use aegis_orchestrator_core::infrastructure::edge::{FleetCommandOwners, PgFleetCommandOwners};
    let Some(db) = Db::new().await else { return };
    let owners = PgFleetCommandOwners::new(db.pool.clone());
    let (member, id) = (NodeId::new(), FleetCommandId(uuid::Uuid::new_v4()));
    let tenant = TenantId::from_realm_slug("fleet-owner").unwrap();

    owners.record(id, member, &tenant).await.unwrap();
    // A second record of the same command keeps the first.
    owners.record(id, NodeId::new(), &tenant).await.unwrap();
    assert_eq!(owners.owner_of(id).await.unwrap(), Some(member));
    owners.release(id).await.unwrap();
    assert_eq!(owners.owner_of(id).await.unwrap(), None);
    db.drop().await;
}

#[test]
fn the_cluster_overview_statements_and_migration_065_pass_pg_strict() {
    use aegis_orchestrator_core::infrastructure::cluster::overview::{
        DUTIES_SQL, EXECUTIONS_SQL, MEMBERS_SQL,
    };
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 65)
        .expect("migration 065 ships");
    for sql in [
        MEMBERS_SQL,
        DUTIES_SQL,
        EXECUTIONS_SQL,
        migration.sql.as_ref(),
    ] {
        assert!(unbounded_writes(sql).is_empty(), "{sql}");
    }
}
