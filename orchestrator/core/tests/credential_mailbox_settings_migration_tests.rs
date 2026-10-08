// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Migration 035 (`credential_bindings.mailbox_settings`, AEGIS ADR-125 D1)
//! against a real PostgreSQL, and the repository's mappings of the
//! `mailbox` type and the `google_mail` and `imap` providers.
//!
//! Like the other Postgres contract tests in this directory, it runs when
//! `AEGIS_DATABASE_URL` or `DATABASE_URL` names a reachable database and
//! returns early otherwise. It works in a schema of its own, created and
//! dropped by the test, applying migration 011 and then 035 from
//! `cli/migrations/` exactly as written.

use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialMetadata, CredentialProvider,
    CredentialScope, CredentialStatus, CredentialType, MailSecurity, MailboxSettings,
    UserCredentialBinding,
};
use aegis_orchestrator_core::domain::secrets::SecretPath;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::PostgresCredentialBindingRepository;
use chrono::Utc;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Executor, Row};

const MIGRATION_011: &str = include_str!("../../../cli/migrations/011_credential_bindings.sql");
const MIGRATION_035: &str =
    include_str!("../../../cli/migrations/035_credential_mailbox_settings.sql");
/// The repository reads every column the current schema has; 042
/// (`reach`, AEGIS ADR-132 (7a) S2) is applied before it reads.
const MIGRATION_042: &str =
    include_str!("../../../cli/migrations/042_credential_binding_reach.sql");
const MIGRATION_047: &str =
    include_str!("../../../cli/migrations/047_credential_calendar_settings.sql");

async fn pool_in_fresh_schema() -> Option<(PgPool, String)> {
    let url = std::env::var("AEGIS_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()?;
    let schema = format!("mbx_{}", uuid::Uuid::new_v4().simple());
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

async fn drop_schema(pool: &PgPool, schema: &str) {
    pool.execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .expect("drop schema");
}

fn imap_settings() -> MailboxSettings {
    MailboxSettings {
        address: "outreach@example.test".to_string(),
        display_name: Some("Outreach".to_string()),
        imap_host: "imap.example.test".to_string(),
        imap_port: 993,
        imap_security: MailSecurity::Tls,
        smtp_host: "smtp.example.test".to_string(),
        smtp_port: 587,
        smtp_security: MailSecurity::Starttls,
        username: "outreach@example.test".to_string(),
    }
}

fn binding(
    tenant: &TenantId,
    provider: CredentialProvider,
    metadata: CredentialMetadata,
) -> UserCredentialBinding {
    let id = CredentialBindingId::new();
    let now = Utc::now();
    UserCredentialBinding {
        id,
        owner_user_id: "owner-sub".to_string(),
        tenant_id: tenant.clone(),
        credential_type: CredentialType::Mailbox,
        provider,
        secret_path: SecretPath::for_tenant(
            tenant.clone(),
            "kv",
            format!("users/{}/owner-sub/credentials/{}", tenant.as_str(), id.0),
        ),
        scope: CredentialScope::Personal,
        status: CredentialStatus::Active,
        metadata,
        grants: Vec::new(),
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn migration_035_keeps_existing_rows_and_stores_mailbox_settings() {
    let Some((pool, schema)) = pool_in_fresh_schema().await else {
        eprintln!("skipped: no AEGIS_DATABASE_URL or DATABASE_URL");
        return;
    };
    pool.execute(MIGRATION_011).await.expect("migration 011");

    // A row written before migration 035, as production holds them.
    let existing_id = uuid::Uuid::new_v4();
    let tenant = TenantId::for_consumer_user("owner-sub").unwrap();
    sqlx::query(
        "INSERT INTO credential_bindings (id, owner_user_id, tenant_id, credential_type, provider, \
         label, secret_path, scope, status, oauth_scopes, external_account_id, service_url, tags) \
         VALUES ($1, 'owner-sub', $2, 'secret', 'openai', 'My key', 'users/x/owner-sub/credentials/k', \
         'personal', 'active', ARRAY['a','b'], 'acct-1', 'https://svc.example', '{\"k\":\"v\"}'::jsonb)",
    )
    .bind(existing_id)
    .bind(tenant.as_str())
    .execute(&pool)
    .await
    .expect("insert existing row");
    let before =
        sqlx::query("SELECT row_to_json(c)::text AS j FROM credential_bindings c WHERE id = $1")
            .bind(existing_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get::<String, _>("j");

    pool.execute(MIGRATION_035).await.expect("migration 035");

    // Every column the row had is unchanged; the new column is NULL.
    let after: serde_json::Value = serde_json::from_str(
        &sqlx::query("SELECT row_to_json(c)::text AS j FROM credential_bindings c WHERE id = $1")
            .bind(existing_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get::<String, _>("j"),
    )
    .unwrap();
    let mut before: serde_json::Value = serde_json::from_str(&before).unwrap();
    before["mailbox_settings"] = serde_json::Value::Null;
    assert_eq!(after, before, "migration 035 changed an existing row");
    pool.execute(MIGRATION_042).await.expect("migration 042");
    pool.execute(MIGRATION_047).await.expect("migration 047");

    let repo = PostgresCredentialBindingRepository::new(pool.clone());
    let read = repo
        .find_by_id(&CredentialBindingId(existing_id))
        .await
        .unwrap()
        .expect("existing row reads back");
    assert_eq!(read.credential_type, CredentialType::Secret);
    assert_eq!(read.provider, CredentialProvider::new("openai"));
    assert_eq!(read.metadata.label, "My key");
    assert_eq!(read.metadata.external_account_id.as_deref(), Some("acct-1"));
    assert!(read.metadata.mailbox.is_none());

    // An imap mailbox stores and reads back its settings and its names.
    let imap = binding(
        &tenant,
        CredentialProvider::imap(),
        CredentialMetadata {
            label: "outreach@example.test".to_string(),
            tags: None,
            service_url: None,
            external_account_id: Some("outreach@example.test".to_string()),
            oauth_scopes: None,
            mailbox: Some(imap_settings()),
            reach: None,
            calendar: None,
        },
    );
    repo.save(&imap).await.expect("save imap mailbox");
    let raw: (String, String) =
        sqlx::query_as("SELECT credential_type, provider FROM credential_bindings WHERE id = $1")
            .bind(imap.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(raw, ("mailbox".to_string(), "imap".to_string()));
    let read = repo.find_by_id(&imap.id).await.unwrap().unwrap();
    assert_eq!(read.credential_type, CredentialType::Mailbox);
    assert_eq!(read.provider, CredentialProvider::imap());
    assert_eq!(read.metadata.mailbox, Some(imap_settings()));

    // A google_mail row reads back as the string it is.
    let google = binding(
        &tenant,
        CredentialProvider::new("google_mail"),
        CredentialMetadata {
            label: "jeshua@100monkeys.example".to_string(),
            tags: None,
            service_url: None,
            external_account_id: Some("jeshua@100monkeys.example".to_string()),
            oauth_scopes: Some(vec!["openid".to_string()]),
            mailbox: None,
            reach: None,
            calendar: None,
        },
    );
    repo.save(&google).await.unwrap();
    let raw: String = sqlx::query_scalar("SELECT provider FROM credential_bindings WHERE id = $1")
        .bind(google.id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(raw, "google_mail");
    let read = repo.find_by_id(&google.id).await.unwrap().unwrap();
    assert_eq!(read.provider.as_str(), "google_mail");
    assert!(read.metadata.mailbox.is_none());

    drop_schema(&pool, &schema).await;
}
