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
//!
//! A process that finds the lock held waits for it, for at most
//! [`MIGRATION_WAIT`] (five minutes), and then stops with
//! [`MigrationError::Busy`], having applied nothing. Because all the
//! migrations run in one transaction, none may hold a statement PostgreSQL
//! refuses inside one ([`refused_in_a_transaction`]).

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
    /// A migration holds a statement PostgreSQL refuses inside a
    /// transaction block, and every migration is applied inside one.
    #[error(
        "migration {version} holds {statement}, which PostgreSQL refuses inside a transaction; \
         no migration was applied"
    )]
    RefusedInATransaction {
        version: i64,
        statement: &'static str,
    },
    #[error(transparent)]
    Migrate(#[from] MigrateError),
}

/// How long a process waits for another that is applying migrations.
pub const MIGRATION_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// A statement PostgreSQL refuses inside a transaction block, when `sql`
/// holds one: its name. Comments and quoted text are not read.
///
/// Every migration is applied inside one transaction, so a migration
/// holding such a statement would fail at every start.
pub fn refused_in_a_transaction(sql: &str) -> Option<&'static str> {
    let code = without_comments_or_quotes(sql).to_ascii_lowercase();
    for statement in code.split(';') {
        let words: Vec<&str> = statement.split_whitespace().collect();
        let starts = |prefix: &[&str]| words.starts_with(prefix);
        let refused = if starts(&["vacuum"]) {
            Some("VACUUM")
        } else if starts(&["create", "database"]) || starts(&["drop", "database"]) {
            Some("CREATE or DROP DATABASE")
        } else if starts(&["create", "tablespace"]) || starts(&["drop", "tablespace"]) {
            Some("CREATE or DROP TABLESPACE")
        } else if starts(&["alter", "system"]) {
            Some("ALTER SYSTEM")
        } else if (starts(&["create"]) || starts(&["drop"]) || starts(&["reindex"]))
            && words.contains(&"concurrently")
        {
            Some("an index statement with CONCURRENTLY")
        } else if starts(&["alter", "type"])
            && words.windows(2).any(|pair| pair == ["add", "value"])
        {
            Some("ALTER TYPE ... ADD VALUE")
        } else {
            None
        };
        if refused.is_some() {
            return refused;
        }
    }
    None
}

/// `sql` with its `--` and `/* */` comments and its quoted text replaced
/// by spaces, so a word inside them is not taken for a statement.
fn without_comments_or_quotes(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '-' if chars.peek() == Some(&'-') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                out.push('\n');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            '\'' | '"' => {
                for d in chars.by_ref() {
                    if d == c {
                        break;
                    }
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Apply every migration the database does not have yet, one process at a
/// time, waiting at most `MIGRATION_WAIT` for another.
pub async fn apply_migrations(pool: &PgPool) -> Result<(), MigrationError> {
    apply_migrations_waiting_at_most(pool, MIGRATION_WAIT).await
}

/// As [`apply_migrations`], waiting at most `wait` for another process.
/// A process that stops waiting returns [`MigrationError::Busy`] and has
/// applied nothing.
pub async fn apply_migrations_waiting_at_most(
    pool: &PgPool,
    wait: std::time::Duration,
) -> Result<(), MigrationError> {
    if let Some((version, statement)) = MIGRATOR
        .iter()
        .find_map(|m| refused_in_a_transaction(&m.sql).map(|s| (m.version, s)))
    {
        return Err(MigrationError::RefusedInATransaction { version, statement });
    }
    let mut tx = pool.begin().await.map_err(MigrateError::from)?;
    // PostgreSQL's lock timeout bounds the wait for the advisory lock. It
    // is set for this transaction only, and lifted once the lock is held,
    // so the migrations' own statements are not bounded by it.
    sqlx::query(&format!(
        "SET LOCAL lock_timeout = {}",
        wait.as_millis().max(1)
    ))
    .execute(&mut *tx)
    .await
    .map_err(MigrateError::from)?;
    let locked = sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *tx)
        .await;
    match locked {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("55P03") => {
            return Err(MigrationError::Busy {
                seconds: wait.as_secs(),
            });
        }
        Err(e) => return Err(MigrateError::from(e).into()),
    }
    sqlx::query("SET LOCAL lock_timeout = 0")
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
