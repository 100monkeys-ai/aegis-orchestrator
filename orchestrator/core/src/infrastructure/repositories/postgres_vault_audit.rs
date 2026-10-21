// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # The vault's audit sinks (AEGIS ADR-150 V7)
//!
//! [`PostgresVaultAuditSink`] appends a [`VaultAuditRow`] to
//! `admin_audit_log` (migration 003), `before_state` null;
//! [`InMemoryVaultAuditSink`] keeps the rows in process, for tests and for a
//! daemon run without a database, and can be told to refuse every write so
//! a test can show that a value is never answered without its row.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use tokio::sync::RwLock;

use crate::domain::vault_audit::{VaultAuditRow, VaultAuditSink};

/// Appends vault audit rows to `admin_audit_log`.
pub struct PostgresVaultAuditSink {
    pool: PgPool,
}

impl PostgresVaultAuditSink {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl VaultAuditSink for PostgresVaultAuditSink {
    async fn append(&self, row: &VaultAuditRow) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO admin_audit_log (actor_id, action, target_resource, before_state, after_state) \
             VALUES ($1, $2, $3, NULL, $4)",
        )
        .bind(&row.actor_id)
        .bind(row.action.as_str())
        .bind(&row.target_resource)
        .bind(&row.after_state)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Keeps vault audit rows in process.
#[derive(Default)]
pub struct InMemoryVaultAuditSink {
    rows: RwLock<Vec<VaultAuditRow>>,
    failing: AtomicBool,
}

impl InMemoryVaultAuditSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// A sink that refuses every write.
    pub fn failing() -> Self {
        let sink = Self::default();
        sink.failing.store(true, Ordering::SeqCst);
        sink
    }

    /// The rows appended so far, in order.
    pub async fn rows(&self) -> Vec<VaultAuditRow> {
        self.rows.read().await.clone()
    }
}

#[async_trait]
impl VaultAuditSink for InMemoryVaultAuditSink {
    async fn append(&self, row: &VaultAuditRow) -> anyhow::Result<()> {
        if self.failing.load(Ordering::SeqCst) {
            anyhow::bail!("the audit store refused the write");
        }
        self.rows.write().await.push(row.clone());
        Ok(())
    }
}
