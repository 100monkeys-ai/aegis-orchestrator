// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A run's execution token (AEGIS ADR-147 Update `execution-token-2026-10-17`)
//!
//! A run that names a tree reaches Carbon Forge only through a token of its
//! own: a child of its person's binding, minted at the run's start with
//! `tokens.mint_execution` (X2), narrowed to the run's instance, workspace,
//! tree path and lane, to the tools its workflow declares (X1) and to a day
//! (NN ADR-0197 E6). Its secret lives in the secret store keyed by the
//! execution id and nowhere else (X3); every Carbon Forge call of the run
//! carries it, and a run with none is refused. The run's terminal event
//! revokes it with `tokens.revoke_execution` and deletes its secret (X4).
//! The binding is used for the mint and the revoke only.

use std::collections::BTreeSet;

use async_trait::async_trait;

use crate::domain::execution::ExecutionContexts;
use crate::domain::run_tree::{
    RunTree, RunTreeError, CARBON_FORGE_ACTION_TOOLS, CARBON_FORGE_GROUND_TOOL,
    CARBON_FORGE_MATERIALIZE_TOOLS,
};
use crate::domain::secrets::SensitiveString;
use crate::domain::tenant::TenantId;
use crate::domain::workflow::{ActionName, ActionNeed, StateKind, Workflow};

/// The Carbon Forge tool that mints a run's execution token (X2).
pub const MINT_EXECUTION_TOOL: &str = "tokens.mint_execution";

/// The Carbon Forge tool that revokes it (X4).
pub const REVOKE_EXECUTION_TOOL: &str = "tokens.revoke_execution";

/// The life of a run's execution token: NN ADR-0197 E6's ceiling.
pub const EXECUTION_TOKEN_LIFETIME_SECONDS: u64 = 86_400;

/// The refusal of a Carbon Forge call of a run that holds no execution
/// token (X3): the binding is never its fallback.
pub const RUN_HOLDS_NO_EXECUTION_TOKEN: &str = "This run holds no execution token for Carbon Forge, so it makes no Carbon Forge call: its reach is the token minted at its start, and it holds none.";

/// A run's execution token as the secret store keeps it: its id in Carbon
/// Forge, its secret, and the Carbon Forge user it acts as (its person,
/// the owner of the run's lane, NN ADR-0193 L7). `Debug` never shows the
/// secret.
#[derive(Debug, Clone)]
pub struct ExecutionToken {
    pub token_id: String,
    pub secret: SensitiveString,
    pub user_id: String,
}

/// The Carbon Forge tools `action` calls: its row of
/// [`CARBON_FORGE_ACTION_TOOLS`], none for an action with no row.
pub fn action_tools(action: ActionName) -> &'static [&'static str] {
    CARBON_FORGE_ACTION_TOOLS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, tools)| *tools)
        .unwrap_or(&[])
}

/// Whether a run of `workflow` materializes its tree at its start: some
/// state is not an Action, or is an Action needing the tree's files (the
/// predicate of `prepare_trees`, AEGIS ADR-149 C2).
pub fn materializes(workflow: &Workflow) -> bool {
    workflow
        .spec
        .states
        .values()
        .any(|state| match &state.kind {
            StateKind::Action { action, .. } => action.needs() == ActionNeed::RunTree,
            _ => true,
        })
}

/// The run's declared Carbon Forge reach (X1): the union, over the Action
/// states `workflow` names, of the tools each calls, the materializing
/// tools when it materializes, and `cortex.ground`. Empty when the workflow
/// calls no Carbon Forge tool.
pub fn declared_reach(workflow: &Workflow) -> BTreeSet<&'static str> {
    let mut reach: BTreeSet<&'static str> = workflow
        .spec
        .states
        .values()
        .filter_map(|state| match &state.kind {
            StateKind::Action { action, .. } => Some(action_tools(*action)),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    if materializes(workflow) {
        reach.extend(CARBON_FORGE_MATERIALIZE_TOOLS.iter().copied());
    }
    if !reach.is_empty() {
        reach.insert(CARBON_FORGE_GROUND_TOOL);
    }
    reach
}

/// Where a run's execution token is minted and released: the interpreter's
/// tool invocation service, which reaches Carbon Forge through the SEAL
/// gateway with the binding the run's `contexts` chose, and keeps the
/// child's secret in the secret store.
#[async_trait]
pub trait ExecutionTokens: Send + Sync {
    /// Mint the execution token of the workflow execution `run` from its
    /// person's binding, for `tree` and `tools`, and keep its secret keyed
    /// by `run` (X2, X3). A refusal is Carbon Forge's own sentence.
    async fn mint_for_run(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        contexts: &ExecutionContexts,
        tree: &RunTree,
        tools: &[&'static str],
    ) -> Result<(), RunTreeError>;

    /// Revoke the execution token of `run`, if it holds one, and delete its
    /// secret (X4). Never fails the caller: its expiry bounds a revoke that
    /// does not arrive.
    async fn release_for_run(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        contexts: &ExecutionContexts,
    );
}
