// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The data step `two_tries_bound_floor` (AEGIS ADR-141, Update of
//! 2026-10-21, clause 13).

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
