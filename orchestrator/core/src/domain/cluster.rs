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

use crate::domain::execution::ExecutionId;
pub use crate::domain::node_config::NodeRole;
use crate::domain::stimulus::StimulusId;
use base64::Engine;
// ... existing code ...
use crate::domain::volume::TenantId;

// ──────────────────────────────────────────────────────────────────────────────
// Identifiers
// ──────────────────────────────────────────────────────────────────────────────

/// Unique stable node identifier (UUIDv4 recommended)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub Uuid);

impl NodeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

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
    pub created_at: DateTime<Utc>,
}

impl NodeCluster {
    pub fn new(controller_node_id: NodeId) -> Self {
        Self {
            id: ClusterId::new(),
            controller_node_id,
            peers: HashMap::new(),
            created_at: Utc::now(),
        }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTokenClaims {
    #[serde(rename = "sub")]
    pub node_id: NodeId,
    pub role: NodeRole,
    #[serde(rename = "cap_hash")]
    pub capabilities_hash: String,
    pub iat: i64,
    pub exp: i64,
}

/// SmcpNodeEnvelope (Value Object)
///
/// Outer wrapper for all authenticated cluster RPCs after initial attestation.
/// Structurally mirrors `SmcpEnvelope` from ADR-035.
pub struct SmcpNodeEnvelope {
    pub node_security_token: NodeSecurityToken,
    /// Ed25519 signature over serialized `inner_payload` using node's persistent keypair
    pub signature: String,
    pub inner_payload: Bytes,
}

/// NodeCapabilityAdvertisement (Value Object)
///
/// Advertised resource profile of a worker node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    async fn deregister(&self, node_id: &NodeId, reason: &str) -> anyhow::Result<()>;
    async fn get_config_version(&self, node_id: &NodeId) -> anyhow::Result<Option<String>>;
    async fn record_config_version(&self, node_id: &NodeId, hash: &str) -> anyhow::Result<()>;
    async fn list_all_peers(&self) -> anyhow::Result<Vec<NodePeer>>;
    async fn count_by_status(&self) -> anyhow::Result<HashMap<NodePeerStatus, usize>>;
    async fn find_registered_node(
        &self,
        node_id: &NodeId,
    ) -> anyhow::Result<Option<RegisteredNode>>;
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

impl NodeChallenge {
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.created_at + chrono::Duration::seconds(60)
    }
}

#[async_trait::async_trait]
pub trait NodeChallengeRepository: Send + Sync {
    async fn save_challenge(&self, challenge: &NodeChallenge) -> anyhow::Result<()>;
    async fn get_challenge(&self, challenge_id: &Uuid) -> anyhow::Result<Option<NodeChallenge>>;
    async fn delete_challenge(&self, challenge_id: &Uuid) -> anyhow::Result<()>;
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

#[derive(Debug, Clone)]
pub struct ConfigSnapshot {
    pub scope: ConfigScope,
    pub scope_key: String,
    pub config_type: ConfigType,
    pub payload: serde_json::Value,
    pub version: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
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
}

// === Node Registry (ADR-061) ===

/// Enriched node record for configuration assignment and registry queries.
/// Extends NodePeer with additional metadata for operational management.
#[derive(Debug, Clone)]
pub struct RegisteredNode {
    pub node_id: NodeId,
    pub hostname: String,
    pub role: NodeRole,
    pub status: NodePeerStatus,
    pub capabilities: NodeCapabilityAdvertisement,
    pub grpc_address: String,
    pub software_version: String,
    pub metadata: HashMap<String, String>,
    pub config_version: Option<String>,
    pub last_heartbeat_at: DateTime<Utc>,
    pub registered_at: DateTime<Utc>,
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
            status: peer.status,
            capabilities: peer.capabilities.clone(),
            grpc_address: peer.grpc_address.clone(),
            software_version,
            metadata,
            config_version,
            last_heartbeat_at: peer.last_heartbeat_at,
            registered_at: peer.registered_at,
        }
    }

    /// Check if node is healthy and eligible for work routing
    pub fn is_active(&self) -> bool {
        self.status == NodePeerStatus::Active
    }

    /// Check if node is intentionally excluded from new work
    pub fn is_draining(&self) -> bool {
        self.status == NodePeerStatus::Draining
    }
}
