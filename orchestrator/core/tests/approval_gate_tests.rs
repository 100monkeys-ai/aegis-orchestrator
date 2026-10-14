// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The approval gate with the real `mail.send` contract (AEGIS ADR-126, its
//! Update of 2026-10-04 clause 1; ADR-125's Update of 2026-10-07 (3) clause
//! 12): the catalogue's contract keys a pending request and an "always
//! allow" policy on the `mailbox` argument and lists exactly `mailbox`,
//! `to`, `cc`, `subject` and `body` in the summary. The gate in the dispatch
//! path is tested in the crate (`tool_invocation_service/approval_gate_tests.rs`).
//!
//! Migration 053 (AEGIS ADR-126, Update of 2026-10-14, clause 1) against
//! PostgreSQL when `AEGIS_TEST_POSTGRES_URL` names one: it adds a nullable
//! JSONB `contexts`, the store seals it with `arguments` and opens it for
//! the request's user, and applied again it changes nothing.

use aegis_orchestrator_core::application::tool_approval_service::{
    GateOutcome, GatedCall, ToolApprovalService,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::tool_approval::{
    ToolApprovalPolicy, ToolApprovalPolicyEffect, ToolApprovalPolicyId, ToolApprovalRepository,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use serde_json::{json, Value};
use std::sync::Arc;

const USER: &str = "gate-user";

fn send_args(mailbox: &str) -> Value {
    json!({
        "mailbox": mailbox,
        "to": ["ann@example.test", "bob@example.test"],
        "cc": ["accounts@example.test"],
        "subject": "Invoice paid",
        "body": "Paid today.",
        "unlisted": "never shown"
    })
}

#[tokio::test]
async fn the_mail_send_contract_keys_the_request_and_the_policy_on_mailbox_and_lists_five_fields() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let repo = Arc::new(InMemoryToolApprovalRepository::new());
    let gate = ToolApprovalService::new(repo.clone(), Arc::new(EventBus::new(64)));
    let tenant = TenantId::default();
    let call = |args: &Value| {
        let args = args.clone();
        let gate = &gate;
        let tenant = &tenant;
        let router = &router;
        async move {
            gate.gate(GatedCall {
                tenant_id: tenant,
                user_sub: Some(USER),
                execution_id: ExecutionId::new(),
                agent_id: AgentId::new(),
                tool_name: "mail.send",
                arguments: &args,
                security_context_name: "zaru-pro",
                conversation_id: None,
                profile_id: None,
                contexts: None,
                contract: router.approval_contract("mail.send"),
            })
            .await
            .unwrap()
        }
    };

    let mut wrong = Vec::new();
    match call(&send_args("b-1")).await {
        GateOutcome::Pending { result } => {
            let expected = "mail.send\nmailbox: b-1\nto: ann@example.test, bob@example.test\ncc: accounts@example.test\nsubject: Invoice paid\nbody: Paid today.\nattachment_names: ";
            if result["summary"] != expected {
                wrong.push(format!("the summary is {:?}", result["summary"]));
            }
        }
        other => wrong.push(format!("a first call was not pending: {other:?}")),
    }
    let rows = repo
        .list_requests_for_user(&tenant, USER, None)
        .await
        .unwrap();
    if rows.first().and_then(|r| r.binding_id.as_deref()) != Some("b-1") {
        wrong.push(format!(
            "the request is keyed on {:?}, not the mailbox",
            rows.first().map(|r| r.binding_id.clone())
        ));
    }

    repo.insert_policy(&ToolApprovalPolicy {
        id: ToolApprovalPolicyId::new(),
        tenant_id: tenant.clone(),
        user_sub: USER.to_string(),
        tool_name: "mail.send".to_string(),
        binding_id: Some("b-1".to_string()),
        profile_id: None,
        effect: ToolApprovalPolicyEffect::Allow,
        created_at: chrono::Utc::now(),
        created_by: USER.to_string(),
        revoked_at: None,
    })
    .await
    .unwrap();
    if !matches!(call(&send_args("b-1")).await, GateOutcome::Proceed { .. }) {
        wrong.push("the policy on b-1 did not allow a send on b-1".to_string());
    }
    if !matches!(call(&send_args("b-2")).await, GateOutcome::Pending { .. }) {
        wrong.push("the policy on b-1 allowed a send on another mailbox".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

mod postgres {
    use super::*;
    use aegis_orchestrator_core::domain::secrets::{
        DomainDynamicSecret, SecretStore, SecretsError, SensitiveString,
    };
    use aegis_orchestrator_core::domain::tool_approval::{
        ToolApprovalId, ToolApprovalRequest, ToolApprovalStatus,
    };
    use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::PostgresToolApprovalRepository;
    use aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use sqlx::{Executor, Row};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    const MIGRATIONS: [(&str, &str); 7] = [
        (
            "036",
            include_str!("../../../cli/migrations/036_tool_approvals.sql"),
        ),
        (
            "044",
            include_str!("../../../cli/migrations/044_tool_approval_conversation.sql"),
        ),
        (
            "045",
            include_str!("../../../cli/migrations/045_tool_approval_sealed.sql"),
        ),
        (
            "046",
            include_str!("../../../cli/migrations/046_tool_approval_policy_effect.sql"),
        ),
        (
            "048",
            include_str!("../../../cli/migrations/048_schedules.sql"),
        ),
        (
            "050",
            include_str!("../../../cli/migrations/050_profile_scoping.sql"),
        ),
        (
            "053",
            include_str!("../../../cli/migrations/053_tool_approval_contexts.sql"),
        ),
    ];
    const MIGRATION_053: &str = MIGRATIONS[6].1;

    /// The two run tables, in the columns migration 048 touches.
    const RUN_TABLES: &str = "
        CREATE TABLE executions (id UUID PRIMARY KEY, status VARCHAR(50) NOT NULL);
        CREATE TABLE workflow_executions (id UUID PRIMARY KEY, status VARCHAR(50) NOT NULL);
    ";

    /// Transit as these tests need it: `encrypt` answers an opaque
    /// `vault:v1:` ciphertext and `decrypt` opens it.
    #[derive(Default)]
    struct Transit(Mutex<HashMap<String, Vec<u8>>>);

    fn not_kept() -> SecretsError {
        SecretsError::ConfigError("the Transit stand-in keeps nothing else".into())
    }

    #[async_trait::async_trait]
    impl SecretStore for Transit {
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
        async fn transit_encrypt(&self, _: &str, plaintext: &[u8]) -> Result<String, SecretsError> {
            let ciphertext = format!("vault:v1:{}", uuid::Uuid::new_v4().simple());
            self.0
                .lock()
                .unwrap()
                .insert(ciphertext.clone(), plaintext.to_vec());
            Ok(ciphertext)
        }
        async fn transit_decrypt(
            &self,
            _: &str,
            ciphertext: &str,
        ) -> Result<Vec<u8>, SecretsError> {
            self.0
                .lock()
                .unwrap()
                .get(ciphertext)
                .cloned()
                .ok_or_else(|| SecretsError::TransitError("not sealed here".into()))
        }
    }

    async fn pool_in_fresh_schema(url: &str) -> (PgPool, String) {
        let schema = format!("approvalctx_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await
            .expect("connect");
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        let search_path = format!("SET search_path TO {schema}, public");
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |conn, _| {
                let sql = search_path.clone();
                Box::pin(async move {
                    conn.execute(sql.as_str()).await?;
                    Ok(())
                })
            })
            .connect(url)
            .await
            .expect("connect in schema");
        (pool, schema)
    }

    /// Every column of `tool_approval_requests`: name, type, nullable.
    async fn request_columns(pool: &PgPool, schema: &str) -> Vec<(String, String, String)> {
        sqlx::query(
            "SELECT column_name, data_type, is_nullable FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = 'tool_approval_requests' \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.get("column_name"),
                row.get("data_type"),
                row.get("is_nullable"),
            )
        })
        .collect()
    }

    /// The stored `contexts` of every row, as text, by id.
    async fn stored_contexts(pool: &PgPool) -> Vec<(uuid::Uuid, Option<String>)> {
        sqlx::query(
            "SELECT id, contexts #>> '{}' AS contexts FROM tool_approval_requests ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get("id"), row.get("contexts")))
        .collect()
    }

    fn request(contexts: Option<Value>) -> ToolApprovalRequest {
        ToolApprovalRequest {
            id: ToolApprovalId::new(),
            tenant_id: TenantId::for_consumer_user(USER).unwrap(),
            user_sub: USER.to_string(),
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            tool_name: "github.push_files".to_string(),
            arguments: json!({"branch": "main"}),
            summary: "github.push_files".to_string(),
            binding_id: None,
            security_context_name: "zaru-pro".to_string(),
            conversation_id: None,
            schedule_id: None,
            schedule_name: None,
            profile_id: None,
            contexts,
            policy_id: None,
            status: ToolApprovalStatus::Pending,
            created_at: chrono::Utc::now(),
            decided_at: None,
            decided_by: None,
            result: None,
            error: None,
        }
    }

    /// Migration 053 adds a nullable JSONB `contexts`; the store seals a
    /// request's contexts (the column holds a `vault:v1:` ciphertext, not
    /// the binding id) and opens them for its user, a request with none
    /// reading none; applied again, 053 changes neither the schema nor a row.
    #[tokio::test]
    async fn migration_053_adds_sealed_contexts_and_applied_twice_changes_nothing() {
        let Ok(url) = std::env::var("AEGIS_TEST_POSTGRES_URL") else {
            eprintln!("skipped: no AEGIS_TEST_POSTGRES_URL");
            return;
        };
        let (pool, schema) = pool_in_fresh_schema(&url).await;
        pool.execute(RUN_TABLES).await.expect("run tables");
        for (number, sql) in MIGRATIONS {
            pool.execute(sql)
                .await
                .unwrap_or_else(|e| panic!("migration {number}: {e}"));
        }
        let once = request_columns(&pool, &schema).await;
        assert!(
            once.contains(&("contexts".to_string(), "jsonb".to_string(), "YES".to_string())),
            "tool_approval_requests has no nullable JSONB contexts column after migration 053: {once:?}"
        );

        let store = PostgresToolApprovalRepository::new(
            pool.clone(),
            Arc::new(SecretsManager::from_store(
                Arc::new(Transit::default()),
                Arc::new(EventBus::new(64)),
            )),
        );
        let binding = "0b6f3c3e-5d4a-4e6b-9c7d-8e9f0a1b2c3d";
        let chosen = request(Some(json!({ "github": [binding] })));
        let none = request(None);
        for row in [&chosen, &none] {
            store.insert_request(row).await.expect("insert");
        }
        let rows_once = stored_contexts(&pool).await;
        let mut wrong = Vec::new();
        for (id, text) in &rows_once {
            let expected_sealed = *id == chosen.id.0;
            match text {
                Some(text)
                    if expected_sealed
                        && text.starts_with("vault:v1:")
                        && !text.contains(binding) => {}
                None if !expected_sealed => {}
                other => wrong.push(format!("row {id} stored contexts {other:?}")),
            }
        }
        for row in [&chosen, &none] {
            let read = store
                .find_request(row.id)
                .await
                .expect("read")
                .expect("found");
            if read.contexts != row.contexts {
                wrong.push(format!(
                    "row {} read contexts {:?}",
                    row.id.0, read.contexts
                ));
            }
        }

        pool.execute(MIGRATION_053)
            .await
            .expect("migration 053 applied a second time");
        if request_columns(&pool, &schema).await != once {
            wrong.push("migration 053 applied again changed the schema".to_string());
        }
        if stored_contexts(&pool).await != rows_once {
            wrong.push("migration 053 applied again changed a stored row".to_string());
        }
        pool.execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
            .await
            .expect("drop schema");
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
