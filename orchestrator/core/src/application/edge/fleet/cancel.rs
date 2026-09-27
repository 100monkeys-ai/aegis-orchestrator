// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Cancel a running fleet operation. Broadcasts cancel to every per-node
//! task and additionally sends `CancelCommand` over the connection registry
//! for any node whose `command_id` is known.

use super::registry::FleetRegistry;
use crate::domain::cluster::FleetCommandId;
use crate::domain::iam::{AegisRole, IdentityKind};
use crate::domain::shared_kernel::TenantId;
use crate::infrastructure::aegis_cluster_proto::{
    edge_command::Command as OutCmd, CancelCommand, EdgeCommand,
};
use crate::infrastructure::edge::EdgeConnectionRegistry;

/// Who is asking to cancel a fleet command.
#[derive(Debug, Clone, Copy)]
pub enum FleetCancelAuthority<'a> {
    /// A caller acting in its resolved tenant: it may cancel only commands
    /// that tenant invoked.
    Tenant(&'a TenantId),
    /// An `aegis:admin` or `aegis:operator` operator: it may cancel any
    /// tenant's command (ADR-073 §3e, §12). `aegis:readonly` is never this.
    WritingOperator,
}

impl<'a> FleetCancelAuthority<'a> {
    /// The authority an authenticated caller of `kind`, acting in `tenant`,
    /// holds over fleet commands. `None` for an `aegis:readonly` operator,
    /// who reads every surface and changes nothing (ADR-073 §3e). Shared by
    /// the REST route and the `aegis.edge.fleet.cancel` SEAL tool so the two
    /// cannot diverge.
    pub fn for_identity(kind: &IdentityKind, tenant: &'a TenantId) -> Option<Self> {
        match kind {
            IdentityKind::Operator {
                aegis_role: AegisRole::Readonly,
            } => None,
            IdentityKind::Operator { .. } => Some(Self::WritingOperator),
            _ => Some(Self::Tenant(tenant)),
        }
    }
}

pub struct CancelFleetService {
    registry: FleetRegistry,
    conn_registry: EdgeConnectionRegistry,
}

impl CancelFleetService {
    pub fn new(registry: FleetRegistry, conn_registry: EdgeConnectionRegistry) -> Self {
        Self {
            registry,
            conn_registry,
        }
    }

    /// Cancel `fleet_id` on behalf of `authority`.
    ///
    /// Returns `false` both when no such command is in flight and when the
    /// caller may not cancel it, so a caller outside the owning tenant
    /// cannot tell another tenant's command from one that does not exist.
    pub async fn cancel(
        &self,
        fleet_id: FleetCommandId,
        authority: FleetCancelAuthority<'_>,
    ) -> bool {
        let Some(handle) = self.registry.get(&fleet_id) else {
            return false;
        };
        if let FleetCancelAuthority::Tenant(tenant) = authority {
            if &handle.tenant_id != tenant {
                return false;
            }
        }
        let _ = handle.cancel_tx.send(());
        for entry in handle.per_node_command_ids.iter() {
            let node_id = *entry.key();
            let cid = *entry.value();
            if let Some(tx) = self.conn_registry.get(&node_id) {
                let _ = tx
                    .send(EdgeCommand {
                        command: Some(OutCmd::Cancel(CancelCommand {
                            command_id: cid.to_string(),
                        })),
                    })
                    .await;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    //! The ownership rule every fleet-cancel surface shares: the REST route
    //! and the `aegis.edge.fleet.cancel` SEAL tool both resolve the caller
    //! through `FleetCancelAuthority::for_identity` and both call `cancel`.

    use super::*;
    use crate::application::edge::fleet::registry::{FleetCommandHandle, FleetRegistry};
    use tokio::sync::broadcast;

    fn tenant(slug: &str) -> TenantId {
        TenantId::from_realm_slug(slug).expect("tenant slug")
    }

    fn service_with_command(
        owner: &TenantId,
    ) -> (CancelFleetService, FleetCommandId, broadcast::Receiver<()>) {
        let registry = FleetRegistry::new();
        let id = FleetCommandId(uuid::Uuid::new_v4());
        let (cancel_tx, cancel_rx) = broadcast::channel(8);
        registry.register(FleetCommandHandle {
            fleet_command_id: id,
            tenant_id: owner.clone(),
            cancel_tx,
            per_node_command_ids: Default::default(),
        });
        (
            CancelFleetService::new(registry, EdgeConnectionRegistry::new()),
            id,
            cancel_rx,
        )
    }

    #[tokio::test]
    async fn fleet_cancel_authority_confines_callers_to_the_owning_tenant() {
        let owner = tenant("owner");
        let other = tenant("other");
        let mut failures = Vec::new();
        let cases: [(&str, IdentityKind, &TenantId, bool); 5] = [
            (
                "tenant user of another tenant",
                IdentityKind::TenantUser {
                    tenant_slug: "other".into(),
                },
                &other,
                false,
            ),
            (
                "service account delegated to another tenant",
                IdentityKind::ServiceAccount {
                    client_id: "svc".into(),
                },
                &other,
                false,
            ),
            (
                "aegis:readonly operator",
                IdentityKind::Operator {
                    aegis_role: AegisRole::Readonly,
                },
                &other,
                false,
            ),
            (
                "tenant user of the owning tenant",
                IdentityKind::TenantUser {
                    tenant_slug: "owner".into(),
                },
                &owner,
                true,
            ),
            (
                "aegis:operator operator",
                IdentityKind::Operator {
                    aegis_role: AegisRole::Operator,
                },
                &other,
                true,
            ),
        ];
        for (label, kind, caller_tenant, may_cancel) in &cases {
            let (svc, id, mut cancel_rx) = service_with_command(&owner);
            let cancelled = match FleetCancelAuthority::for_identity(kind, caller_tenant) {
                Some(authority) => svc.cancel(id, authority).await,
                None => false,
            };
            let signalled = cancel_rx.try_recv().is_ok();
            if cancelled != *may_cancel || signalled != *may_cancel {
                failures.push(format!(
                    "{label}: cancel returned {cancelled}, signal sent {signalled}; expected {may_cancel}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "fleet-cancel authority is not confined to the owning tenant:\n{}",
            failures.join("\n")
        );
    }
}
