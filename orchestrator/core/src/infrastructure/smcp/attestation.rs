// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct AttestationRequest {
    pub agent_id: String,
    pub execution_id: String,
    pub container_id: String,
    pub public_key_pem: String,
}

#[derive(Debug, Clone)]
pub struct AttestationResponse {
    pub security_token: String,
}

#[async_trait]
pub trait AttestationService: Send + Sync {
    async fn attest(&self, request: AttestationRequest) -> Result<AttestationResponse>;
}
