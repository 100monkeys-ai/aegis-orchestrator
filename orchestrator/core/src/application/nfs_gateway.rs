// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! NFS Gateway Application Service
//!
//! Manages the lifecycle of the NFS server that provides POSIX-transparent 
//! volume access to agent containers per ADR-036.
//!
//! ## Architecture
//! - Single shared NFS server on port 2049
//! - Routes by export path `/{tenant_id}/{volume_id}`
//! - Always-on service (starts with orchestrator)
//! - Uses AegisFSAL for authorization and audit
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for nfs gateway

use crate::domain::{
    fsal::{AegisFSAL, EventPublisher},
    repository::VolumeRepository,
    storage::StorageProvider,
    events::StorageEvent,
    execution::ExecutionId,
    volume::VolumeId,
    policy::FilesystemPolicy,
};
use crate::infrastructure::nfs::server::{NfsServer, NfsServerError, NfsVolumeContext};
use std::sync::Arc;
use std::collections::HashMap;
use parking_lot::{RwLock, Mutex};
use thiserror::Error;
use async_trait::async_trait;
use tracing::debug;

/// NFS Gateway service errors
#[derive(Debug, Error)]
pub enum NfsGatewayError {
    #[error("NFS server already running")]
    AlreadyRunning,

    #[error("NFS server not running")]
    NotRunning,

    #[error("Failed to bind to port {port}: {error}")]
    BindFailed { port: u16, error: String },

    #[error("Server health check failed: {0}")]
    HealthCheckFailed(String),

    #[error("NFS server error: {0}")]
    ServerError(#[from] NfsServerError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Volume context registry for NFS export path routing
/// 
/// Maps VolumeId to execution context (execution_id, UID/GID, policy).
/// Thread-safe with RwLock for concurrent access from NFS operations.
#[derive(Clone)]
pub struct NfsVolumeRegistry {
    contexts: Arc<RwLock<HashMap<VolumeId, NfsVolumeContext>>>,
}

impl NfsVolumeRegistry {
    pub fn new() -> Self {
        Self {
            contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a volume with its execution context
    pub fn register(
        &self,
        volume_id: VolumeId,
        execution_id: ExecutionId,
        container_uid: u32,
        container_gid: u32,
        policy: FilesystemPolicy,
    ) {
        let context = NfsVolumeContext {
            execution_id,
            volume_id,
            container_uid,
            container_gid,
            policy,
        };
        self.contexts.write().insert(volume_id, context);
        debug!("Registered NFS volume context: volume_id={}, execution_id={}", volume_id, execution_id);
    }

    /// Deregister a volume
    pub fn deregister(&self, volume_id: VolumeId) {
        self.contexts.write().remove(&volume_id);
        debug!("Deregistered NFS volume context: volume_id={}", volume_id);
    }

    /// Lookup execution context for a volume
    pub fn lookup(&self, volume_id: VolumeId) -> Option<NfsVolumeContext> {
        self.contexts.read().get(&volume_id).cloned()
    }

    /// Get all registered volume IDs
    pub fn list_volumes(&self) -> Vec<VolumeId> {
        self.contexts.read().keys().copied().collect()
    }

    /// Get count of registered volumes
    pub fn count(&self) -> usize {
        self.contexts.read().len()
    }
}

/// NFS Gateway application service
///
/// Provides lifecycle management for the NFS server gateway.
/// This is an application service (not domain or infrastructure) because it
/// orchestrates domain entities (AegisFSAL) and infrastructure (NFS server).
pub struct NfsGatewayService {
    /// NFS server instance (infrastructure)
    nfs_server: NfsServer,
    
    /// Volume registry for export path routing
    volume_registry: NfsVolumeRegistry,
    
    /// Whether server is running (wrapped in Mutex for interior mutability)
    is_running: Arc<Mutex<bool>>,
}

impl NfsGatewayService {
    /// Create a new NFS gateway service
    ///
    /// # Arguments
    /// * `storage_provider` - Backend storage (SeaweedFS, Local, etc.)
    /// * `volume_repository` - Volume repository for ownership validation
    /// * `event_publisher` - Event bus for audit trail
    /// * `bind_port` - Port to bind NFS server (default: 2049)
    pub fn new(
        storage_provider: Arc<dyn StorageProvider>,
        volume_repository: Arc<dyn VolumeRepository>,
        event_publisher: Arc<dyn EventPublisher>,
        bind_port: Option<u16>,
    ) -> Self {
        let fsal = Arc::new(AegisFSAL::new(
            storage_provider,
            volume_repository,
            event_publisher,
        ));

        let volume_registry = NfsVolumeRegistry::new();
        let bind_port = bind_port.unwrap_or(2049);
        let nfs_server = NfsServer::new(fsal, volume_registry.contexts.clone(), bind_port);

        Self {
            nfs_server,
            volume_registry,
            is_running: Arc::new(Mutex::new(false)),
        }
    }

    /// Start the NFS server
    ///
    /// Spawns the NFS server in a background task.
    /// Returns immediately after server starts listening.
    ///
    /// # Errors
    /// - `AlreadyRunning` if server is already started
    /// - `BindFailed` if cannot bind to port
    pub async fn start_server(&self) -> Result<(), NfsGatewayError> {
        if *self.is_running.lock() {
            return Err(NfsGatewayError::AlreadyRunning);
        }

        tracing::info!("Starting NFS server gateway on port {}", self.nfs_server.bind_port());

        // Start the NFS server
        self.nfs_server.start().await?;
        *self.is_running.lock() = true;
        
        tracing::info!("NFS server gateway started successfully");
        
        Ok(())
    }

    /// Stop the NFS server gracefully
    ///
    /// Waits for active connections to close before shutting down.
    ///
    /// # Errors
    /// - `NotRunning` if server is not started
    pub async fn stop_server(&self) -> Result<(), NfsGatewayError> {
        if !*self.is_running.lock() {
            return Err(NfsGatewayError::NotRunning);
        }

        tracing::info!("Stopping NFS server gateway");

        // Stop the NFS server (this aborts the task)
        self.nfs_server.stop().await?;

        *self.is_running.lock() = false;
        debug!("NFS server gateway stopped");
        
        Ok(())
    }

    /// Check if NFS server is healthy and responding
    ///
    /// Verifies the server is running and responsive.
    pub async fn health_check(&self) -> Result<(), NfsGatewayError> {
        if !*self.is_running.lock() {
            return Err(NfsGatewayError::NotRunning);
        }

        // Check if server task is still running
        if !self.nfs_server.is_running() {
            return Err(NfsGatewayError::HealthCheckFailed(
                "Server task has stopped".to_string()
            ));
        }

        // TODO: Send NULL RPC to verify server responding
        // This would require NFS client implementation or raw RPC call
        
        Ok(())
    }

    /// Get the FSAL instance (for testing/introspection)
    pub fn fsal(&self) -> &Arc<AegisFSAL> {
        self.nfs_server.fsal()
    }

    /// Check if server is running
    pub fn is_running(&self) -> bool {
        *self.is_running.lock() && self.nfs_server.is_running()
    }

    /// Get bind port
    pub fn bind_port(&self) -> u16 {
        self.nfs_server.bind_port()
    }

    /// Register a volume with the NFS server for export path routing
    pub fn register_volume(
        &self,
        volume_id: VolumeId,
        execution_id: ExecutionId,
        container_uid: u32,
        container_gid: u32,
        policy: FilesystemPolicy,
    ) {
        self.volume_registry.register(volume_id, execution_id, container_uid, container_gid, policy);
    }

    /// Deregister a volume from the NFS server
    pub fn deregister_volume(&self, volume_id: VolumeId) {
        self.volume_registry.deregister(volume_id);
    }

    /// Get the volume registry (for introspection/testing)
    pub fn volume_registry(&self) -> &NfsVolumeRegistry {
        &self.volume_registry
    }
}

/// Event publisher adapter for EventBus
///
/// Adapts the domain EventPublisher trait to the infrastructure EventBus.
pub struct EventBusPublisher {
    event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
}

impl EventBusPublisher {
    pub fn new(event_bus: Arc<crate::infrastructure::event_bus::EventBus>) -> Self {
        Self { event_bus }
    }
}

#[async_trait]
impl EventPublisher for EventBusPublisher {
    async fn publish_storage_event(&self, event: StorageEvent) {
        // Publish StorageEvent via EventBus
        self.event_bus.publish_storage_event(event);
    }
}

#[cfg(test)]
mod tests {
    // Note: Full integration tests require nfsserve implementation
    // These are unit tests for service lifecycle only

    #[tokio::test]
    async fn test_gateway_lifecycle() {
        // Mock dependencies would go here
        // For now, this is a placeholder for future tests
    }

    #[test]
    fn test_gateway_creation() {
        // Test basic construction
        // Requires mock implementations
    }
}
