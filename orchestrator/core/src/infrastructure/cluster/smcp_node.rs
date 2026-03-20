// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SMCP Node Verifier
//!
//! Verifies signatures and tokens for inter-node communication.

use crate::domain::cluster::{SmcpNodeEnvelope, NodeSecurityToken};
use anyhow::bail;

#[derive(Debug, Default)]
pub struct SmcpNodeVerifier {}

impl SmcpNodeVerifier {
    pub fn new() -> Self {
        Self {}
    }

    pub fn verify_envelope(&self, _envelope: &SmcpNodeEnvelope) -> anyhow::Result<()> {
        bail!("inter-node SMCP verification is disabled in the single-node Phase 1 baseline")
    }

    pub fn verify_token(&self, _token: &NodeSecurityToken) -> anyhow::Result<()> {
        bail!("inter-node SMCP verification is disabled in the single-node Phase 1 baseline")
    }
}
