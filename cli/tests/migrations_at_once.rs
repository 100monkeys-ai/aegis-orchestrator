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

use aegis_orchestrator::daemon::migrations::{apply_migrations, MIGRATOR};
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
