// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Cluster Domain Aggregate (BC-7, AGENTS.md §Infrastructure & Hosting Domain)
//!
//! Defines the `NodeCluster` aggregate root and the `NodePeer` entity that
//! together implement the **Controller-Worker cluster topology** (ADR-059).
//!
//! ## Aggregate Invariants
//!
//! - A `NodeCluster` must have a unique `controller_node_id`.
//! - `NodePeer` identifiers must be unique within a cluster.
//! - Status transitions for peers: `Active → Draining → Deregistered` or `Active → Unhealthy`.
//!
//! See ADR-059 (Multi-Node Deployment).

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub use crate::domain::shared_kernel::NodeId;

pub use crate::domain::edge::{
    EdgeCapabilities, EdgeConnectionState, EdgeDaemon, EdgeGroup, EdgeGroupId, EdgeRouterError,
    EdgeSelector, EdgeTarget, EnrollmentToken, EnrollmentTokenClaims, LabelMatch, TagMatch,
};
pub use crate::domain::edge_fleet::{
    FailurePolicy, FleetCommand, FleetCommandId, FleetDispatchPolicy, FleetExecutionResult,
    FleetMode, FleetSummary, PerNodeOutcome, SkipReason,
};
pub use crate::domain::node_config::NodeRole;
use crate::domain::shared_kernel::{ExecutionId, StimulusId, TenantId};
use base64::Engine;

/// Unique cluster identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClusterId(pub Uuid);

impl ClusterId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ClusterId {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Entities and Aggregates
// ──────────────────────────────────────────────────────────────────────────────

/// NodeCluster (Aggregate Root)
///
/// The set of `NodePeer`s known to this controller. Enforces uniqueness of `NodeId`
/// within the cluster; owns all cluster-level invariants.
pub struct NodeCluster {
    pub id: ClusterId,
    pub controller_node_id: NodeId,
    pub peers: HashMap<NodeId, NodePeer>,
    /// ADR-117: edge daemons enrolled against this controller. Tenant-bound;
    /// access filtered via [`NodeCluster::edges_for_tenant`].
    pub edges: HashMap<NodeId, EdgeDaemon>,
    pub created_at: DateTime<Utc>,
}

impl NodeCluster {
    pub fn new(controller_node_id: NodeId) -> Self {
        Self {
            id: ClusterId::new(),
            controller_node_id,
            peers: HashMap::new(),
            edges: HashMap::new(),
            created_at: Utc::now(),
        }
    }

    /// ADR-117: edges visible to the given tenant.
    pub fn edges_for_tenant(&self, tenant: &TenantId) -> Vec<&EdgeDaemon> {
        self.edges
            .values()
            .filter(|e| &e.tenant_id == tenant)
            .collect()
    }

    pub fn upsert_edge(&mut self, edge: EdgeDaemon) {
        self.edges.insert(edge.node_id, edge);
    }

    pub fn remove_edge(&mut self, node_id: &NodeId) -> Option<EdgeDaemon> {
        self.edges.remove(node_id)
    }

    pub fn register_peer(&mut self, peer: NodePeer) -> Result<(), String> {
        self.peers.insert(peer.node_id, peer);
        Ok(())
    }

    pub fn deregister_peer(&mut self, node_id: &NodeId, _reason: &str) -> Result<(), String> {
        if self.peers.remove(node_id).is_some() {
            Ok(())
        } else {
            Err("Peer not found".to_string())
        }
    }

    pub fn record_heartbeat(
        &mut self,
        node_id: &NodeId,
        _snapshot: ResourceSnapshot,
    ) -> Result<(), String> {
        if let Some(peer) = self.peers.get_mut(node_id) {
            peer.last_heartbeat_at = Utc::now();
            Ok(())
        } else {
            Err("Peer not found".to_string())
        }
    }

    pub fn mark_unhealthy(&mut self, node_id: &NodeId) -> Result<NodePeer, String> {
        if let Some(peer) = self.peers.get_mut(node_id) {
            peer.status = NodePeerStatus::Unhealthy;
            Ok(peer.clone())
        } else {
            Err("Peer not found".to_string())
        }
    }

    pub fn healthy_workers(&self) -> Vec<&NodePeer> {
        self.peers
            .values()
            .filter(|p| {
                p.status == NodePeerStatus::Active
                    && (p.role == NodeRole::Worker || p.role == NodeRole::Hybrid)
            })
            .collect()
    }
}

/// NodePeer (Entity within NodeCluster)
///
/// A registered node with mutable status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodePeer {
    pub node_id: NodeId,
    pub role: NodeRole,
    pub public_key: Vec<u8>,
    pub capabilities: NodeCapabilityAdvertisement,
    pub grpc_address: String,
    pub status: NodePeerStatus,
    pub last_heartbeat_at: DateTime<Utc>,
    pub registered_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodePeerStatus {
    Active,
    Draining,
    Unhealthy,
}

/// Cluster-level health rollup per ADR-062.
///
/// - `Healthy`: all registered peers are Active.
/// - `Degraded`: at least one peer is Unhealthy or Draining, but more than half are Active.
/// - `Critical`: half or more peers are non-Active, or zero nodes registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClusterSummaryStatus {
    Healthy,
    Degraded,
    Critical,
}

impl ClusterSummaryStatus {
    /// Compute from peer status counts.
    pub fn from_counts(active: usize, draining: usize, unhealthy: usize) -> Self {
        let total = active + draining + unhealthy;
        if total == 0 {
            return Self::Critical;
        }
        if draining == 0 && unhealthy == 0 {
            return Self::Healthy;
        }
        if active * 2 > total {
            Self::Degraded
        } else {
            Self::Critical
        }
    }
}

impl std::fmt::Display for ClusterSummaryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Degraded => write!(f, "degraded"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Value Objects
// ──────────────────────────────────────────────────────────────────────────────

/// NodeSecurityToken (Value Object)
///
/// RS256 JWT issued by controller's OpenBao Transit key.
/// Carries `node_id`, `role`, `capabilities_hash`, `iat`, `exp`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSecurityToken(pub String);

impl NodeSecurityToken {
    pub fn claims(&self) -> Result<NodeTokenClaims, String> {
        let parts: Vec<&str> = self.0.split('.').collect();
        if parts.len() != 3 {
            return Err("Invalid JWT format".to_string());
        }

        // We use manual decode here because the domain object doesn't hold the public key.
        // Verification happens at the application/infrastructure layer (e.g. gRPC interceptor).
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|e: base64::DecodeError| e.to_string())?;

        let claims: NodeTokenClaims =
            serde_json::from_slice(&decoded).map_err(|e: serde_json::Error| e.to_string())?;

        Ok(claims)
    }

    pub fn is_expired(&self) -> bool {
        match self.claims() {
            Ok(claims) => Utc::now().timestamp() >= claims.exp,
            Err(_) => true,
        }
    }

    pub fn seconds_until_expiry(&self) -> i64 {
        match self.claims() {
            Ok(claims) => {
                let now = Utc::now().timestamp();
                if claims.exp > now {
                    claims.exp - now
                } else {
                    0
                }
            }
            Err(_) => 0,
        }
    }
}

/// Claims carried inside a `NodeSecurityToken` (RS256 JWT issued at the end of
/// the AttestNode/ChallengeNode handshake).
///
/// For ADR-117 edge daemons the controller additionally embeds:
///
/// * `tid` — persistent tenant binding established at enrollment time. Every
///   request issued under this token is authoritatively scoped to this tenant
///   regardless of any caller-supplied tenant hints.
/// * `cep` — controller endpoint advertised at enrollment time. Allows the
///   daemon to detect endpoint drift between attestation and reconnection so
///   it can re-enroll instead of trusting a stale endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTokenClaims {
    #[serde(rename = "sub")]
    pub node_id: NodeId,
    pub role: NodeRole,
    #[serde(rename = "cap_hash")]
    pub capabilities_hash: String,
    pub iat: i64,
    pub exp: i64,
    /// ADR-117: persistent tenant binding for edge daemons. `None` for worker /
    /// controller / hybrid nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tid: Option<String>,
    /// ADR-117: controller endpoint advertised at edge enrollment. `None` for
    /// non-edge nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cep: Option<String>,
}

/// SealNodeEnvelope (Value Object)
///
/// Outer wrapper for all authenticated cluster RPCs after initial attestation.
/// Structurally mirrors `SealEnvelope` from ADR-035.
pub struct SealNodeEnvelope {
    pub node_security_token: NodeSecurityToken,
    /// Ed25519 signature over serialized `payload` using node's persistent keypair
    pub signature: String,
    pub payload: Bytes,
}

/// NodeCapabilityAdvertisement (Value Object)
///
/// Advertised resource profile of a worker node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeCapabilityAdvertisement {
    pub gpu_count: u32,
    pub vram_gb: u32,
    pub cpu_cores: u32,
    pub available_memory_gb: u32,
    pub supported_runtimes: Vec<String>,
    pub tags: Vec<String>,
}

impl NodeCapabilityAdvertisement {
    /// Returns true if `self` satisfies all requirements in `required`.
    pub fn satisfies(&self, required: &NodeCapabilityAdvertisement) -> bool {
        self.gpu_count >= required.gpu_count
            && self.vram_gb >= required.vram_gb
            && self.cpu_cores >= required.cpu_cores
            && self.available_memory_gb >= required.available_memory_gb
            && required
                .supported_runtimes
                .iter()
                .all(|r| self.supported_runtimes.contains(r))
            && required.tags.iter().all(|t| self.tags.contains(t))
    }

    /// SHA-256 hash of canonical JSON representation.
    pub fn hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let json = serde_json::to_string(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

/// ExecutionRoute (Value Object)
///
/// Output of `NodeRouter::select_worker`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRoute {
    pub target_node_id: NodeId,
    pub worker_grpc_address: String,
}

/// Snapshot of current node resource utilization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub cpu_utilization: f32,
    pub gpu_utilization: f32,
    pub active_executions: u32,
}

// ──────────────────────────────────────────────────────────────────────────────
// Events
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClusterEvent {
    NodeAttested {
        node_id: NodeId,
        role: NodeRole,
        attested_at: DateTime<Utc>,
    },
    NodeRegistered {
        node_id: NodeId,
        capabilities: NodeCapabilityAdvertisement,
        registered_at: DateTime<Utc>,
    },
    NodeDeregistered {
        node_id: NodeId,
        reason: String,
        deregistered_at: DateTime<Utc>,
    },
    NodeUnhealthy {
        node_id: NodeId,
        last_seen: DateTime<Utc>,
        marked_at: DateTime<Utc>,
    },
    ExecutionForwarded {
        execution_id: ExecutionId,
        from_node_id: NodeId,
        to_node_id: NodeId,
        tenant_id: TenantId,
        forwarded_at: DateTime<Utc>,
    },
    ClusterConfigPushed {
        node_id: NodeId,
        config_version: String, // SHA-256 hash of config delta
        pushed_at: DateTime<Utc>,
    },
    // ── Edge Mode (ADR-117) ────────────────────────────────────────────────
    EdgeEnrolled {
        node_id: NodeId,
        tenant_id: TenantId,
        enrolled_at: DateTime<Utc>,
    },
    EdgeConnected {
        node_id: NodeId,
        tenant_id: TenantId,
        stream_id: String,
        connected_at: DateTime<Utc>,
    },
    EdgeDisconnected {
        node_id: NodeId,
        disconnected_at: DateTime<Utc>,
    },
    EdgeCommandDispatched {
        node_id: NodeId,
        command_id: Uuid,
        tool_name: String,
        dispatched_at: DateTime<Utc>,
    },
    EdgeRevoked {
        node_id: NodeId,
        revoked_at: DateTime<Utc>,
    },
    EdgeTagsChanged {
        node_id: NodeId,
        tags: Vec<String>,
        changed_at: DateTime<Utc>,
    },
    EdgeGroupCreated {
        group_id: EdgeGroupId,
        tenant_id: TenantId,
        name: String,
        created_at: DateTime<Utc>,
    },
    EdgeGroupUpdated {
        group_id: EdgeGroupId,
        updated_at: DateTime<Utc>,
    },
    EdgeGroupDeleted {
        group_id: EdgeGroupId,
        deleted_at: DateTime<Utc>,
    },
    FleetCommandStarted {
        fleet_command_id: FleetCommandId,
        tenant_id: TenantId,
        tool_name: String,
        targets: Vec<NodeId>,
        started_at: DateTime<Utc>,
    },
    FleetCommandCompleted {
        fleet_command_id: FleetCommandId,
        ok: usize,
        err: usize,
        completed_at: DateTime<Utc>,
    },
    FleetCommandCancelled {
        fleet_command_id: FleetCommandId,
        cancelled_at: DateTime<Utc>,
    },
}

// ──────────────────────────────────────────────────────────────────────────────
// Domain Services
// ──────────────────────────────────────────────────────────────────────────────

/// Routes execution requests to the best available worker.
pub trait NodeRouter: Send + Sync {
    fn select_worker(
        &self,
        required: &NodeCapabilityAdvertisement,
        cluster: &NodeCluster,
    ) -> Result<ExecutionRoute, NodeRouterError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeRouterError {
    NoHealthyWorkers,
    NoCapableWorkers {
        required: NodeCapabilityAdvertisement,
    },
    ClusterEmpty,
}

impl std::fmt::Display for NodeRouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeRouterError::NoHealthyWorkers => write!(f, "No healthy workers available"),
            NodeRouterError::NoCapableWorkers { .. } => {
                write!(f, "No workers found with required capabilities")
            }
            NodeRouterError::ClusterEmpty => write!(f, "Cluster is empty"),
        }
    }
}

impl std::error::Error for NodeRouterError {}

// ──────────────────────────────────────────────────────────────────────────────
// Repository Traits
// ──────────────────────────────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait NodeClusterRepository: Send + Sync {
    async fn upsert_peer(&self, peer: &NodePeer) -> anyhow::Result<()>;
    async fn find_peer(&self, node_id: &NodeId) -> anyhow::Result<Option<NodePeer>>;
    async fn list_peers_by_status(&self, status: NodePeerStatus) -> anyhow::Result<Vec<NodePeer>>;
    async fn record_heartbeat(
        &self,
        node_id: &NodeId,
        snapshot: ResourceSnapshot,
    ) -> anyhow::Result<()>;
    async fn mark_unhealthy(&self, node_id: &NodeId) -> anyhow::Result<()>;
    /// Transition a node to Draining status without marking it unhealthy.
    async fn start_drain(&self, node_id: &NodeId) -> anyhow::Result<()>;
    async fn deregister(&self, node_id: &NodeId, reason: &str) -> anyhow::Result<()>;
    async fn get_config_version(&self, node_id: &NodeId) -> anyhow::Result<Option<String>>;
    async fn record_config_version(&self, node_id: &NodeId, hash: &str) -> anyhow::Result<()>;
    async fn list_all_peers(&self) -> anyhow::Result<Vec<NodePeer>>;
    async fn count_by_status(&self) -> anyhow::Result<HashMap<NodePeerStatus, usize>>;
}

/// Repository for node configuration and registry assignment (ADR-061).
///
/// Distinct from [`NodeClusterRepository`] which handles cluster membership
/// and health. This repository manages what config/runtime each node should run.
#[async_trait::async_trait]
pub trait NodeRegistryRepository: Send + Sync {
    /// Find a registered node by ID.
    async fn find_registered_node(
        &self,
        node_id: &NodeId,
    ) -> anyhow::Result<Option<RegisteredNode>>;
    /// List all registered nodes.
    async fn list_registered_nodes(&self) -> anyhow::Result<Vec<RegisteredNode>>;
    /// Insert or update a registered node record.
    async fn upsert_registered_node(&self, node: &RegisteredNode) -> anyhow::Result<()>;
    /// Assign a config layer to a node.
    async fn assign_config(&self, assignment: &NodeConfigAssignment) -> anyhow::Result<()>;
    /// Get all config assignments for a node.
    async fn get_config_assignments(
        &self,
        node_id: &NodeId,
    ) -> anyhow::Result<Vec<NodeConfigAssignment>>;
    /// Assign a runtime registry to a node.
    async fn assign_runtime_registry(
        &self,
        assignment: &RuntimeRegistryAssignment,
    ) -> anyhow::Result<()>;
    /// Get all runtime registry assignments for a node.
    async fn get_runtime_registry_assignments(
        &self,
        node_id: &NodeId,
    ) -> anyhow::Result<Vec<RuntimeRegistryAssignment>>;
}

/// Multi-node stimulus idempotency store (fulfills ADR-021 Phase 2 deferral).
#[async_trait::async_trait]
pub trait StimulusIdempotencyRepository: Send + Sync {
    /// Returns true if the stimulus ID is newly seen (successfully inserted).
    async fn check_and_insert(&self, id: &StimulusId) -> anyhow::Result<bool>;
}

/// NodeChallenge (Entity)
///
/// Temporary challenge issued to a node during attestation handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeChallenge {
    pub challenge_id: Uuid,
    pub node_id: NodeId,
    pub nonce: Vec<u8>,
    pub public_key: Vec<u8>,
    pub role: NodeRole,
    pub capabilities: NodeCapabilityAdvertisement,
    pub grpc_address: String,
    pub created_at: DateTime<Utc>,
}

/// Challenge TTL in seconds per ADR-059 §4.1 attestation handshake (5 minutes).
const CHALLENGE_TTL_SECONDS: i64 = 300;

impl NodeChallenge {
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.created_at + chrono::Duration::seconds(CHALLENGE_TTL_SECONDS)
    }
}

#[async_trait::async_trait]
pub trait NodeChallengeRepository: Send + Sync {
    async fn save_challenge(&self, challenge: &NodeChallenge) -> anyhow::Result<()>;
    async fn get_challenge(&self, challenge_id: &Uuid) -> anyhow::Result<Option<NodeChallenge>>;
    async fn delete_challenge(&self, challenge_id: &Uuid) -> anyhow::Result<()>;
}

// ──────────────────────────────────────────────────────────────────────────────
// Cluster Enrolment Tokens (security audit 002 §4.9)
// ──────────────────────────────────────────────────────────────────────────────
//
// Cluster admission gate for non-edge worker / controller / hybrid nodes.
//
// A `ClusterEnrolmentToken` is a single-use, opaque credential issued out of
// band by the controller's admin path (separate from the `AttestNode` RPC).
// Each token is bound to a specific `NodeId` at issue time. The
// `AttestNodeUseCase` validates the token and atomically marks it redeemed
// before accepting the supplied public key. This prevents an unauthenticated
// caller with network reach from attesting an arbitrary self-generated key.

/// Errors raised while validating a `ClusterEnrolmentToken`.
#[derive(Debug, thiserror::Error)]
pub enum ClusterEnrolmentTokenError {
    /// Token format malformed (expected `<token_id>.<secret_b64>`).
    #[error("malformed enrolment token")]
    Malformed,
    /// Token does not exist or has already been redeemed.
    #[error("enrolment token not found or already redeemed")]
    NotFound,
    /// Token is bound to a different node identity than the caller claimed.
    #[error("enrolment token node_id mismatch: bound to {bound:?}, presented for {presented:?}")]
    NodeIdMismatch { bound: NodeId, presented: NodeId },
    /// Token expiry exceeded.
    #[error("enrolment token expired at {0}")]
    Expired(DateTime<Utc>),
    /// Repository / transport error.
    #[error("enrolment token repository error: {0}")]
    Other(#[source] anyhow::Error),
}

/// Repository for redeeming cluster enrolment tokens. Redemption is single-use
/// and atomic — implementations MUST guarantee that a token can only be
/// redeemed once even under concurrent attest calls.
#[async_trait::async_trait]
pub trait ClusterEnrolmentTokenRepository: Send + Sync {
    /// Validate and redeem a token in one atomic step. The token format is
    /// `<token_id>.<secret_b64>`. Implementations look up the row by
    /// `token_id`, verify the supplied secret matches (constant-time), check
    /// expiry, and mark the row redeemed. If the row is already redeemed,
    /// returns `NotFound`. Returns the bound `NodeId` on success.
    async fn redeem(
        &self,
        token: &str,
        presented_node_id: &NodeId,
    ) -> Result<NodeId, ClusterEnrolmentTokenError>;
}

// ──────────────────────────────────────────────────────────────────────────────
// Hierarchical Configuration (ADR-060)
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigScope {
    Global,
    Tenant,
    Node,
}

impl std::fmt::Display for ConfigScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => write!(f, "global"),
            Self::Tenant => write!(f, "tenant"),
            Self::Node => write!(f, "node"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigType {
    AegisConfig,
    RuntimeRegistry,
}

impl std::fmt::Display for ConfigType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AegisConfig => write!(f, "aegis-config"),
            Self::RuntimeRegistry => write!(f, "runtime-registry"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub scope: ConfigScope,
    pub scope_key: String,
    pub config_type: ConfigType,
    pub payload: serde_json::Value,
    pub version: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedConfig {
    pub payload: serde_json::Value,
    pub version: String,
}

/// Repository for hierarchical configuration layers
#[async_trait::async_trait]
pub trait ConfigLayerRepository: Send + Sync {
    /// Get a specific config layer
    async fn get_layer(
        &self,
        scope: &ConfigScope,
        scope_key: &str,
        config_type: &ConfigType,
    ) -> anyhow::Result<Option<ConfigSnapshot>>;

    /// Upsert a config layer (compute version hash automatically)
    async fn upsert_layer(
        &self,
        scope: &ConfigScope,
        scope_key: &str,
        config_type: &ConfigType,
        payload: serde_json::Value,
    ) -> anyhow::Result<ConfigSnapshot>;

    /// Get merged config for a node: global < tenant < node precedence
    async fn get_merged_config(
        &self,
        node_id: &NodeId,
        tenant_id: Option<&str>,
        config_type: &ConfigType,
    ) -> anyhow::Result<MergedConfig>;

    /// List all layers for a given config type
    async fn list_layers(&self, config_type: &ConfigType) -> anyhow::Result<Vec<ConfigSnapshot>>;

    /// Delete a specific config layer. Returns true if a row was deleted.
    async fn delete_layer(
        &self,
        scope: &ConfigScope,
        scope_key: &str,
        config_type: &ConfigType,
    ) -> anyhow::Result<bool>;
}

// === Node Registry (ADR-061) ===

/// Registry lifecycle status for a `RegisteredNode`.
///
/// Distinct from [`NodePeerStatus`] which tracks transient runtime health.
/// `RegistryStatus` tracks the operator-managed lifecycle of a node in the
/// durable registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistryStatus {
    Pending,
    Active,
    Decommissioned,
}

/// Domain events emitted by [`RegisteredNode`] lifecycle transitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeRegistryEvent {
    NodeActivated {
        node_id: NodeId,
        activated_at: DateTime<Utc>,
    },
    NodeDecommissioned {
        node_id: NodeId,
        decommissioned_at: DateTime<Utc>,
    },
}

/// Aggregate root for the durable node registry (ADR-061).
///
/// Separates operator-managed registration state from transient cluster peer
/// health tracked by [`NodePeer`] / [`NodeCluster`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredNode {
    pub node_id: NodeId,
    pub hostname: String,
    pub role: NodeRole,
    pub registry_status: RegistryStatus,
    pub software_version: String,
    pub metadata: HashMap<String, String>,
    pub config_version: Option<String>,
    pub registered_at: DateTime<Utc>,
    pub decommissioned_at: Option<DateTime<Utc>>,
}

impl RegisteredNode {
    /// Create from an existing NodePeer with additional registry metadata
    pub fn from_peer(
        peer: &NodePeer,
        hostname: String,
        software_version: String,
        metadata: HashMap<String, String>,
        config_version: Option<String>,
    ) -> Self {
        Self {
            node_id: peer.node_id,
            hostname,
            role: peer.role,
            registry_status: RegistryStatus::Pending,
            software_version,
            metadata,
            config_version,
            registered_at: peer.registered_at,
            decommissioned_at: None,
        }
    }

    /// Transition from Pending → Active.
    pub fn activate(&mut self) -> Result<NodeRegistryEvent, anyhow::Error> {
        if self.registry_status != RegistryStatus::Pending {
            return Err(anyhow::anyhow!(
                "Cannot activate node in {:?} state",
                self.registry_status
            ));
        }
        self.registry_status = RegistryStatus::Active;
        Ok(NodeRegistryEvent::NodeActivated {
            node_id: self.node_id,
            activated_at: Utc::now(),
        })
    }

    /// Transition to Decommissioned (from Pending or Active).
    pub fn decommission(&mut self) -> Result<NodeRegistryEvent, anyhow::Error> {
        if self.registry_status == RegistryStatus::Decommissioned {
            return Err(anyhow::anyhow!("Node already decommissioned"));
        }
        self.registry_status = RegistryStatus::Decommissioned;
        self.decommissioned_at = Some(Utc::now());
        Ok(NodeRegistryEvent::NodeDecommissioned {
            node_id: self.node_id,
            decommissioned_at: Utc::now(),
        })
    }

    /// Check if node is active in the registry and eligible for work routing
    pub fn is_active(&self) -> bool {
        self.registry_status == RegistryStatus::Active
    }
}

/// Explicit assignment of a configuration layer to a node (ADR-061).
///
/// Separates the "is this node alive?" question (NodeClusterRepository) from
/// the "what config should this node run?" question (NodeRegistryRepository).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfigAssignment {
    pub node_id: NodeId,
    pub config_type: ConfigType,
    pub scope: ConfigScope,
    pub assigned_at: DateTime<Utc>,
}

/// Assignment of a runtime registry to a node (ADR-061).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRegistryAssignment {
    pub node_id: NodeId,
    pub registry_name: String,
    pub assigned_at: DateTime<Utc>,
}

/// Validates that a merged effective configuration contains all required fields
/// before a node begins accepting work (ADR-060 §4).
pub struct EffectiveConfigValidator;

impl EffectiveConfigValidator {
    /// Validate that the merged config has all required top-level sections.
    /// Returns the list of missing required fields, or Ok(()) if valid.
    pub fn validate(merged: &MergedConfig) -> Result<(), Vec<String>> {
        let required_fields = ["runtime", "storage", "llm"];
        let mut missing = Vec::new();
        if let Some(obj) = merged.payload.as_object() {
            for field in &required_fields {
                if !obj.contains_key(*field) {
                    missing.push(field.to_string());
                }
            }
        } else {
            missing.push("root object (expected JSON object)".to_string());
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effective_config_validator_rejects_missing_fields() {
        let merged = MergedConfig {
            payload: serde_json::json!({
                "storage": { "backend": "seaweedfs" }
            }),
            version: "v1".to_string(),
        };
        let result = EffectiveConfigValidator::validate(&merged);
        assert!(result.is_err());
        let missing = result.unwrap_err();
        assert!(missing.contains(&"runtime".to_string()));
        assert!(missing.contains(&"llm".to_string()));
        assert!(!missing.contains(&"storage".to_string()));
    }

    #[test]
    fn test_effective_config_validator_accepts_complete_config() {
        let merged = MergedConfig {
            payload: serde_json::json!({
                "runtime": { "default_isolation": "docker" },
                "storage": { "backend": "seaweedfs" },
                "llm": { "provider": "openai" }
            }),
            version: "v1".to_string(),
        };
        assert!(EffectiveConfigValidator::validate(&merged).is_ok());
    }

    #[test]
    fn test_effective_config_validator_rejects_non_object() {
        let merged = MergedConfig {
            payload: serde_json::json!("not an object"),
            version: "v1".to_string(),
        };
        let result = EffectiveConfigValidator::validate(&merged);
        assert!(result.is_err());
        let missing = result.unwrap_err();
        assert!(missing[0].contains("root object"));
    }

    /// Regression test: an empty `{}` DB payload (no layers stored yet) must be
    /// detected as "no overlay" so the validator is never called on it.  Before
    /// the fix, `EffectiveConfigValidator::validate` was invoked on the raw DB
    /// result BEFORE `apply_merged_overlay`, causing fresh deployments to fail
    /// with "missing required fields: [runtime, storage, llm]".
    ///
    /// This test confirms:
    /// 1. An empty-object `MergedConfig` is correctly identified as having no
    ///    DB overlay (the `is_empty()` guard that skips validation).
    /// 2. The validator would indeed reject that empty payload — proving it MUST
    ///    NOT be called before the overlay is applied.
    #[test]
    fn test_empty_db_payload_is_detected_as_no_overlay() {
        let empty_db_result = MergedConfig {
            payload: serde_json::json!({}),
            version: "da39a3ee".to_string(), // SHA1 of empty string — what PgConfigLayerRepository returns
        };

        // Guard that the startup code uses to skip validation on empty payloads.
        let has_db_overlay = empty_db_result
            .payload
            .as_object()
            .map(|o| !o.is_empty())
            .unwrap_or(false);
        assert!(
            !has_db_overlay,
            "empty DB payload must be recognised as 'no overlay'"
        );

        // Confirm the validator would have rejected this, proving the old code was wrong.
        let validation_result = EffectiveConfigValidator::validate(&empty_db_result);
        assert!(
            validation_result.is_err(),
            "validator must reject an empty payload — it should never be called on one"
        );
    }

    /// Build a minimal `RegisteredNode` in Pending state for testing.
    fn make_pending_node() -> RegisteredNode {
        let peer = NodePeer {
            node_id: NodeId(Uuid::new_v4()),
            role: NodeRole::Worker,
            public_key: vec![],
            capabilities: NodeCapabilityAdvertisement::default(),
            grpc_address: "http://localhost:9090".to_string(),
            status: NodePeerStatus::Active,
            last_heartbeat_at: Utc::now(),
            registered_at: Utc::now(),
        };
        RegisteredNode::from_peer(&peer, "host-1".into(), "0.1.0".into(), HashMap::new(), None)
    }

    #[test]
    fn registered_node_lifecycle_pending_to_active_to_decommissioned() {
        let mut node = make_pending_node();
        assert_eq!(node.registry_status, RegistryStatus::Pending);
        assert!(!node.is_active());

        // Pending → Active
        let event = node.activate().expect("activate should succeed");
        assert_eq!(node.registry_status, RegistryStatus::Active);
        assert!(node.is_active());
        assert!(matches!(event, NodeRegistryEvent::NodeActivated { .. }));

        // Active → Decommissioned
        let event = node.decommission().expect("decommission should succeed");
        assert_eq!(node.registry_status, RegistryStatus::Decommissioned);
        assert!(!node.is_active());
        assert!(node.decommissioned_at.is_some());
        assert!(matches!(
            event,
            NodeRegistryEvent::NodeDecommissioned { .. }
        ));
    }

    #[test]
    fn activate_from_non_pending_returns_error() {
        let mut node = make_pending_node();
        node.activate().unwrap();
        // Active → activate again should fail
        let err = node.activate().unwrap_err();
        assert!(
            err.to_string().contains("Cannot activate"),
            "Expected 'Cannot activate' error, got: {err}"
        );
    }

    #[test]
    fn edges_for_tenant_filters_by_tenant() {
        let mut cluster = NodeCluster::new(NodeId::new());
        let tenant_a = TenantId::new("tenant-a").unwrap();
        let tenant_b = TenantId::new("tenant-b").unwrap();

        let mk = |tid: TenantId| EdgeDaemon {
            node_id: NodeId::new(),
            tenant_id: tid,
            public_key: vec![],
            capabilities: EdgeCapabilities::default(),
            status: NodePeerStatus::Active,
            connection: EdgeConnectionState::Disconnected { since: Utc::now() },
            last_heartbeat_at: None,
            enrolled_at: Utc::now(),
        };
        cluster.upsert_edge(mk(tenant_a.clone()));
        cluster.upsert_edge(mk(tenant_a.clone()));
        cluster.upsert_edge(mk(tenant_b.clone()));

        assert_eq!(cluster.edges_for_tenant(&tenant_a).len(), 2);
        assert_eq!(cluster.edges_for_tenant(&tenant_b).len(), 1);
    }

    #[test]
    fn decommission_already_decommissioned_returns_error() {
        let mut node = make_pending_node();
        node.decommission().unwrap();
        // Decommissioned → decommission again should fail
        let err = node.decommission().unwrap_err();
        assert!(
            err.to_string().contains("already decommissioned"),
            "Expected 'already decommissioned' error, got: {err}"
        );
    }
}
