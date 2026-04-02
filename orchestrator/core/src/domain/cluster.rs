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

/// SealNodeEnvelope (Value Object)
///
/// Outer wrapper for all authenticated cluster RPCs after initial attestation.
/// Structurally mirrors `SealEnvelope` from ADR-035.
pub struct SealNodeEnvelope {
    pub node_security_token: NodeSecurityToken,
    /// Ed25519 signature over serialized `inner_payload` using node's persistent keypair
    pub signature: String,
    pub inner_payload: Bytes,
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
    async fn find_registered_node(
        &self,
        node_id: &NodeId,
    ) -> anyhow::Result<Option<RegisteredNode>>;
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

/// Enriched node record for configuration assignment and registry queries.
/// Extends NodePeer with additional metadata for operational management.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
