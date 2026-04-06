// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! PostgreSQL-backed RealmRepository — ADR-041.

use crate::domain::iam::{IdentityRealm, RealmKind, RealmRepository};
use async_trait::async_trait;
use sqlx::{postgres::PgPool, Row};

pub struct PostgresRealmRepository {
    pool: PgPool,
}

impl PostgresRealmRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn realm_kind_to_str(kind: &RealmKind) -> String {
    match kind {
        RealmKind::System => "system".to_string(),
        RealmKind::Consumer => "consumer".to_string(),
        RealmKind::Tenant { slug } => format!("tenant:{slug}"),
    }
}

fn str_to_realm_kind(s: &str) -> RealmKind {
    match s {
        "system" => RealmKind::System,
        "consumer" => RealmKind::Consumer,
        other => {
            let slug = other.strip_prefix("tenant:").unwrap_or(other).to_string();
            RealmKind::Tenant { slug }
        }
    }
}

fn row_to_realm(row: &sqlx::postgres::PgRow) -> anyhow::Result<IdentityRealm> {
    let realm_slug: String = row.get("slug");
    let issuer_url: String = row.get("issuer_url");
    let jwks_uri: String = row.get("jwks_uri");
    let audience: String = row.get("audience");
    let realm_kind_str: String = row.get("realm_kind");
    let realm_kind = str_to_realm_kind(&realm_kind_str);

    Ok(IdentityRealm {
        realm_slug,
        issuer_url,
        jwks_uri,
        audience,
        realm_kind,
    })
}

#[async_trait]
impl RealmRepository for PostgresRealmRepository {
    async fn save(&self, realm: IdentityRealm) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            INSERT INTO identity_realms (slug, issuer_url, jwks_uri, audience, realm_kind)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (slug) DO UPDATE SET
                issuer_url = EXCLUDED.issuer_url,
                jwks_uri   = EXCLUDED.jwks_uri,
                audience   = EXCLUDED.audience,
                realm_kind = EXCLUDED.realm_kind,
                updated_at = NOW()
            "#,
        )
        .bind(&realm.realm_slug)
        .bind(&realm.issuer_url)
        .bind(&realm.jwks_uri)
        .bind(&realm.audience)
        .bind(realm_kind_to_str(&realm.realm_kind))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_by_slug(&self, slug: &str) -> anyhow::Result<Option<IdentityRealm>> {
        let row = sqlx::query(
            "SELECT slug, issuer_url, jwks_uri, audience, realm_kind FROM identity_realms WHERE slug = $1",
        )
        .bind(slug)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_realm(&r)).transpose()
    }

    async fn list_all(&self) -> anyhow::Result<Vec<IdentityRealm>> {
        let rows = sqlx::query(
            "SELECT slug, issuer_url, jwks_uri, audience, realm_kind FROM identity_realms ORDER BY slug",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_realm).collect()
    }

    async fn delete(&self, slug: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM identity_realms WHERE slug = $1")
            .bind(slug)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
