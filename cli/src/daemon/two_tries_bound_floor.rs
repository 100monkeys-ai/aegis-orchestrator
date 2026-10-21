// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The data step `two_tries_bound_floor` (AEGIS ADR-141, Update of
//! 2026-10-21, clause 13).
//!
//! Creating or updating an agent whose `max_retries` is 2 or more is refused
//! when its bound is under `two_tries_bound` (clause 12). The agents stored
//! before that check are answered by this step: once, at the daemon's start
//! after the schema migrations, it raises `resources.timeout` to
//! `two_tries_bound` on every stored agent row, of any status, that the
//! check would refuse, and changes nothing else in the manifest. The row's
//! `manifest_yaml`, `manifest_json`, `timeout_seconds` and `security_policy`
//! are written from the one changed manifest, so they agree. One line is
//! logged per row: its id, name, version, old bound and new bound.
//!
//! The step records its name in `data_steps` (migration 056) in the
//! transaction that changes the rows. A second start, or a second process
//! starting at once, finds the name and changes nothing.
//!
//! A row whose manifest has no `spec.security` and needs more than the
//! default bound would need a `security` block added, which changes more
//! than its bound; it is left as it is and logged.

use aegis_orchestrator_core::domain::agent::AgentManifest;
use aegis_orchestrator_core::domain::supervisor::two_tries_refusal;
use aegis_orchestrator_core::domain::tenant::SYSTEM_SLUG;
use anyhow::{Context, Result};
use sqlx::{PgPool, Row};
use tracing::{error, info, warn};

/// The step's name in `data_steps`.
pub const STEP: &str = "two_tries_bound_floor";

/// One agent row the step raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raised {
    pub agent_id: uuid::Uuid,
    pub tenant_id: String,
    pub name: String,
    pub version: String,
    pub old_bound: u64,
    pub new_bound: u64,
}

/// Run the step, once: the rows it raised, or `None` when it had already
/// run. Every change and the record that it ran are one transaction.
pub async fn run(pool: &PgPool) -> Result<Option<Vec<Raised>>> {
    let mut tx = pool.begin().await.context("begin the step's transaction")?;
    let claimed =
        sqlx::query("INSERT INTO data_steps (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
            .bind(STEP)
            .execute(&mut *tx)
            .await
            .context("record the step in data_steps")?
            .rows_affected();
    if claimed == 0 {
        tx.rollback().await.context("end the step's transaction")?;
        return Ok(None);
    }
    let rows = sqlx::query(
        "SELECT id, tenant_id, name, version, manifest_json FROM agents ORDER BY created_at, id \
         FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await
    .context("read the stored agents")?;
    let mut raised = Vec::new();
    for row in rows {
        let agent_id: uuid::Uuid = row.try_get("id").context("agents.id")?;
        let tenant_id: String = row.try_get("tenant_id").context("agents.tenant_id")?;
        let name: String = row.try_get("name").context("agents.name")?;
        let version: String = row.try_get("version").context("agents.version")?;
        let stored: serde_json::Value = row
            .try_get("manifest_json")
            .context("agents.manifest_json")?;
        let mut manifest: AgentManifest = match serde_json::from_value(stored) {
            Ok(manifest) => manifest,
            Err(e) => {
                warn!(
                    agent_id = %agent_id,
                    agent_name = %name,
                    version = %version,
                    error = %e,
                    "Left agent {name} {version} ({agent_id}) as it is: its stored manifest does \
                     not read"
                );
                continue;
            }
        };
        let Some(refusal) = two_tries_refusal(&manifest, tenant_id == SYSTEM_SLUG) else {
            continue;
        };
        let Some(security) = manifest.spec.security.as_mut() else {
            warn!(
                agent_id = %agent_id,
                agent_name = %name,
                version = %version,
                bound = refusal.bound,
                need = refusal.need,
                "Left agent {name} {version} ({agent_id}) as it is: it declares no \
                 spec.security, and two whole tries need {} s, over the default {} s",
                refusal.need,
                refusal.bound
            );
            continue;
        };
        security.resources.timeout = Some(format!("{}s", refusal.need));
        let manifest_json = serde_json::to_value(&manifest).context("write the manifest's JSON")?;
        let manifest_yaml =
            serde_yaml::to_string(&manifest).context("write the manifest's YAML")?;
        let security_policy =
            serde_json::to_value(&manifest.spec.security).context("write the security policy")?;
        let timeout_seconds =
            i32::try_from(refusal.need).context("the new bound fits timeout_seconds")?;
        sqlx::query(
            "UPDATE agents SET manifest_yaml = $2, manifest_json = $3, timeout_seconds = $4, \
             security_policy = $5 WHERE id = $1",
        )
        .bind(agent_id)
        .bind(&manifest_yaml)
        .bind(&manifest_json)
        .bind(timeout_seconds)
        .bind(&security_policy)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("raise the bound of agent {agent_id}"))?;
        info!(
            agent_id = %agent_id,
            tenant_id = %tenant_id,
            agent_name = %name,
            version = %version,
            old_bound = refusal.bound,
            new_bound = refusal.need,
            "Raised agent {name} {version} ({agent_id}) from {} s to {} s, the bound two whole \
             tries need",
            refusal.bound,
            refusal.need
        );
        raised.push(Raised {
            agent_id,
            tenant_id,
            name,
            version,
            old_bound: refusal.bound,
            new_bound: refusal.need,
        });
    }
    tx.commit().await.context("commit the step")?;
    Ok(Some(raised))
}

/// Run the step at the daemon's start and log what it did. A step that
/// fails is logged and the daemon starts: nothing was changed, and the next
/// start runs it again.
pub async fn run_at_start(pool: &PgPool) {
    match run(pool).await {
        Ok(Some(raised)) => info!(
            step = STEP,
            raised = raised.len(),
            "The data step {STEP} ran: {} agent bounds raised to two whole tries",
            raised.len()
        ),
        Ok(None) => info!(step = STEP, "The data step {STEP} has run before"),
        Err(e) => error!(
            step = STEP,
            error = %format!("{e:#}"),
            "The data step {STEP} failed and changed nothing; it runs again at the next start"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::agent::{Agent, AgentManifest, AgentScope};
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
                panic!(
                    "AEGIS_TEST_POSTGRES_URL is not set; in CI these tests must reach PostgreSQL"
                )
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
            let name = format!("aegis_two_tries_{}", uuid::Uuid::new_v4().simple());
            sqlx::query(&format!("CREATE DATABASE {name}"))
                .execute(&server)
                .await
                .expect("create the test database");
            let options: PgConnectOptions =
                url.parse::<PgConnectOptions>().unwrap().database(&name);
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

    /// `mail-reply-drafter`'s shape: a semantic judge of 180 s and no
    /// `iteration_timeout`, so two whole tries need 2 x (600 + 180) + 15 +
    /// 30 = 1605 s.
    fn manifest(name: &str, max_retries: u32, timeout: &str) -> AgentManifest {
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
    instruction: Draft a reply to the email in the input.
  execution:
    mode: iterative
    max_retries: {max_retries}
    validation:
      - type: semantic
        judge_agent: email-draft-judge
        criteria: The reply answers every question the email asks.
        min_score: 0.7
        timeout_seconds: 180
  security:
    resources:
      timeout: "{timeout}"
"#
        ))
        .expect("the test manifest parses")
    }

    /// A stored row's `manifest_yaml`, `manifest_json` and `timeout_seconds`.
    async fn stored(pool: &PgPool, agent: &Agent) -> (String, serde_json::Value, i32) {
        let row = sqlx::query(
            "SELECT manifest_yaml, manifest_json, timeout_seconds FROM agents WHERE id = $1",
        )
        .bind(agent.id.0)
        .fetch_one(pool)
        .await
        .expect("the row is stored");
        (
            row.get("manifest_yaml"),
            row.get("manifest_json"),
            row.get("timeout_seconds"),
        )
    }

    /// Trigger (p): a stored row at 900 s reads 1605 s after the step, in
    /// its YAML, its JSON and its `timeout_seconds`; the rows at 2000 s, at
    /// `max_retries: 1` and on the exemption list are unchanged; and a
    /// second start changes nothing.
    #[tokio::test]
    async fn the_floor_raises_a_short_bound_once_and_leaves_the_rest() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        let repository = PostgresAgentRepository::new(db.pool.clone());
        let alice = TenantId::for_consumer_user("alice").expect("a tenant id for alice");
        // An agent row names its tenant (`agents_tenant_fk`); the system
        // tenant is seeded by the migrations, alice's is added here.
        sqlx::query(
            "INSERT INTO tenants (slug, display_name, keycloak_realm, openbao_namespace) \
             VALUES ($1, $1, 'zaru-consumer', $1) ON CONFLICT DO NOTHING",
        )
        .bind(alice.as_str())
        .execute(&db.pool)
        .await
        .expect("insert alice's tenant");
        let save = |name: &str, max_retries: u32, timeout: &str, tenant: &TenantId| {
            let mut agent = Agent::new(manifest(name, max_retries, timeout));
            agent.tenant_id = tenant.clone();
            if tenant.is_system() {
                agent.scope = AgentScope::Global;
            }
            agent
        };
        let short = save("mail-reply-drafter", 3, "900s", &alice);
        let roomy = save("roomy-drafter", 3, "2000s", &alice);
        let one_try = save("one-try-drafter", 1, "900s", &alice);
        let exempt = save("goal-judge", 3, "300s", &TenantId::system());
        for agent in [&short, &roomy, &one_try, &exempt] {
            repository
                .save_for_tenant(&agent.tenant_id, agent)
                .await
                .expect("the row is saved");
        }
        let mut before = Vec::new();
        for agent in [&roomy, &one_try, &exempt] {
            before.push(stored(&db.pool, agent).await);
        }

        let mut complaints = Vec::new();
        let first = run(&db.pool).await.expect("the step runs");
        let raised: Vec<(String, u64, u64)> = first
            .iter()
            .flatten()
            .map(|row| (row.name.clone(), row.old_bound, row.new_bound))
            .collect();
        if raised != vec![("mail-reply-drafter".to_string(), 900, 1605)] {
            complaints.push(format!("the first start raised {raised:?}"));
        }
        let (yaml, json, timeout_seconds) = stored(&db.pool, &short).await;
        let from_json: AgentManifest =
            serde_json::from_value(json.clone()).expect("the raised JSON is a manifest");
        let from_yaml: AgentManifest =
            serde_yaml::from_str(&yaml).expect("the raised YAML is a manifest");
        let bound = |m: &AgentManifest| {
            m.spec
                .security
                .as_ref()
                .and_then(|s| s.resources.timeout.clone())
        };
        if bound(&from_json).as_deref() != Some("1605s") {
            complaints.push(format!("manifest_json reads {:?}", bound(&from_json)));
        }
        if from_yaml != from_json {
            complaints.push("manifest_yaml and manifest_json disagree".to_string());
        }
        if timeout_seconds != 1605 {
            complaints.push(format!("timeout_seconds reads {timeout_seconds}"));
        }
        let mut expected_from_json = short.manifest.clone();
        if let Some(security) = expected_from_json.spec.security.as_mut() {
            security.resources.timeout = Some("1605s".to_string());
        }
        if from_json != expected_from_json {
            complaints.push("the step changed more of the manifest than its bound".to_string());
        }
        let raised_row = (yaml, json, timeout_seconds);
        for (agent, was) in [&roomy, &one_try, &exempt].into_iter().zip(&before) {
            if &stored(&db.pool, agent).await != was {
                complaints.push(format!("{} changed", agent.name));
            }
        }

        let second = run(&db.pool).await.expect("the step runs again");
        if second.is_some() {
            complaints.push(format!("the second start ran the step again: {second:?}"));
        }
        if stored(&db.pool, &short).await != raised_row {
            complaints.push("the second start changed the raised row".to_string());
        }
        for (agent, was) in [&roomy, &one_try, &exempt].into_iter().zip(&before) {
            if &stored(&db.pool, agent).await != was {
                complaints.push(format!("{} changed on the second start", agent.name));
            }
        }
        db.remove().await;
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }
}
