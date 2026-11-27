// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §D — single-target reverse-RPC dispatch.

use prost_types::Struct;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::domain::edge::{EdgeDaemonRepository, EdgeRouterError};
use crate::domain::shared_kernel::{NodeId, TenantId};
use crate::infrastructure::aegis_cluster_proto::{
    edge_command::Command as OutCmd, CancelCommand, EdgeCommand, EdgeResult, InvokeToolCommand,
    SealEnvelope,
};
use crate::infrastructure::edge::EdgeConnectionRegistry;

/// Runs a dispatch on a stream another process holds (ADR-152 D8): the
/// member whose Relay Coordinator holds it, reached over the members'
/// mutual TLS, or this member's own Relay Coordinator.
#[async_trait::async_trait]
pub trait RemoteEdgeInvoker: Send + Sync {
    /// `from_peer`: the dispatch already crossed members; it may go to this
    /// member's own Relay Coordinator but never to another member.
    async fn invoke(
        &self,
        req: &DispatchRequest,
        from_peer: bool,
    ) -> Result<EdgeResult, EdgeRouterError>;
}

pub struct DispatchToEdgeService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    registry: EdgeConnectionRegistry,
    remote: std::sync::OnceLock<Arc<dyn RemoteEdgeInvoker>>,
}

#[derive(Debug, Clone)]
pub struct DispatchRequest {
    pub node_id: NodeId,
    pub tenant_id: TenantId,
    pub tool_name: String,
    pub args: Struct,
    pub security_context_name: String,
    pub user_seal_envelope: SealEnvelope,
    pub deadline: Duration,
}

impl DispatchToEdgeService {
    pub fn new(edge_repo: Arc<dyn EdgeDaemonRepository>, registry: EdgeConnectionRegistry) -> Self {
        Self {
            edge_repo,
            registry,
            remote: std::sync::OnceLock::new(),
        }
    }

    /// ADR-152 D8: an edge whose stream this process does not hold is
    /// reached through `remote` (set once the cluster's transport exists).
    pub fn set_remote(&self, remote: Arc<dyn RemoteEdgeInvoker>) {
        let _ = self.remote.set(remote);
    }

    pub async fn dispatch(&self, req: DispatchRequest) -> Result<EdgeResult, EdgeRouterError> {
        self.dispatch_from(req, false).await
    }

    /// A dispatch a peer forwarded here (InvokeEdgeTool): this process's
    /// streams, or this member's own Relay Coordinator, never another member.
    pub async fn dispatch_forwarded(
        &self,
        req: DispatchRequest,
    ) -> Result<EdgeResult, EdgeRouterError> {
        self.dispatch_from(req, true).await
    }

    async fn dispatch_from(
        &self,
        req: DispatchRequest,
        from_peer: bool,
    ) -> Result<EdgeResult, EdgeRouterError> {
        // Tenant-ownership check.
        let edge = match self.edge_repo.get(&req.node_id).await {
            Ok(Some(e)) => e,
            _ => {
                return Err(EdgeRouterError::EdgeUnavailable {
                    node_id: req.node_id,
                })
            }
        };
        if edge.tenant_id != req.tenant_id {
            return Err(EdgeRouterError::CrossTenantAccessDenied {
                node_id: req.node_id,
                tenant: req.tenant_id.as_str().to_string(),
            });
        }

        let Some(sender) = self.registry.get(&req.node_id) else {
            return match self.remote.get() {
                Some(remote) => remote.invoke(&req, from_peer).await,
                None => Err(EdgeRouterError::EdgeUnavailable {
                    node_id: req.node_id,
                }),
            };
        };

        let command_id = Uuid::new_v4();
        let pending = self.registry.pending().clone();
        let rx = pending.register(command_id, req.node_id);

        let cmd = EdgeCommand {
            command: Some(OutCmd::InvokeTool(InvokeToolCommand {
                command_id: command_id.to_string(),
                security_context_name: req.security_context_name.clone(),
                seal_envelope: Some(req.user_seal_envelope.clone()),
                tool_name: req.tool_name.clone(),
                args: Some(req.args.clone()),
                deadline: Some(prost_types::Duration {
                    seconds: req.deadline.as_secs() as i64,
                    nanos: 0,
                }),
            })),
        };
        if sender.send(cmd).await.is_err() {
            pending.fail(&command_id, EdgeRouterError::EdgeDisconnected);
            return Err(EdgeRouterError::EdgeDisconnected);
        }

        match tokio::time::timeout(req.deadline, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(EdgeRouterError::EdgeDisconnected),
            Err(_) => {
                // Timeout — best-effort cancel.
                let cancel = EdgeCommand {
                    command: Some(OutCmd::Cancel(CancelCommand {
                        command_id: command_id.to_string(),
                    })),
                };
                let _ = sender.send(cancel).await;
                pending.fail(&command_id, EdgeRouterError::Timeout(req.deadline));
                Err(EdgeRouterError::Timeout(req.deadline))
            }
        }
    }
}
