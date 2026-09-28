// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Applying the database migrations this binary ships.

use sqlx::migrate::{MigrateError, Migrator};
use sqlx::PgPool;

/// The migrations under `cli/migrations/`, built into the binary.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Apply every migration the database does not have yet.
pub async fn apply_migrations(pool: &PgPool) -> Result<(), MigrateError> {
    MIGRATOR.run(pool).await
}
