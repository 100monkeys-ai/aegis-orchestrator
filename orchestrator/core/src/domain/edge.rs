// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Edge Mode Domain (ADR-117)
//!
//! Domain types for the AEGIS Edge Daemon: a user-installed daemon enrolled
//! against a tenant via a short-lived `EnrollmentToken` and reachable via a
//! long-lived `ConnectEdge` bidirectional stream.
//!
//! See ADR-117 and BC-016.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::domain::cluster::NodePeerStatus;
use crate::domain::shared_kernel::{NodeId, TenantId};

// ──────────────────────────────────────────────────────────────────────────────
// EdgeCapabilities — daemon-advertised + operator-managed view
// ──────────────────────────────────────────────────────────────────────────────

/// Capabilities advertised by an edge daemon (ADR-117 §A).
///
/// `tags` are operator-managed server-side; the daemon never originates them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeCapabilities {
    pub os: String,
    pub arch: String,
    pub local_tools: Vec<String>,
    pub mount_points: Vec<String>,
    pub custom_labels: HashMap<String, String>,
    /// Operator-managed flat classifiers. Mutated only via REST/CLI; the daemon's
    /// `CapabilityUpdate` never overwrites this field.
    pub tags: Vec<String>,
}

impl EdgeCapabilities {
    /// True iff this capability set satisfies every predicate in `selector`.
    /// AND-semantics across `tools`, `labels`, `tags`, plus `os`/`arch` exact match.
    pub fn satisfies(&self, selector: &EdgeSelector) -> bool {
        if let Some(os) = &selector.os {
            if &self.os != os {
                return false;
            }
        }
        if let Some(arch) = &selector.arch {
            if &self.arch != arch {
                return false;
            }
        }
        // tools: ALL must be present.
        for tool in &selector.tools {
            if !self.local_tools.iter().any(|t| t == tool) {
                return false;
            }
        }
        // labels: AND across LabelMatch.
        for lm in &selector.labels {
            if !lm.matches(&self.custom_labels) {
                return false;
            }
        }
        // tags: AND across TagMatch.
        for tm in &selector.tags {
            if !tm.matches(&self.tags) {
                return false;
            }
        }
        true
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// EdgeSelector / LabelMatch / TagMatch
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeSelector {
    pub os: Option<String>,
    pub arch: Option<String>,
    pub tools: Vec<String>,
    pub labels: Vec<LabelMatch>,
    pub tags: Vec<TagMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelMatch {
    Equals(String, String),
    Exists(String),
    In(String, Vec<String>),
}

impl LabelMatch {
    pub fn matches(&self, labels: &HashMap<String, String>) -> bool {
        match self {
            LabelMatch::Equals(k, v) => labels.get(k).map(|x| x == v).unwrap_or(false),
            LabelMatch::Exists(k) => labels.contains_key(k),
            LabelMatch::In(k, vs) => labels.get(k).map(|x| vs.contains(x)).unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TagMatch {
    Has(String),
    AnyOf(Vec<String>),
    AllOf(Vec<String>),
    NoneOf(Vec<String>),
}

impl TagMatch {
    pub fn matches(&self, tags: &[String]) -> bool {
        match self {
            TagMatch::Has(t) => tags.iter().any(|x| x == t),
            TagMatch::AnyOf(ts) => ts.iter().any(|t| tags.contains(t)),
            TagMatch::AllOf(ts) => ts.iter().all(|t| tags.contains(t)),
            TagMatch::NoneOf(ts) => !ts.iter().any(|t| tags.contains(t)),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// EdgeTarget / EdgeGroup
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EdgeGroupId(pub Uuid);

impl EdgeGroupId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for EdgeGroupId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EdgeGroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeTarget {
    Node(NodeId),
    Group(EdgeGroupId),
    Selector(EdgeSelector),
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeGroup {
    pub id: EdgeGroupId,
    pub tenant_id: TenantId,
    pub name: String,
    pub selector: EdgeSelector,
    pub pinned_members: Vec<NodeId>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

// ──────────────────────────────────────────────────────────────────────────────
// EdgeDaemon (sibling aggregate of NodePeer)
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeConnectionState {
    Connected {
        stream_id: String,
        since: DateTime<Utc>,
    },
    Disconnected {
        since: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeDaemon {
    pub node_id: NodeId,
    pub tenant_id: TenantId,
    pub public_key: Vec<u8>,
    pub capabilities: EdgeCapabilities,
    pub status: NodePeerStatus,
    pub connection: EdgeConnectionState,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub enrolled_at: DateTime<Utc>,
}

// ──────────────────────────────────────────────────────────────────────────────
// EnrollmentToken
// ──────────────────────────────────────────────────────────────────────────────

/// Short-lived enrollment JWT. The string is opaque outside the security
/// infrastructure; claims are exposed via [`EnrollmentTokenClaims`] after
/// verification at the application layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentToken(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentTokenClaims {
    /// Tenant id this token enrolls for.
    pub tid: String,
    /// Subject issuing the token (Keycloak `sub` of the operator).
    pub sub: String,
    /// JWT id; one-time-use, redeemed atomically server-side.
    pub jti: Uuid,
    pub exp: i64,
    pub nbf: i64,
    pub aud: String,
    pub iss: String,
    /// Controller endpoint the daemon should connect to (host:port).
    pub cep: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// EdgeRouterError
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EdgeRouterError {
    #[error("edge {node_id} unavailable")]
    EdgeUnavailable { node_id: NodeId },
    #[error("no edge matches selector")]
    NoMatchingEdges,
    #[error("edge {node_id} not owned by tenant {tenant}")]
    CrossTenantAccessDenied { node_id: NodeId, tenant: String },
    #[error("edge group {0} not found")]
    GroupNotFound(EdgeGroupId),
    #[error("dispatch deadline {0:?} exceeded")]
    Timeout(std::time::Duration),
    #[error("edge disconnected during dispatch")]
    EdgeDisconnected,
    #[error("require_min_targets={required} but only {available} matched")]
    InsufficientTargets { required: usize, available: usize },
}

// ──────────────────────────────────────────────────────────────────────────────
// Repository traits
// ──────────────────────────────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait EdgeDaemonRepository: Send + Sync {
    async fn upsert(&self, edge: &EdgeDaemon) -> anyhow::Result<()>;
    async fn get(&self, node_id: &NodeId) -> anyhow::Result<Option<EdgeDaemon>>;
    async fn list_by_tenant(&self, tenant_id: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>>;
    async fn update_status(&self, node_id: &NodeId, status: NodePeerStatus) -> anyhow::Result<()>;
    async fn update_tags(&self, node_id: &NodeId, tags: &[String]) -> anyhow::Result<()>;
    async fn update_capabilities(
        &self,
        node_id: &NodeId,
        capabilities: &EdgeCapabilities,
    ) -> anyhow::Result<()>;
    async fn delete(&self, node_id: &NodeId) -> anyhow::Result<()>;
}

#[derive(Debug, thiserror::Error)]
pub enum EnrollmentTokenError {
    #[error("token already redeemed")]
    AlreadyRedeemed,
    #[error("token not found")]
    NotFound,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait::async_trait]
pub trait EnrollmentTokenRepository: Send + Sync {
    /// Atomically record the enrollment. Returns `Err(AlreadyRedeemed)` if
    /// the JTI was previously inserted.
    async fn redeem(
        &self,
        jti: Uuid,
        tenant_id: &TenantId,
        issued_to: &str,
        exp: DateTime<Utc>,
    ) -> Result<(), EnrollmentTokenError>;
}

#[derive(Debug, thiserror::Error)]
pub enum EdgeGroupRepoError {
    #[error("group with name already exists for tenant")]
    GroupExists,
    #[error("group not found")]
    NotFound,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait::async_trait]
pub trait EdgeGroupRepository: Send + Sync {
    async fn create(&self, group: &EdgeGroup) -> Result<(), EdgeGroupRepoError>;
    async fn get(&self, id: &EdgeGroupId) -> Result<Option<EdgeGroup>, EdgeGroupRepoError>;
    async fn list_by_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<EdgeGroup>, EdgeGroupRepoError>;
    async fn update(&self, group: &EdgeGroup) -> Result<(), EdgeGroupRepoError>;
    async fn delete(&self, id: &EdgeGroupId) -> Result<(), EdgeGroupRepoError>;
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_with(
        os: &str,
        arch: &str,
        tools: &[&str],
        labels: &[(&str, &str)],
        tags: &[&str],
    ) -> EdgeCapabilities {
        EdgeCapabilities {
            os: os.to_string(),
            arch: arch.to_string(),
            local_tools: tools.iter().map(|s| s.to_string()).collect(),
            mount_points: vec![],
            custom_labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn satisfies_os_arch_tools_and() {
        let caps = caps_with("linux", "x86_64", &["docker", "kubectl"], &[], &[]);
        let sel = EdgeSelector {
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            tools: vec!["docker".into()],
            ..Default::default()
        };
        assert!(caps.satisfies(&sel));

        let sel_bad_os = EdgeSelector {
            os: Some("darwin".into()),
            ..Default::default()
        };
        assert!(!caps.satisfies(&sel_bad_os));

        let sel_missing_tool = EdgeSelector {
            tools: vec!["helm".into()],
            ..Default::default()
        };
        assert!(!caps.satisfies(&sel_missing_tool));
    }

    #[test]
    fn label_match_variants() {
        let caps = caps_with(
            "linux",
            "x86_64",
            &[],
            &[("region", "us"), ("gpu", "rtx-4090")],
            &[],
        );
        assert!(caps.satisfies(&EdgeSelector {
            labels: vec![LabelMatch::Equals("region".into(), "us".into())],
            ..Default::default()
        }));
        assert!(!caps.satisfies(&EdgeSelector {
            labels: vec![LabelMatch::Equals("region".into(), "eu".into())],
            ..Default::default()
        }));
        assert!(caps.satisfies(&EdgeSelector {
            labels: vec![LabelMatch::Exists("gpu".into())],
            ..Default::default()
        }));
        assert!(!caps.satisfies(&EdgeSelector {
            labels: vec![LabelMatch::Exists("missing".into())],
            ..Default::default()
        }));
        assert!(caps.satisfies(&EdgeSelector {
            labels: vec![LabelMatch::In(
                "region".into(),
                vec!["us".into(), "eu".into()]
            )],
            ..Default::default()
        }));
    }

    #[test]
    fn tag_match_variants() {
        let caps = caps_with("linux", "x86_64", &[], &[], &["prod", "db-host"]);
        assert!(caps.satisfies(&EdgeSelector {
            tags: vec![TagMatch::Has("prod".into())],
            ..Default::default()
        }));
        assert!(caps.satisfies(&EdgeSelector {
            tags: vec![TagMatch::AnyOf(vec!["x".into(), "prod".into()])],
            ..Default::default()
        }));
        assert!(!caps.satisfies(&EdgeSelector {
            tags: vec![TagMatch::AnyOf(vec!["x".into(), "y".into()])],
            ..Default::default()
        }));
        assert!(caps.satisfies(&EdgeSelector {
            tags: vec![TagMatch::AllOf(vec!["prod".into(), "db-host".into()])],
            ..Default::default()
        }));
        assert!(!caps.satisfies(&EdgeSelector {
            tags: vec![TagMatch::AllOf(vec!["prod".into(), "x".into()])],
            ..Default::default()
        }));
        assert!(caps.satisfies(&EdgeSelector {
            tags: vec![TagMatch::NoneOf(vec!["staging".into()])],
            ..Default::default()
        }));
        assert!(!caps.satisfies(&EdgeSelector {
            tags: vec![TagMatch::NoneOf(vec!["prod".into()])],
            ..Default::default()
        }));
    }

    #[test]
    fn satisfies_compound_and() {
        let caps = caps_with(
            "linux",
            "x86_64",
            &["docker"],
            &[("region", "us")],
            &["prod"],
        );
        let sel = EdgeSelector {
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            tools: vec!["docker".into()],
            labels: vec![LabelMatch::Equals("region".into(), "us".into())],
            tags: vec![TagMatch::Has("prod".into())],
        };
        assert!(caps.satisfies(&sel));
    }
}
