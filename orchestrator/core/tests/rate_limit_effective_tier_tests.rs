// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! AEGIS ADR-072 §4 step 3 against a real PostgreSQL, with the migrations
//! the daemon ships (`cli/migrations`, applied in order): a consumer's tier
//! is the one the store the `zaru_tier` claim is written from holds
//! (`EffectiveTierService::compute_effective_tier`), not the tier a writer
//! guessed when it rebuilt the identity (AEGIS known defect
//! `pro-tenant-refused-on-free-limits-2026-10-10`).
//!
//! Each writer here is built as the daemon builds it: the policy resolver
//! from the pool (`server.rs`), the composite enforcer over the burst and
//! window enforcers. The rebuilt identities are the writers' own: the
//! dispatch gateway's consumer at Free (`dispatch.rs`), and a schedule's
//! owner from its stored `owner_zaru_tier` (`ScheduleOwner::to_identity`).
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere it says it was skipped and passes.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::ports::{
    ScheduleEnginePort, StartWorkflowParams, TemporalScheduleDescription, TemporalScheduleSpec,
    WorkflowEnginePort,
};
use aegis_orchestrator_core::application::schedule_service::{ScheduleService, ServiceRunStarter};
use aegis_orchestrator_core::application::start_workflow_execution::StandardStartWorkflowExecutionUseCase;
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentScope};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::rate_limit::{
    tier_defaults, RateLimitEnforcer, RateLimitPolicyResolver, RateLimitResourceType,
    RateLimitScope,
};
use aegis_orchestrator_core::domain::repository::{AgentVersion, WorkflowRepository};
use aegis_orchestrator_core::domain::schedule::{
    FireOutcome, RecurrenceInput, Schedule, ScheduleDraft, ScheduleRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    StateKind, StateName, Workflow, WorkflowMetadata, WorkflowSpec, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::rate_limit::{
    CompositeRateLimitEnforcer, GovernorBurstEnforcer, HierarchicalPolicyResolver,
    PostgresWindowEnforcer,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::InMemoryScheduleRepository;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use futures::Stream;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;

fn postgres_url() -> Option<String> {
    match std::env::var("AEGIS_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("AEGIS_TEST_POSTGRES_URL is not set; in CI this test must reach PostgreSQL")
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
    pool: PgPool,
}

impl TestDb {
    /// A database of its own on the test server, migrated as the daemon
    /// migrates its own.
    async fn create() -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_tier_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .expect("connect to the test database");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli/migrations");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read cli/migrations")
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
            .collect();
        files.sort();
        for file in files {
            let sql = std::fs::read_to_string(&file).unwrap();
            sqlx::raw_sql(&sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("apply {}: {e}", file.display()));
        }
        Some(Self { server, name, pool })
    }

    async fn remove(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("drop the test database");
    }
}

/// A consumer subscriber: a Keycloak `sub` and the personal tenant
/// `u-<hex>` it maps to.
fn subscriber() -> (String, TenantId) {
    let sub = Uuid::new_v4().to_string();
    let tenant = TenantId::for_consumer_user(&sub).unwrap();
    (sub, tenant)
}

/// The personal tenant and its active subscription at `tier`, as billing
/// stores them: the rows `compute_effective_tier` reads.
async fn subscribed(pool: &PgPool, sub: &str, tenant: &TenantId, tier: &str) {
    sqlx::query(
        "INSERT INTO tenants (slug, display_name, keycloak_realm, openbao_namespace) \
         VALUES ($1, $1, 'zaru-consumer', $1)",
    )
    .bind(tenant.as_str())
    .execute(pool)
    .await
    .expect("store the personal tenant");
    sqlx::query(
        "INSERT INTO tenant_subscriptions (tenant_id, stripe_customer_id, tier, status, user_sub) \
         VALUES ($1, 'cus_test', $2, 'active', $3)",
    )
    .bind(tenant.as_str())
    .bind(tier)
    .bind(sub)
    .execute(pool)
    .await
    .expect("store the subscription");
}

/// `count` charges of `resource` made an hour ago for `scope`, stored in
/// the daily window as the window enforcer stores a charge
/// (`window_start = charge time - window`).
async fn charged_today(pool: &PgPool, scope: &RateLimitScope, resource: &str, count: i64) {
    let (scope_type, scope_id) = PostgresWindowEnforcer::scope_parts(scope);
    sqlx::query(
        "INSERT INTO rate_limit_counters \
         (scope_type, scope_id, resource_type, bucket, window_start, counter) \
         VALUES ($1, $2, $3, 'daily', $4, $5)",
    )
    .bind(scope_type)
    .bind(&scope_id)
    .bind(resource)
    .bind(Utc::now() - Duration::hours(1) - Duration::days(1))
    .bind(count)
    .execute(pool)
    .await
    .expect("store the day's charges");
}

/// The daemon's enforcer: burst first, then the stored windows.
fn enforcer(pool: &PgPool) -> Arc<dyn RateLimitEnforcer> {
    Arc::new(CompositeRateLimitEnforcer::new(
        Arc::new(GovernorBurstEnforcer::new()),
        Arc::new(PostgresWindowEnforcer::new(pool.clone())),
    ))
}

/// The daemon's resolver (`server.rs`).
fn resolver(pool: &PgPool) -> Arc<dyn RateLimitPolicyResolver> {
    Arc::new(HierarchicalPolicyResolver::new(pool.clone()))
}

/// A consumer identity carrying the claim `zaru_tier`.
fn consumer(sub: &str, tenant: &TenantId, zaru_tier: ZaruTier) -> UserIdentity {
    UserIdentity {
        sub: sub.to_string(),
        realm_slug: "zaru-consumer".to_string(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier,
            tenant_id: tenant.clone(),
        },
    }
}

/// (a) A model call through the dispatch gateway, whose identity is the
/// execution's initiating user rebuilt at Free (`dispatch.rs`), for a Pro
/// subscriber who has made 600 model calls today: Pro's daily 3,000 bound
/// it, not Free's 500.
#[tokio::test]
async fn a_pro_subscribers_dispatched_model_call_after_600_today_is_allowed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (sub, tenant) = subscriber();
    subscribed(&db.pool, &sub, &tenant, "pro").await;
    let scope = RateLimitScope::User {
        tenant_id: tenant.clone(),
        user_id: sub.clone(),
    };
    charged_today(&db.pool, &scope, "llm_call", 600).await;

    // The dispatch gateway's identity, as `dispatch.rs` builds it.
    let identity = consumer(&sub, &tenant, ZaruTier::Free);
    let policy = resolver(&db.pool)
        .resolve_policy(&identity, &tenant, &RateLimitResourceType::LlmCall)
        .await
        .expect("resolve the policy");
    let decision = enforcer(&db.pool)
        .check_and_increment(&scope, &policy, 1)
        .await
        .expect("check the charge");
    db.remove().await;

    // The inner loop's refusal sentence (`inner_loop_service.rs`), from the
    // decision it would refuse on.
    let retry_hint = decision
        .retry_after_seconds
        .map(|s| format!(", retry after {s}s"))
        .unwrap_or_default();
    assert!(
        decision.allowed,
        "Rate limit exceeded for LLM calls{retry_hint}"
    );
}

/// Every schedule start of this test is refused only by a call it makes.
struct NoAgents;

#[async_trait]
impl ExecutionService for NoAgents {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        _: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<Execution> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_unscoped(&self, _: ExecutionId) -> Result<Execution> {
        anyhow::bail!("not exercised")
    }
    async fn get_iterations_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> Result<Vec<Iteration>> {
        anyhow::bail!("not exercised")
    }
    async fn cancel_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn stream_execution(
        &self,
        _: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn list_executions_for_tenant(
        &self,
        _: &TenantId,
        _: Option<AgentId>,
        _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
        _: usize,
    ) -> Result<Vec<Execution>> {
        anyhow::bail!("not exercised")
    }
    async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: aegis_orchestrator_core::domain::execution::LlmInteraction,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
}

#[async_trait]
impl AgentLifecycleService for NoAgents {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: AgentScope,
        _: Option<&UserIdentity>,
    ) -> Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
        anyhow::bail!("not exercised")
    }
    async fn update_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: AgentManifest,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

/// Temporal's schedule service, answering every call.
struct QuietTemporal;

#[async_trait]
impl ScheduleEnginePort for QuietTemporal {
    async fn create_schedule(&self, _: &TemporalScheduleSpec) -> Result<()> {
        Ok(())
    }
    async fn update_schedule(&self, _: &TemporalScheduleSpec) -> Result<()> {
        Ok(())
    }
    async fn set_schedule_paused(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    async fn delete_schedule(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn describe_schedule(&self, _: &str) -> Result<Option<TemporalScheduleDescription>> {
        Ok(None)
    }
}

/// Temporal's workflow service, starting every workflow.
struct StartingTemporal;

#[async_trait]
impl WorkflowEnginePort for StartingTemporal {
    async fn register_workflow(&self, _: &TemporalWorkflowDefinition) -> Result<()> {
        Ok(())
    }
    async fn start_workflow(&self, _: StartWorkflowParams<'_>) -> Result<String> {
        Ok("run".to_string())
    }
}

/// A one-state workflow with no input schema.
fn one_state_workflow(name: &str) -> Workflow {
    let mut states = HashMap::new();
    states.insert(
        StateName::new("DONE").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo done".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
            max_state_visits: None,
        },
    );
    Workflow::new(
        WorkflowMetadata {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
            output_schema: None,
            output_template: None,
        },
        WorkflowSpec {
            initial_state: StateName::new("DONE").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
            max_total_transitions: None,
            default_schedule: None,
            repositories: None,
        },
    )
    .unwrap()
}

/// (b) A schedule's fire, whose owner is rebuilt from the stored
/// `owner_zaru_tier` `free` (written when the claim read Free), for a Pro
/// subscriber who has started 11 workflows today: the run starts, bound by
/// Pro's daily 100, not Free's 10.
#[tokio::test]
async fn a_pro_subscribers_schedule_stored_at_free_starts_after_11_today() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (sub, tenant) = subscriber();
    subscribed(&db.pool, &sub, &tenant, "pro").await;
    charged_today(
        &db.pool,
        &RateLimitScope::User {
            tenant_id: tenant.clone(),
            user_id: sub.clone(),
        },
        "workflow_execution",
        11,
    )
    .await;

    let workflows = Arc::new(InMemoryWorkflowRepository::new());
    workflows
        .save_for_tenant(&tenant, &one_state_workflow("email-inbox-triage"))
        .await
        .unwrap();
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let start = StandardStartWorkflowExecutionUseCase::new(
        workflows,
        executions.clone(),
        Arc::new(tokio::sync::RwLock::new(Some(
            Arc::new(StartingTemporal) as Arc<dyn WorkflowEnginePort>
        ))),
        Arc::new(EventBus::new(8)),
    )
    .with_rate_limiting(enforcer(&db.pool), resolver(&db.pool));
    let runs = ServiceRunStarter::new(
        Arc::new(NoAgents),
        Arc::new(NoAgents),
        Some(Arc::new(start)),
        Some(executions),
    );
    let store = Arc::new(InMemoryScheduleRepository::new());
    let service = ScheduleService::new(store.clone(), Arc::new(QuietTemporal), Arc::new(runs));

    // The schedule as it was made while the owner's claim read Free.
    let schedule = Schedule::create(
        ScheduleDraft {
            name: Some("triage".into()),
            target_kind: Some("workflow".into()),
            target: Some("email-inbox-triage".into()),
            intent: Some("triage my inbox".into()),
            input: Some(json!({})),
            recurrence: Some(RecurrenceInput {
                cron: Some("30 8 * * *".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        &consumer(&sub, &tenant, ZaruTier::Free),
        tenant.clone(),
        Utc::now(),
    )
    .unwrap();
    assert_eq!(schedule.owner.zaru_tier.as_deref(), Some("free"));
    store.insert(&schedule).await.unwrap();

    let fire = service
        .fire(&tenant, schedule.id, Utc::now())
        .await
        .expect("the fire is decided");
    db.remove().await;

    assert_eq!(
        fire.outcome,
        FireOutcome::Started,
        "{}",
        fire.detail.unwrap_or_default()
    );
}

/// (c) A consumer the store holds nothing for (no subscription, no
/// membership) keeps the tier its own claim carries.
#[tokio::test]
async fn a_consumer_with_no_stored_tier_keeps_the_identitys_claim() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (sub, tenant) = subscriber();
    let resolver = resolver(&db.pool);
    let mut resolved = Vec::new();
    for claim in [ZaruTier::Free, ZaruTier::Pro] {
        let policy = resolver
            .resolve_policy(
                &consumer(&sub, &tenant, claim.clone()),
                &tenant,
                &RateLimitResourceType::WorkflowExecution,
            )
            .await
            .expect("resolve the policy");
        resolved.push((claim, policy));
    }
    db.remove().await;

    for (claim, policy) in resolved {
        let expected = tier_defaults(&claim)
            .into_iter()
            .find(|p| p.resource_type == RateLimitResourceType::WorkflowExecution)
            .unwrap();
        assert_eq!(policy, expected, "the claim {claim:?} was not kept");
    }
}
