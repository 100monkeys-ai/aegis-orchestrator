// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The data step `one_shot_tries_kept` against PostgreSQL (AEGIS ADR-005,
//! Update of 2026-10-21, Trigger (c)).

use super::*;
use aegis_orchestrator_core::domain::agent::{
    Agent, AgentManifest, AgentScope, AgentStatus, ExecutionMode,
};
use aegis_orchestrator_core::domain::repository::AgentRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::agent_manifest_parser::AgentManifestParser;
use aegis_orchestrator_core::infrastructure::repositories::postgres_agent::PostgresAgentRepository;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;

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

/// A database of its own on the test PostgreSQL, migrated as the daemon
/// migrates it, dropped at the end.
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
        let name = format!("aegis_one_shot_{}", uuid::Uuid::new_v4().simple());
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
        crate::daemon::migrations::MIGRATOR
            .run(&pool)
            .await
            .expect("apply the migrations");
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

/// A manifest with the `mode` and `max_retries` given.
fn manifest(name: &str, mode: &str, max_retries: u32) -> AgentManifest {
    AgentManifestParser::parse_yaml(&format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: {name}
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: Answer the question in the input.
  execution:
    mode: {mode}
    max_retries: {max_retries}
"#
    ))
    .expect("the test manifest parses")
}

/// A stored row's `manifest_yaml` and `manifest_json`.
async fn stored(pool: &PgPool, agent: &Agent) -> (String, serde_json::Value) {
    let row = sqlx::query("SELECT manifest_yaml, manifest_json FROM agents WHERE id = $1")
        .bind(agent.id.0)
        .fetch_one(pool)
        .await
        .expect("the row is stored");
    (row.get("manifest_yaml"), row.get("manifest_json"))
}

/// Rows at one-shot/3 (a tenant's, active) and one-shot/5 (the system
/// tenant's, archived) read `iterative` after the step, in their YAML and
/// their JSON, with their counts kept and nothing else changed; the rows at
/// one-shot/1 and iterative/3 are unchanged; and a second start changes
/// nothing.
#[tokio::test]
async fn one_shot_tries_kept_keeps_every_stored_count() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repository = PostgresAgentRepository::new(db.pool.clone());
    let alice = TenantId::for_consumer_user("alice").expect("a tenant id for alice");
    // An agent row names its tenant (`agents_tenant_fk`); the system tenant
    // is seeded by the migrations, alice's is added here.
    sqlx::query(
        "INSERT INTO tenants (slug, display_name, keycloak_realm, openbao_namespace) \
         VALUES ($1, $1, 'zaru-consumer', $1) ON CONFLICT DO NOTHING",
    )
    .bind(alice.as_str())
    .execute(&db.pool)
    .await
    .expect("insert alice's tenant");
    let save = |name: &str, mode: &str, max_retries: u32, tenant: &TenantId| {
        let mut agent = Agent::new(manifest(name, mode, max_retries));
        agent.tenant_id = tenant.clone();
        if tenant.is_system() {
            agent.scope = AgentScope::Global;
        }
        agent
    };
    let three = save("three-tries", "one-shot", 3, &alice);
    let mut five = save("five-tries", "one-shot", 5, &TenantId::system());
    five.status = AgentStatus::Archived;
    let one = save("one-try", "one-shot", 1, &alice);
    let iterative = save("iterative-three", "iterative", 3, &alice);
    for agent in [&three, &five, &one, &iterative] {
        repository
            .save_for_tenant(&agent.tenant_id, agent)
            .await
            .expect("the row is saved");
    }
    let mut before = Vec::new();
    for agent in [&one, &iterative] {
        before.push(stored(&db.pool, agent).await);
    }

    let mut complaints = Vec::new();
    let first = run(&db.pool).await.expect("the step runs");
    let mut kept: Vec<(String, u32)> = first
        .iter()
        .flatten()
        .map(|row| (row.name.clone(), row.max_retries))
        .collect();
    kept.sort();
    if kept != vec![("five-tries".to_string(), 5), ("three-tries".to_string(), 3)] {
        complaints.push(format!("the first start kept {kept:?}"));
    }
    let mut changed = Vec::new();
    for (agent, max_retries) in [(&three, 3), (&five, 5)] {
        let (yaml, json) = stored(&db.pool, agent).await;
        let from_json: AgentManifest =
            serde_json::from_value(json.clone()).expect("the changed JSON is a manifest");
        let from_yaml: AgentManifest =
            serde_yaml::from_str(&yaml).expect("the changed YAML is a manifest");
        let execution = from_json.spec.execution.clone().unwrap_or_default();
        let read = (execution.mode, execution.max_retries);
        if read != (ExecutionMode::Iterative, max_retries) {
            complaints.push(format!("{} reads {read:?}", agent.name));
        }
        if from_yaml != from_json {
            complaints.push(format!("{}: manifest_yaml and manifest_json disagree", agent.name));
        }
        let mut expected = agent.manifest.clone();
        if let Some(execution) = expected.spec.execution.as_mut() {
            execution.mode = ExecutionMode::Iterative;
        }
        if from_json != expected {
            complaints.push(format!("{}: the step changed more than its mode", agent.name));
        }
        changed.push((yaml, json));
    }
    for (agent, was) in [&one, &iterative].into_iter().zip(&before) {
        if &stored(&db.pool, agent).await != was {
            complaints.push(format!("{} changed", agent.name));
        }
    }

    let second = run(&db.pool).await.expect("the step runs again");
    if second.is_some() {
        complaints.push(format!("the second start ran the step again: {second:?}"));
    }
    for (agent, was) in [&three, &five].into_iter().zip(&changed) {
        if &stored(&db.pool, agent).await != was {
            complaints.push(format!("{} changed on the second start", agent.name));
        }
    }
    for (agent, was) in [&one, &iterative].into_iter().zip(&before) {
        if &stored(&db.pool, agent).await != was {
            complaints.push(format!("{} changed on the second start", agent.name));
        }
    }
    db.remove().await;
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
