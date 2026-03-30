// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Temporal gRPC client factory helpers.

use anyhow::{Context, Result};
use tonic::transport::Channel;

use aegis_orchestrator_core::domain::node_config::{resolve_env_value, NodeConfigManifest};
use aegis_orchestrator_core::infrastructure::temporal_proto::temporal::api::workflowservice::v1::workflow_service_client::WorkflowServiceClient;

pub(crate) async fn connect_temporal_workflow_client(
    config: &NodeConfigManifest,
) -> Result<WorkflowServiceClient<Channel>> {
    let temporal = config
        .spec
        .temporal
        .as_ref()
        .context("Temporal configuration is not available")?;
    let address = resolve_env_value(&temporal.address).unwrap_or_else(|_| temporal.address.clone());
    let endpoint = if address.contains("://") {
        address
    } else {
        format!("http://{address}")
    };
    let channel = Channel::from_shared(endpoint)
        .context("Invalid Temporal address")?
        .connect()
        .await
        .context("Failed to connect to Temporal server")?;
    Ok(WorkflowServiceClient::new(channel))
}

pub(crate) fn temporal_namespace(config: &NodeConfigManifest) -> Result<String> {
    let temporal = config
        .spec
        .temporal
        .as_ref()
        .context("Temporal configuration is not available")?;
    Ok(resolve_env_value(&temporal.namespace).unwrap_or_else(|_| temporal.namespace.clone()))
}
