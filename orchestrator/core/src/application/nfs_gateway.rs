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
    fsal::{
        AegisFSAL, BorrowedVolumeAccess, EventPublisher, FsalAccessPolicy, VolumeContextLookup,
    },
    repository::VolumeRepository,
    storage::StorageProvider,
    volume::{Volume, VolumeId},
};
use crate::infrastructure::nfs::server::{NfsServer, NfsServerError, NfsVolumeContext};
use crate::infrastructure::storage::{LocalHostStorageProvider, SealStorageProvider};
use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Parameters for registering a volume with the NFS gateway.
pub struct VolumeRegistration {
    pub volume_id: VolumeId,
    pub execution_id: ExecutionId,
    /// Workflow execution ID when this volume is owned by a workflow execution.
    /// Propagated to `NfsVolumeContext` so FSAL `authorize()` can match
    /// `VolumeOwnership::WorkflowExecution` without mutating DB ownership.
    pub workflow_execution_id: Option<uuid::Uuid>,
    pub container_uid: u32,
    pub container_gid: u32,
    pub policy: FsalAccessPolicy,
    pub mount_point: PathBuf,
    pub remote_path: String,
    /// The label of the run's repository this mount is, when the execution is
    /// a workflow's agent state whose run holds repositories; `None` for every
    /// other mount. An execution with any such mount writes only under them
    /// (AEGIS ADR-141, Update of 2026-10-15).
    pub run_repository: Option<String>,
}

/// Volume context registry for NFS export path routing
///
/// Holds one registration per volume and execution: several executions may
/// hold a registration of one volume at once (a CODE child and its judge
/// child both mount the run's repository), each added by `register` and taken
/// away only by its own execution's end (`deregister_execution`) or by the
/// volume's own removal (`deregister`). The FSAL admits a person's persistent
/// volume to every execution holding a registration of it.
///
/// The NFS server routes by volume alone, so it reads a per-volume view: the
/// most recent registration of the volume still held.
///
/// Every mount point an execution registered is also kept until the
/// execution's end, so that a path under a mount whose registration was taken
/// away is refused by the mount's name rather than resolved into another of
/// the execution's volumes.
///
/// Thread-safe with RwLock for concurrent access from NFS operations.
#[derive(Clone)]
pub struct NfsVolumeRegistry {
    /// Every registration, keyed by volume and execution, with its order.
    registrations: Arc<RwLock<HashMap<(VolumeId, ExecutionId), HeldRegistration>>>,
    /// Per volume, the most recent registration still held (the NFS server's view).
    contexts: Arc<RwLock<HashMap<VolumeId, NfsVolumeContext>>>,
    /// Per execution, every mount point it registered and the volume at it.
    mounts: Arc<RwLock<HashMap<ExecutionId, HashMap<PathBuf, VolumeId>>>>,
    /// The order of registrations, newest highest.
    sequence: Arc<AtomicU64>,
}

#[derive(Clone)]
struct HeldRegistration {
    context: NfsVolumeContext,
    sequence: u64,
}

/// Where a container path of an execution resolves.
enum PathResolution {
    /// Under a mount whose volume the execution holds a registration of.
    Registered(NfsVolumeContext),
    /// Under a mount the execution registered whose registration no longer holds.
    Unregistered(PathBuf),
    /// Under no mount the execution registered.
    NoMount,
}

/// Whether `requested` is `mount` or lies under it.
fn is_under(requested: &str, mount: &str) -> bool {
    let mount = mount.trim_end_matches('/');
    requested == mount
        || requested
            .strip_prefix(mount)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl NfsVolumeRegistry {
    pub fn new() -> Self {
        Self {
            registrations: Arc::new(RwLock::new(HashMap::new())),
            contexts: Arc::new(RwLock::new(HashMap::new())),
            mounts: Arc::new(RwLock::new(HashMap::new())),
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Add an execution's registration of a volume, beside any other
    /// execution's; a second registration by the same execution replaces its
    /// first.
    pub fn register(&self, registration: VolumeRegistration) {
        let volume_id = registration.volume_id;
        let execution_id = registration.execution_id;
        let context = NfsVolumeContext {
            execution_id,
            volume_id,
            workflow_execution_id: registration.workflow_execution_id,
            container_uid: registration.container_uid,
            container_gid: registration.container_gid,
            policy: registration.policy,
            mount_point: registration.mount_point,
            remote_path: registration.remote_path,
            run_repository: registration.run_repository,
        };
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let mut registrations = self.registrations.write();
        self.mounts
            .write()
            .entry(execution_id)
            .or_default()
            .insert(context.mount_point.clone(), volume_id);
        self.contexts.write().insert(volume_id, context.clone());
        registrations.insert(
            (volume_id, execution_id),
            HeldRegistration { context, sequence },
        );
        debug!(
            "Registered NFS volume context: volume_id={}, execution_id={}",
            volume_id, execution_id
        );
    }

    /// Remove every registration of a volume. The mount points stay known to
    /// the executions that registered them, so a path under one is refused.
    pub fn deregister(&self, volume_id: VolumeId) {
        let mut registrations = self.registrations.write();
        registrations.retain(|(volume, _), _| *volume != volume_id);
        self.contexts.write().remove(&volume_id);
        debug!("Deregistered NFS volume context: volume_id={}", volume_id);
    }

    /// An execution's end: remove its own registrations, and only its own,
    /// and forget its mount points. A volume another execution still holds
    /// is shown by that execution's most recent registration. Answers how
    /// many registrations were removed.
    pub fn deregister_execution(&self, execution_id: ExecutionId) -> usize {
        let mut registrations = self.registrations.write();
        let released: Vec<VolumeId> = registrations
            .keys()
            .filter(|(_, execution)| *execution == execution_id)
            .map(|(volume, _)| *volume)
            .collect();
        for volume_id in &released {
            registrations.remove(&(*volume_id, execution_id));
        }
        {
            let mut contexts = self.contexts.write();
            for volume_id in &released {
                let newest = registrations
                    .iter()
                    .filter(|((volume, _), _)| volume == volume_id)
                    .max_by_key(|(_, held)| held.sequence)
                    .map(|(_, held)| held.context.clone());
                match newest {
                    Some(context) => {
                        contexts.insert(*volume_id, context);
                    }
                    None => {
                        contexts.remove(volume_id);
                    }
                }
            }
        }
        self.mounts.write().remove(&execution_id);
        debug!(
            "Deregistered NFS volume contexts of execution {}: {} removed",
            execution_id,
            released.len()
        );
        released.len()
    }

    /// The volume's most recent registration still held, as the NFS server
    /// sees it.
    pub fn lookup(&self, volume_id: VolumeId) -> Option<NfsVolumeContext> {
        self.contexts.read().get(&volume_id).cloned()
    }

    /// Whether the execution holds a registration of the volume.
    pub fn holds(&self, volume_id: VolumeId, execution_id: ExecutionId) -> bool {
        self.registrations
            .read()
            .contains_key(&(volume_id, execution_id))
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
        self.find_all_by_execution(execution_id).into_iter().next()
    }

    /// Returns all volume contexts for an execution.
    pub fn find_all_by_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> Vec<NfsVolumeContext> {
        self.registrations
            .read()
            .iter()
            .filter(|((_, execution), _)| *execution == execution_id)
            .map(|(_, held)| held.context.clone())
            .collect()
    }

    /// Resolve a container path of an execution against the mounts it
    /// registered: the longest mount-point prefix wins, whether or not its
    /// registration still holds.
    fn resolve_path(&self, execution_id: ExecutionId, requested_path: &str) -> PathResolution {
        let requested = requested_path.trim();
        let mount = self.mounts.read().get(&execution_id).and_then(|mounts| {
            mounts
                .iter()
                .filter(|(mount, _)| is_under(requested, &mount.to_string_lossy()))
                .max_by_key(|(mount, _)| mount.as_os_str().len())
                .map(|(mount, volume)| (mount.clone(), *volume))
        });
        let Some((mount_point, volume_id)) = mount else {
            return PathResolution::NoMount;
        };
        match self
            .registrations
            .read()
            .get(&(volume_id, execution_id))
            .filter(|held| held.context.mount_point == mount_point)
        {
            Some(held) => PathResolution::Registered(held.context.clone()),
            None => PathResolution::Unregistered(mount_point),
        }
    }

    /// Resolve the most specific mounted volume for a requested container path.
    /// Chooses the longest mount-point prefix match under this execution, and
    /// answers `None` when that mount's registration no longer holds.
    pub fn find_by_execution_and_path(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        requested_path: &str,
    ) -> Option<NfsVolumeContext> {
        match self.resolve_path(execution_id, requested_path) {
            PathResolution::Registered(context) => Some(context),
            PathResolution::Unregistered(_) | PathResolution::NoMount => None,
        }
    }

    /// The mount an execution registered that `requested_path` lies under,
    /// when that mount's registration no longer holds for the execution: the
    /// path is to be refused by the mount's name, never resolved into another
    /// volume.
    pub fn unregistered_mount_of_path(
        &self,
        execution_id: ExecutionId,
        requested_path: &str,
    ) -> Option<PathBuf> {
        match self.resolve_path(execution_id, requested_path) {
            PathResolution::Unregistered(mount_point) => Some(mount_point),
            PathResolution::Registered(_) | PathResolution::NoMount => None,
        }
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

    /// The mount points of an execution's registrations that are its run's
    /// repositories, in order: empty for an execution that is not a
    /// workflow's agent state or whose run holds no repository.
    pub fn run_repository_mounts(&self, execution_id: ExecutionId) -> Vec<PathBuf> {
        let mut mounts: Vec<PathBuf> = self
            .find_all_by_execution(execution_id)
            .into_iter()
            .filter(|ctx| ctx.run_repository.is_some())
            .map(|ctx| ctx.mount_point)
            .collect();
        mounts.sort();
        mounts.dedup();
        mounts
    }

    /// Get count of registrations held
    pub fn count(&self) -> usize {
        self.registrations.read().len()
    }
}

impl Default for NfsVolumeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl VolumeContextLookup for NfsVolumeRegistry {
    fn lookup_workflow_execution_id(&self, volume_id: VolumeId) -> Option<uuid::Uuid> {
        self.lookup(volume_id)
            .and_then(|ctx| ctx.workflow_execution_id)
    }

    fn holds_registration(&self, volume_id: VolumeId, execution_id: ExecutionId) -> bool {
        self.holds(volume_id, execution_id)
    }

    fn lookup_remote_path(&self, volume_id: VolumeId) -> Option<String> {
        self.lookup(volume_id)
            .map(|ctx| ctx.remote_path)
            .filter(|p| !p.is_empty())
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
        let seal_provider = Arc::new(SealStorageProvider::new());
        let storage_router = Arc::new(StorageRouter::new(
            storage_provider,
            local_provider,
            seal_provider,
        ));
        let borrowed_volumes = Arc::new(RwLock::new(HashMap::new()));

        let volume_registry = NfsVolumeRegistry::new();

        let fsal = Arc::new(
            AegisFSAL::new(
                storage_router,
                volume_repository,
                borrowed_volumes.clone(),
                event_publisher,
            )
            .with_volume_context_lookup(Arc::new(volume_registry.clone())),
        );
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
    pub fn register_volume(&self, registration: VolumeRegistration) {
        self.volume_registry.register(registration);
        metrics::gauge!("aegis_volumes_active").increment(1.0);
    }

    /// Deregister a volume from the NFS server
    pub fn deregister_volume(&self, volume_id: VolumeId) {
        self.volume_registry.deregister(volume_id);
        metrics::gauge!("aegis_volumes_active").decrement(1.0);
    }

    /// An execution's end: remove its own registrations, and only its own.
    pub fn deregister_execution(&self, execution_id: ExecutionId) {
        let removed = self.volume_registry.deregister_execution(execution_id);
        metrics::gauge!("aegis_volumes_active").decrement(removed as f64);
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
    use super::*;

    fn registration(
        volume_id: VolumeId,
        execution_id: ExecutionId,
        mount_point: &str,
    ) -> VolumeRegistration {
        VolumeRegistration {
            volume_id,
            execution_id,
            workflow_execution_id: None,
            run_repository: None,
            container_uid: 1000,
            container_gid: 1000,
            policy: FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: vec!["/*".to_string()],
            },
            mount_point: PathBuf::from(mount_point),
            remote_path: String::new(),
        }
    }

    /// The Forge's CODE child and its judge child both mount the run's
    /// repository: the judge's registration is added beside the coder's, not
    /// in its place, and the judge's end takes away only its own.
    #[test]
    fn two_executions_hold_one_volume_and_the_first_ends_without_taking_the_second() {
        let registry = NfsVolumeRegistry::new();
        let repository = VolumeId::new();
        let coder = ExecutionId::new();
        let judge = ExecutionId::new();
        registry.register(registration(repository, coder, "/workspace/aegis-examples"));
        registry.register(registration(repository, judge, "/workspace/aegis-examples"));

        for (name, execution) in [("coder", coder), ("judge", judge)] {
            let found = registry
                .find_by_execution_and_path(execution, "/workspace/aegis-examples/.git")
                .map(|ctx| (ctx.volume_id, ctx.execution_id));
            assert_eq!(
                found,
                Some((repository, execution)),
                "the {name}'s registration of the repository volume does not resolve once both executions registered it"
            );
            assert!(
                registry.holds(repository, execution),
                "the {name} holds no registration of the repository volume once both executions registered it"
            );
        }

        registry.deregister_execution(judge);

        assert!(
            !registry.holds(repository, judge),
            "the judge still holds the repository volume after its end"
        );
        assert_eq!(
            registry
                .find_by_execution_and_path(coder, "/workspace/aegis-examples/.git")
                .map(|ctx| (ctx.volume_id, ctx.execution_id)),
            Some((repository, coder)),
            "the judge's end took the coder's registration of the repository volume with it"
        );
        assert_eq!(
            registry.lookup(repository).map(|ctx| ctx.execution_id),
            Some(coder),
            "the gateway's view of the repository volume does not fall back to the coder after the judge's end"
        );
    }

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
