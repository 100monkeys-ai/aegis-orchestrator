// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The data step `one_shot_tries_kept` (AEGIS ADR-005, Update of
//! 2026-10-21, E4).
//!
//! An agent's `spec.execution.mode` is honoured: `one-shot` runs one try,
//! whatever `max_retries` says, and `iterative` runs up to `max_retries`
//! (`ExecutionStrategy::tries`). Before that, `mode` was read by nothing and
//! defaulted to `one-shot`, and every stored row carries it, so a stored
//! `one-shot` cannot be told from a default. This step keeps every stored
//! agent's count of tries: once, at the daemon's start after the schema
//! migrations, every stored agent row, of any tenant and status, whose
//! `spec.execution.mode` is `one-shot` and whose `max_retries` is 2 or more
//! has its mode set to `iterative` in its `manifest_yaml` and its
//! `manifest_json`, and nothing else changes. A row at `max_retries` 1 runs
//! one try either way and is left as it is. One line is logged per row
//! changed: its id, name, version and the count it keeps.
//!
//! Each stored form is changed at its `spec.execution.mode` key alone, and
//! both are read back: a row whose two changed forms do not read as its
//! manifest with the mode `iterative` is left as it is and logged. The
//! version history in `agent_versions` is not touched.
//!
//! The step records its name in `data_steps` (migration 056) in the
//! transaction that changes the rows. A second start, or a second process
//! starting at once, finds the name and changes nothing.

use anyhow::Result;
use sqlx::PgPool;

/// The step's name in `data_steps`.
pub const STEP: &str = "one_shot_tries_kept";

/// One agent row whose mode the step set to `iterative`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub agent_id: uuid::Uuid,
    pub tenant_id: String,
    pub name: String,
    pub version: String,
    /// The `max_retries` the row keeps, now its count of tries.
    pub max_retries: u32,
}

/// Run the step, once: the rows it changed, or `None` when it had already
/// run. Every change and the record that it ran are one transaction.
pub async fn run(pool: &PgPool) -> Result<Option<Vec<Kept>>> {
    let _ = pool;
    todo!("the data step {STEP}")
}

#[cfg(test)]
#[path = "execution_defaults_tests.rs"]
mod tests;
