// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Two orchestrator processes that start at once both apply the database
//! migrations. They must not both apply one: the second must wait for the
//! first, find the migrations applied, and carry on.
//!
//! The deployment reaches PostgreSQL through PgBouncer in transaction
//! pooling mode, where a lock a session takes does not stay with the client
//! that took it. So these tests go through a PgBouncer in that mode, with one
//! server connection: CI runs one and sets `AEGIS_TEST_PGBOUNCER_URL` to it,
//! beside `AEGIS_TEST_POSTGRES_URL` for the PostgreSQL behind it. Without
//! them the tests do nothing and say so; in CI they fail instead.

use aegis_orchestrator::daemon::migrations::{
    apply_migrations, apply_migrations_waiting_at_most, refused_in_a_transaction, MigrationError,
    MIGRATOR,
};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

fn url(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("{name} is not set; in CI these tests must reach PostgreSQL through PgBouncer")
        }
        _ => {
            eprintln!("skipped: {name} is not set");
            None
        }
    }
}

/// A pool on `database` through PgBouncer, as an orchestrator process has.
async fn process_pool(bouncer: &str, database: &str) -> PgPool {
    let options = bouncer
        .parse::<PgConnectOptions>()
        .expect("the PgBouncer URL parses")
        .database(database);
    PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("connect through PgBouncer")
}

/// Two processes start at once on a new database, each applying the
/// migrations. Both carry on, and every migration is applied once.
#[tokio::test]
async fn two_processes_that_migrate_at_once_both_carry_on() {
    let (Some(postgres), Some(bouncer)) = (
        url("AEGIS_TEST_POSTGRES_URL"),
        url("AEGIS_TEST_PGBOUNCER_URL"),
    ) else {
        return;
    };
    let server = PgPoolOptions::new()
        .max_connections(1)
        .connect(&postgres)
        .await
        .expect("connect to the test PostgreSQL");

    let total = MIGRATOR.iter().count() as i64;
    let mut failures = Vec::new();
    for round in 0..3 {
        let name = format!("aegis_migrate_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let (a, b) = tokio::join!(process_pool(&bouncer, &name), process_pool(&bouncer, &name));
        let (first, second) = tokio::join!(apply_migrations(&a), apply_migrations(&b));
        for (which, result) in [("first", first), ("second", second)] {
            if let Err(e) = result {
                failures.push(format!("round {round}, {which} process: {e}"));
            }
        }
        let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&a)
            .await
            .unwrap_or(-1);
        if applied != total {
            failures.push(format!(
                "round {round}: {applied} migrations recorded, {total} shipped"
            ));
        }
        a.close().await;
        b.close().await;
        sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .execute(&server)
            .await
            .expect("drop the test database");
    }
    assert!(
        failures.is_empty(),
        "two processes that migrated at once did not both carry on: {failures:?}"
    );
}

/// Every migration is applied inside one transaction (so that one lock
/// holds for every process). PostgreSQL refuses some statements inside a
/// transaction block; a migration holding one would fail at every start.
#[test]
fn no_migration_holds_a_statement_refused_inside_a_transaction() {
    let refused: Vec<String> = MIGRATOR
        .iter()
        .filter_map(|m| {
            refused_in_a_transaction(&m.sql)
                .map(|statement| format!("{} {}: {statement}", m.version, m.description))
        })
        .collect();
    assert!(
        refused.is_empty(),
        "these migrations hold a statement PostgreSQL refuses inside a transaction, and every \
         migration is applied inside one: {refused:?}. Do the work another way (a plain \
         CREATE INDEX; a new enum value in a migration of its own, used only by a later \
         migration's release) or apply it outside the migrations"
    );
}

/// The check above catches each statement it is meant to, whatever its
/// case and spacing, and ignores one written only in a comment.
#[test]
fn the_statement_check_catches_what_postgresql_refuses_in_a_transaction() {
    for sql in [
        "CREATE INDEX CONCURRENTLY idx ON t (c);",
        "create unique index  concurrently idx on t (c);",
        "DROP INDEX CONCURRENTLY idx;",
        "REINDEX INDEX CONCURRENTLY idx;",
        "VACUUM t;",
        "vacuum analyze t;",
        "CREATE DATABASE other;",
        "DROP DATABASE other;",
        "ALTER SYSTEM SET work_mem = '64MB';",
        "CREATE TABLESPACE fast LOCATION '/x';",
        "ALTER TYPE status ADD VALUE 'new';",
        "alter type status add value if not exists 'new' after 'old';",
    ] {
        assert!(
            refused_in_a_transaction(sql).is_some(),
            "the check missed a statement refused in a transaction: {sql}"
        );
    }
    for sql in [
        "CREATE INDEX idx ON t (c);",
        "-- CREATE INDEX CONCURRENTLY is not used here\nCREATE INDEX idx ON t (c);",
        "/* VACUUM later */ CREATE TABLE t (c int);",
        "ALTER TABLE t ADD COLUMN vacuumed boolean;",
        "INSERT INTO notes VALUES ('run vacuum at night');",
    ] {
        assert!(
            refused_in_a_transaction(sql).is_none(),
            "the check refused a statement PostgreSQL accepts in a transaction: {sql}"
        );
    }
}

/// A process that waits for another applying migrations waits no longer
/// than it is given, then fails with a sentence saying so, and applies
/// nothing.
#[tokio::test]
async fn a_process_waits_a_stated_time_for_another_that_is_migrating() {
    let Some(postgres) = url("AEGIS_TEST_POSTGRES_URL") else {
        return;
    };
    let server = PgPoolOptions::new()
        .max_connections(1)
        .connect(&postgres)
        .await
        .expect("connect to the test PostgreSQL");
    let name = format!("aegis_migrate_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&server)
        .await
        .expect("create the test database");
    let options = postgres
        .parse::<PgConnectOptions>()
        .unwrap()
        .database(&name);
    let holder = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let waiter = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();

    // Another process is applying migrations: it holds the lock.
    let mut other = holder.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(0x6165_6769_736d_6967_i64)
        .execute(&mut *other)
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let waited = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        apply_migrations_waiting_at_most(&waiter, std::time::Duration::from_secs(2)),
    )
    .await;
    let elapsed = started.elapsed();
    other.rollback().await.unwrap();
    let applied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_name = '_sqlx_migrations'",
    )
    .fetch_one(&waiter)
    .await
    .unwrap();
    holder.close().await;
    waiter.close().await;
    sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .execute(&server)
        .await
        .unwrap();

    let result = waited.unwrap_or_else(|_| {
        panic!("a process waited {elapsed:?} for another's migrations, past the 2 seconds it was given")
    });
    match result {
        Err(e @ MigrationError::Busy { .. }) => {
            let message = e.to_string();
            assert!(
                message.contains("2 seconds") && message.contains("applied none"),
                "the refusal does not say what happened: {message}"
            );
        }
        other => panic!("a process that waited too long did not fail plainly: {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "it waited {elapsed:?}"
    );
    assert_eq!(
        applied, 0,
        "a process that stopped waiting applied migrations"
    );
}
