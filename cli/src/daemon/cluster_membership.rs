// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Joining an active-active cluster at start (AEGIS ADR-152 D1, D2).
//!
//! A node with `spec.cluster.enabled`, role controller, worker or hybrid,
//! and a database joins `cluster_nodes` under a new incarnation, renews its
//! lease from then on, and runs its singleton duties under leases. Every
//! other node (clustering off, no database) runs every duty alone, as one
//! process always has.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sqlx::PgPool;

use aegis_orchestrator_core::application::cluster::{DutyLeasing, LeaseTiming, NodeMembership};
use aegis_orchestrator_core::domain::cluster::{NodeId, NodeRole};
use aegis_orchestrator_core::domain::cluster_membership::NodeJoin;
use aegis_orchestrator_core::domain::node_config::{
    resolve_env_value, ClusterConfig, NodeConfigManifest,
};
use aegis_orchestrator_core::infrastructure::cluster::{
    PgClusterDutyRepository, PgNodeLeaseRepository,
};

/// This process's membership, and what its peers' probes and its own need.
pub(crate) struct JoinedCluster {
    pub(crate) membership: Arc<NodeMembership>,
    pub(crate) leases: Arc<PgNodeLeaseRepository>,
    pub(crate) signing_key: Arc<ed25519_dalek::SigningKey>,
    pub(crate) role: NodeRole,
    pub(crate) pool: PgPool,
}

/// The cluster block, when this node is a peer that leases its duties.
pub(crate) fn leasing_cluster(config: &NodeConfigManifest) -> Option<&ClusterConfig> {
    config.spec.cluster.as_ref().filter(|c| {
        c.enabled
            && matches!(
                c.role,
                NodeRole::Controller | NodeRole::Worker | NodeRole::Hybrid
            )
    })
}

/// The address peers dial this node's NodeClusterService at: the private
/// network address when configured, else the bind address, which must then
/// be one a peer can dial.
pub(crate) fn peer_address(
    cluster: &ClusterConfig,
    bind_addr: &str,
) -> Result<(String, Option<String>)> {
    let vrack = match cluster.membership.vrack_address.as_deref() {
        Some(raw) => Some(
            resolve_env_value(raw).context("cluster.membership.vrack_address could not be read")?,
        ),
        None => None,
    };
    let host = vrack.clone().unwrap_or_else(|| bind_addr.to_string());
    if host.is_empty() || host == "0.0.0.0" || host == "::" || host == "[::]" {
        bail!(
            "cluster.membership.vrack_address is required: peers cannot dial the bind address {host:?}"
        );
    }
    Ok((format!("{host}:{}", cluster.cluster_grpc_port), vrack))
}

/// Read the node's Ed25519 keypair, a raw 32-byte seed written by
/// `aegis node init`.
pub(crate) async fn read_signing_key(cluster: &ClusterConfig) -> Result<ed25519_dalek::SigningKey> {
    let path = &cluster.node_keypair_path;
    let bytes = tokio::fs::read(path).await.with_context(|| {
        format!(
            "a cluster node needs its keypair at {} (cluster.node_keypair_path; `aegis node init` writes it)",
            path.display()
        )
    })?;
    let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!(
            "{} holds {} bytes; a node keypair is a 32-byte Ed25519 seed",
            path.display(),
            bytes.len()
        )
    })?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&seed))
}

/// Join when this node is a leasing peer with a database; `None` otherwise.
pub(crate) async fn join_cluster(
    config: &NodeConfigManifest,
    pool: Option<&PgPool>,
    bind_addr: &str,
) -> Result<Option<JoinedCluster>> {
    let Some(cluster) = leasing_cluster(config) else {
        return Ok(None);
    };
    let Some(pool) = pool else {
        tracing::warn!(
            "Cluster mode enabled but no database configured: this node runs every duty alone"
        );
        return Ok(None);
    };
    let m = &cluster.membership;
    let timing = LeaseTiming::new(
        Duration::from_secs(m.lease_renew_secs),
        Duration::from_secs(m.lease_ttl_secs),
    )?;
    let node_id = NodeId::from_string(&config.spec.node.id).with_context(|| {
        format!(
            "spec.node.id {:?} is not a UUID; a cluster node keeps one id across restarts",
            config.spec.node.id
        )
    })?;
    let signing_key = read_signing_key(cluster).await?;
    let (grpc_address, vrack_address) = peer_address(cluster, bind_addr)?;
    let leases = Arc::new(PgNodeLeaseRepository::new(pool.clone()));
    let membership = NodeMembership::join(
        leases.clone(),
        NodeJoin {
            node_id,
            role: format!("{:?}", cluster.role).to_lowercase(),
            public_key: signing_key.verifying_key().to_bytes().to_vec(),
            grpc_address,
            vrack_address,
            lease_ttl: timing.ttl,
            max_executions: cluster.membership.max_executions,
        },
        timing,
    )
    .await
    .context("joining the cluster (cluster_nodes)")?;
    membership.clone().spawn_renewal();
    Ok(Some(JoinedCluster {
        membership,
        leases,
        signing_key: Arc::new(signing_key),
        role: cluster.role,
        pool: pool.clone(),
    }))
}

/// How the duty runner holds its duties.
pub(crate) fn duty_leasing(joined: Option<&JoinedCluster>) -> DutyLeasing {
    match joined {
        Some(j) => {
            let timing = j.membership.timing();
            DutyLeasing::Leased {
                repo: Arc::new(PgClusterDutyRepository::new(j.pool.clone())),
                membership: j.membership.clone(),
                renew_every: timing.renew_every,
                ttl: timing.ttl,
            }
        }
        None => DutyLeasing::Solo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::node_config::ClusterMembershipConfig;

    fn cluster(vrack: Option<&str>) -> ClusterConfig {
        let mut c: ClusterConfig = serde_yaml::from_str("enabled: true\nrole: hybrid\n").unwrap();
        c.membership = ClusterMembershipConfig {
            vrack_address: vrack.map(str::to_string),
            ..Default::default()
        };
        c
    }

    #[test]
    fn peers_dial_the_vrack_address_on_the_cluster_port() {
        let (address, vrack) = peer_address(&cluster(Some("10.120.0.11")), "0.0.0.0").unwrap();
        assert_eq!(address, "10.120.0.11:50056");
        assert_eq!(vrack.as_deref(), Some("10.120.0.11"));
    }

    #[test]
    fn an_unspecified_bind_address_is_no_peer_address() {
        let err = peer_address(&cluster(None), "0.0.0.0").unwrap_err();
        assert!(
            err.to_string().contains("vrack_address is required"),
            "{err}"
        );
        let (address, vrack) = peer_address(&cluster(None), "10.0.0.5").unwrap();
        assert_eq!((address.as_str(), vrack), ("10.0.0.5:50056", None));
    }

    #[test]
    fn only_enabled_execution_roles_lease_their_duties() {
        let mut config: NodeConfigManifest = NodeConfigManifest::default();
        assert!(leasing_cluster(&config).is_none(), "no cluster block");
        let mut c = cluster(None);
        config.spec.cluster = Some(c.clone());
        assert!(leasing_cluster(&config).is_some());
        c.enabled = false;
        config.spec.cluster = Some(c.clone());
        assert!(leasing_cluster(&config).is_none(), "clustering off");
        c.enabled = true;
        c.role = NodeRole::RelayCoordinator;
        config.spec.cluster = Some(c);
        assert!(leasing_cluster(&config).is_none(), "a relay coordinator");
    }
}
