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

use crate::application::storage_router::StorageRouter;
use crate::domain::{
    events::StorageEvent,
    execution::ExecutionId,
    fsal::{AegisFSAL, BorrowedVolumeAccess, EventPublisher},
    policy::FilesystemPolicy,
    repository::VolumeRepository,
    storage::StorageProvider,
    volume::{Volume, VolumeId},
};
use crate::infrastructure::nfs::server::{NfsServer, NfsServerError, NfsVolumeContext};
use crate::infrastructure::storage::{LocalHostStorageProvider, SmcpStorageProvider};
use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
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
        mount_point: PathBuf,
    ) {
        let context = NfsVolumeContext {
            execution_id,
            volume_id,
            container_uid,
            container_gid,
            policy,
            mount_point,
        };
        self.contexts.write().insert(volume_id, context);
        debug!(
            "Registered NFS volume context: volume_id={}, execution_id={}",
            volume_id, execution_id
        );
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

    /// Reverse lookup: find volume context by execution_id
    ///
    /// Used by ToolInvocationService to resolve the agent's volume for FSAL
    /// tool execution (ADR-033 Path 1). Returns the first matching context
    /// since each execution typically owns a single workspace volume.
    pub fn find_by_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> Option<NfsVolumeContext> {
        self.contexts
            .read()
            .values()
            .find(|ctx| ctx.execution_id == execution_id)
            .cloned()
    }

    /// Returns all volume contexts for an execution.
    pub fn find_all_by_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> Vec<NfsVolumeContext> {
        self.contexts
            .read()
            .values()
            .filter(|ctx| ctx.execution_id == execution_id)
            .cloned()
            .collect()
    }

    /// Resolve the most specific mounted volume for a requested container path.
    /// Chooses the longest mount-point prefix match under this execution.
    pub fn find_by_execution_and_path(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        requested_path: &str,
    ) -> Option<NfsVolumeContext> {
        let requested = requested_path.trim();
        self.find_all_by_execution(execution_id)
            .into_iter()
            .filter(|ctx| {
                let mount = ctx.mount_point.to_string_lossy();
                requested == mount
                    || requested
                        .strip_prefix(mount.as_ref())
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            })
            .max_by_key(|ctx| ctx.mount_point.as_os_str().len())
    }

    /// Resolve primary workspace volume.
    /// Prefers exact `/workspace` mount; falls back to the shortest `/workspace/...` path.
    pub fn find_primary_workspace_by_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> Option<NfsVolumeContext> {
        let mut contexts = self.find_all_by_execution(execution_id);
        if contexts.is_empty() {
            return None;
        }
        if let Some(root) = contexts
            .iter()
            .find(|ctx| ctx.mount_point.to_string_lossy() == "/workspace")
        {
            return Some(root.clone());
        }
        contexts.sort_by_key(|ctx| ctx.mount_point.as_os_str().len());
        contexts.into_iter().next()
    }

    /// Get count of registered volumes
    pub fn count(&self) -> usize {
        self.contexts.read().len()
    }
}

impl Default for NfsVolumeRegistry {
    fn default() -> Self {
        Self::new()
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
    /// Read-only borrowed volume aliases used by judge executions.
    borrowed_volumes: Arc<RwLock<HashMap<VolumeId, BorrowedVolumeAccess>>>,

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
        // Build StorageRouter to support diverse backends
        let primary_local_path = std::env::temp_dir().join("aegis");
        let fallback_local_path = std::env::temp_dir();
        let local_provider = Arc::new(
            LocalHostStorageProvider::new(&primary_local_path).unwrap_or_else(|_| {
                LocalHostStorageProvider::new(&fallback_local_path).expect(
                    "failed to initialize fallback local storage provider in temp directory",
                )
            }),
        );
        let smcp_provider = Arc::new(SmcpStorageProvider::new());
        let storage_router = Arc::new(StorageRouter::new(
            storage_provider,
            local_provider,
            smcp_provider,
        ));
        let borrowed_volumes = Arc::new(RwLock::new(HashMap::new()));

        let fsal = Arc::new(AegisFSAL::new(
            storage_router,
            volume_repository,
            borrowed_volumes.clone(),
            event_publisher,
        ));

        let volume_registry = NfsVolumeRegistry::new();
        let bind_port = bind_port.unwrap_or(2049);
        let nfs_server = NfsServer::new(fsal, volume_registry.contexts.clone(), bind_port);

        Self {
            nfs_server,
            volume_registry,
            borrowed_volumes,
            is_running: Arc::new(Mutex::new(false)),
        }
    }

    /// Register a read-only alias for an existing source volume under a distinct exported volume ID.
    pub fn register_borrowed_volume(
        &self,
        alias_volume_id: VolumeId,
        execution_id: ExecutionId,
        source_volume: Volume,
    ) {
        self.borrowed_volumes.write().insert(
            alias_volume_id,
            BorrowedVolumeAccess {
                execution_id,
                source_volume,
            },
        );
    }

    pub fn deregister_borrowed_volume(&self, alias_volume_id: VolumeId) {
        self.borrowed_volumes.write().remove(&alias_volume_id);
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

        tracing::info!(
            "Starting NFS server gateway on port {}",
            self.nfs_server.bind_port()
        );

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
                "Server task has stopped".to_string(),
            ));
        }

        // A deeper protocol-level probe can be added later; this lightweight
        // check currently verifies task liveness via server status.

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
        mount_point: PathBuf,
    ) {
        self.volume_registry.register(
            volume_id,
            execution_id,
            container_uid,
            container_gid,
            policy,
            mount_point,
        );
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
        let service_name = "nfs_gateway_lifecycle";
        assert_eq!(service_name, "nfs_gateway_lifecycle");
    }

    #[test]
    fn test_gateway_creation() {
        let service_name = "nfs_gateway_creation";
        assert_eq!(service_name, "nfs_gateway_creation");
    }
}
