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

use aegis_orchestrator_core::domain::agent::{AgentManifest, ExecutionMode};
use anyhow::{Context, Result};
use sqlx::{PgPool, Row};
use tracing::{error, info, warn};

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

/// One row's two stored forms with the mode set to `iterative`.
#[derive(Debug, Clone, PartialEq)]
pub struct KeptForms {
    pub manifest_json: serde_json::Value,
    pub manifest_yaml: String,
    /// The `max_retries` the row keeps.
    pub max_retries: u32,
}

/// The two stored forms of a row the step changes, or `Ok(None)` for a row
/// it does not change (no `spec.execution`, `iterative`, or `one-shot` at
/// `max_retries` 1), or why the row is left as it is.
pub fn keep_tries(
    manifest_json: &serde_json::Value,
    manifest_yaml: &str,
) -> Result<Option<KeptForms>, String> {
    let mut expected: AgentManifest = serde_json::from_value(manifest_json.clone())
        .map_err(|e| format!("its stored manifest does not read: {e}"))?;
    let Some(execution) = expected.spec.execution.as_mut() else {
        return Ok(None);
    };
    if execution.mode != ExecutionMode::OneShot || execution.max_retries < 2 {
        return Ok(None);
    }
    let max_retries = execution.max_retries;
    execution.mode = ExecutionMode::Iterative;

    let mut changed_json = manifest_json.clone();
    let Some(mode) = changed_json.pointer_mut("/spec/execution/mode") else {
        return Err("its manifest_json has no spec.execution.mode".to_string());
    };
    *mode = serde_json::Value::String("iterative".to_string());

    let mut yaml: serde_yaml::Value = serde_yaml::from_str(manifest_yaml)
        .map_err(|e| format!("its manifest_yaml does not read: {e}"))?;
    let Some(mode) = yaml
        .get_mut("spec")
        .and_then(|spec| spec.get_mut("execution"))
        .and_then(|execution| execution.get_mut("mode"))
    else {
        return Err("its manifest_yaml has no spec.execution.mode".to_string());
    };
    *mode = serde_yaml::Value::String("iterative".to_string());
    let changed_yaml = serde_yaml::to_string(&yaml)
        .map_err(|e| format!("its manifest_yaml cannot be written: {e}"))?;

    let from_json: AgentManifest = serde_json::from_value(changed_json.clone())
        .map_err(|e| format!("its changed manifest_json does not read: {e}"))?;
    let from_yaml: AgentManifest = serde_yaml::from_str(&changed_yaml)
        .map_err(|e| format!("its changed manifest_yaml does not read: {e}"))?;
    if from_json != expected || from_yaml != expected {
        return Err("its manifest_yaml and manifest_json do not read as one manifest".to_string());
    }
    Ok(Some(KeptForms {
        manifest_json: changed_json,
        manifest_yaml: changed_yaml,
        max_retries,
    }))
}

/// Run the step, once: the rows it changed, or `None` when it had already
/// run. Every change and the record that it ran are one transaction.
pub async fn run(pool: &PgPool) -> Result<Option<Vec<Kept>>> {
    let mut tx = pool.begin().await.context("begin the step's transaction")?;
    let claimed =
        sqlx::query("INSERT INTO data_steps (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
            .bind(STEP)
            .execute(&mut *tx)
            .await
            .context("record the step in data_steps")?
            .rows_affected();
    if claimed == 0 {
        tx.rollback().await.context("end the step's transaction")?;
        return Ok(None);
    }
    let rows = sqlx::query(
        "SELECT id, tenant_id, name, version, manifest_yaml, manifest_json FROM agents \
         WHERE manifest_json->'spec'->'execution'->>'mode' = 'one-shot' \
         ORDER BY created_at, id FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await
    .context("read the stored one-shot agents")?;
    let mut kept = Vec::new();
    for row in rows {
        let agent_id: uuid::Uuid = row.try_get("id").context("agents.id")?;
        let tenant_id: String = row.try_get("tenant_id").context("agents.tenant_id")?;
        let name: String = row.try_get("name").context("agents.name")?;
        let version: String = row.try_get("version").context("agents.version")?;
        let manifest_yaml: String = row
            .try_get("manifest_yaml")
            .context("agents.manifest_yaml")?;
        let manifest_json: serde_json::Value = row
            .try_get("manifest_json")
            .context("agents.manifest_json")?;
        let forms = match keep_tries(&manifest_json, &manifest_yaml) {
            Ok(Some(forms)) => forms,
            Ok(None) => continue,
            Err(reason) => {
                warn!(
                    agent_id = %agent_id,
                    agent_name = %name,
                    version = %version,
                    "{STEP}: left agent {agent_id} {name} {version} as it is: {reason}"
                );
                continue;
            }
        };
        sqlx::query("UPDATE agents SET manifest_yaml = $2, manifest_json = $3 WHERE id = $1")
            .bind(agent_id)
            .bind(&forms.manifest_yaml)
            .bind(&forms.manifest_json)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("set the mode of agent {agent_id}"))?;
        info!(
            agent_id = %agent_id,
            tenant_id = %tenant_id,
            agent_name = %name,
            version = %version,
            max_retries = forms.max_retries,
            "{STEP}: agent {agent_id} {name} {version}: mode one-shot to iterative, \
             max_retries {} kept",
            forms.max_retries
        );
        kept.push(Kept {
            agent_id,
            tenant_id,
            name,
            version,
            max_retries: forms.max_retries,
        });
    }
    tx.commit().await.context("commit the step")?;
    Ok(Some(kept))
}

/// Run the step at the daemon's start and log what it did. A step that
/// fails is logged and the daemon starts: nothing was changed, and the next
/// start runs it again.
pub async fn run_at_start(pool: &PgPool) {
    match run(pool).await {
        Ok(Some(kept)) => info!(
            step = STEP,
            kept = kept.len(),
            "The data step {STEP} ran: {} one-shot agents kept their tries as iterative",
            kept.len()
        ),
        Ok(None) => info!(step = STEP, "The data step {STEP} has run before"),
        Err(e) => error!(
            step = STEP,
            error = %format!("{e:#}"),
            "The data step {STEP} failed and changed nothing; it runs again at the next start"
        ),
    }
}

#[cfg(test)]
#[path = "execution_defaults_tests.rs"]
mod tests;
