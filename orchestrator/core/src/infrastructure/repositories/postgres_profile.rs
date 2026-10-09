// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Profile repositories (AEGIS ADR-140 D1, D4)
//!
//! [`PostgresProfileRepository`] stores profiles in the `profiles` and
//! `profile_bindings` tables of migration `049_profiles.sql`; a name the
//! owner already uses (ignoring case) is answered [`ProfileSave::NameTaken`]
//! from the unique index. [`InMemoryProfileRepository`] keeps them in
//! process, for tests and for a daemon run without a database.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::domain::credential::CredentialBindingId;
use crate::domain::profile::{
    Profile, ProfileId, ProfileRepository, ProfileSave, ToolAllowList, ToolPattern,
};
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;

const PROFILE_COLUMNS: &str = "id, tenant_id, user_sub, name, tools, repository_binding_id, \
     notes_workspace, instructions, created_at, updated_at, deleted_at";

/// The SQLSTATE of a unique violation.
const UNIQUE_VIOLATION: &str = "23505";

pub struct PostgresProfileRepository {
    pool: PgPool,
}

impl PostgresProfileRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn bindings_of(&self, ids: &[Uuid]) -> Result<HashMap<Uuid, Vec<Uuid>>, RepositoryError> {
        let rows = sqlx::query(
            "SELECT profile_id, binding_id FROM profile_bindings \
             WHERE profile_id = ANY($1) ORDER BY profile_id, position",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        let mut bindings: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for row in rows {
            let profile: Uuid = column(&row, "profile_id")?;
            let binding: Uuid = column(&row, "binding_id")?;
            bindings.entry(profile).or_default().push(binding);
        }
        Ok(bindings)
    }

    async fn hydrate_all(&self, rows: Vec<PgRow>) -> Result<Vec<Profile>, RepositoryError> {
        let ids = rows
            .iter()
            .map(|row| column::<Uuid>(row, "id"))
            .collect::<Result<Vec<_>, _>>()?;
        let mut bindings = self.bindings_of(&ids).await?;
        rows.iter()
            .map(|row| {
                let id: Uuid = column(row, "id")?;
                hydrate(row, bindings.remove(&id).unwrap_or_default())
            })
            .collect()
    }
}

fn column<'r, T>(row: &'r PgRow, name: &str) -> Result<T, RepositoryError>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(name)
        .map_err(|e| RepositoryError::Serialization(format!("{name}: {e}")))
}

fn hydrate(row: &PgRow, bindings: Vec<Uuid>) -> Result<Profile, RepositoryError> {
    let tenant: String = column(row, "tenant_id")?;
    let tools: Vec<String> = serde_json::from_value(column(row, "tools")?)
        .map_err(|e| RepositoryError::Serialization(format!("tools: {e}")))?;
    let tools = tools
        .iter()
        .map(|raw| ToolPattern::parse(raw).map_err(RepositoryError::Serialization))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Profile {
        id: ProfileId(column(row, "id")?),
        tenant_id: TenantId::new(tenant)
            .map_err(|e| RepositoryError::Serialization(format!("tenant_id: {e}")))?,
        user_sub: column(row, "user_sub")?,
        name: column(row, "name")?,
        bindings: bindings.into_iter().map(CredentialBindingId).collect(),
        tools: ToolAllowList(tools),
        repository: column(row, "repository_binding_id")?,
        notes_workspace: column(row, "notes_workspace")?,
        instructions: column(row, "instructions")?,
        created_at: column(row, "created_at")?,
        updated_at: column(row, "updated_at")?,
        deleted_at: column(row, "deleted_at")?,
    })
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some(UNIQUE_VIOLATION))
}

/// Run a statement of a save; a unique violation is the name being taken.
async fn saved<T>(
    result: Result<T, sqlx::Error>,
    tx: sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<Option<(T, sqlx::Transaction<'_, sqlx::Postgres>)>, RepositoryError> {
    match result {
        Ok(done) => Ok(Some((done, tx))),
        Err(e) if is_unique_violation(&e) => {
            tx.rollback().await?;
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

async fn write_bindings(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    profile: &Profile,
) -> Result<(), RepositoryError> {
    sqlx::query("DELETE FROM profile_bindings WHERE profile_id = $1")
        .bind(profile.id.0)
        .execute(&mut **tx)
        .await?;
    for (position, binding) in profile.bindings.iter().enumerate() {
        sqlx::query(
            "INSERT INTO profile_bindings (profile_id, binding_id, position) VALUES ($1, $2, $3)",
        )
        .bind(profile.id.0)
        .bind(binding.0)
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn tools_json(profile: &Profile) -> serde_json::Value {
    serde_json::Value::from(profile.tools.as_strings())
}

#[async_trait]
impl ProfileRepository for PostgresProfileRepository {
    async fn insert(&self, p: &Profile) -> Result<ProfileSave, RepositoryError> {
        let tx = self.pool.begin().await?;
        let mut tx = tx;
        let result = sqlx::query(&format!(
            "INSERT INTO profiles ({PROFILE_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"
        ))
        .bind(p.id.0)
        .bind(p.tenant_id.as_str())
        .bind(&p.user_sub)
        .bind(&p.name)
        .bind(tools_json(p))
        .bind(p.repository)
        .bind(&p.notes_workspace)
        .bind(&p.instructions)
        .bind(p.created_at)
        .bind(p.updated_at)
        .bind(p.deleted_at)
        .execute(&mut *tx)
        .await;
        let Some((_, mut tx)) = saved(result, tx).await? else {
            return Ok(ProfileSave::NameTaken);
        };
        write_bindings(&mut tx, p).await?;
        tx.commit().await?;
        Ok(ProfileSave::Saved)
    }

    async fn update(&self, p: &Profile) -> Result<ProfileSave, RepositoryError> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE profiles SET name = $2, tools = $3, repository_binding_id = $4, \
             notes_workspace = $5, instructions = $6, updated_at = $7 \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(p.id.0)
        .bind(&p.name)
        .bind(tools_json(p))
        .bind(p.repository)
        .bind(&p.notes_workspace)
        .bind(&p.instructions)
        .bind(p.updated_at)
        .execute(&mut *tx)
        .await;
        let Some((done, mut tx)) = saved(result, tx).await? else {
            return Ok(ProfileSave::NameTaken);
        };
        if done.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(RepositoryError::NotFound(format!("profile {}", p.id)));
        }
        write_bindings(&mut tx, p).await?;
        tx.commit().await?;
        Ok(ProfileSave::Saved)
    }

    async fn find(&self, id: &ProfileId) -> Result<Option<Profile>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {PROFILE_COLUMNS} FROM profiles WHERE id = $1 AND deleted_at IS NULL"
        ))
        .bind(id.0)
        .fetch_all(&self.pool)
        .await?;
        Ok(self.hydrate_all(rows).await?.into_iter().next())
    }

    async fn list_for_owner(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> Result<Vec<Profile>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {PROFILE_COLUMNS} FROM profiles \
             WHERE tenant_id = $1 AND user_sub = $2 AND deleted_at IS NULL \
             ORDER BY lower(name), id"
        ))
        .bind(tenant_id.as_str())
        .bind(user_sub)
        .fetch_all(&self.pool)
        .await?;
        self.hydrate_all(rows).await
    }

    async fn delete(&self, id: &ProfileId, at: DateTime<Utc>) -> Result<bool, RepositoryError> {
        let done = sqlx::query(
            "UPDATE profiles SET deleted_at = $2, updated_at = $2 \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id.0)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }
}

/// Profiles kept in process.
#[derive(Default)]
pub struct InMemoryProfileRepository {
    profiles: RwLock<HashMap<ProfileId, Profile>>,
}

impl InMemoryProfileRepository {
    pub fn new() -> Self {
        Self::default()
    }

    fn name_taken(profiles: &HashMap<ProfileId, Profile>, p: &Profile) -> bool {
        profiles.values().any(|other| {
            other.id != p.id
                && other.deleted_at.is_none()
                && other.tenant_id == p.tenant_id
                && other.user_sub == p.user_sub
                && other.name.to_lowercase() == p.name.to_lowercase()
        })
    }
}

#[async_trait]
impl ProfileRepository for InMemoryProfileRepository {
    async fn insert(&self, p: &Profile) -> Result<ProfileSave, RepositoryError> {
        let mut profiles = self.profiles.write().await;
        if Self::name_taken(&profiles, p) {
            return Ok(ProfileSave::NameTaken);
        }
        profiles.insert(p.id, p.clone());
        Ok(ProfileSave::Saved)
    }

    async fn update(&self, p: &Profile) -> Result<ProfileSave, RepositoryError> {
        let mut profiles = self.profiles.write().await;
        if !profiles
            .get(&p.id)
            .is_some_and(|stored| stored.deleted_at.is_none())
        {
            return Err(RepositoryError::NotFound(format!("profile {}", p.id)));
        }
        if Self::name_taken(&profiles, p) {
            return Ok(ProfileSave::NameTaken);
        }
        profiles.insert(p.id, p.clone());
        Ok(ProfileSave::Saved)
    }

    async fn find(&self, id: &ProfileId) -> Result<Option<Profile>, RepositoryError> {
        Ok(self
            .profiles
            .read()
            .await
            .get(id)
            .filter(|p| p.deleted_at.is_none())
            .cloned())
    }

    async fn list_for_owner(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> Result<Vec<Profile>, RepositoryError> {
        let mut owned: Vec<Profile> = self
            .profiles
            .read()
            .await
            .values()
            .filter(|p| {
                p.deleted_at.is_none() && &p.tenant_id == tenant_id && p.user_sub == user_sub
            })
            .cloned()
            .collect();
        owned.sort_by_key(|p| (p.name.to_lowercase(), p.id.0));
        Ok(owned)
    }

    async fn delete(&self, id: &ProfileId, at: DateTime<Utc>) -> Result<bool, RepositoryError> {
        let mut profiles = self.profiles.write().await;
        match profiles.get_mut(id) {
            Some(p) if p.deleted_at.is_none() => {
                p.deleted_at = Some(at);
                p.updated_at = at;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
