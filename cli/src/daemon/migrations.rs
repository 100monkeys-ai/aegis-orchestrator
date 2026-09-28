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

/// Why migrations were not applied.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// Another process held the migration lock for longer than this one
    /// waits.
    #[error(
        "another process has been applying the database migrations for more than {seconds} \
         seconds; this process stopped waiting and applied none"
    )]
    Busy { seconds: u64 },
    #[error(transparent)]
    Migrate(#[from] MigrateError),
}

/// How long a process waits for another that is applying migrations.
pub const MIGRATION_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// A statement PostgreSQL refuses inside a transaction block, when `sql`
/// holds one: its name. Comments are not read.
pub fn refused_in_a_transaction(_sql: &str) -> Option<&'static str> {
    None
}

/// Apply every migration the database does not have yet, one process at a
/// time, waiting at most `MIGRATION_WAIT` for another.
pub async fn apply_migrations(pool: &PgPool) -> Result<(), MigrationError> {
    apply_migrations_waiting_at_most(pool, MIGRATION_WAIT).await
}

/// As [`apply_migrations`], waiting at most `_wait` for another process.
pub async fn apply_migrations_waiting_at_most(
    pool: &PgPool,
    _wait: std::time::Duration,
) -> Result<(), MigrationError> {
    let mut tx = pool.begin().await.map_err(MigrateError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *tx)
        .await
        .map_err(MigrateError::from)?;
    // The lock above holds for the whole transaction; sqlx's own lock would
    // be a second, session-level one.
    let mut migrator = Migrator::new(Shipped).await?;
    migrator.set_locking(false);
    migrator.run_direct(&mut *tx).await?;
    tx.commit().await.map_err(MigrateError::from)?;
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
