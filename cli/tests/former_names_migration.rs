// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The daemon's rewrite of stored former server names (Carbon Forge
//! ADR-0198, Update of 2026-10-21, R3 step 6) against a real PostgreSQL,
//! with the migrations this binary ships.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. The test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere it says it was skipped and passes.
//!
//! One row per table names the former name `nuclear-notes` and one names an
//! unrelated server; the rewrite runs twice under the production
//! configuration `{name: carbon-forge, former_names: [nuclear-notes]}`. The
//! first run rewrites the six former-named rows and leaves the unrelated
//! rows, the decided approval request and the policy whose key already has
//! a standing choice under the server's own name; the second rewrites
//! nothing.

use std::sync::Arc;

use chrono::{SubsecRound, Utc};
use serde_json::{json, Value};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;
use uuid::Uuid;

use aegis_orchestrator_core::domain::agent::AgentManifest;
use aegis_orchestrator_core::domain::node_config::{FormerNames, SealGatewayConfig};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::former_names_migration::{
    run, TableRewrite,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::transit_key;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};

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
    pool: PgPool,
}

impl TestDb {
    async fn create() -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_former_names_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect to the test database");
        MIGRATOR.run(&pool).await.expect("apply the migrations");
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

/// The production configuration since the flip.
fn renamed() -> FormerNames {
    let config: SealGatewayConfig = serde_yaml::from_str(
        "url: https://aegis-seal-gateway:50055\nremote_servers:\n  - name: carbon-forge\n    \
         former_names: [nuclear-notes]\n  - github\n",
    )
    .expect("the renamed configuration loads");
    config.former_names()
}

fn secrets() -> Arc<SecretsManager> {
    Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::new()),
        Arc::new(EventBus::new(64)),
    ))
}

/// A manifest as the agent repository stores one: its YAML and JSON both
/// serialised from the parsed manifest.
fn manifest(name: &str, tools: &[&str], service: Option<&str>) -> (String, Value) {
    let listed: String = tools.iter().map(|t| format!("    - \"{t}\"\n")).collect();
    let contexts = service
        .map(|s| format!("  contexts:\n    - service: {s}\n"))
        .unwrap_or_default();
    let source = format!(
        "apiVersion: 100monkeys.ai/v1\nkind: Agent\nmetadata:\n  name: {name}\n  version: \
         \"1.0.0\"\nspec:\n  runtime:\n    language: python\n    version: \"3.11\"\n  task:\n    \
         instruction: Read the pages the request names.\n{contexts}  tools:\n{listed}"
    );
    let manifest: AgentManifest = serde_yaml::from_str(&source).expect("the fixture is a manifest");
    (
        serde_yaml::to_string(&manifest).unwrap(),
        serde_json::to_value(&manifest).unwrap(),
    )
}

async fn seal(secrets: &SecretsManager, tenant: &TenantId, value: &Value) -> Value {
    Value::String(
        secrets
            .encrypt(&transit_key(tenant), &serde_json::to_vec(value).unwrap())
            .await
            .expect("seal"),
    )
}

async fn open(secrets: &SecretsManager, tenant: &TenantId, value: &Value) -> Value {
    let bytes = secrets
        .decrypt(&transit_key(tenant), value.as_str().expect("a ciphertext"))
        .await
        .expect("open");
    serde_json::from_slice(&bytes).unwrap()
}

/// Ids of the seeded rows, by table and by what they name.
struct Seeded {
    binding_former: Uuid,
    binding_other: Uuid,
    schedule_former: Uuid,
    schedule_other: Uuid,
    profile_former: Uuid,
    profile_other: Uuid,
    policy_former: Uuid,
    policy_other: Uuid,
    policy_standing: Uuid,
    request_pending: Uuid,
    request_other: Uuid,
    request_decided: Uuid,
    agent_former: Uuid,
    agent_other: Uuid,
}

async fn seed(pool: &PgPool, secrets: &SecretsManager, tenant: &TenantId) -> Seeded {
    let now = Utc::now().trunc_subsecs(6);
    let binding = |provider: &'static str| async move {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO credential_bindings (id, owner_user_id, tenant_id, credential_type, \
             provider, label, secret_path) VALUES ($1, $2, $3, 'oauth2', $4, $4, 'p')",
        )
        .bind(id)
        .bind(USER)
        .bind(tenant.as_str())
        .bind(provider)
        .execute(pool)
        .await
        .expect("insert a binding");
        id
    };
    let binding_former = binding("nuclear-notes").await;
    let binding_other = binding("github").await;

    let schedule = |contexts: Value| async move {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO schedules (id, tenant_id, owner_sub, owner_realm, owner_kind, name, \
             target_kind, target, contexts, run_at, state, temporal_schedule_id, created_at, \
             updated_at) VALUES ($1, $2, $3, 'zaru-consumer', 'consumer_user', 'morning', \
             'agent', 'notes-agent', $4, $5, 'active', $6, $5, $5)",
        )
        .bind(id)
        .bind(tenant.as_str())
        .bind(USER)
        .bind(contexts)
        .bind(now)
        .bind(format!("schedule-{id}"))
        .execute(pool)
        .await
        .expect("insert a schedule");
        id
    };
    let schedule_former = schedule(json!({"nuclear-notes": [binding_former.to_string()]})).await;
    let schedule_other = schedule(json!({"github": [binding_other.to_string()]})).await;

    let profile = |name: &'static str, tools: Value| async move {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO profiles (id, tenant_id, user_sub, name, tools) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(tenant.as_str())
        .bind(USER)
        .bind(name)
        .bind(tools)
        .execute(pool)
        .await
        .expect("insert a profile");
        id
    };
    let profile_former = profile(
        "Notes",
        json!(["nuclear-notes.*", "mail.reply", "nuclear-notes.pages.read"]),
    )
    .await;
    let profile_other = profile("Code", json!(["github.*"])).await;

    let policy = |tool: &'static str| async move {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO tool_approval_policies (id, tenant_id, user_sub, tool_name, created_at, \
             created_by) VALUES ($1, $2, $3, $4, $5, $3)",
        )
        .bind(id)
        .bind(tenant.as_str())
        .bind(USER)
        .bind(tool)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert a policy");
        id
    };
    let policy_former = policy("nuclear-notes.pages.read").await;
    let policy_other = policy("github.get_me").await;
    // A standing choice under the server's own name and one under the former
    // name for the same key: the former one is left.
    policy("carbon-forge.pages.update").await;
    let policy_standing = policy("nuclear-notes.pages.update").await;

    let request = |tool: &'static str, status: &'static str, contexts: Value, sealed: bool| async move {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO tool_approval_requests (id, tenant_id, user_sub, execution_id, \
             agent_id, tool_name, arguments, summary, security_context_name, status, created_at, \
             decided_at, contexts, sealed) VALUES ($1, $2, $3, $4, $5, $6, '{}'::jsonb, 's', \
             'zaru-free', $7, $8, CASE WHEN $7 = 'pending' THEN NULL ELSE $8 END, $9, $10)",
        )
        .bind(id)
        .bind(tenant.as_str())
        .bind(USER)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(tool)
        .bind(status)
        .bind(now)
        .bind(contexts)
        .bind(sealed)
        .execute(pool)
        .await
        .expect("insert a request");
        id
    };
    let former_contexts = json!({"nuclear-notes": [binding_former.to_string()]});
    let request_pending = request(
        "nuclear-notes.pages.read",
        "pending",
        seal(secrets, tenant, &former_contexts).await,
        true,
    )
    .await;
    let request_other = request(
        "github.get_me",
        "pending",
        json!({"github": [binding_other.to_string()]}),
        false,
    )
    .await;
    let request_decided = request(
        "nuclear-notes.pages.read",
        "approved_once",
        seal(secrets, tenant, &former_contexts).await,
        true,
    )
    .await;

    sqlx::query(
        "INSERT INTO tenants (slug, display_name, keycloak_realm, openbao_namespace) \
         VALUES ($1, $1, 'zaru-consumer', $1) ON CONFLICT DO NOTHING",
    )
    .bind(tenant.as_str())
    .execute(pool)
    .await
    .expect("insert the tenant");
    let agent = |name: &'static str, (yaml, json): (String, Value)| async move {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO agents (id, tenant_id, name, manifest_yaml, manifest_json, runtime, \
             security_policy) VALUES ($1, $2, $3, $4, $5, 'python:3.11', '{}'::jsonb)",
        )
        .bind(id)
        .bind(tenant.as_str())
        .bind(name)
        .bind(yaml)
        .bind(json)
        .execute(pool)
        .await
        .expect("insert an agent");
        id
    };
    let agent_former = agent(
        "notes-agent",
        manifest(
            "notes-agent",
            &["nuclear-notes.pages.read", "aegis.tools.list"],
            Some("nuclear-notes"),
        ),
    )
    .await;
    let agent_other = agent(
        "code-agent",
        manifest("code-agent", &["github.get_me"], Some("github")),
    )
    .await;

    Seeded {
        binding_former,
        binding_other,
        schedule_former,
        schedule_other,
        profile_former,
        profile_other,
        policy_former,
        policy_other,
        policy_standing,
        request_pending,
        request_other,
        request_decided,
        agent_former,
        agent_other,
    }
}

async fn text(pool: &PgPool, sql: &str, id: Uuid) -> String {
    sqlx::query(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read a row")
        .get(0)
}

async fn jsonb(pool: &PgPool, sql: &str, id: Uuid) -> Value {
    sqlx::query(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read a row")
        .get(0)
}

fn counts(
    outcomes: &[aegis_orchestrator_core::infrastructure::repositories::former_names_migration::TableOutcome],
) -> Vec<(&'static str, TableRewrite)> {
    outcomes
        .iter()
        .map(|o| {
            (
                o.table,
                *o.result
                    .as_ref()
                    .unwrap_or_else(|e| panic!("the rewrite of {} failed: {e}", o.table)),
            )
        })
        .collect()
}

/// The six rewrites, the unrelated and decided rows untouched, the
/// per-table counts (first run n, second run 0).
#[tokio::test]
async fn stored_former_names_are_rewritten_once_and_nothing_else_is() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let secrets = secrets();
    let tenant = TenantId::for_consumer_user(USER).unwrap();
    let former = renamed();
    let ids = seed(pool, &secrets, &tenant).await;
    let decided_contexts_before = jsonb(
        pool,
        "SELECT contexts FROM tool_approval_requests WHERE id = $1",
        ids.request_decided,
    )
    .await;
    let other_yaml_before = text(
        pool,
        "SELECT manifest_yaml FROM agents WHERE id = $1",
        ids.agent_other,
    )
    .await;

    let mut complaints = Vec::new();
    let rewrite = |rewritten, left| TableRewrite { rewritten, left };
    let first = counts(&run(pool, secrets.clone(), &former).await);
    let expected_first = vec![
        ("credential_bindings", rewrite(1, 0)),
        ("schedules", rewrite(1, 0)),
        ("profiles", rewrite(1, 0)),
        ("tool_approval_policies", rewrite(1, 1)),
        ("tool_approval_requests", rewrite(1, 0)),
        ("agents", rewrite(1, 0)),
    ];
    if first != expected_first {
        complaints.push(format!(
            "the first run answered {first:?}, not {expected_first:?}"
        ));
    }

    // The six rewrites.
    let provider = text(
        pool,
        "SELECT provider FROM credential_bindings WHERE id = $1",
        ids.binding_former,
    )
    .await;
    if provider != "carbon-forge" {
        complaints.push(format!("the binding's provider reads {provider}"));
    }
    let contexts = jsonb(
        pool,
        "SELECT contexts FROM schedules WHERE id = $1",
        ids.schedule_former,
    )
    .await;
    if contexts != json!({"carbon-forge": [ids.binding_former.to_string()]}) {
        complaints.push(format!("the schedule's contexts read {contexts}"));
    }
    let tools = jsonb(
        pool,
        "SELECT tools FROM profiles WHERE id = $1",
        ids.profile_former,
    )
    .await;
    if tools != json!(["carbon-forge.*", "mail.reply", "carbon-forge.pages.read"]) {
        complaints.push(format!("the profile's tools read {tools}"));
    }
    let policy = text(
        pool,
        "SELECT tool_name FROM tool_approval_policies WHERE id = $1",
        ids.policy_former,
    )
    .await;
    if policy != "carbon-forge.pages.read" {
        complaints.push(format!("the policy's tool reads {policy}"));
    }
    let request_tool = text(
        pool,
        "SELECT tool_name FROM tool_approval_requests WHERE id = $1",
        ids.request_pending,
    )
    .await;
    let request_contexts = open(
        &secrets,
        &tenant,
        &jsonb(
            pool,
            "SELECT contexts FROM tool_approval_requests WHERE id = $1",
            ids.request_pending,
        )
        .await,
    )
    .await;
    if request_tool != "carbon-forge.pages.read"
        || request_contexts != json!({"carbon-forge": [ids.binding_former.to_string()]})
    {
        complaints.push(format!(
            "the pending request reads {request_tool} with contexts {request_contexts}"
        ));
    }
    let (expected_yaml, expected_json) = manifest(
        "notes-agent",
        &["carbon-forge.pages.read", "aegis.tools.list"],
        Some("carbon-forge"),
    );
    let agent_yaml = text(
        pool,
        "SELECT manifest_yaml FROM agents WHERE id = $1",
        ids.agent_former,
    )
    .await;
    let agent_json = jsonb(
        pool,
        "SELECT manifest_json FROM agents WHERE id = $1",
        ids.agent_former,
    )
    .await;
    if agent_yaml != expected_yaml || agent_json != expected_json {
        complaints.push(format!(
            "the agent's manifest reads\n{agent_yaml}\n{agent_json}\nnot\n{expected_yaml}\n{expected_json}"
        ));
    }

    // What stays.
    for (sql, id, expected) in [
        (
            "SELECT provider FROM credential_bindings WHERE id = $1",
            ids.binding_other,
            "github",
        ),
        (
            "SELECT tool_name FROM tool_approval_policies WHERE id = $1",
            ids.policy_other,
            "github.get_me",
        ),
        (
            "SELECT tool_name FROM tool_approval_policies WHERE id = $1",
            ids.policy_standing,
            "nuclear-notes.pages.update",
        ),
        (
            "SELECT tool_name FROM tool_approval_requests WHERE id = $1",
            ids.request_other,
            "github.get_me",
        ),
        (
            "SELECT tool_name FROM tool_approval_requests WHERE id = $1",
            ids.request_decided,
            "nuclear-notes.pages.read",
        ),
    ] {
        let got = text(pool, sql, id).await;
        if got != expected {
            complaints.push(format!("{sql} [{id}] reads {got}, not {expected}"));
        }
    }
    for (sql, id, expected) in [
        (
            "SELECT contexts FROM schedules WHERE id = $1",
            ids.schedule_other,
            json!({"github": [ids.binding_other.to_string()]}),
        ),
        (
            "SELECT tools FROM profiles WHERE id = $1",
            ids.profile_other,
            json!(["github.*"]),
        ),
        (
            "SELECT contexts FROM tool_approval_requests WHERE id = $1",
            ids.request_other,
            json!({"github": [ids.binding_other.to_string()]}),
        ),
        (
            "SELECT contexts FROM tool_approval_requests WHERE id = $1",
            ids.request_decided,
            decided_contexts_before.clone(),
        ),
    ] {
        let got = jsonb(pool, sql, id).await;
        if got != expected {
            complaints.push(format!("{sql} [{id}] reads {got}, not {expected}"));
        }
    }
    let other_yaml = text(
        pool,
        "SELECT manifest_yaml FROM agents WHERE id = $1",
        ids.agent_other,
    )
    .await;
    if other_yaml != other_yaml_before {
        complaints.push(format!(
            "the unrelated agent's manifest changed:\n{other_yaml}"
        ));
    }

    // A second run rewrites nothing.
    let second = counts(&run(pool, secrets.clone(), &former).await);
    let expected_second = vec![
        ("credential_bindings", rewrite(0, 0)),
        ("schedules", rewrite(0, 0)),
        ("profiles", rewrite(0, 0)),
        ("tool_approval_policies", rewrite(0, 1)),
        ("tool_approval_requests", rewrite(0, 0)),
        ("agents", rewrite(0, 0)),
    ];
    if second != expected_second {
        complaints.push(format!(
            "the second run answered {second:?}, not {expected_second:?}"
        ));
    }

    db.remove().await;
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
