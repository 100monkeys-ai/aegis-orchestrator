// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A cancel reaches the node that owns the execution (ADR-152 D5.4)
//!
//! A cancel may arrive at any node. The node that receives it records it on
//! the execution (`cancel_requested_at`) and, when another node owns the
//! run, forwards it there over the cluster's private network
//! (`CancelExecution`, a SEAL node envelope in mutual TLS), where the
//! owner's supervisor stops the container. An owner the forward does not
//! reach still reads the recorded request while the run is live and stops
//! it, so a lost or blocked forward delays a cancel, never loses it.

use async_trait::async_trait;

use crate::domain::cluster::NodeId;
use crate::domain::execution::ExecutionId;
use crate::domain::tenant::TenantId;

/// Sends a recorded cancel to the node that owns the execution.
#[async_trait]
pub trait CancelForwarder: Send + Sync {
    /// `Ok(true)` when the owner accepted it (it owns the run and told its
    /// supervisor to stop, or the run had ended).
    async fn forward(
        &self,
        owner: NodeId,
        execution_id: ExecutionId,
        tenant_id: &TenantId,
    ) -> anyhow::Result<bool>;
}

/// The owner's side: stop a run this process supervises.
#[async_trait]
pub trait OwnedExecutionCanceller: Send + Sync {
    /// `Ok(true)` when this process supervises the run and stopped it, or
    /// the run had already ended; `Ok(false)` when it does not run here.
    async fn cancel_if_owned(
        &self,
        tenant_id: &TenantId,
        execution_id: ExecutionId,
    ) -> anyhow::Result<bool>;
}
