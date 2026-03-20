// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Connection Pool (ADR-025)
//!
//! Wraps `sqlx::postgres::PgPool` in a thin `Database` newtype that can be
//! injected into all PostgreSQL repository implementations.
//!
//! PostgreSQL connection pool wrapper used by repository implementations when
//! persistent storage is configured.
//!
//! See ADR-025 (PostgreSQL Schema Design).

use anyhow::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};

#[derive(Clone)]
pub struct Database {
    pool: PgPool,
}

impl Database {
    pub async fn new(connection_string: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(connection_string)
            .await?;

        Ok(Self { pool })
    }

    pub fn get_pool(&self) -> &PgPool {
        &self.pool
    }
}
