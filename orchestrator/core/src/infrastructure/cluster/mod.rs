// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Cluster Infrastructure

pub mod adoption_repo;
pub mod cluster_tls;
pub mod config_repo;
pub mod enrolment_token_repo;
pub mod event_relay;
pub mod grpc_client;
pub mod grpc_server;
pub mod membership_repo;
pub mod node_envelope;
pub mod node_registry_repo;
pub mod peer_transport;
pub mod postgres_repo;
pub mod round_robin_router;

pub use adoption_repo::PgExecutionAdoptionRepository;
pub use cluster_tls::ClusterTls;
pub use config_repo::PgConfigLayerRepository;
pub use enrolment_token_repo::PgClusterEnrolmentTokenRepository;
pub use event_relay::ClusterEventRelay;
pub use grpc_client::NodeClusterClient;
pub use grpc_server::NodeClusterServiceHandler;
pub use membership_repo::{PgClusterDutyRepository, PgNodeLeaseRepository, PgPlacementRepository};
pub use node_envelope::{
    ClusterNodeKeys, EdgeDaemonKeys, EnvelopeError, NodeEnvelopeVerifier, NodeKeys,
};
pub use node_registry_repo::PgNodeRegistryRepository;
pub use peer_transport::{
    ClusterEdgeInvoker, GrpcCancelForwarder, GrpcExecutionForwarder, GrpcFleetCancelForwarder,
    GrpcPeerTransport, SelfIssuedToken,
};
pub use postgres_repo::{
    PgNodeChallengeRepository, PgNodeClusterRepository, PgStimulusIdempotencyRepository,
};
pub use round_robin_router::RoundRobinNodeRouter;
