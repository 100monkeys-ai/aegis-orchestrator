// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Applying the database migrations this binary ships.
//!
//! More than one orchestrator process can start at once against one
//! database (the core and the relay coordinator do on every deploy), and
//! each applies the migrations it finds missing. They must be applied by one
//! process at a time: the others wait, then find them applied and carry on.
//!
//! sqlx's own migrator locks with a session-level advisory lock taken in one
//! statement. The deployment reaches PostgreSQL through PgBouncer in
//! transaction pooling mode, where a session lock stays with the server
//! connection that ran the statement, not with the client: the next
//! statement may run on another connection, and another client may be given
//! the connection that holds the lock (advisory locks are re-entrant within
//! a session). So the lock excludes nobody there. A transaction does stay on
//! one server connection until it ends, so all the migrations are applied in
//! one transaction that first takes a transaction-level advisory lock.
//! Each migration still runs in its own savepoint and is recorded with it.

use sqlx::error::BoxDynError;
use sqlx::migrate::{MigrateError, Migration, MigrationSource, Migrator};
use sqlx::PgPool;
use std::future::Future;
use std::pin::Pin;

/// The migrations under `cli/migrations/`, built into the binary.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// The advisory lock every process takes to apply migrations. Advisory
/// locks are kept per database, so one key serves every database.
const MIGRATION_LOCK_KEY: i64 = 0x6165_6769_736d_6967; // "aegismig"

/// Apply every migration the database does not have yet, one process at a
/// time. A process that waited for another finds the migrations applied and
/// returns `Ok`.
pub async fn apply_migrations(pool: &PgPool) -> Result<(), MigrateError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    // The lock above holds for the whole transaction; sqlx's own lock would
    // be a second, session-level one.
    let mut migrator = Migrator::new(Shipped).await?;
    migrator.set_locking(false);
    migrator.run_direct(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// The shipped migrations, as a source for a migrator whose settings can
/// be changed.
#[derive(Debug)]
struct Shipped;

impl MigrationSource<'static> for Shipped {
    fn resolve(self) -> Pin<Box<dyn Future<Output = Result<Vec<Migration>, BoxDynError>> + Send>> {
        Box::pin(async { Ok(MIGRATOR.iter().cloned().collect()) })
    }
}
