// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Register Workflow Use Case
//!
//! Application service for registering new workflows with the Temporal engine.
//!
//! # DDD Pattern: Application Service
//!
//! - **Layer:** Application
//! - **Responsibility:** Orchestrate registration of workflow definitions
//! - **Collaborators:** 
//!   - Domain: Workflow aggregate (validation)
//!   - Infrastructure: WorkflowParser, WorkflowRepository, TemporalClient, EventBus
//!
//! # Flow
//!
//! 1. Accept YAML manifest as input
//! 2. Parse via WorkflowParser → get Workflow domain aggregate
//! 3. Validate via Workflow invariants (no gaps, all transitions valid)
//! 4. Map via TemporalWorkflowMapper → TemporalWorkflowDefinition (JSON)
//! 5. Register with Temporal via TemporalClient (HTTP POST to TypeScript worker)
//! 6. Persist workflow to WorkflowRepository (PostgreSQL)
//! 7. Publish DomainEvent::WorkflowRegistered
//! 8. Return workflow_id and registration status
//!
//! # Error Handling
//!
//! Returns anyhow::Error with context:
//! - ParseError: Invalid YAML or manifest structure
//! - ValidationError: Workflow invariants violated (missing states, circular references)
//! - RegistrationError: Temporal server unavailable or registration failed
//! - PersistenceError: Database save failed

use crate::domain::repository::WorkflowRepository;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::temporal_client::TemporalClient;
use crate::infrastructure::workflow_parser::WorkflowParser;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tracing::info;

/// Registered workflow response
#[derive(Debug, Clone, serde::Serialize)]
pub struct RegisteredWorkflow {
    pub workflow_id: String,
    pub name: String,
    pub version: String,
    pub status: String,
    pub registered_at: chrono::DateTime<chrono::Utc>,
}

/// Register Workflow Use Case
#[async_trait]
pub trait RegisterWorkflowUseCase: Send + Sync {
    /// Register a new workflow from YAML manifest
    ///
    /// # Arguments
    ///
    /// * `yaml_manifest` - Complete workflow manifest in YAML format
    ///
    /// # Returns
    ///
    /// Registered workflow metadata on success
    ///
    /// # Errors
    ///
    /// - Parse errors: Invalid YAML
    /// - Validation errors: Workflow invariants violated
    /// - Registration errors: Temporal server unavailable
    /// - Persistence errors: Database save failed
    async fn register_workflow(&self, yaml_manifest: &str) -> Result<RegisteredWorkflow>;
}

/// Standard implementation of RegisterWorkflowUseCase
pub struct StandardRegisterWorkflowUseCase {
    #[allow(dead_code)]
    workflow_parser: Arc<WorkflowParser>,
    workflow_repository: Arc<dyn WorkflowRepository>,
    temporal_client: Arc<tokio::sync::RwLock<Option<Arc<TemporalClient>>>>,
    event_bus: Arc<EventBus>,
}

impl StandardRegisterWorkflowUseCase {
    pub fn new(
        workflow_parser: Arc<WorkflowParser>,
        workflow_repository: Arc<dyn WorkflowRepository>,
        temporal_client: Arc<tokio::sync::RwLock<Option<Arc<TemporalClient>>>>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            workflow_parser,
            workflow_repository,
            temporal_client,
            event_bus,
        }
    }
}

#[async_trait]
impl RegisterWorkflowUseCase for StandardRegisterWorkflowUseCase {
    async fn register_workflow(&self, yaml_manifest: &str) -> Result<RegisteredWorkflow> {
        info!("Registering workflow from manifest");

        // Step 1: Parse YAML → Workflow domain aggregate
        let workflow = WorkflowParser::parse_yaml(yaml_manifest)
            .map_err(|e| anyhow::anyhow!("Failed to parse workflow YAML manifest: {}", e))?;

        let workflow_id = workflow.id.to_string();
        let workflow_name = workflow.metadata.name.clone();
        let workflow_version = workflow.metadata.version.clone().unwrap_or_else(|| "0.1.0".to_string());

        // Step 2: Map to Temporal definition via anti-corruption layer
        let temporal_definition = crate::application::temporal_mapper::TemporalWorkflowMapper::to_temporal_definition(&workflow)
            .context("Failed to map workflow to Temporal definition")?;

        // Step 3: Register with Temporal via HTTP to TypeScript worker
        let client = {
            let lock = self.temporal_client.read().await;
            lock.clone().ok_or_else(|| anyhow::anyhow!("Temporal client not connected yet"))?
        };

        client
            .register_temporal_workflow(&temporal_definition)
            .await
            .context("Failed to register workflow with Temporal server")?;

        // Step 4: Persist workflow to repository
        self.workflow_repository
            .save(&workflow)
            .await
            .context("Failed to persist workflow to repository")?;

        // Step 5: Publish domain event
        self.event_bus.publish_workflow_event(
            crate::domain::events::WorkflowEvent::WorkflowRegistered {
                workflow_id: workflow.id,
                name: workflow_name.clone(),
                version: workflow_version.clone(),
                registered_at: chrono::Utc::now(),
            },
        );

        Ok(RegisteredWorkflow {
            workflow_id,
            name: workflow_name,
            version: workflow_version,
            status: "registered".to_string(),
            registered_at: chrono::Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_register_workflow_success() {
        // Integration test - see ../tests/temporal_integration_test.rs
    }
}
