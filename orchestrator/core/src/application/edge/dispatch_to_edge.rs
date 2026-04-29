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

pub struct DispatchToEdgeService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    registry: EdgeConnectionRegistry,
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
        }
    }

    pub async fn dispatch(&self, req: DispatchRequest) -> Result<EdgeResult, EdgeRouterError> {
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

        let sender = self
            .registry
            .get(&req.node_id)
            .ok_or(EdgeRouterError::EdgeUnavailable {
                node_id: req.node_id,
            })?;

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
