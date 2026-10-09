// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The profile store (AEGIS ADR-140 D1, D4) against a real PostgreSQL, with
//! the migrations this binary ships.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere the tests say they were skipped and pass.
//!
//! They cover: a profile round-trips with its bindings in order, its tools
//! and its defaults; a name the owner already uses, ignoring case, is
//! answered taken while another person may use it; an update replaces the
//! bindings whole; a deleted profile is no longer found or listed and frees
//! its name; and migration 049 run again changes nothing.

use chrono::{DateTime, SubsecRound, Utc};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;

use aegis_orchestrator_core::domain::credential::CredentialBindingId;
use aegis_orchestrator_core::domain::profile::{
    Profile, ProfileId, ProfileRepository, ProfileSave, ToolAllowList, ToolPattern,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_profile::PostgresProfileRepository;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

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
        let name = format!("aegis_profiles_{}", uuid::Uuid::new_v4().simple());
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

fn now() -> DateTime<Utc> {
    // PostgreSQL keeps microseconds.
    Utc::now().trunc_subsecs(6)
}

fn profile(sub: &str, name: &str, bindings: usize) -> Profile {
    let at = now();
    Profile {
        id: ProfileId::new(),
        tenant_id: TenantId::for_consumer_user(sub).unwrap(),
        user_sub: sub.to_string(),
        name: name.to_string(),
        bindings: (0..bindings).map(|_| CredentialBindingId::new()).collect(),
        tools: ToolAllowList(vec![
            ToolPattern::parse("mail.reply").unwrap(),
            ToolPattern::parse("github.*").unwrap(),
        ]),
        repository: Some(uuid::Uuid::new_v4()),
        notes_workspace: Some("fundraising".into()),
        instructions: Some("Keep replies short.".into()),
        created_at: at,
        updated_at: at,
        deleted_at: None,
    }
}

/// A profile round-trips; its bindings keep their order; a name taken
/// ignoring case is answered taken for its owner only; an update replaces
/// the bindings whole; a deleted profile frees its name.
#[tokio::test]
async fn a_profile_round_trips_and_its_name_is_unique_for_its_owner() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresProfileRepository::new(db.pool.clone());
    let mut made = profile("owner-sub", "Fundraising", 3);
    assert_eq!(repo.insert(&made).await.unwrap(), ProfileSave::Saved);
    assert_eq!(repo.find(&made.id).await.unwrap(), Some(made.clone()));

    let mut taken = profile("owner-sub", "FUNDRAISING", 0);
    assert_eq!(repo.insert(&taken).await.unwrap(), ProfileSave::NameTaken);
    assert_eq!(
        repo.find(&taken.id).await.unwrap(),
        None,
        "a refused insert stored a row"
    );
    let theirs = profile("other-sub", "Fundraising", 1);
    assert_eq!(repo.insert(&theirs).await.unwrap(), ProfileSave::Saved);

    taken.name = "Other".into();
    assert_eq!(repo.insert(&taken).await.unwrap(), ProfileSave::Saved);
    taken.name = "fundraising".into();
    assert_eq!(repo.update(&taken).await.unwrap(), ProfileSave::NameTaken);

    made.bindings = vec![made.bindings[2], CredentialBindingId::new()];
    made.tools = ToolAllowList::default();
    made.instructions = None;
    made.updated_at = now();
    assert_eq!(repo.update(&made).await.unwrap(), ProfileSave::Saved);
    assert_eq!(repo.find(&made.id).await.unwrap(), Some(made.clone()));

    let listed = repo
        .list_for_owner(&made.tenant_id, "owner-sub")
        .await
        .unwrap();
    let names: Vec<&str> = listed.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["Fundraising", "Other"]);

    assert!(repo.delete(&made.id, now()).await.unwrap());
    assert!(!repo.delete(&made.id, now()).await.unwrap());
    assert_eq!(repo.find(&made.id).await.unwrap(), None);
    taken.name = "fundraising".into();
    assert_eq!(
        repo.update(&taken).await.unwrap(),
        ProfileSave::Saved,
        "a deleted profile's name was not freed"
    );
    db.remove().await;
}

/// Migration 049 applied again over a migrated schema, with rows in it,
/// changes nothing.
#[tokio::test]
async fn migration_049_run_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresProfileRepository::new(db.pool.clone());
    repo.insert(&profile("owner-sub", "Fundraising", 2))
        .await
        .unwrap();
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM profiles) AS profiles, \
                    (SELECT count(*) FROM profile_bindings) AS bindings, \
                    (SELECT count(*) FROM pg_indexes WHERE tablename LIKE 'profile%') AS indexes",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| {
            (
                row.get::<i64, _>("profiles"),
                row.get::<i64, _>("bindings"),
                row.get::<i64, _>("indexes"),
            )
        })
        .unwrap()
    };
    let before = snapshot().await;
    assert_eq!((before.0, before.1), (1, 2));
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 49)
        .expect("migration 049 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 049 run again");
    assert_eq!(snapshot().await, before);
    db.remove().await;
}
