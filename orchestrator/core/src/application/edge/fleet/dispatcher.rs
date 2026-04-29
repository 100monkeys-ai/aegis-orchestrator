// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Drives a [`FleetCommand`] across resolved targets per its
//! [`FleetDispatchPolicy`]. Per-node results stream as [`FleetEvent`].

use prost_types::Struct;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Semaphore};

use super::registry::{FleetCommandHandle, FleetRegistry};
use crate::application::edge::dispatch_to_edge::{DispatchRequest, DispatchToEdgeService};
use crate::domain::cluster::{
    FailurePolicy, FleetCommandId, FleetDispatchPolicy, FleetMode, FleetSummary,
};
use crate::domain::edge::EdgeRouterError;
use crate::domain::shared_kernel::{NodeId, TenantId};
use crate::infrastructure::aegis_cluster_proto::SealEnvelope;

#[derive(Debug, Clone, Serialize)]
pub enum FleetEvent {
    Started {
        fleet_command_id: FleetCommandId,
        resolved: Vec<NodeId>,
        skipped: Vec<(NodeId, String)>,
    },
    NodeStarted {
        node_id: NodeId,
    },
    NodeProgress {
        node_id: NodeId,
        chunk: Vec<u8>,
        stderr: bool,
    },
    NodeCompleted {
        node_id: NodeId,
        ok: bool,
        exit_code: i32,
        error_kind: Option<String>,
        error_message: Option<String>,
    },
    Cancelled {
        reason: String,
    },
    Done {
        summary: FleetSummary,
    },
}

#[derive(Debug, Clone)]
pub struct FleetInvocation {
    pub fleet_command_id: FleetCommandId,
    pub tenant_id: TenantId,
    pub tool_name: String,
    pub args: Struct,
    pub security_context_name: String,
    pub user_seal_envelope: SealEnvelope,
    pub resolved: Vec<NodeId>,
    pub policy: FleetDispatchPolicy,
}

pub struct FleetDispatcher {
    dispatch: Arc<DispatchToEdgeService>,
    registry: FleetRegistry,
}

impl FleetDispatcher {
    pub fn new(dispatch: Arc<DispatchToEdgeService>, registry: FleetRegistry) -> Self {
        Self { dispatch, registry }
    }

    /// Spawn the fleet operation and return a stream of `FleetEvent`s.
    pub fn spawn(self: Arc<Self>, inv: FleetInvocation) -> mpsc::Receiver<FleetEvent> {
        let (tx, rx) = mpsc::channel::<FleetEvent>(64);
        let (cancel_tx, _) = broadcast::channel::<()>(8);
        let handle = FleetCommandHandle {
            fleet_command_id: inv.fleet_command_id,
            cancel_tx: cancel_tx.clone(),
            per_node_command_ids: dashmap::DashMap::new(),
        };
        self.registry.register(handle);

        let fleet_id = inv.fleet_command_id;
        let registry = self.registry.clone();
        let dispatcher = self.clone();
        tokio::spawn(async move {
            let _ = tx
                .send(FleetEvent::Started {
                    fleet_command_id: inv.fleet_command_id,
                    resolved: inv.resolved.clone(),
                    skipped: vec![],
                })
                .await;

            // require_min_targets guard.
            if let Some(min) = inv.policy.require_min_targets {
                if inv.resolved.len() < min {
                    let _ = tx
                        .send(FleetEvent::Cancelled {
                            reason: format!(
                                "require_min_targets={min} unmet ({} resolved)",
                                inv.resolved.len()
                            ),
                        })
                        .await;
                    let _ = tx
                        .send(FleetEvent::Done {
                            summary: FleetSummary {
                                ok: 0,
                                err: 0,
                                timed_out: 0,
                                skipped: inv.resolved.len(),
                            },
                        })
                        .await;
                    registry.remove(&fleet_id);
                    return;
                }
            }

            let summary = dispatcher
                .drive(inv.clone(), tx.clone(), cancel_tx.clone())
                .await;
            let _ = tx.send(FleetEvent::Done { summary }).await;
            registry.remove(&fleet_id);
        });
        rx
    }

    async fn drive(
        self: Arc<Self>,
        inv: FleetInvocation,
        tx: mpsc::Sender<FleetEvent>,
        cancel_tx: broadcast::Sender<()>,
    ) -> FleetSummary {
        let mut summary = FleetSummary {
            ok: 0,
            err: 0,
            timed_out: 0,
            skipped: 0,
        };

        match inv.policy.mode.clone() {
            FleetMode::Sequential => {
                for node_id in &inv.resolved {
                    if cancel_tx.receiver_count() > 0
                        && cancel_tx.clone().subscribe().try_recv().is_ok()
                    {
                        break;
                    }
                    let outcome = self
                        .clone()
                        .dispatch_one(&inv, *node_id, tx.clone(), cancel_tx.subscribe())
                        .await;
                    if !accumulate(&mut summary, &outcome, &inv.policy.failure_policy) {
                        let _ = cancel_tx.send(());
                        break;
                    }
                }
            }
            FleetMode::Parallel => {
                let max = inv.policy.max_concurrency.unwrap_or(inv.resolved.len());
                let sem = Arc::new(Semaphore::new(max.max(1)));
                let mut joins = Vec::new();
                for node_id in inv.resolved.iter().copied() {
                    let permit = sem.clone();
                    let inv_c = inv.clone();
                    let tx_c = tx.clone();
                    let cancel_rx = cancel_tx.subscribe();
                    let me = self.clone();
                    joins.push(tokio::spawn(async move {
                        let _p = permit.acquire_owned().await.ok()?;
                        Some(me.dispatch_one(&inv_c, node_id, tx_c, cancel_rx).await)
                    }));
                }
                for j in joins {
                    if let Ok(Some(outcome)) = j.await {
                        if !accumulate(&mut summary, &outcome, &inv.policy.failure_policy) {
                            let _ = cancel_tx.send(());
                        }
                    }
                }
            }
            FleetMode::Rolling { batch } => {
                let batch = batch.max(1);
                for chunk in inv.resolved.chunks(batch) {
                    let mut joins = Vec::new();
                    for node_id in chunk.iter().copied() {
                        let inv_c = inv.clone();
                        let tx_c = tx.clone();
                        let cancel_rx = cancel_tx.subscribe();
                        let me = self.clone();
                        joins.push(tokio::spawn(async move {
                            me.dispatch_one(&inv_c, node_id, tx_c, cancel_rx).await
                        }));
                    }
                    let mut should_stop = false;
                    for j in joins {
                        if let Ok(outcome) = j.await {
                            if !accumulate(&mut summary, &outcome, &inv.policy.failure_policy) {
                                should_stop = true;
                            }
                        }
                    }
                    if should_stop {
                        let _ = cancel_tx.send(());
                        break;
                    }
                }
            }
        }
        summary
    }

    async fn dispatch_one(
        self: Arc<Self>,
        inv: &FleetInvocation,
        node_id: NodeId,
        tx: mpsc::Sender<FleetEvent>,
        mut cancel_rx: broadcast::Receiver<()>,
    ) -> NodeOutcome {
        let _ = tx.send(FleetEvent::NodeStarted { node_id }).await;
        let req = DispatchRequest {
            node_id,
            tenant_id: inv.tenant_id.clone(),
            tool_name: inv.tool_name.clone(),
            args: inv.args.clone(),
            security_context_name: inv.security_context_name.clone(),
            user_seal_envelope: inv.user_seal_envelope.clone(),
            deadline: inv.policy.per_target_deadline,
        };
        let dispatch = self.dispatch.clone();
        let dispatch_fut = async move { dispatch.dispatch(req).await };

        let result = tokio::select! {
            r = dispatch_fut => r,
            _ = cancel_rx.recv() => Err(EdgeRouterError::EdgeDisconnected),
        };

        match result {
            Ok(res) => {
                let _ = tx
                    .send(FleetEvent::NodeCompleted {
                        node_id,
                        ok: res.ok,
                        exit_code: res.exit_code,
                        error_kind: if res.error_kind.is_empty() {
                            None
                        } else {
                            Some(res.error_kind)
                        },
                        error_message: if res.error_message.is_empty() {
                            None
                        } else {
                            Some(res.error_message)
                        },
                    })
                    .await;
                NodeOutcome {
                    ok: res.ok,
                    timed_out: false,
                }
            }
            Err(EdgeRouterError::Timeout(_)) => {
                let _ = tx
                    .send(FleetEvent::NodeCompleted {
                        node_id,
                        ok: false,
                        exit_code: -1,
                        error_kind: Some("timeout".into()),
                        error_message: None,
                    })
                    .await;
                NodeOutcome {
                    ok: false,
                    timed_out: true,
                }
            }
            Err(e) => {
                let _ = tx
                    .send(FleetEvent::NodeCompleted {
                        node_id,
                        ok: false,
                        exit_code: -1,
                        error_kind: Some("error".into()),
                        error_message: Some(e.to_string()),
                    })
                    .await;
                NodeOutcome {
                    ok: false,
                    timed_out: false,
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct NodeOutcome {
    ok: bool,
    timed_out: bool,
}

/// Returns `true` to continue, `false` if `failure_policy` says abort.
fn accumulate(s: &mut FleetSummary, o: &NodeOutcome, fp: &FailurePolicy) -> bool {
    if o.ok {
        s.ok += 1;
    } else if o.timed_out {
        s.timed_out += 1;
    } else {
        s.err += 1;
    }
    let total_failures = s.err + s.timed_out;
    match fp {
        FailurePolicy::FailFast => o.ok,
        FailurePolicy::ContinueOnError => true,
        FailurePolicy::StopAfter(n) => total_failures < *n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_fast_aborts_on_first_failure() {
        let mut s = FleetSummary {
            ok: 0,
            err: 0,
            timed_out: 0,
            skipped: 0,
        };
        let cont = accumulate(
            &mut s,
            &NodeOutcome {
                ok: false,
                timed_out: false,
            },
            &FailurePolicy::FailFast,
        );
        assert!(!cont);
        assert_eq!(s.err, 1);
    }

    #[test]
    fn continue_on_error_does_not_abort() {
        let mut s = FleetSummary {
            ok: 0,
            err: 0,
            timed_out: 0,
            skipped: 0,
        };
        let cont = accumulate(
            &mut s,
            &NodeOutcome {
                ok: false,
                timed_out: false,
            },
            &FailurePolicy::ContinueOnError,
        );
        assert!(cont);
    }

    #[test]
    fn stop_after_n_failures_halts() {
        let mut s = FleetSummary {
            ok: 0,
            err: 0,
            timed_out: 0,
            skipped: 0,
        };
        let pol = FailurePolicy::StopAfter(2);
        assert!(accumulate(
            &mut s,
            &NodeOutcome {
                ok: false,
                timed_out: false
            },
            &pol
        ));
        assert!(!accumulate(
            &mut s,
            &NodeOutcome {
                ok: false,
                timed_out: false
            },
            &pol
        ));
    }
}
