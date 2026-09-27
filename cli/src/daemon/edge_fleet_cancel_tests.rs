// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Regression tests for `POST /v1/edge/fleet/{id}/cancel` (ADR-117 §F).
//!
//! A fleet command is cancelled only by a caller whose resolved tenant
//! invoked it, or by an operator whose role may write (`aegis:admin`,
//! `aegis:operator`; ADR-073 §3e, §12). Anyone else is answered exactly as
//! for a command that does not exist, so an id cannot be probed. Driven
//! through the edge router the daemon merges, beneath the daemon's real
//! authentication stack, with a command registered in the same in-memory
//! `FleetRegistry` the cancel service reads.

use std::sync::Arc;

use aegis_orchestrator_core::api::rest::edge::{router as edge_router, EdgeApiState};
use aegis_orchestrator_core::application::edge::dispatch_to_edge::DispatchToEdgeService;
use aegis_orchestrator_core::application::edge::fleet::{
    CancelFleetService, EdgeFleetResolver, FleetCommandHandle, FleetDispatcher, FleetRegistry,
};
use aegis_orchestrator_core::application::edge::issue_enrollment_token::{
    EnrollmentTokenIssuer, IssuedEnrollmentToken,
};
use aegis_orchestrator_core::application::edge::manage_groups::ManageGroupsService;
use aegis_orchestrator_core::application::edge::manage_tags::ManageTagsService;
use aegis_orchestrator_core::application::edge::revoke_edge::RevokeEdgeService;
use aegis_orchestrator_core::domain::cluster::NodePeerStatus;
use aegis_orchestrator_core::domain::edge::{
    EdgeCapabilities, EdgeDaemon, EdgeDaemonRepository, EdgeGroup, EdgeGroupId, EdgeGroupRepoError,
    EdgeGroupRepository,
};
use aegis_orchestrator_core::domain::edge_fleet::FleetCommandId;
use aegis_orchestrator_core::domain::iam::{AegisRole, UserIdentity};
use aegis_orchestrator_core::domain::shared_kernel::{NodeId, TenantId};
use aegis_orchestrator_core::infrastructure::edge::EdgeConnectionRegistry;
use tokio::sync::broadcast;

use crate::daemon::handlers::test_support::{
    consumer, identity_provider, operator, send, serve, tenant_user,
};

struct EmptyEdgeRepo;

#[async_trait::async_trait]
impl EdgeDaemonRepository for EmptyEdgeRepo {
    async fn upsert(&self, _edge: &EdgeDaemon) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get(&self, _node_id: &NodeId) -> anyhow::Result<Option<EdgeDaemon>> {
        Ok(None)
    }
    async fn list_by_tenant(&self, _tenant_id: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>> {
        Ok(Vec::new())
    }
    async fn list_all(&self) -> anyhow::Result<Vec<EdgeDaemon>> {
        Ok(Vec::new())
    }
    async fn update_status(
        &self,
        _node_id: &NodeId,
        _status: NodePeerStatus,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn record_heartbeat(&self, _node_id: &NodeId) -> anyhow::Result<()> {
        Ok(())
    }
    async fn update_tags(&self, _node_id: &NodeId, _tags: &[String]) -> anyhow::Result<()> {
        Ok(())
    }
    async fn update_display_name(
        &self,
        _node_id: &NodeId,
        _display_name: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn update_capabilities(
        &self,
        _node_id: &NodeId,
        _capabilities: &EdgeCapabilities,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete(&self, _node_id: &NodeId) -> anyhow::Result<()> {
        Ok(())
    }
}

struct EmptyGroupRepo;

#[async_trait::async_trait]
impl EdgeGroupRepository for EmptyGroupRepo {
    async fn create(&self, _group: &EdgeGroup) -> Result<(), EdgeGroupRepoError> {
        Ok(())
    }
    async fn get(&self, _id: &EdgeGroupId) -> Result<Option<EdgeGroup>, EdgeGroupRepoError> {
        Ok(None)
    }
    async fn list_by_tenant(
        &self,
        _tenant_id: &TenantId,
    ) -> Result<Vec<EdgeGroup>, EdgeGroupRepoError> {
        Ok(Vec::new())
    }
    async fn list_all(&self) -> Result<Vec<EdgeGroup>, EdgeGroupRepoError> {
        Ok(Vec::new())
    }
    async fn update(&self, _group: &EdgeGroup) -> Result<(), EdgeGroupRepoError> {
        Ok(())
    }
    async fn delete(&self, _id: &EdgeGroupId) -> Result<(), EdgeGroupRepoError> {
        Ok(())
    }
}

struct NoTokens;

#[async_trait::async_trait]
impl EnrollmentTokenIssuer for NoTokens {
    async fn issue(
        &self,
        _tenant_id: &TenantId,
        _issued_to_sub: &str,
        _bearer_token: Option<&str>,
    ) -> anyhow::Result<IssuedEnrollmentToken> {
        anyhow::bail!("enrollment is not exercised by these tests")
    }
}

fn edge_state(fleet_registry: FleetRegistry) -> EdgeApiState {
    let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(EmptyEdgeRepo);
    let group_repo: Arc<dyn EdgeGroupRepository> = Arc::new(EmptyGroupRepo);
    let conn_registry = EdgeConnectionRegistry::new();
    let dispatch_service = Arc::new(DispatchToEdgeService::new(
        edge_repo.clone(),
        conn_registry.clone(),
    ));
    EdgeApiState {
        issue_token: Arc::new(NoTokens),
        edge_repo: edge_repo.clone(),
        group_service: Arc::new(ManageGroupsService::new(group_repo.clone())),
        tag_service: Arc::new(ManageTagsService::new(edge_repo.clone())),
        revoke_service: Arc::new(RevokeEdgeService::new(
            edge_repo.clone(),
            conn_registry.clone(),
        )),
        resolver: Arc::new(EdgeFleetResolver::new(
            edge_repo.clone(),
            group_repo,
            conn_registry.clone(),
        )),
        fleet_dispatcher: Arc::new(FleetDispatcher::new(
            dispatch_service.clone(),
            fleet_registry.clone(),
        )),
        fleet_cancel: Arc::new(CancelFleetService::new(
            fleet_registry,
            conn_registry.clone(),
        )),
        dispatch_service,
        connection_registry: conn_registry,
    }
}

fn owner() -> UserIdentity {
    consumer("fleet-owner-sub")
}

fn owner_tenant() -> TenantId {
    TenantId::for_consumer_user("fleet-owner-sub").expect("owner tenant")
}

/// Register an in-flight fleet command owned by the owner's tenant and
/// return its id with a receiver that observes its cancel signal.
fn in_flight_command(registry: &FleetRegistry) -> (FleetCommandId, broadcast::Receiver<()>) {
    let id = FleetCommandId(uuid::Uuid::new_v4());
    let (cancel_tx, cancel_rx) = broadcast::channel(8);
    registry.register(FleetCommandHandle {
        fleet_command_id: id,
        tenant_id: owner_tenant(),
        cancel_tx,
        per_node_command_ids: Default::default(),
    });
    (id, cancel_rx)
}

/// Cancel `path_id` as `caller` (`None`: no identity, no IAM layer).
async fn cancel_as(
    registry: &FleetRegistry,
    caller: Option<&UserIdentity>,
    path_id: &str,
) -> (u16, serde_json::Value) {
    let iam = caller.map(|id| identity_provider(&[("caller", id.clone(), "")]));
    let base = serve(edge_router(edge_state(registry.clone())), iam, None).await;
    send(
        &base,
        &reqwest::Method::POST,
        &format!("/v1/edge/fleet/{path_id}/cancel"),
        &None,
        caller.map(|_| "caller"),
    )
    .await
}

#[tokio::test]
async fn fleet_cancel_refuses_callers_outside_the_owning_tenant() {
    let callers: [(&str, Option<UserIdentity>, u16); 4] = [
        (
            "consumer in another tenant",
            Some(consumer("other-sub")),
            404,
        ),
        (
            "tenant user of acme",
            Some(tenant_user("acme-sub", "acme")),
            404,
        ),
        (
            "aegis:readonly operator",
            Some(operator(AegisRole::Readonly)),
            403,
        ),
        ("request with no identity", None, 401),
    ];
    let mut failures = Vec::new();
    for (label, caller, want) in &callers {
        let registry = FleetRegistry::new();
        let (id, mut cancel_rx) = in_flight_command(&registry);
        let (status, body) = cancel_as(&registry, caller.as_ref(), &id.0.to_string()).await;
        if cancel_rx.try_recv().is_ok() {
            failures.push(format!(
                "{label} cancelled another tenant's fleet command (answered {status} {body})"
            ));
        }
        if status != *want {
            failures.push(format!("{label} answered {status} {body}; expected {want}"));
        }
        if *want == 404 {
            // Indistinguishable from an id that was never issued.
            let unknown = uuid::Uuid::new_v4().to_string();
            let (u_status, u_body) = cancel_as(&registry, caller.as_ref(), &unknown).await;
            if (u_status, &u_body) != (status, &body) {
                failures.push(format!(
                    "{label} can tell a foreign command ({status} {body}) from a missing one ({u_status} {u_body})"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "a caller outside the owning tenant reached an edge fleet command:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
async fn fleet_cancel_serves_the_owning_tenant_and_writing_operators() {
    let callers = [
        ("owning consumer", owner()),
        ("aegis:admin operator", operator(AegisRole::Admin)),
        ("aegis:operator operator", operator(AegisRole::Operator)),
    ];
    let mut failures = Vec::new();
    for (label, caller) in &callers {
        let registry = FleetRegistry::new();
        let (id, mut cancel_rx) = in_flight_command(&registry);
        let (status, body) = cancel_as(&registry, Some(caller), &id.0.to_string()).await;
        if status != 204 || cancel_rx.try_recv().is_err() {
            failures.push(format!(
                "{label} answered {status} {body}; expected 204 and the command cancelled"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "a caller entitled to cancel an edge fleet command was refused:\n{}",
        failures.join("\n")
    );
}
