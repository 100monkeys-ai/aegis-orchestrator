// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Migration 042 (`credential_bindings.reach`, AEGIS ADR-132 (7a) S2)
//! against a real PostgreSQL: an existing row is unchanged and reads back
//! with no reach, and a binding's reach is stored and read back.
//!
//! Like the other Postgres contract tests in this directory, it runs when
//! `AEGIS_DATABASE_URL` or `DATABASE_URL` names a reachable database and
//! returns early otherwise, in a schema of its own, applying migrations
//! 011, 035 and 042 from `cli/migrations/` exactly as written.

use aegis_orchestrator_core::domain::credential::{
    BindingReach, CredentialBindingId, CredentialBindingRepository, CredentialMetadata,
    CredentialProvider, CredentialScope, CredentialStatus, CredentialType, ReachKind,
    UserCredentialBinding,
};
use aegis_orchestrator_core::domain::secrets::SecretPath;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::PostgresCredentialBindingRepository;
use chrono::{SubsecRound, Utc};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Executor, Row};

const MIGRATION_011: &str = include_str!("../../../cli/migrations/011_credential_bindings.sql");
const MIGRATION_035: &str =
    include_str!("../../../cli/migrations/035_credential_mailbox_settings.sql");
const MIGRATION_042: &str =
    include_str!("../../../cli/migrations/042_credential_binding_reach.sql");

async fn pool_in_fresh_schema() -> Option<(PgPool, String)> {
    let url = std::env::var("AEGIS_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()?;
    let schema = format!("reach_{}", uuid::Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .ok()?;
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
        .connect(&url)
        .await
        .expect("connect in schema");
    Some((pool, schema))
}

#[tokio::test]
async fn migration_042_keeps_existing_rows_and_stores_a_bindings_reach() {
    let Some((pool, schema)) = pool_in_fresh_schema().await else {
        eprintln!("skipped: no AEGIS_DATABASE_URL or DATABASE_URL");
        return;
    };
    pool.execute(MIGRATION_011).await.expect("migration 011");
    pool.execute(MIGRATION_035).await.expect("migration 035");

    let existing_id = uuid::Uuid::new_v4();
    let tenant = TenantId::for_consumer_user("owner-sub").unwrap();
    sqlx::query(
        "INSERT INTO credential_bindings (id, owner_user_id, tenant_id, credential_type, provider, \
         label, secret_path, scope, status) VALUES ($1, 'owner-sub', $2, 'secret', \
         'nuclear-notes', 'Old token', 'users/x/owner-sub/credentials/k', 'personal', 'active')",
    )
    .bind(existing_id)
    .bind(tenant.as_str())
    .execute(&pool)
    .await
    .expect("insert existing row");
    let row_json = |pool: PgPool| async move {
        let text = sqlx::query(
            "SELECT row_to_json(c)::text AS j FROM credential_bindings c WHERE id = $1",
        )
        .bind(existing_id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get::<String, _>("j");
        serde_json::from_str::<serde_json::Value>(&text).unwrap()
    };
    let mut before = row_json(pool.clone()).await;

    pool.execute(MIGRATION_042).await.expect("migration 042");
    pool.execute(MIGRATION_042)
        .await
        .expect("migration 042 again changes nothing");
    before["reach"] = serde_json::Value::Null;
    assert_eq!(
        row_json(pool.clone()).await,
        before,
        "migration 042 changed an existing row"
    );

    let repo = PostgresCredentialBindingRepository::new(pool.clone());
    let existing = repo
        .find_by_id(&CredentialBindingId(existing_id))
        .await
        .unwrap()
        .expect("existing row reads back");
    assert_eq!(existing.metadata.reach, None);

    let id = CredentialBindingId::new();
    let now = Utc::now();
    let reach = BindingReach {
        kind: ReachKind::Instance,
        instance_slug: Some("play2".to_string()),
        instance_id: Some("inst-play2-id".to_string()),
        workspace_id: None,
        grounded_at: now.trunc_subsecs(6),
    };
    let binding = UserCredentialBinding {
        id,
        owner_user_id: "owner-sub".to_string(),
        tenant_id: tenant.clone(),
        credential_type: CredentialType::Secret,
        provider: CredentialProvider::new("nuclear-notes"),
        secret_path: SecretPath::for_tenant(
            tenant.clone(),
            "kv",
            format!("users/{}/owner-sub/credentials/{}", tenant.as_str(), id.0),
        ),
        scope: CredentialScope::Personal,
        status: CredentialStatus::Active,
        metadata: CredentialMetadata {
            label: "Work instance".to_string(),
            tags: None,
            service_url: None,
            external_account_id: None,
            oauth_scopes: None,
            mailbox: None,
            reach: Some(reach.clone()),
        },
        grants: Vec::new(),
        created_at: now,
        updated_at: now,
    };
    repo.save(&binding).await.expect("save");
    let read = repo.find_by_id(&id).await.unwrap().expect("reads back");
    assert_eq!(read.metadata.reach, Some(reach.clone()));

    let mut apex = binding.clone();
    apex.metadata.reach = Some(BindingReach {
        kind: ReachKind::Apex,
        instance_slug: None,
        instance_id: None,
        workspace_id: None,
        grounded_at: now.trunc_subsecs(6),
    });
    repo.save(&apex).await.expect("rewrite");
    let read = repo.find_by_id(&id).await.unwrap().expect("reads back");
    assert_eq!(read.metadata.reach, apex.metadata.reach);
    let stored: serde_json::Value =
        sqlx::query("SELECT reach FROM credential_bindings WHERE id = $1")
            .bind(id.0)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("reach");
    assert_eq!(stored["kind"], "apex");

    pool.execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .expect("drop schema");
}
