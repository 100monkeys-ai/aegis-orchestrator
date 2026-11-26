// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # The `execution-adoption` duty (ADR-152 D5.7)
//!
//! The holder of the duty ends, at each tick, the unfinished executions of
//! every node this node's prober holds dead: its lease expired by the
//! database clock **and** it failed `probe_failures_to_suspect` consecutive
//! probes ([`PeerProber::suspects`], D1.4). A node whose lease expired but
//! which still answers is DB-partitioned and is never adopted from; a node
//! whose lease is live is never adopted from either.
//!
//! Each execution is ended as failed with
//! [`OWNER_LOST_FAILURE_REASON`](crate::domain::execution::OWNER_LOST_FAILURE_REASON),
//! or as cancelled when a cancel was asked for, in one fenced statement that
//! makes this process its owner at the next epoch; its terminal events are
//! published only when that statement took it. It is not resumed: its
//! container, model call and unflushed iteration died with its node, and
//! resuming mid-iteration would replay tool side effects. Its volumes are
//! intact in SeaweedFS; a caller that retries (Temporal, D5.8) starts a new
//! run.
//!
//! D1.4 asks for failed probes "from every live peer"; probe views are not
//! shared between nodes yet, so the holder's own view stands for them. With
//! the lease condition this never adopts from a node that renews.

use std::sync::Arc;

use chrono::Utc;

use crate::application::cluster::PeerProber;
use crate::application::execution::publish_execution_failure;
use crate::domain::cluster_membership::{DutyFence, ExecutionAdoptionRepository, NodeProcess};
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::ExecutionStatus;
use crate::infrastructure::event_bus::EventBus;

pub struct ExecutionAdoption {
    prober: Arc<PeerProber>,
    repository: Arc<dyn ExecutionAdoptionRepository>,
    me: NodeProcess,
    event_bus: Arc<EventBus>,
}

impl ExecutionAdoption {
    pub fn new(
        prober: Arc<PeerProber>,
        repository: Arc<dyn ExecutionAdoptionRepository>,
        me: NodeProcess,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            prober,
            repository,
            me,
            event_bus,
        }
    }

    /// One tick: adopt what every dead peer left. Returns how many
    /// executions this tick ended.
    pub async fn run_once(&self, fence: DutyFence) -> usize {
        let mut ended = 0;
        for dead in self.prober.suspects() {
            if dead == self.me.node_id {
                continue;
            }
            let unfinished = match self.repository.unfinished_of(dead).await {
                Ok(unfinished) => unfinished,
                Err(e) => {
                    tracing::warn!(node = %dead, error = %e, "Could not list a dead node's executions; next tick retries");
                    continue;
                }
            };
            for (mut execution, cancel_requested) in unfinished {
                let failed_iteration = execution.end_owner_lost(cancel_requested);
                match self
                    .repository
                    .adopt(&execution, dead, self.me, fence)
                    .await
                {
                    Ok(true) => {
                        ended += 1;
                        metrics::counter!("aegis_cluster_executions_adopted_total").increment(1);
                        tracing::info!(
                            execution_id = %execution.id,
                            dead_node = %dead,
                            status = ?execution.status,
                            "Adopted an execution of a dead node and ended it (not resumed)"
                        );
                        self.publish_end(&execution, failed_iteration);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(execution_id = %execution.id, error = %e, "Could not adopt an execution of a dead node; next tick retries");
                    }
                }
            }
        }
        ended
    }

    fn publish_end(
        &self,
        execution: &crate::domain::execution::Execution,
        failed_iteration: Option<u8>,
    ) {
        if execution.status == ExecutionStatus::Cancelled {
            self.event_bus
                .publish_execution_event(ExecutionEvent::ExecutionCancelled {
                    execution_id: execution.id,
                    agent_id: execution.agent_id,
                    reason: Some("cancel requested; its node died before it stopped".to_string()),
                    cancelled_at: execution.ended_at.unwrap_or_else(Utc::now),
                });
        } else {
            publish_execution_failure(
                &self.event_bus,
                execution,
                failed_iteration,
                execution.error.clone().unwrap_or_default(),
            );
        }
    }
}
