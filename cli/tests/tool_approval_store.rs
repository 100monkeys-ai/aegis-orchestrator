// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The approval gate's durable store (AEGIS ADR-126 D3) against a real
//! PostgreSQL, with the migrations this binary ships.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere the tests say they were skipped and pass.
//!
//! They cover: a pending request is still there, and can be answered, after
//! the service is rebuilt on the same database (the redeploy case); one
//! answer wins when two race; a policy matches only its own user, tool and
//! binding (the argument the tool's contract declares, ADR-126 Update of
//! 2026-10-04 clause 1), and stops matching once revoked; the sweep expires only what has
//! waited 72 hours; migration 036 run again changes nothing; and the
//! conversation a request was made in (ADR-126, Update of 2026-10-07 (2),
//! clause 1): migration 044 run again changes nothing, and the store
//! round-trips `conversation_id`, a value and null.
//!
//! The sealing (ADR-126, Update of 2026-10-08): a stored request's
//! `arguments`, `summary`, `result` and `error` hold Transit ciphertext under
//! the tenant's own key and no plaintext of a planted message; a read answers
//! the plaintext to the request's user; a row written before migration 045
//! still reads; 045 run again changes nothing; the operator's list and the
//! sweep decrypt nothing; a refused seal stores nothing. Transit is
//! [`TransitStandIn`], local to this file: its ciphertext is opaque
//! (`vault:v1:` and random characters, no plaintext encoded in it) and it opens
//! a ciphertext only under the key that sealed it.
//!
//! The deny policies (ADR-126, Update of 2026-10-08 (2)): migration 046 run
//! again changes nothing and a policy stored before it reads `allow`; a call
//! matching a deny policy is stored `auto_denied`, which the status CHECK
//! admits; an allow and a deny policy are never both unrevoked for one key,
//! under concurrent writes too, and another binding's policy is untouched.
//!
//! The schedule a gated call's run belongs to (AEGIS ADR-139 N9): the gate
//! stores it from the run's record (an agent run's own, else its workflow
//! run's), and every read answers the schedule's name, a deleted one's too.
//!
//! The profile a standing choice was made in (AEGIS ADR-140 D9): migration
//! 050 over a policy stored before it leaves that policy with no profile,
//! still matching raw-binding calls, and run again changes nothing; a choice
//! made in a profile matches only that profile's calls and one made on raw
//! bindings only raw-binding calls, each replacing only its own key; deleting
//! the profile revokes its choices and no other.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aegis_orchestrator_core::application::tool_approval_service::{
    ApprovedCallRunner, GateOutcome, GatedCall, ToolApprovalService,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::secrets::{
    DomainDynamicSecret, SecretStore, SecretsError, SensitiveString,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::tool_approval::{
    ApprovalContract, ToolApprovalDecision, ToolApprovalId, ToolApprovalPolicy,
    ToolApprovalPolicyEffect, ToolApprovalPolicyId, ToolApprovalRepository, ToolApprovalRequest,
    ToolApprovalStatus,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::{
    transit_key, PostgresToolApprovalRepository,
};
use aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager;
use serde_json::{json, Value};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

const USER: &str = "owner-sub";

fn postgres_url() -> Option<String> {
    match std::env::var("AEGIS_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("AEGIS_TEST_POSTGRES_URL is not set; in CI these tests must reach PostgreSQL")
        }
        _ => {
            eprintln!("skipped: AEGIS_TEST_POSTGRES_URL is not set");
            None
        }
    }
}

struct TestDb {
    server: PgPool,
    name: String,
    options: PgConnectOptions,
    pool: PgPool,
}

impl TestDb {
    async fn create() -> Option<Self> {
        Self::create_with(&MIGRATOR).await
    }

    /// A database migrated only to the migrations numbered below `version`.
    async fn create_before(version: i64) -> Option<Self> {
        let before = Migrator {
            migrations: Cow::Owned(
                MIGRATOR
                    .iter()
                    .filter(|m| m.version < version)
                    .cloned()
                    .collect(),
            ),
            ignore_missing: false,
            locking: true,
            no_tx: false,
        };
        Self::create_with(&before).await
    }

    async fn create_with(migrator: &Migrator) -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_approvals_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
        let pool = Self::connect(&options).await;
        migrator.run(&pool).await.expect("apply the migrations");
        Some(Self {
            server,
            name,
            options,
            pool,
        })
    }

    async fn connect(options: &PgConnectOptions) -> PgPool {
        PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.clone())
            .await
            .expect("connect to the test database")
    }

    async fn remove(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("drop the test database");
    }
}

#[derive(Default)]
struct RecordingRunner {
    ran: Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl ApprovedCallRunner for RecordingRunner {
    async fn run_approved_call(&self, request: &ToolApprovalRequest) -> Result<Value, String> {
        self.ran.lock().unwrap().push(request.arguments.clone());
        Ok(json!({"message_id": "<1@example.com>"}))
    }
}

/// Transit as OpenBao answers it, for these tests: `encrypt` returns an opaque
/// `vault:v1:` ciphertext carrying nothing of the plaintext, and `decrypt`
/// opens it only under the key that sealed it. It records the key of every
/// ciphertext, counts decrypts, and can be told to refuse.
#[derive(Default)]
struct TransitStandIn {
    sealed: Mutex<HashMap<String, (String, Vec<u8>)>>,
    decrypts: AtomicUsize,
    refuse: AtomicBool,
}

impl TransitStandIn {
    fn key_of(&self, ciphertext: &str) -> Option<String> {
        self.sealed
            .lock()
            .unwrap()
            .get(ciphertext)
            .map(|(key, _)| key.clone())
    }
}

fn not_kept() -> SecretsError {
    SecretsError::ConfigError("the Transit stand-in keeps no KV or dynamic secrets".into())
}

#[async_trait::async_trait]
impl SecretStore for TransitStandIn {
    async fn read(
        &self,
        _: &str,
        _: &str,
    ) -> Result<HashMap<String, SensitiveString>, SecretsError> {
        Err(not_kept())
    }
    async fn write(
        &self,
        _: &str,
        _: &str,
        _: HashMap<String, SensitiveString>,
    ) -> Result<(), SecretsError> {
        Err(not_kept())
    }
    async fn generate_dynamic(
        &self,
        _: &str,
        _: &str,
    ) -> Result<DomainDynamicSecret, SecretsError> {
        Err(not_kept())
    }
    async fn renew_lease(&self, _: &str, _: Duration) -> Result<Duration, SecretsError> {
        Err(not_kept())
    }
    async fn revoke_lease(&self, _: &str) -> Result<(), SecretsError> {
        Err(not_kept())
    }
    async fn transit_sign(&self, _: &str, _: &[u8]) -> Result<String, SecretsError> {
        Err(not_kept())
    }
    async fn transit_verify(&self, _: &str, _: &[u8], _: &str) -> Result<bool, SecretsError> {
        Err(not_kept())
    }
    async fn transit_encrypt(&self, key: &str, plaintext: &[u8]) -> Result<String, SecretsError> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(SecretsError::TransitError(
                "Encrypt failed: permission denied".into(),
            ));
        }
        let ciphertext = format!(
            "vault:v1:{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        self.sealed
            .lock()
            .unwrap()
            .insert(ciphertext.clone(), (key.to_string(), plaintext.to_vec()));
        Ok(ciphertext)
    }
    async fn transit_decrypt(&self, key: &str, ciphertext: &str) -> Result<Vec<u8>, SecretsError> {
        self.decrypts.fetch_add(1, Ordering::SeqCst);
        match self.sealed.lock().unwrap().get(ciphertext) {
            Some((sealed_under, plaintext)) if sealed_under == key => Ok(plaintext.clone()),
            _ => Err(SecretsError::TransitError(
                "Decrypt failed: cipher: message authentication failed".into(),
            )),
        }
    }
}

/// The Transit every rebuilt service of a test process shares, as every core
/// pod shares one OpenBao.
fn shared_transit() -> Arc<TransitStandIn> {
    static TRANSIT: OnceLock<Arc<TransitStandIn>> = OnceLock::new();
    TRANSIT.get_or_init(Default::default).clone()
}

fn secrets(transit: &Arc<TransitStandIn>) -> Arc<SecretsManager> {
    Arc::new(SecretsManager::from_store(
        transit.clone(),
        Arc::new(EventBus::new(64)),
    ))
}

fn repo_with(pool: &PgPool, transit: &Arc<TransitStandIn>) -> PostgresToolApprovalRepository {
    PostgresToolApprovalRepository::new(pool.clone(), secrets(transit))
}

fn repo(pool: &PgPool) -> PostgresToolApprovalRepository {
    repo_with(pool, &shared_transit())
}

fn service_with(pool: &PgPool, transit: &Arc<TransitStandIn>) -> ToolApprovalService {
    ToolApprovalService::new(
        Arc::new(repo_with(pool, transit)),
        Arc::new(EventBus::new(64)),
    )
}

fn service(pool: &PgPool) -> ToolApprovalService {
    service_with(pool, &shared_transit())
}

fn tenant() -> TenantId {
    TenantId::for_consumer_user(USER).unwrap()
}

/// The contract the gated tools declare: a binding argument the gate knows
/// by no name of its own, and the arguments a user reads.
fn contract() -> ApprovalContract {
    ApprovalContract {
        binding_argument: Some("account".to_string()),
        approval_summary: Some(vec![
            "to".to_string(),
            "subject".to_string(),
            "body".to_string(),
        ]),
    }
}

async fn gate(svc: &ToolApprovalService, user: &str, tool: &str, args: &Value) -> GateOutcome {
    gate_declaring(svc, user, tool, args, contract()).await
}

async fn gate_declaring(
    svc: &ToolApprovalService,
    user: &str,
    tool: &str,
    args: &Value,
    contract: ApprovalContract,
) -> GateOutcome {
    svc.gate(GatedCall {
        tenant_id: &tenant(),
        user_sub: Some(user),
        execution_id: ExecutionId::new(),
        agent_id: AgentId::new(),
        tool_name: tool,
        arguments: args,
        security_context_name: "zaru-pro",
        conversation_id: None,
        profile_id: None,
        contract,
    })
    .await
    .expect("gate")
}

fn pending_id(outcome: GateOutcome) -> ToolApprovalId {
    match outcome {
        GateOutcome::Pending { result } => {
            assert_eq!(result["status"], "approval_pending");
            ToolApprovalId::from_string(result["approval_id"].as_str().unwrap()).unwrap()
        }
        other => panic!("expected pending, got {other:?}"),
    }
}

/// The redeploy case: a request stored by one process is listed, and
/// answered, by a service built afresh on the same database.
#[tokio::test]
async fn a_pending_request_survives_a_rebuild_of_the_service_on_the_same_database() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let args = json!({"account": "b-1", "to": "x@example.com", "subject": "Hi", "body": "Hello"});
    let id = {
        let before = service(&db.pool);
        pending_id(gate(&before, USER, "outbound.send", &args).await)
    };

    let pool = TestDb::connect(&db.options).await;
    let after = service(&pool);
    let listed = after
        .list_for_user(&tenant(), USER, Some(ToolApprovalStatus::Pending))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].arguments, args, "the exact arguments are stored");
    assert!(
        listed[0].summary.contains("subject: Hi"),
        "{}",
        listed[0].summary
    );

    let runner = RecordingRunner::default();
    let decided = after
        .decide(id, &tenant(), USER, ToolApprovalDecision::Once, &runner)
        .await
        .unwrap();
    assert_eq!(decided.status, ToolApprovalStatus::ApprovedOnce);
    assert_eq!(*runner.ran.lock().unwrap(), vec![args]);
    let stored = repo(&pool).find_request(id).await.unwrap().unwrap();
    assert_eq!(
        stored.result,
        Some(json!({"message_id": "<1@example.com>"}))
    );
    pool.close().await;
    db.remove().await;
}

/// Two answers at once: exactly one wins and the call runs once.
#[tokio::test]
async fn one_of_two_racing_answers_wins() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = Arc::new(service(&db.pool));
    let id = pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    let runner = Arc::new(RecordingRunner::default());
    let mut handles = Vec::new();
    for _ in 0..4 {
        let (svc, runner) = (svc.clone(), runner.clone());
        handles.push(tokio::spawn(async move {
            svc.decide(
                id,
                &tenant(),
                USER,
                ToolApprovalDecision::Once,
                runner.as_ref(),
            )
            .await
            .is_ok()
        }));
    }
    let mut wins = 0;
    for h in handles {
        if h.await.unwrap() {
            wins += 1;
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(runner.ran.lock().unwrap().len(), 1);
    db.remove().await;
}

/// A policy matches only its own user, tool and binding (a tool with no
/// binding included), and stops matching once revoked.
#[tokio::test]
async fn a_policy_matches_only_its_own_user_tool_and_binding_until_revoked() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let runner = RecordingRunner::default();
    for args in [json!({"account": "b-1"}), json!({})] {
        let id = pending_id(gate(&svc, USER, "outbound.send", &args).await);
        svc.decide(id, &tenant(), USER, ToolApprovalDecision::Always, &runner)
            .await
            .unwrap();
        assert!(
            matches!(
                gate(&svc, USER, "outbound.send", &args).await,
                GateOutcome::Proceed { .. }
            ),
            "the policy for {args} did not apply"
        );
    }
    pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-2"})).await);
    pending_id(gate(&svc, USER, "outbound.reply", &json!({"account": "b-1"})).await);
    pending_id(
        gate(
            &svc,
            "other-sub",
            "outbound.send",
            &json!({"account": "b-1"}),
        )
        .await,
    );

    let auto = svc
        .list_for_user(&tenant(), USER, Some(ToolApprovalStatus::AutoAllowed))
        .await
        .unwrap();
    assert_eq!(auto.len(), 2);
    assert!(auto.iter().all(|r| r.policy_id.is_some()));

    for policy in svc.list_policies(&tenant(), USER).await.unwrap() {
        svc.revoke_policy(policy.id, &tenant(), USER).await.unwrap();
    }
    pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    pending_id(gate(&svc, USER, "outbound.send", &json!({})).await);
    db.remove().await;
}

/// The sweep expires what has waited 72 hours and nothing younger.
#[tokio::test]
async fn the_sweep_expires_only_requests_pending_72_hours() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let old = pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    let young = pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-2"})).await);
    sqlx::query(
        "UPDATE tool_approval_requests SET created_at = now() - interval '73 hours' WHERE id = $1",
    )
    .bind(old.0)
    .execute(&db.pool)
    .await
    .unwrap();

    assert_eq!(svc.expire_stale(chrono::Utc::now()).await.unwrap(), 1);
    let repo = repo(&db.pool);
    assert_eq!(
        repo.find_request(old).await.unwrap().unwrap().status,
        ToolApprovalStatus::Expired
    );
    assert_eq!(
        repo.find_request(young).await.unwrap().unwrap().status,
        ToolApprovalStatus::Pending
    );
    db.remove().await;
}

/// Migration 036 applied again over a migrated schema, with rows in it,
/// changes nothing.
#[tokio::test]
async fn migration_036_run_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM tool_approval_requests) AS requests, \
                    (SELECT count(*) FROM pg_indexes WHERE tablename LIKE 'tool_approval%') AS indexes",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| (row.get::<i64, _>("requests"), row.get::<i64, _>("indexes")))
        .unwrap()
    };
    let before = snapshot().await;
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 36)
        .expect("migration 036 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 036 run again");
    assert_eq!(snapshot().await, before);
    db.remove().await;
}

/// Test (a) against PostgreSQL: a policy keyed on the contract-declared
/// argument `account` matches only its own tool and binding; where the tool
/// declares no binding argument, `account` is no key and the call waits.
#[tokio::test]
async fn a_policy_keyed_on_a_contract_declared_argument_matches_only_its_own_tool_and_binding() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let runner = RecordingRunner::default();
    let id = pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-1"})).await);
    let decided = svc
        .decide(id, &tenant(), USER, ToolApprovalDecision::Always, &runner)
        .await
        .unwrap();
    assert_eq!(decided.binding_id.as_deref(), Some("b-1"));
    assert!(matches!(
        gate(
            &svc,
            USER,
            "outbound.send",
            &json!({"account": "b-1", "to": "x"})
        )
        .await,
        GateOutcome::Proceed { .. }
    ));
    pending_id(gate(&svc, USER, "outbound.send", &json!({"account": "b-2"})).await);
    pending_id(gate(&svc, USER, "outbound.reply", &json!({"account": "b-1"})).await);
    pending_id(
        gate_declaring(
            &svc,
            USER,
            "outbound.send",
            &json!({"account": "b-1"}),
            ApprovalContract::default(),
        )
        .await,
    );
    let stored = repo(&db.pool)
        .list_requests_for_user(&tenant(), USER, Some(ToolApprovalStatus::Pending))
        .await
        .unwrap();
    let summaries: Vec<&str> = stored.iter().map(|r| r.summary.as_str()).collect();
    assert!(
        summaries.contains(&"outbound.reply\nto: \nsubject: \nbody: "),
        "{summaries:?}"
    );
    db.remove().await;
}

const CONVERSATION: &str = "6c1f0b52-8a3e-4d7b-9f21-0e5d4c3b2a19";

/// A pending request of `USER`'s, made in `conversation` when given, as the
/// gate builds one.
fn request_in(conversation: Option<&str>) -> ToolApprovalRequest {
    ToolApprovalRequest {
        id: ToolApprovalId::new(),
        tenant_id: tenant(),
        user_sub: USER.to_string(),
        execution_id: ExecutionId::new(),
        agent_id: AgentId::new(),
        tool_name: "outbound.send".to_string(),
        arguments: json!({"account": "b-1"}),
        summary: "outbound.send".to_string(),
        binding_id: Some("b-1".to_string()),
        security_context_name: "zaru-pro".to_string(),
        conversation_id: conversation.map(str::to_string),
        schedule_id: None,
        schedule_name: None,
        profile_id: None,
        policy_id: None,
        status: ToolApprovalStatus::Pending,
        created_at: chrono::Utc::now(),
        decided_at: None,
        decided_by: None,
        result: None,
        error: None,
    }
}

/// Migration 044 adds `conversation_id`, and applied again over a migrated
/// schema, with a row naming a conversation in it, changes nothing.
#[tokio::test]
async fn migration_044_run_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM tool_approval_requests) AS requests, \
                    (SELECT count(*) FROM information_schema.columns \
                      WHERE table_name = 'tool_approval_requests' \
                        AND column_name = 'conversation_id' AND is_nullable = 'YES') AS columns",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| (row.get::<i64, _>("requests"), row.get::<i64, _>("columns")))
        .unwrap()
    };
    assert_eq!(
        snapshot().await.1,
        1,
        "tool_approval_requests has no nullable conversation_id column after every migration"
    );
    let repo = repo(&db.pool);
    let stored = request_in(Some(CONVERSATION));
    repo.insert_request(&stored).await.unwrap();
    let before = snapshot().await;
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 44)
        .expect("migration 044 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 044 run again");
    assert_eq!(
        snapshot().await,
        before,
        "migration 044 run again changed the schema or the rows"
    );
    let found = repo.find_request(stored.id).await.unwrap().unwrap();
    assert_eq!(
        found.conversation_id.as_deref(),
        Some(CONVERSATION),
        "migration 044 run again lost a stored conversation_id"
    );
    db.remove().await;
}

/// The store keeps a request's conversation: a value, and null for a
/// request no conversation started, read back by id and in the user's list.
#[tokio::test]
async fn a_conversation_id_round_trips_through_the_store_a_value_and_null() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = repo(&db.pool);
    let in_conversation = request_in(Some(CONVERSATION));
    let in_none = request_in(None);
    repo.insert_request(&in_conversation).await.unwrap();
    repo.insert_request(&in_none).await.unwrap();
    let by_id = (
        repo.find_request(in_conversation.id)
            .await
            .unwrap()
            .unwrap()
            .conversation_id,
        repo.find_request(in_none.id)
            .await
            .unwrap()
            .unwrap()
            .conversation_id,
    );
    let listed = repo
        .list_requests_for_user(&tenant(), USER, None)
        .await
        .unwrap();
    let in_list = |id: ToolApprovalId| {
        listed
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.conversation_id.clone())
    };
    assert_eq!(
        (by_id, in_list(in_conversation.id), in_list(in_none.id)),
        (
            (Some(CONVERSATION.to_string()), None),
            Some(Some(CONVERSATION.to_string())),
            Some(None)
        ),
        "the store did not round-trip conversation_id (a value and null)"
    );
    db.remove().await;
}

// ── The schedule a gated call's run belongs to (AEGIS ADR-139 N9) ──────────

/// A schedule row of `USER`'s named `name`, deleted when `deleted`.
async fn schedule_row(pool: &PgPool, name: &str, deleted: bool) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO schedules (id, tenant_id, owner_sub, owner_realm, owner_kind, name, \
         target_kind, target, run_at, state, temporal_schedule_id, created_at, updated_at, \
         deleted_at) VALUES ($1, $2, $3, 'zaru-consumer', 'consumer_user', $4, 'agent', \
         'palindrome-checker', NOW() + INTERVAL '1 day', 'active', $5, NOW(), NOW(), \
         CASE WHEN $6 THEN NOW() END)",
    )
    .bind(id)
    .bind(tenant().as_str())
    .bind(USER)
    .bind(name)
    .bind(format!("aegis-schedule-{id}"))
    .bind(deleted)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// A gated call of a run a schedule started stores that schedule's id,
/// read from the run's record: an agent run's own, and a workflow run's for
/// the agent run of one of its states; a run no schedule started stores
/// none. Every read answers the schedule's name, a deleted schedule's too.
#[tokio::test]
async fn a_gated_call_of_a_scheduled_run_stores_its_schedule_and_reads_its_name() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    const TENANT: &str = "aegis-system";
    let agent = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agents (id, tenant_id, name, manifest_yaml, manifest_json, runtime, security_policy) \
         VALUES ($1, $2, 'palindrome-checker', 'x', '{}', 'python:3.11', '{}')",
    )
    .bind(agent)
    .bind(TENANT)
    .execute(&db.pool)
    .await
    .unwrap();
    let workflow = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workflows (id, tenant_id, name, version, yaml_source, domain_json, temporal_def_json) \
         VALUES ($1, $2, 'email-inbox-triage', '1.0.0', 'x', '{}', '{}')",
    )
    .bind(workflow)
    .bind(TENANT)
    .execute(&db.pool)
    .await
    .unwrap();
    let digest = schedule_row(&db.pool, "Morning digest", false).await;
    let triage = schedule_row(&db.pool, "Weekly triage", true).await;

    let workflow_run = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workflow_executions (id, tenant_id, workflow_id, temporal_workflow_id, \
         temporal_run_id, started_at, schedule_id) VALUES ($1, $2, $3, 't', 'r', NOW(), $4)",
    )
    .bind(workflow_run)
    .bind(TENANT)
    .bind(workflow)
    .bind(triage)
    .execute(&db.pool)
    .await
    .unwrap();
    let run = |schedule: Option<uuid::Uuid>, workflow_run: Option<uuid::Uuid>| {
        let pool = db.pool.clone();
        async move {
            let id = uuid::Uuid::new_v4();
            sqlx::query(
                "INSERT INTO executions (id, tenant_id, agent_id, input, security_context_name, \
                 schedule_id, workflow_execution_id) VALUES ($1, $2, $3, '{}', 'zaru-free', $4, $5)",
            )
            .bind(id)
            .bind(TENANT)
            .bind(agent)
            .bind(schedule)
            .bind(workflow_run)
            .execute(&pool)
            .await
            .unwrap();
            ExecutionId(id)
        }
    };
    let agent_run = run(Some(digest), None).await;
    let state_run = run(None, Some(workflow_run)).await;
    let unscheduled = run(None, None).await;

    let svc = service(&db.pool);
    let mut stored = Vec::new();
    for execution_id in [agent_run, state_run, unscheduled] {
        let outcome = svc
            .gate(GatedCall {
                tenant_id: &tenant(),
                user_sub: Some(USER),
                execution_id,
                agent_id: AgentId(agent),
                tool_name: "outbound.send",
                arguments: &json!({"account": "b-1"}),
                security_context_name: "zaru-pro",
                conversation_id: None,
                profile_id: None,
                contract: contract(),
            })
            .await
            .expect("gate");
        let GateOutcome::Pending { result } = outcome else {
            panic!("expected pending, got {outcome:?}");
        };
        stored.push(ToolApprovalId::from_string(result["approval_id"].as_str().unwrap()).unwrap());
    }

    let repo = repo(&db.pool);
    let mut by_id = Vec::new();
    for id in &stored {
        let request = repo.find_request(*id).await.unwrap().unwrap();
        by_id.push((request.schedule_id, request.schedule_name));
    }
    let listed = repo
        .list_requests_for_user(&tenant(), USER, Some(ToolApprovalStatus::Pending))
        .await
        .unwrap();
    let in_list: Vec<_> = stored
        .iter()
        .map(|id| {
            listed
                .iter()
                .find(|r| r.id == *id)
                .map(|r| (r.schedule_id, r.schedule_name.clone()))
        })
        .collect();
    let expected = vec![
        (Some(digest), Some("Morning digest".to_string())),
        (Some(triage), Some("Weekly triage".to_string())),
        (None, None),
    ];
    assert_eq!(
        (by_id, in_list),
        (
            expected.clone(),
            expected.into_iter().map(Some).collect::<Vec<_>>()
        ),
        "a scheduled run's gated call did not store its schedule's id and read its name \
         (an agent run, a workflow state's run, a run no schedule started)"
    );
    db.remove().await;
}

// ── Sealing (ADR-126, Update of 2026-10-08) ─────────────────────────────────

const PLANTED: &str = "PLANTED-7f3a the meeting moves to Thursday at the old place";

fn planted_args() -> Value {
    json!({
        "account": "b-1",
        "to": "friend@example.com",
        "subject": "Planted subject 7f3a",
        "body": PLANTED,
    })
}

/// Answers the run with the planted message in its result.
struct EchoRunner;

#[async_trait::async_trait]
impl ApprovedCallRunner for EchoRunner {
    async fn run_approved_call(&self, _: &ToolApprovalRequest) -> Result<Value, String> {
        Ok(json!({"sent": PLANTED}))
    }
}

/// Refuses the run with an error naming the planted message.
struct RefusingRunner;

#[async_trait::async_trait]
impl ApprovedCallRunner for RefusingRunner {
    async fn run_approved_call(&self, _: &ToolApprovalRequest) -> Result<Value, String> {
        Err(format!(
            "'{PLANTED}' is not an email address this tool can send to."
        ))
    }
}

async fn gate_for(svc: &ToolApprovalService, sub: &str, args: &Value) -> GateOutcome {
    svc.gate(GatedCall {
        tenant_id: &TenantId::for_consumer_user(sub).unwrap(),
        user_sub: Some(sub),
        execution_id: ExecutionId::new(),
        agent_id: AgentId::new(),
        tool_name: "outbound.send",
        arguments: args,
        security_context_name: "zaru-pro",
        conversation_id: None,
        profile_id: None,
        contract: contract(),
    })
    .await
    .expect("gate")
}

/// The four sealed columns of a row as their text values.
async fn stored_columns(pool: &PgPool, id: ToolApprovalId) -> Vec<(&'static str, Option<String>)> {
    let row = sqlx::query(
        "SELECT arguments #>> '{}' AS arguments, summary, result #>> '{}' AS result, error \
         FROM tool_approval_requests WHERE id = $1",
    )
    .bind(id.0)
    .fetch_one(pool)
    .await
    .unwrap();
    ["arguments", "summary", "result", "error"]
        .into_iter()
        .map(|c| (c, row.get::<Option<String>, _>(c)))
        .collect()
}

/// Test 1: after the write, no sealed column holds plaintext of the planted
/// message; each holds `vault:v1:` ciphertext.
#[tokio::test]
async fn a_stored_requests_arguments_summary_result_and_error_hold_no_plaintext_of_its_message() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let sent = pending_id(gate(&svc, USER, "outbound.send", &planted_args()).await);
    svc.decide(
        sent,
        &tenant(),
        USER,
        ToolApprovalDecision::Once,
        &EchoRunner,
    )
    .await
    .unwrap();
    let refused = pending_id(gate(&svc, USER, "outbound.send", &planted_args()).await);
    svc.decide(
        refused,
        &tenant(),
        USER,
        ToolApprovalDecision::Once,
        &RefusingRunner,
    )
    .await
    .unwrap();

    let mut failures = Vec::new();
    for (id, columns) in [
        (sent, ["arguments", "summary", "result"].as_slice()),
        (refused, ["arguments", "summary", "error"].as_slice()),
    ] {
        for (column, value) in stored_columns(&db.pool, id).await {
            if !columns.contains(&column) {
                continue;
            }
            let value = value.unwrap_or_default();
            if value.contains("PLANTED-7f3a") || value.contains("Planted subject") {
                failures.push(format!(
                    "{column} holds plaintext of the planted message: {value}"
                ));
            } else if !value.starts_with("vault:v1:") {
                failures.push(format!("{column} is not Transit ciphertext: {value}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    db.remove().await;
}

/// Test 2: every read the request's user makes answers the plaintext, and
/// the approved run receives the plaintext arguments.
#[tokio::test]
async fn a_read_answers_the_plaintext_to_its_user() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let id = pending_id(gate(&svc, USER, "outbound.send", &planted_args()).await);
    let listed = svc
        .list_for_user(&tenant(), USER, Some(ToolApprovalStatus::Pending))
        .await
        .unwrap();
    assert_eq!(listed[0].arguments, planted_args());
    assert!(listed[0].summary.contains(PLANTED), "{}", listed[0].summary);

    let runner = RecordingRunner::default();
    svc.decide(id, &tenant(), USER, ToolApprovalDecision::Once, &runner)
        .await
        .unwrap();
    assert_eq!(*runner.ran.lock().unwrap(), vec![planted_args()]);
    let read = svc.get_for_user(id, &tenant(), USER).await.unwrap();
    assert_eq!(read.arguments, planted_args());
    assert_eq!(read.result, Some(json!({"message_id": "<1@example.com>"})));
    db.remove().await;
}

/// Test 3: a row written before migration 045 reads as it was stored, and
/// its approved run receives its arguments.
#[tokio::test]
async fn a_row_written_before_migration_045_still_reads() {
    let Some(db) = TestDb::create_before(45).await else {
        return;
    };
    let id = ToolApprovalId::new();
    let args = json!({"account": "b-1", "to": "old@example.com", "body": "written before"});
    sqlx::query(
        "INSERT INTO tool_approval_requests (id, tenant_id, user_sub, execution_id, agent_id, \
         tool_name, arguments, summary, binding_id, security_context_name, status, created_at) \
         VALUES ($1, $2, $3, $4, $5, 'outbound.send', $6, 'outbound.send\nbody: written before', \
         'b-1', 'zaru-pro', 'pending', now())",
    )
    .bind(id.0)
    .bind(tenant().as_str())
    .bind(USER)
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .bind(&args)
    .execute(&db.pool)
    .await
    .unwrap();
    MIGRATOR.run(&db.pool).await.expect("apply migration 045");

    let svc = service(&db.pool);
    let read = svc.get_for_user(id, &tenant(), USER).await;
    let read = read.unwrap_or_else(|e| panic!("a row written before 045 did not read: {e}"));
    assert_eq!(read.arguments, args);
    assert_eq!(read.summary, "outbound.send\nbody: written before");
    let runner = RecordingRunner::default();
    let decided = svc
        .decide(id, &tenant(), USER, ToolApprovalDecision::Once, &runner)
        .await
        .unwrap_or_else(|e| panic!("a row written before 045 could not be answered: {e}"));
    assert_eq!(*runner.ran.lock().unwrap(), vec![args]);
    assert_eq!(
        decided.result,
        Some(json!({"message_id": "<1@example.com>"}))
    );
    db.remove().await;
}

/// Test 4: migration 045 applied again over a migrated schema, with sealed
/// rows in it, changes nothing.
#[tokio::test]
async fn migration_045_run_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    pending_id(gate(&svc, USER, "outbound.send", &planted_args()).await);
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM tool_approval_requests WHERE sealed) AS sealed, \
                    (SELECT count(*) FROM information_schema.columns \
                     WHERE table_name = 'tool_approval_requests') AS columns",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| (row.get::<i64, _>("sealed"), row.get::<i64, _>("columns")))
        .unwrap()
    };
    let before = snapshot().await;
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 45)
        .expect("migration 045 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 045 run again");
    assert_eq!(snapshot().await, before);
    db.remove().await;
}

/// Test 5: each tenant's rows are sealed under its own key.
#[tokio::test]
async fn each_tenant_seals_under_its_own_key() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let transit = Arc::new(TransitStandIn::default());
    let svc = service_with(&db.pool, &transit);
    let mut failures = Vec::new();
    for sub in [USER, "other-sub"] {
        let id = pending_id(gate_for(&svc, sub, &planted_args()).await);
        let expected = transit_key(&TenantId::for_consumer_user(sub).unwrap());
        for (column, value) in stored_columns(&db.pool, id).await {
            let Some(value) = value else { continue };
            match transit.key_of(&value) {
                Some(key) if key == expected => {}
                other => failures.push(format!(
                    "{sub}'s {column} is not sealed under {expected}: sealed under {other:?}"
                )),
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    db.remove().await;
}

/// Test 6: the operator's list answers the sealed columns as ciphertext, and
/// neither it nor the sweep decrypts anything.
#[tokio::test]
async fn the_operator_read_and_the_sweep_decrypt_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let transit = Arc::new(TransitStandIn::default());
    let svc = service_with(&db.pool, &transit);
    pending_id(gate(&svc, USER, "outbound.send", &planted_args()).await);
    let old = pending_id(gate(&svc, USER, "outbound.send", &planted_args()).await);
    sqlx::query(
        "UPDATE tool_approval_requests SET created_at = now() - interval '73 hours' WHERE id = $1",
    )
    .bind(old.0)
    .execute(&db.pool)
    .await
    .unwrap();

    let mut failures = Vec::new();
    for request in svc.list_all(None).await.unwrap() {
        let arguments = request.arguments.as_str().unwrap_or_default().to_string();
        for (column, value) in [
            ("arguments", arguments),
            ("summary", request.summary.clone()),
        ] {
            if !value.starts_with("vault:v1:") {
                failures.push(format!(
                    "the operator's list answers {column} unsealed: {value}"
                ));
            }
        }
    }
    assert_eq!(svc.expire_stale(chrono::Utc::now()).await.unwrap(), 1);
    let decrypts = transit.decrypts.load(Ordering::SeqCst);
    if decrypts != 0 {
        failures.push(format!(
            "the operator's list and the sweep decrypted {decrypts} values"
        ));
    }
    assert!(failures.is_empty(), "{failures:#?}");
    db.remove().await;
}

/// Test 7: a seal Transit refuses stores no row, and the gate fails.
#[tokio::test]
async fn a_refused_seal_stores_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let transit = Arc::new(TransitStandIn::default());
    transit.refuse.store(true, Ordering::SeqCst);
    let svc = service_with(&db.pool, &transit);
    let outcome = svc
        .gate(GatedCall {
            tenant_id: &tenant(),
            user_sub: Some(USER),
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tool_name: "outbound.send",
            arguments: &planted_args(),
            security_context_name: "zaru-pro",
            conversation_id: None,
            profile_id: None,
            contract: contract(),
        })
        .await;
    let rows: i64 = sqlx::query("SELECT count(*) AS n FROM tool_approval_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get("n");
    assert!(
        outcome.is_err() && rows == 0,
        "a refused seal stored {rows} rows and the gate answered {outcome:?}"
    );
    db.remove().await;
}

// ─── Deny policies (ADR-126, Update of 2026-10-08 (2)) ────────────────────

fn policy_on(binding: &str, effect: ToolApprovalPolicyEffect) -> ToolApprovalPolicy {
    ToolApprovalPolicy {
        id: ToolApprovalPolicyId::new(),
        tenant_id: tenant(),
        user_sub: USER.to_string(),
        tool_name: "outbound.send".to_string(),
        binding_id: Some(binding.to_string()),
        profile_id: None,
        effect,
        created_at: chrono::Utc::now(),
        created_by: USER.to_string(),
        revoked_at: None,
    }
}

/// Clause 1: migration 046 over a database holding a policy stored before
/// it leaves that policy reading `allow`, admits `auto_denied` in the status
/// CHECK, and run again changes nothing.
#[tokio::test]
async fn migration_046_run_again_changes_nothing_and_a_policy_stored_before_it_reads_allow() {
    let Some(db) = TestDb::create_before(46).await else {
        return;
    };
    let before_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tool_approval_policies \
         (id, tenant_id, user_sub, tool_name, binding_id, created_at, created_by) \
         VALUES ($1, $2, $3, 'outbound.send', 'b-1', now(), $3)",
    )
    .bind(before_id)
    .bind(tenant().as_str())
    .bind(USER)
    .execute(&db.pool)
    .await
    .expect("store a policy before migration 046");
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM tool_approval_policies) AS policies, \
                    (SELECT count(*) FROM information_schema.columns \
                      WHERE table_name = 'tool_approval_policies' AND column_name = 'effect') \
                      AS effect_columns, \
                    (SELECT string_agg(pg_get_constraintdef(oid), ' | ' ORDER BY conname) \
                       FROM pg_constraint \
                      WHERE conrelid IN ('tool_approval_requests'::regclass, \
                                         'tool_approval_policies'::regclass) \
                        AND contype = 'c') AS checks",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| {
            (
                row.get::<i64, _>("policies"),
                row.get::<i64, _>("effect_columns"),
                row.get::<Option<String>, _>("checks").unwrap_or_default(),
            )
        })
        .unwrap()
    };
    let mut wrong = Vec::new();
    let Some(migration) = MIGRATOR.iter().find(|m| m.version == 46) else {
        panic!("{:#?}", vec!["migration 046 does not ship"]);
    };
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 046 over a policy stored before it");
    let after_first = snapshot().await;
    if after_first.1 != 1 {
        wrong.push("tool_approval_policies has no effect column after migration 046".to_string());
    }
    if !after_first.2.contains("'auto_denied'") {
        wrong.push(format!(
            "the status CHECK does not admit auto_denied after migration 046: {}",
            after_first.2
        ));
    }
    // The store reads the columns of every migration it ships (050 adds
    // `profile_id`): the later migrations are applied before it reads, as
    // the 045 test applies them.
    MIGRATOR
        .run(&db.pool)
        .await
        .expect("apply the migrations after 046");
    match repo(&db.pool).list_active_policies(&tenant(), USER).await {
        Ok(policies) => {
            if policies.len() != 1 || policies[0].effect != ToolApprovalPolicyEffect::Allow {
                wrong.push(format!(
                    "the policy stored before migration 046 does not read allow: {policies:?}"
                ));
            }
        }
        Err(e) => wrong.push(format!(
            "the policies cannot be read after migration 046: {e}"
        )),
    }
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 046 run again");
    if snapshot().await != after_first {
        wrong.push("migration 046 run again changed the schema or the rows".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    db.remove().await;
}

/// Clause 3 against the real schema: a call matching a deny policy is
/// stored `auto_denied` with the policy's id, and the status CHECK admits it.
#[tokio::test]
async fn a_call_matching_a_deny_policy_is_stored_auto_denied() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let svc = service(&db.pool);
    let runner = RecordingRunner::default();
    let args = json!({"account": "b-1", "to": "x@example.com"});
    let id = pending_id(gate(&svc, USER, "outbound.send", &args).await);
    svc.decide(
        id,
        &tenant(),
        USER,
        ToolApprovalDecision::AlwaysDeny,
        &runner,
    )
    .await
    .expect("always_deny");
    let mut wrong = Vec::new();
    match svc
        .gate(GatedCall {
            tenant_id: &tenant(),
            user_sub: Some(USER),
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tool_name: "outbound.send",
            arguments: &args,
            security_context_name: "zaru-pro",
            conversation_id: None,
            profile_id: None,
            contract: contract(),
        })
        .await
    {
        Ok(GateOutcome::Denied { result }) if result["status"] == "auto_denied" => {}
        other => wrong.push(format!(
            "a call matching the deny policy answered {other:?}, not auto_denied"
        )),
    }
    let policy = svc.list_policies(&tenant(), USER).await.unwrap();
    let rows = svc
        .list_for_user(&tenant(), USER, Some(ToolApprovalStatus::AutoDenied))
        .await
        .unwrap();
    if rows.len() != 1 || rows[0].policy_id != policy.first().map(|p| p.id) {
        wrong.push(format!(
            "no single stored auto_denied row with the deny policy's id: {rows:?}"
        ));
    }
    if !runner.ran.lock().unwrap().is_empty() {
        wrong.push("a denied call ran".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    db.remove().await;
}

/// Clause 4: an allow and a deny policy are never both unrevoked for one
/// key: a later write replaces the earlier one, sixteen concurrent writes of
/// one key leave exactly one, and another binding's policy is untouched.
#[tokio::test]
async fn an_allow_and_a_deny_policy_never_coexist_for_one_key() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(repo(&db.pool));
    let mut wrong = Vec::new();
    store
        .insert_policy(&policy_on("b-2", ToolApprovalPolicyEffect::Allow))
        .await
        .unwrap();
    store
        .insert_policy(&policy_on("b-1", ToolApprovalPolicyEffect::Allow))
        .await
        .unwrap();
    store
        .insert_policy(&policy_on("b-1", ToolApprovalPolicyEffect::Deny))
        .await
        .unwrap();

    let writers: Vec<_> = (0..16)
        .map(|i| {
            let store = store.clone();
            let effect = if i % 2 == 0 {
                ToolApprovalPolicyEffect::Allow
            } else {
                ToolApprovalPolicyEffect::Deny
            };
            tokio::spawn(async move { store.insert_policy(&policy_on("b-3", effect)).await })
        })
        .collect();
    for writer in writers {
        writer.await.unwrap().expect("a concurrent policy write");
    }

    let active = store.list_active_policies(&tenant(), USER).await.unwrap();
    let on = |binding: &str| -> Vec<ToolApprovalPolicyEffect> {
        active
            .iter()
            .filter(|p| p.binding_id.as_deref() == Some(binding))
            .map(|p| p.effect)
            .collect()
    };
    if on("b-1") != vec![ToolApprovalPolicyEffect::Deny] {
        wrong.push(format!(
            "b-1 holds {:?}, not the one deny that replaced the allow",
            on("b-1")
        ));
    }
    if on("b-2") != vec![ToolApprovalPolicyEffect::Allow] {
        wrong.push(format!(
            "another binding's policy was touched: b-2 holds {:?}",
            on("b-2")
        ));
    }
    if on("b-3").len() != 1 {
        wrong.push(format!(
            "sixteen concurrent writes of one key left {} unrevoked policies: {:?}",
            on("b-3").len(),
            on("b-3")
        ));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    db.remove().await;
}

// ─── Profiles (AEGIS ADR-140 D9) ──────────────────────────────────────────

/// Migration 050 over a database holding a policy stored before it: the
/// policy has no profile and keeps matching raw-binding calls, the three
/// tables gain `profile_id`, and the migration run again changes nothing.
#[tokio::test]
async fn migration_050_run_again_changes_nothing_and_a_policy_stored_before_it_matches_raw_calls() {
    let Some(db) = TestDb::create_before(50).await else {
        return;
    };
    sqlx::query(
        "INSERT INTO tool_approval_policies \
         (id, tenant_id, user_sub, tool_name, binding_id, created_at, created_by) \
         VALUES ($1, $2, $3, 'outbound.send', 'b-1', now(), $3)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(tenant().as_str())
    .bind(USER)
    .execute(&db.pool)
    .await
    .expect("store a policy before migration 050");
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM tool_approval_policies) AS policies, \
                    (SELECT count(*) FROM information_schema.columns \
                      WHERE column_name = 'profile_id' AND table_name IN \
                      ('tool_approval_policies', 'tool_approval_requests', 'schedules')) \
                      AS profile_columns, \
                    (SELECT count(*) FROM pg_indexes \
                      WHERE tablename = 'tool_approval_policies') AS indexes",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| {
            (
                row.get::<i64, _>("policies"),
                row.get::<i64, _>("profile_columns"),
                row.get::<i64, _>("indexes"),
            )
        })
        .unwrap()
    };
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 50)
        .expect("migration 050 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 050 over a policy stored before it");
    let once = snapshot().await;
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 050 run again");
    let twice = snapshot().await;
    let found = repo(&db.pool)
        .find_active_policy(&tenant(), USER, "outbound.send", Some("b-1"), None)
        .await
        .unwrap();
    let mut wrong = Vec::new();
    if once.1 != 3 {
        wrong.push(format!("{} of three tables gained profile_id", once.1));
    }
    if once != twice {
        wrong.push(format!("run again it changed {once:?} to {twice:?}"));
    }
    if found.map(|p| p.profile_id) != Some(None) {
        wrong.push("the policy stored before it no longer matches a raw call".to_string());
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    db.remove().await;
}

/// A choice made in a profile and one made on raw bindings, on one tool and
/// binding, coexist and each matches only its own calls; deleting the
/// profile revokes its choice and leaves the raw one.
#[tokio::test]
async fn a_choice_made_in_a_profile_matches_only_that_profiles_calls() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = repo(&db.pool);
    let profile = uuid::Uuid::new_v4();
    let mut in_profile = policy_on("b-1", ToolApprovalPolicyEffect::Allow);
    in_profile.profile_id = Some(profile);
    let raw = policy_on("b-1", ToolApprovalPolicyEffect::Deny);
    store.insert_policy(&in_profile).await.unwrap();
    store.insert_policy(&raw).await.unwrap();
    let find = |profile_id: Option<uuid::Uuid>| {
        let store = &store;
        async move {
            store
                .find_active_policy(&tenant(), USER, "outbound.send", Some("b-1"), profile_id)
                .await
                .unwrap()
                .map(|p| p.id)
        }
    };
    let mut wrong = Vec::new();
    if find(Some(profile)).await != Some(in_profile.id) {
        wrong.push("the profile's call did not find the profile's choice".to_string());
    }
    if find(None).await != Some(raw.id) {
        wrong.push(
            "a raw call did not find the raw choice, or the profile's replaced it".to_string(),
        );
    }
    if find(Some(uuid::Uuid::new_v4())).await.is_some() {
        wrong.push("another profile's call found a choice".to_string());
    }
    let revoked = store
        .revoke_profile_policies(&tenant(), USER, profile, chrono::Utc::now())
        .await
        .unwrap();
    if revoked != 1 || find(Some(profile)).await.is_some() || find(None).await != Some(raw.id) {
        wrong.push(format!(
            "deleting the profile revoked {revoked}, or left its choice, or took the raw one"
        ));
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    db.remove().await;
}
