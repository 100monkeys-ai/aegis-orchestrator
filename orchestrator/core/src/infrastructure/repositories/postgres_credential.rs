// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Credential Binding Repository — ADR-025, ADR-078
//!
//! Production [`CredentialBindingRepository`] implementation backed by the
//! `credential_bindings`, `credential_grants`, and `oauth_pending_states` tables
//! introduced in migration `011_credential_bindings.sql`.
//!
//! ## Schema Summary
//!
//! ```sql
//! credential_bindings (id, owner_user_id, tenant_id, credential_type, provider,
//!                      label, secret_path, scope, scope_team_id, status,
//!                      oauth_scopes, external_account_id, service_url, tags,
//!                      created_at, updated_at, mailbox_settings, reach,
//!                      calendar_settings)
//! credential_grants    (id, binding_id, target_type, target_value, granted_at, granted_by)
//! oauth_pending_states (state, binding_id, pkce_verifier, redirect_uri, created_at)
//! ```

use crate::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialGrantId,
    CredentialMetadata, CredentialProvider, CredentialScope, CredentialStatus, CredentialType,
    GrantTarget, OAuthPendingState, UserCredentialBinding,
};
use crate::domain::secrets::SecretPath;
use crate::domain::shared_kernel::{AgentId, TenantId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

pub struct PostgresCredentialBindingRepository {
    pool: PgPool,
}

impl PostgresCredentialBindingRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

// ============================================================================
// Mapping helpers
// ============================================================================

fn credential_type_to_str(ct: &CredentialType) -> &'static str {
    match ct {
        CredentialType::Secret => "secret",
        CredentialType::OAuth2 => "oauth2",
        CredentialType::Variable => "variable",
        CredentialType::ServiceAccount => "service_account",
        CredentialType::Mailbox => "mailbox",
        CredentialType::Calendar => "calendar",
    }
}

fn str_to_credential_type(s: &str) -> anyhow::Result<CredentialType> {
    match s {
        "secret" => Ok(CredentialType::Secret),
        "oauth2" => Ok(CredentialType::OAuth2),
        "variable" => Ok(CredentialType::Variable),
        "service_account" => Ok(CredentialType::ServiceAccount),
        "mailbox" => Ok(CredentialType::Mailbox),
        "calendar" => Ok(CredentialType::Calendar),
        other => Err(anyhow::anyhow!("Unknown credential_type: {other}")),
    }
}

/// The prefix the code before AEGIS ADR-125's Update of 2026-10-04 wrote
/// before a provider it did not enumerate (`custom:stripe`). Such rows exist
/// and are read, never written again.
const LEGACY_CUSTOM_PREFIX: &str = "custom:";

/// A provider is stored as its name, as given (ADR-125, Update of
/// 2026-10-04, clause 1).
fn provider_to_str(p: &CredentialProvider) -> String {
    p.as_str().to_string()
}

/// A stored provider reads back as the string it is, a row of a provider no
/// registry serves any more included; a row written as `custom:<name>` reads as `<name>`, the name
/// its user gave.
fn str_to_provider(s: &str) -> CredentialProvider {
    CredentialProvider::new(s.strip_prefix(LEGACY_CUSTOM_PREFIX).unwrap_or(s))
}

fn status_to_str(s: &CredentialStatus) -> &'static str {
    match s {
        CredentialStatus::Active => "active",
        CredentialStatus::Revoked => "revoked",
        CredentialStatus::Expired => "expired",
        CredentialStatus::PendingOAuth => "pending_oauth",
        CredentialStatus::PendingMigration => "pending_migration",
    }
}

fn str_to_status(s: &str) -> anyhow::Result<CredentialStatus> {
    match s {
        "active" => Ok(CredentialStatus::Active),
        "revoked" => Ok(CredentialStatus::Revoked),
        "expired" => Ok(CredentialStatus::Expired),
        "pending_oauth" => Ok(CredentialStatus::PendingOAuth),
        "pending_migration" => Ok(CredentialStatus::PendingMigration),
        other => Err(anyhow::anyhow!("Unknown credential status: {other}")),
    }
}

/// Encode a [`GrantTarget`] into a `(target_type, target_value)` pair stored
/// in the `credential_grants` table.
fn grant_target_to_db(target: &GrantTarget) -> (String, String) {
    match target {
        GrantTarget::Agent { agent_id } => ("agent".to_string(), agent_id.0.to_string()),
        GrantTarget::Workflow { workflow_id } => ("workflow".to_string(), workflow_id.to_string()),
        GrantTarget::AllAgents => ("all_agents".to_string(), "".to_string()),
    }
}

fn db_to_grant_target(target_type: &str, target_value: &str) -> anyhow::Result<GrantTarget> {
    match target_type {
        "agent" => {
            let uuid = Uuid::parse_str(target_value)
                .map_err(|e| anyhow::anyhow!("Invalid agent_id UUID in grant: {e}"))?;
            Ok(GrantTarget::Agent {
                agent_id: AgentId(uuid),
            })
        }
        "workflow" => {
            let uuid = Uuid::parse_str(target_value)
                .map_err(|e| anyhow::anyhow!("Invalid workflow_id UUID in grant: {e}"))?;
            Ok(GrantTarget::Workflow { workflow_id: uuid })
        }
        "all_agents" => Ok(GrantTarget::AllAgents),
        other => Err(anyhow::anyhow!("Unknown grant target_type: {other}")),
    }
}

// ============================================================================
// Row hydration
// ============================================================================

/// Hydrate a [`UserCredentialBinding`] from a flat `credential_bindings` row.
///
/// Grants must be loaded separately and appended by the caller.
fn hydrate_binding(row: &sqlx::postgres::PgRow) -> anyhow::Result<UserCredentialBinding> {
    let id: Uuid = row.try_get("id")?;
    let owner_user_id: String = row.try_get("owner_user_id")?;
    let tenant_id_str: String = row.try_get("tenant_id")?;
    let credential_type_str: String = row.try_get("credential_type")?;
    let provider_str: String = row.try_get("provider")?;
    let label: String = row.try_get("label")?;
    let secret_path_str: String = row.try_get("secret_path")?;
    let scope_str: String = row.try_get("scope")?;
    let scope_team_id: Option<Uuid> = row.try_get("scope_team_id")?;
    let status_str: String = row.try_get("status")?;
    let oauth_scopes: Option<Vec<String>> = row.try_get("oauth_scopes")?;
    let external_account_id: Option<String> = row.try_get("external_account_id")?;
    let service_url: Option<String> = row.try_get("service_url")?;
    let tags: Option<serde_json::Value> = row.try_get("tags")?;
    // Migration 035 (AEGIS ADR-125 D1); NULL for every non-mailbox row.
    let mailbox_settings: Option<serde_json::Value> = row.try_get("mailbox_settings")?;
    let mailbox = mailbox_settings
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| anyhow::anyhow!("Invalid mailbox_settings for binding {id}: {e}"))?;
    // Migration 042 (AEGIS ADR-132 (7a) S2); NULL for every binding whose
    // provider is not a remote server.
    let reach_json: Option<serde_json::Value> = row.try_get("reach")?;
    let reach = reach_json
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| anyhow::anyhow!("Invalid reach for binding {id}: {e}"))?;
    // Migration 047 (AEGIS ADR-138 K4); NULL for every binding that is not a
    // calendar account.
    let calendar_settings: Option<serde_json::Value> = row.try_get("calendar_settings")?;
    let calendar = calendar_settings
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| anyhow::anyhow!("Invalid calendar_settings for binding {id}: {e}"))?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at")?;

    let tenant_id = TenantId::new(tenant_id_str).map_err(|e| anyhow::anyhow!(e))?;
    let credential_type = str_to_credential_type(&credential_type_str)?;
    let provider = str_to_provider(&provider_str);
    let status = str_to_status(&status_str)?;

    // Reconstruct SecretPath from the stored canonical string.
    // The stored value is the `full_path()` output: namespace/mount_point/path.
    // We store it as a simple opaque string and reconstruct using `new("","","<path>")`.
    // The effective_mount is recomputed at use-time from the tenant_id on the binding.
    let secret_path = SecretPath::for_tenant(tenant_id.clone(), "kv", secret_path_str);

    let scope = match scope_str.as_str() {
        "team" => {
            let team_id = scope_team_id
                .ok_or_else(|| anyhow::anyhow!("scope='team' but scope_team_id is NULL"))?;
            CredentialScope::Team { team_id }
        }
        _ => CredentialScope::Personal,
    };

    Ok(UserCredentialBinding {
        id: CredentialBindingId(id),
        owner_user_id,
        tenant_id,
        credential_type,
        provider,
        secret_path,
        scope,
        status,
        metadata: CredentialMetadata {
            label,
            tags,
            service_url,
            external_account_id,
            oauth_scopes,
            mailbox,
            reach,
            calendar,
        },
        grants: Vec::new(),
        created_at,
        updated_at,
    })
}

/// Hydrate a [`CredentialGrant`] from a `credential_grants` row.
fn hydrate_grant(row: &sqlx::postgres::PgRow) -> anyhow::Result<CredentialGrant> {
    let id: Uuid = row.try_get("id")?;
    let binding_id: Uuid = row.try_get("binding_id")?;
    let target_type: String = row.try_get("target_type")?;
    let target_value: String = row.try_get("target_value")?;
    let granted_at: DateTime<Utc> = row.try_get("granted_at")?;
    let granted_by: String = row.try_get("granted_by")?;

    let target = db_to_grant_target(&target_type, &target_value)?;

    Ok(CredentialGrant {
        id: CredentialGrantId(id),
        binding_id: CredentialBindingId(binding_id),
        target,
        granted_at,
        granted_by,
    })
}

// ============================================================================
// Repository implementation
// ============================================================================

#[async_trait]
impl CredentialBindingRepository for PostgresCredentialBindingRepository {
    // -----------------------------------------------------------------------
    // save
    // -----------------------------------------------------------------------

    async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()> {
        let (scope_str, scope_team_id): (&str, Option<Uuid>) = match &binding.scope {
            CredentialScope::Personal => ("personal", None),
            CredentialScope::Team { team_id } => ("team", Some(*team_id)),
        };
        let mailbox_settings: Option<serde_json::Value> = binding
            .metadata
            .mailbox
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| anyhow::anyhow!("Failed to encode mailbox_settings: {e}"))?;
        let reach: Option<serde_json::Value> = binding
            .metadata
            .reach
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| anyhow::anyhow!("Failed to encode reach: {e}"))?;
        let calendar_settings: Option<serde_json::Value> = binding
            .metadata
            .calendar
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| anyhow::anyhow!("Failed to encode calendar_settings: {e}"))?;

        sqlx::query(
            r#"
            INSERT INTO credential_bindings (
                id, owner_user_id, tenant_id, credential_type, provider,
                label, secret_path, scope, scope_team_id, status,
                oauth_scopes, external_account_id, service_url, tags,
                created_at, updated_at, mailbox_settings, reach, calendar_settings
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
            ON CONFLICT (id) DO UPDATE SET
                owner_user_id       = EXCLUDED.owner_user_id,
                tenant_id           = EXCLUDED.tenant_id,
                credential_type     = EXCLUDED.credential_type,
                provider            = EXCLUDED.provider,
                label               = EXCLUDED.label,
                secret_path         = EXCLUDED.secret_path,
                scope               = EXCLUDED.scope,
                scope_team_id       = EXCLUDED.scope_team_id,
                status              = EXCLUDED.status,
                oauth_scopes        = EXCLUDED.oauth_scopes,
                external_account_id = EXCLUDED.external_account_id,
                service_url         = EXCLUDED.service_url,
                tags                = EXCLUDED.tags,
                updated_at          = EXCLUDED.updated_at,
                mailbox_settings    = EXCLUDED.mailbox_settings,
                reach               = EXCLUDED.reach,
                calendar_settings   = EXCLUDED.calendar_settings
            "#,
        )
        .bind(binding.id.0)
        .bind(&binding.owner_user_id)
        .bind(binding.tenant_id.as_str())
        .bind(credential_type_to_str(&binding.credential_type))
        .bind(provider_to_str(&binding.provider))
        .bind(&binding.metadata.label)
        .bind(&binding.secret_path.path)
        .bind(scope_str)
        .bind(scope_team_id)
        .bind(status_to_str(&binding.status))
        .bind(binding.metadata.oauth_scopes.as_deref())
        .bind(binding.metadata.external_account_id.as_deref())
        .bind(binding.metadata.service_url.as_deref())
        .bind(&binding.metadata.tags)
        .bind(binding.created_at)
        .bind(binding.updated_at)
        .bind(&mailbox_settings)
        .bind(&reach)
        .bind(&calendar_settings)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to save credential binding {}: {e}", binding.id))?;

        // Replace grants: DELETE existing rows then INSERT the current set.
        sqlx::query("DELETE FROM credential_grants WHERE binding_id = $1")
            .bind(binding.id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to delete grants for binding {}: {e}", binding.id)
            })?;

        for grant in &binding.grants {
            let (target_type, target_value) = grant_target_to_db(&grant.target);
            sqlx::query(
                r#"
                INSERT INTO credential_grants (id, binding_id, target_type, target_value, granted_at, granted_by)
                VALUES ($1, $2, $3, $4, $5, $6)
                "#,
            )
            .bind(grant.id.0)
            .bind(binding.id.0)
            .bind(&target_type)
            .bind(&target_value)
            .bind(grant.granted_at)
            .bind(&grant.granted_by)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to insert grant {} for binding {}: {e}",
                    grant.id,
                    binding.id
                )
            })?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // find_by_id
    // -----------------------------------------------------------------------

    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        let row = sqlx::query("SELECT * FROM credential_bindings WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to find credential binding {}: {e}", id))?;

        let Some(row) = row else {
            return Ok(None);
        };

        let mut binding = hydrate_binding(&row)?;

        // Attach grants
        let grant_rows = sqlx::query(
            "SELECT * FROM credential_grants WHERE binding_id = $1 ORDER BY granted_at",
        )
        .bind(id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load grants for binding {}: {e}", id))?;

        for row in &grant_rows {
            binding.grants.push(hydrate_grant(row)?);
        }

        Ok(Some(binding))
    }

    // -----------------------------------------------------------------------
    // find_by_owner
    // -----------------------------------------------------------------------

    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        let rows = sqlx::query(
            "SELECT * FROM credential_bindings WHERE tenant_id = $1 AND owner_user_id = $2 ORDER BY created_at",
        )
        .bind(tenant_id.as_str())
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to list credential bindings for tenant {} / user {}: {e}",
                tenant_id.as_str(),
                owner_user_id
            )
        })?;

        let mut bindings = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut binding = hydrate_binding(row)?;
            let grant_rows = sqlx::query(
                "SELECT * FROM credential_grants WHERE binding_id = $1 ORDER BY granted_at",
            )
            .bind(binding.id.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to load grants for binding {}: {e}", binding.id)
            })?;

            for grow in &grant_rows {
                binding.grants.push(hydrate_grant(grow)?);
            }

            bindings.push(binding);
        }

        Ok(bindings)
    }

    // -----------------------------------------------------------------------
    // find_active_grants_for_target
    // -----------------------------------------------------------------------

    async fn find_active_grants_for_target(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
        provider: &CredentialProvider,
        target: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>> {
        let (target_type, target_value) = grant_target_to_db(target);
        // A row written as `custom:<name>` before ADR-125's Update of
        // 2026-10-04 answers its name too (the query's `'custom:' || $3`).
        let provider_str = provider_to_str(provider);

        let rows = sqlx::query(
            r#"
            SELECT g.*
            FROM credential_grants g
            JOIN credential_bindings b ON b.id = g.binding_id
            WHERE b.tenant_id    = $1
              AND b.owner_user_id = $2
              AND b.provider IN ($3, 'custom:' || $3)
              AND b.status        = 'active'
              AND g.target_type   = $4
              AND g.target_value  = $5
            ORDER BY g.granted_at
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(owner_user_id)
        .bind(&provider_str)
        .bind(&target_type)
        .bind(&target_value)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query active grants: {e}"))?;

        let mut grants = Vec::with_capacity(rows.len());
        for row in &rows {
            grants.push(hydrate_grant(row)?);
        }

        Ok(grants)
    }

    // -----------------------------------------------------------------------
    // delete
    // -----------------------------------------------------------------------

    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
        // credential_grants rows cascade-delete via ON DELETE CASCADE.
        sqlx::query("DELETE FROM credential_bindings WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to delete credential binding {}: {e}", id))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // OAuth pending state
    // -----------------------------------------------------------------------

    async fn save_oauth_state(
        &self,
        state: &str,
        binding_id: &CredentialBindingId,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            INSERT INTO oauth_pending_states (state, binding_id, pkce_verifier, redirect_uri, created_at)
            VALUES ($1, $2, $3, $4, NOW())
            "#,
        )
        .bind(state)
        .bind(binding_id.0)
        .bind(pkce_verifier)
        .bind(redirect_uri)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to save OAuth pending state: {e}"))?;
        Ok(())
    }

    async fn find_oauth_state(&self, state: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        let row = sqlx::query(
            "SELECT state, binding_id, pkce_verifier, redirect_uri, created_at FROM oauth_pending_states WHERE state = $1",
        )
        .bind(state)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to look up OAuth pending state: {e}"))?;

        let Some(row) = row else {
            return Ok(None);
        };

        let binding_id_uuid: Uuid = row.try_get("binding_id")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;

        Ok(Some(OAuthPendingState {
            state: row.try_get("state")?,
            binding_id: CredentialBindingId(binding_id_uuid),
            pkce_verifier: row.try_get("pkce_verifier")?,
            redirect_uri: row.try_get("redirect_uri")?,
            created_at,
        }))
    }

    async fn delete_oauth_state(&self, state: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM oauth_pending_states WHERE state = $1")
            .bind(state)
            .execute(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to delete OAuth pending state: {e}"))?;
        Ok(())
    }

    async fn delete_expired_oauth_states(&self, older_than: DateTime<Utc>) -> anyhow::Result<u64> {
        let result = sqlx::query("DELETE FROM oauth_pending_states WHERE created_at < $1")
            .bind(older_than)
            .execute(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to delete expired OAuth pending states: {e}"))?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod provider_storage_tests {
    use super::*;

    /// Test (d), the repository's half: a provider is stored as the
    /// registry's string, whatever it is, and reads back as it; a stored
    /// `google_mail` reads back as `google_mail`; an old `custom:` row reads
    /// as the name its user gave.
    #[test]
    fn a_provider_is_stored_and_read_back_as_its_string() {
        for name in ["google", "slack", "github", "openai", "imap", "google_mail"] {
            let stored = provider_to_str(&CredentialProvider::new(name));
            assert_eq!(stored, name);
            assert_eq!(str_to_provider(&stored).as_str(), name);
        }
        assert_eq!(str_to_provider("google_mail").as_str(), "google_mail");
        assert_eq!(str_to_provider("custom:gitlab").as_str(), "gitlab");
    }

    /// Test (d) against PostgreSQL (`AEGIS_DATABASE_URL` or `DATABASE_URL`;
    /// skipped without one), in a schema of its own with migrations 011 and
    /// 035: an OAuth2 binding of the registry entry `google` round-trips as
    /// `google`; a row stored as `google_mail` reads back as `google_mail`;
    /// a row stored as `custom:gitlab` reads as `gitlab` and answers the
    /// grant query for `gitlab`.
    #[tokio::test]
    async fn providers_round_trip_through_postgres_as_their_strings() {
        use crate::domain::credential::{
            CredentialBindingId, CredentialBindingRepository, CredentialMetadata, CredentialScope,
            CredentialStatus, CredentialType, GrantTarget, UserCredentialBinding,
        };
        use crate::domain::secrets::SecretPath;
        use sqlx::postgres::PgPoolOptions;
        use sqlx::Executor;

        let Ok(url) =
            std::env::var("AEGIS_DATABASE_URL").or_else(|_| std::env::var("DATABASE_URL"))
        else {
            eprintln!("skipped: no AEGIS_DATABASE_URL or DATABASE_URL");
            return;
        };
        let schema = format!("prov_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
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
            .unwrap();
        pool.execute(include_str!(
            "../../../../../cli/migrations/011_credential_bindings.sql"
        ))
        .await
        .unwrap();
        pool.execute(include_str!(
            "../../../../../cli/migrations/035_credential_mailbox_settings.sql"
        ))
        .await
        .unwrap();
        pool.execute(include_str!(
            "../../../../../cli/migrations/042_credential_binding_reach.sql"
        ))
        .await
        .unwrap();
        pool.execute(include_str!(
            "../../../../../cli/migrations/047_credential_calendar_settings.sql"
        ))
        .await
        .unwrap();
        let repo = PostgresCredentialBindingRepository::new(pool.clone());
        let tenant = TenantId::for_consumer_user("owner-sub").unwrap();
        let binding = |provider: &str, credential_type: CredentialType| {
            let id = CredentialBindingId::new();
            UserCredentialBinding {
                id,
                owner_user_id: "owner-sub".to_string(),
                tenant_id: tenant.clone(),
                credential_type,
                provider: CredentialProvider::new(provider),
                secret_path: SecretPath::for_tenant(tenant.clone(), "kv", format!("c/{}", id.0)),
                scope: CredentialScope::Personal,
                status: CredentialStatus::Active,
                metadata: CredentialMetadata {
                    label: provider.to_string(),
                    tags: None,
                    service_url: None,
                    external_account_id: None,
                    oauth_scopes: None,
                    mailbox: None,
                    reach: None,
                    calendar: None,
                },
                grants: Vec::new(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }
        };
        let raw = |id: CredentialBindingId| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT provider FROM credential_bindings WHERE id = $1",
                )
                .bind(id.0)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };

        let google = binding("google", CredentialType::OAuth2);
        repo.save(&google).await.unwrap();
        assert_eq!(raw(google.id).await, "google");
        let read = repo.find_by_id(&google.id).await.unwrap().unwrap();
        assert_eq!(read.provider.as_str(), "google");
        assert_eq!(read.credential_type, CredentialType::OAuth2);

        let old = binding("google_mail", CredentialType::Mailbox);
        repo.save(&old).await.unwrap();
        assert_eq!(raw(old.id).await, "google_mail");
        let read = repo.find_by_id(&old.id).await.unwrap().unwrap();
        assert_eq!(read.provider.as_str(), "google_mail");

        let mut gitlab = binding("gitlab", CredentialType::Secret);
        gitlab.add_grant(GrantTarget::AllAgents, "owner-sub".to_string());
        repo.save(&gitlab).await.unwrap();
        sqlx::query("UPDATE credential_bindings SET provider = 'custom:gitlab' WHERE id = $1")
            .bind(gitlab.id.0)
            .execute(&pool)
            .await
            .unwrap();
        let read = repo.find_by_id(&gitlab.id).await.unwrap().unwrap();
        assert_eq!(read.provider.as_str(), "gitlab");
        let grants = repo
            .find_active_grants_for_target(
                &tenant,
                "owner-sub",
                &CredentialProvider::new("gitlab"),
                &GrantTarget::AllAgents,
            )
            .await
            .unwrap();
        assert_eq!(grants.len(), 1, "the old custom: row answers its name");

        pool.close().await;
        admin
            .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
            .await
            .unwrap();
    }
}
