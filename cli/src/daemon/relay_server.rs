// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Relay-coordinator boot path (ADR-117).
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Self-contained boot path for `cluster.role = relay-coordinator`.
//!
//! The Relay Coordinator is a specialized deployment of the orchestrator
//! binary (e.g. `relay.myzaru.com`) that brokers gRPC streams between
//! Zaru / orchestrator-core and user-installed Edge Daemons. It hosts no
//! agent execution, no FSAL/NFS/FUSE volume infrastructure, and no
//! workflow engine — it serves only `NodeClusterService` over the cluster
//! gRPC port.
//!
//! Initializing the full daemon stack on a relay is incorrect (and
//! historically panicked at storage init because `spec.storage` is absent
//! from a relay's config). This module replaces the entire post-IAM
//! boot sequence with a relay-only subset.

use anyhow::{Context, Result};
use sqlx::postgres::PgPool;
use std::sync::Arc;
use tracing::{debug, info};

use aegis_orchestrator_core::domain::cluster::NodeId;
use aegis_orchestrator_core::domain::iam::IdentityProvider;
use aegis_orchestrator_core::domain::node_config::{resolve_env_value, NodeConfigManifest};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;

/// Run the relay-coordinator boot path: initialize the cluster gRPC server
/// (NodeClusterService with edge + IssueEnrollmentToken capabilities) and
/// idle on the spawned task. Returns when SIGTERM is observed by the
/// process supervisor.
///
/// This function deliberately reuses the early-init bits already built by
/// `start_daemon` (config, db_pool, event_bus, iam_service) so that role
/// dispatch happens at exactly one branch point in the daemon entrypoint.
pub async fn run_relay_coordinator(
    config: NodeConfigManifest,
    db_pool: Option<PgPool>,
    event_bus: Arc<EventBus>,
    iam_service: Option<Arc<dyn IdentityProvider>>,
) -> Result<()> {
    info!("RelayCoordinator role: starting relay-only boot path (ADR-117)");

    // ── Secrets manager ─────────────────────────────────────────────────
    // The relay holds the OpenBao policy `transit/sign/edge-enrollment-token`
    // and uses the secret store to (a) sign edge enrollment tokens via
    // `IssueEnrollmentToken` and (b) verify node bootstrap challenges.
    let secrets_manager: Arc<
        aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager,
    > = match config
        .spec
        .secrets
        .as_ref()
        .and_then(|s| s.backend.as_ref())
    {
        Some(backend_cfg) => Arc::new(
            aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager::from_config(
                backend_cfg,
                event_bus.clone(),
            )
            .await
            .context("Failed to initialize OpenBao secrets manager for relay-coordinator")?,
        ),
        None => {
            tracing::warn!(
                    "No spec.secrets.backend configured on relay-coordinator; using in-memory secret store (development/testing only)"
                );
            Arc::new(
                    aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager::from_store(
                        Arc::new(aegis_orchestrator_core::infrastructure::secrets_manager::TestSecretStore::new()),
                        event_bus.clone(),
                    ),
                )
        }
    };

    // ── Cluster gRPC server (ADR-059, ADR-117) ──────────────────────────
    let pool = db_pool.ok_or_else(|| {
        anyhow::anyhow!(
            "RelayCoordinator role requires spec.database; the cluster repositories \
             (NodeRegistry, ChallengeRepo, EdgeDaemonRepo, EnrolmentTokenRepo) require \
             PostgreSQL persistence."
        )
    })?;

    let bind_addr = config
        .spec
        .network
        .as_ref()
        .map(|n| n.bind_address.as_str())
        .unwrap_or("0.0.0.0");

    let cluster_grpc_port = config
        .spec
        .cluster
        .as_ref()
        .map(|c| c.cluster_grpc_port)
        .unwrap_or(50056);

    let cluster_addr_str = format!("{bind_addr}:{cluster_grpc_port}");
    let cluster_addr: std::net::SocketAddr = cluster_addr_str
        .parse()
        .with_context(|| format!("Failed to parse cluster gRPC address: {cluster_addr_str}"))?;

    let controller_node_id = NodeId(
        uuid::Uuid::parse_str(&config.spec.node.id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
    );

    use aegis_orchestrator_core::application::cluster::{
        AttestNodeUseCase, ChallengeNodeUseCase, HeartbeatUseCase, PushConfigUseCase,
        RegisterNodeUseCase, RouteExecutionUseCase, SyncConfigUseCase,
    };
    use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::node_cluster_service_server::NodeClusterServiceServer;
    use aegis_orchestrator_core::infrastructure::cluster::{
        NodeClusterServiceHandler, PgClusterEnrolmentTokenRepository, PgConfigLayerRepository,
        PgNodeChallengeRepository, PgNodeClusterRepository, PgNodeRegistryRepository,
        RoundRobinNodeRouter,
    };

    let cluster_repo: Arc<dyn aegis_orchestrator_core::domain::cluster::NodeClusterRepository> =
        Arc::new(PgNodeClusterRepository::new(pool.clone()));
    let challenge_repo: Arc<dyn aegis_orchestrator_core::domain::cluster::NodeChallengeRepository> =
        Arc::new(PgNodeChallengeRepository::new(pool.clone()));
    let cluster_enrolment_repo: Arc<
        dyn aegis_orchestrator_core::domain::cluster::ClusterEnrolmentTokenRepository,
    > = Arc::new(PgClusterEnrolmentTokenRepository::new(pool.clone()));

    let attest_uc = Arc::new(AttestNodeUseCase::new(
        challenge_repo.clone(),
        cluster_enrolment_repo.clone(),
    ));
    let challenge_uc = Arc::new(ChallengeNodeUseCase::new(
        challenge_repo.clone(),
        cluster_repo.clone(),
        secrets_manager.secret_store(),
    ));
    let registry_repo: Arc<dyn aegis_orchestrator_core::domain::cluster::NodeRegistryRepository> =
        Arc::new(PgNodeRegistryRepository::new(pool.clone()));
    let register_uc = Arc::new(RegisterNodeUseCase::new(
        cluster_repo.clone(),
        registry_repo.clone(),
        controller_node_id,
    ));
    let heartbeat_uc = Arc::new(HeartbeatUseCase::new(cluster_repo.clone()));
    let router: Arc<dyn aegis_orchestrator_core::domain::cluster::NodeRouter> =
        Arc::new(RoundRobinNodeRouter::new());
    let route_uc = Arc::new(RouteExecutionUseCase::new(
        cluster_repo.clone(),
        router,
        controller_node_id,
    ));

    let config_layer_repo: Arc<
        dyn aegis_orchestrator_core::domain::cluster::ConfigLayerRepository,
    > = Arc::new(PgConfigLayerRepository::new(Arc::new(pool.clone())));
    let sync_config_uc = Arc::new(SyncConfigUseCase::new(
        config_layer_repo.clone(),
        cluster_repo.clone(),
    ));
    let push_config_uc = Arc::new(PushConfigUseCase::new(config_layer_repo));

    // ADR-117: ForwardExecution is intentionally absent on the relay — the
    // relay does not host agent execution and never dispatches forwarded
    // executions. Calling the RPC against this node returns
    // FailedPrecondition with an explanatory message.
    let mut handler = NodeClusterServiceHandler::new(
        attest_uc,
        challenge_uc,
        register_uc,
        heartbeat_uc,
        route_uc,
        None,
        sync_config_uc,
        push_config_uc,
        cluster_repo.clone(),
    );

    // ── Edge services + IssueEnrollmentToken (ADR-117) ──────────────────
    use aegis_orchestrator_core::application::edge::connect_edge::ConnectEdgeService;
    use aegis_orchestrator_core::application::edge::issue_enrollment_token::{
        IssueEnrollmentToken, EDGE_ENROLLMENT_SIGNING_KEY,
    };
    use aegis_orchestrator_core::application::edge::rotate_edge_key::RotateEdgeKeyService;
    use aegis_orchestrator_core::infrastructure::edge::{
        EdgeConnectionRegistry, PgEdgeDaemonRepository,
    };

    let edge_repo: Arc<dyn aegis_orchestrator_core::domain::edge::EdgeDaemonRepository> =
        Arc::new(PgEdgeDaemonRepository::new(pool.clone()));
    let conn_registry = EdgeConnectionRegistry::new();
    let connect_edge_service = Arc::new(ConnectEdgeService::new(
        edge_repo.clone(),
        conn_registry.clone(),
    ));
    let rotate_edge_key_service = Arc::new(RotateEdgeKeyService::with_default_ttl(
        edge_repo.clone(),
        secrets_manager.secret_store(),
        EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
    ));
    handler = handler.with_edge_services(connect_edge_service, rotate_edge_key_service);

    let issuer_url = config
        .spec
        .zaru
        .as_ref()
        .and_then(|z| resolve_env_value(&z.public_url).ok())
        .unwrap_or_else(|| "aegis-relay-coordinator".to_string());
    let cluster_public_endpoint = config
        .spec
        .cluster
        .as_ref()
        .and_then(|c| c.ingress.as_ref())
        .and_then(|i| resolve_env_value(&i.public_endpoint).ok())
        .unwrap_or_else(|| cluster_addr_str.clone());
    let local_signer = Arc::new(IssueEnrollmentToken::new(
        secrets_manager.secret_store(),
        issuer_url,
        cluster_public_endpoint,
        EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
    ));
    let cluster_grpc_auth = match (&iam_service, config.spec.grpc_auth.clone()) {
        (Some(iam), Some(cfg)) if cfg.enabled => Some(
            aegis_orchestrator_core::presentation::grpc::auth_interceptor::GrpcIamAuthInterceptor::new(
                iam.clone(),
                &cfg,
            ),
        ),
        (Some(iam), None) => Some(
            aegis_orchestrator_core::presentation::grpc::auth_interceptor::GrpcIamAuthInterceptor::new(
                iam.clone(),
                &aegis_orchestrator_core::domain::node_config::GrpcAuthConfig {
                    enabled: true,
                    exempt_methods: vec![],
                },
            ),
        ),
        _ => None,
    };
    handler = handler.with_issue_enrollment_token(local_signer, cluster_grpc_auth);

    debug!(
        address = %cluster_addr,
        "RelayCoordinator: spawning cluster gRPC server (NodeClusterService only — no RemoteStorage, no ForwardExecution)"
    );

    let server_handle = tokio::spawn(async move {
        info!(
            address = %cluster_addr,
            "RelayCoordinator gRPC server listening on {cluster_grpc_port}"
        );
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(NodeClusterServiceServer::new(handler))
            .serve(cluster_addr)
            .await
        {
            tracing::error!(error = %e, "RelayCoordinator gRPC server failed");
        }
    });

    // ADR-062: health sweeper for stale heartbeat detection (relay also
    // tracks edge daemon liveness).
    let stale_threshold = std::time::Duration::from_secs(
        config
            .spec
            .cluster
            .as_ref()
            .and_then(|c| c.stale_threshold_secs)
            .unwrap_or(90),
    );
    let sweep_interval = std::time::Duration::from_secs(
        config
            .spec
            .cluster
            .as_ref()
            .and_then(|c| c.sweep_interval_secs)
            .unwrap_or(30),
    );
    let sweeper = aegis_orchestrator_core::application::cluster::HealthSweeper::new(
        cluster_repo,
        event_bus,
        stale_threshold,
        sweep_interval,
    );
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        sweeper.run(shutdown_rx).await;
    });

    info!(
        "RelayCoordinator role: skipping HTTP listener — gRPC NodeClusterService is the only \
         ingress (ADR-117). Idling on cluster gRPC server task."
    );

    // The cluster gRPC server task owns the lifetime of the process for
    // this role. Await it; if it exits, the relay exits.
    let _ = server_handle.await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use aegis_orchestrator_core::domain::node_config::{NodeConfigManifest, NodeRole};

    /// Regression for the relay-coordinator startup crash-loop. Previously
    /// `cli/src/daemon/server.rs:631` hard-required `spec.storage` for every
    /// role and panicked when the relay's config (no storage block) was
    /// loaded. The fix dispatches the RelayCoordinator role to this
    /// self-contained boot path before storage init is reached. This test
    /// verifies the dispatch predicate: the canonical relay manifest (no
    /// `spec.storage`) parses, and the role gate that drives dispatch in
    /// `start_daemon` evaluates to true.
    #[test]
    fn relay_coordinator_dispatch_gate_matches_role_with_no_storage() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: NodeConfig
metadata:
  name: "aegis-relay-coordinator-1"
  version: "1.0.0"
spec:
  node:
    id: "aegis-relay-coordinator-1"
    type: "orchestrator"
  cluster:
    enabled: true
    role: "relay-coordinator"
    cluster_grpc_port: 50056
    ingress:
      public_endpoint: "https://relay.myzaru.com"
"#;
        let manifest: NodeConfigManifest = serde_yaml::from_str(yaml).expect("parse");
        // Replicates the dispatch predicate in `start_daemon`.
        let is_relay = matches!(
            manifest.spec.cluster.as_ref().map(|c| c.role),
            Some(NodeRole::RelayCoordinator)
        );
        assert!(
            is_relay,
            "RelayCoordinator role must trigger relay dispatch"
        );
        assert!(
            manifest.spec.storage.is_none(),
            "relay manifest must have no spec.storage block — the relay's boot \
             path skips volume/FSAL infrastructure entirely"
        );
    }

    /// Negative gate: a Controller config does NOT trigger relay dispatch.
    /// This preserves the existing storage-required safety guarantee for
    /// execution-hosting roles (Controller / Worker / Hybrid).
    #[test]
    fn controller_role_does_not_trigger_relay_dispatch() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: NodeConfig
metadata:
  name: "aegis-controller-1"
  version: "1.0.0"
spec:
  node:
    id: "aegis-controller-1"
    type: "orchestrator"
  cluster:
    enabled: true
    role: "controller"
    cluster_grpc_port: 50056
    controller:
      endpoint: "controller.example.com:50056"
"#;
        let manifest: NodeConfigManifest = serde_yaml::from_str(yaml).expect("parse");
        let is_relay = matches!(
            manifest.spec.cluster.as_ref().map(|c| c.role),
            Some(NodeRole::RelayCoordinator)
        );
        assert!(
            !is_relay,
            "Controller role must take the full daemon boot path (storage required)"
        );
    }
}
