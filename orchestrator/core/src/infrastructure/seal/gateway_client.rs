// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! HTTP client adapter for pre-creating SEAL sessions on the gateway (ADR-088 §A8).
//!
//! Implements [`SealGatewayClient`] by POSTing session data to the SEAL gateway's
//! control plane endpoint before the container is spawned. This eliminates the
//! shared-default security context fallback that was the highest-priority AEGIS gap.

use crate::application::ports::{SealGatewayClient, SealSessionCreateRequest};

/// HTTP-based SEAL gateway client for session pre-creation.
pub struct HttpSealGatewayClient {
    client: reqwest::Client,
    gateway_url: String,
    operator_token: Option<String>,
}

impl HttpSealGatewayClient {
    pub fn new(gateway_url: String, operator_token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            gateway_url,
            operator_token,
        }
    }
}

#[async_trait::async_trait]
impl SealGatewayClient for HttpSealGatewayClient {
    async fn create_session(
        &self,
        request: SealSessionCreateRequest,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/v1/seal/sessions", self.gateway_url);
        let mut req = self.client.post(&url).json(&request);
        if let Some(ref token) = self.operator_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(
                format!("SEAL gateway session creation failed (HTTP {status}): {body}").into(),
            );
        }
        Ok(())
    }
}
