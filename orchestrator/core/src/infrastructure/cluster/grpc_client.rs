// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # gRPC Client for Cluster Coordination

use anyhow::bail;

#[derive(Debug, Clone)]
pub struct NodeClusterClient {
    pub endpoint: String,
}

impl NodeClusterClient {
    pub fn new(endpoint: String) -> Self {
        Self { endpoint }
    }

    pub async fn attest_node(&self) -> anyhow::Result<()> {
        bail!(
            "multi-node cluster attestation is disabled in the single-node Phase 1 baseline (endpoint: {})",
            self.endpoint
        )
    }

    pub async fn heartbeat(&self) -> anyhow::Result<()> {
        bail!(
            "multi-node cluster heartbeats are disabled in the single-node Phase 1 baseline (endpoint: {})",
            self.endpoint
        )
    }

    pub async fn forward_execution(&self) -> anyhow::Result<()> {
        bail!(
            "multi-node execution forwarding is disabled in the single-node Phase 1 baseline (endpoint: {})",
            self.endpoint
        )
    }
}
