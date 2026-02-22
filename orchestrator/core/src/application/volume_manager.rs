// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Volume Manager Application Service (BC-7, ADR-032)
//!
//! Orchestrates the full lifecycle of `Volume` aggregates: creation, mounting,
//! detachment, deletion, and periodic garbage-collection of expired ephemeral
//! volumes.
//!
//! ## Orchestrator Proxy Pattern
//!
//! The volume manager follows the same Orchestrator Proxy Pattern established
//! for MCP tool routing (ADR-033). Agent containers **never** access storage
//! directly — all I/O flows through the NFS Server Gateway, which in turn calls
//! the `StorageProvider` abstraction over SeaweedFS (ADR-032).
//!
//! ## Storage Classes
//!
//! | Class | Lifecycle | NFS mount |
//! |-------|-----------|---------------------|
//! | `Ephemeral` | TTL-based; GC runs on schedule | Unmounted on execution end |
//! | `Persistent` | Manual deletion only | Re-mounted by name |
//!
//! ## Usage
//!
//! Inject `Arc<dyn VolumeService>` wherever volumes need to be provisioned.
//! The concrete implementation is `VolumeServiceImpl` below.
//!
//! See ADR-032 (Unified Storage via SeaweedFS), ADR-036 (NFS Gateway),
//! AGENTS.md §BC-7 Storage Gateway.

use crate::domain::volume::{
    Volume, VolumeId, TenantId, StorageClass, FilerEndpoint, 
    VolumeOwnership, VolumeMount, AccessMode,
};
use crate::domain::repository::VolumeRepository;
use crate::domain::storage::StorageProvider;
use crate::domain::runtime::InstanceId;
use crate::domain::events::VolumeEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::agent::VolumeSpec;
use crate::infrastructure::event_bus::EventBus;
use anyhow::{Result, Context};
use async_trait::async_trait;
use std::sync::Arc;
use std::path::PathBuf;
use chrono::Utc;
use tracing::{info, warn, error, debug};

// ============================================================================
// Service Trait
// ============================================================================

/// Application service trait for volume lifecycle management (BC-7, ADR-032).
///
/// All volume operations are mediated by the orchestrator. Agent containers
/// access volumes exclusively via the NFS Server Gateway; direct bind mounts
/// are an anti-pattern (see AGENTS.md Anti-Patterns §Bypassing Storage Gateway).
#[async_trait]
pub trait VolumeService: Send + Sync {
    /// Create a new volume with the specified storage class and size.
    ///
    /// Publishes a [`crate::domain::events::VolumeEvent::VolumeCreated`] event.
    ///
    /// # Errors
    ///
    /// Returns an error if the SeaweedFS backend is unavailable or the
    /// `size_limit_mb` exceeds per-tenant quota.
    async fn create_volume(
        &self,
        name: String,
        tenant_id: TenantId,
        storage_class: StorageClass,
        size_limit_mb: u64,
        ownership: VolumeOwnership,
    ) -> Result<VolumeId>;

    /// Retrieve volume metadata by ID.
    ///
    /// # Errors
    ///
    /// Returns `Err` if no volume with `id` exists or the repository is unavailable.
    async fn get_volume(&self, id: VolumeId) -> Result<Volume>;

    /// List all volumes belonging to `tenant_id`.
    async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> Result<Vec<Volume>>;

    /// List volumes filtered by their ownership record.
    ///
    /// Use `VolumeOwnership::Execution(id)` to find all volumes mounted in a
    /// specific execution.
    async fn list_volumes_by_ownership(&self, ownership: &VolumeOwnership) -> Result<Vec<Volume>>;

    /// Attach a volume to a running instance, mounting it at `mount_point`.
    ///
    /// For `StorageClass::Persistent` with `AccessMode::ReadWrite`, this call
    /// reserves the volume exclusively (only one execution at a time, Phase 1
    /// NFS `nolock` constraint).
    ///
    /// Publishes [`crate::domain::events::VolumeEvent::VolumeAttached`].
    ///
    /// # Errors
    ///
    /// - Volume not found.
    /// - Volume already exclusively mounted (persistent ReadWrite).
    /// - NFS mount failed.
    async fn attach_volume(
        &self,
        volume_id: VolumeId,
        instance_id: InstanceId,
        mount_point: PathBuf,
        access_mode: AccessMode,
    ) -> Result<VolumeMount>;

    /// Detach a volume from an instance.
    ///
    /// Publishes [`crate::domain::events::VolumeEvent::VolumeDetached`].
    async fn detach_volume(
        &self,
        volume_id: VolumeId,
        instance_id: InstanceId,
    ) -> Result<()>;

    /// Mark a volume for deletion.
    ///
    /// Actual data removal is performed asynchronously by the storage backend.
    /// Publishes [`crate::domain::events::VolumeEvent::VolumeDeleted`].
    ///
    /// # Errors
    ///
    /// Returns an error if the volume is currently attached to a running instance.
    async fn delete_volume(&self, volume_id: VolumeId) -> Result<()>;

    /// Return the current used-bytes count for `volume_id`.
    ///
    /// Used by the NFS FSAL layer to enforce `VolumeQuotaExceeded` limits.
    async fn get_volume_usage(&self, volume_id: VolumeId) -> Result<u64>;

    /// Garbage-collect expired ephemeral volumes (`StorageClass::Ephemeral`).
    ///
    /// Should be called on a scheduled interval (e.g. every 60 s). Volumes
    /// whose TTL has elapsed are deleted; their NFS exports are removed.
    ///
    /// Returns the number of volumes purged.
    async fn cleanup_expired_volumes(&self) -> Result<usize>;

    /// Provision volumes declared in an agent manifest for a new execution.
    ///
    /// Parses the `spec.volumes` field, creates each volume according to its
    /// spec, and returns the full `Volume` list with resolved host paths.
    ///
    /// # Arguments
    ///
    /// * `execution_id` — The execution that will own the volumes.
    /// * `tenant_id` — Tenant namespace for quota and NFS export isolation.
    /// * `volume_specs` — Parsed from `agent.yaml` `spec.volumes[]`.
    /// * `storage_mode` — `"seaweedfs"` or `"local"` (test/dev only).
    /// * `fallback_to_local` — If `true`, fall back to local fs when SeaweedFS
    ///   is unreachable (safe for dev, never for prod).
    /// * `local_path` — Base path for local fallback volumes.
    async fn create_volumes_for_execution(
        &self,
        execution_id: ExecutionId,
        tenant_id: TenantId,
        volume_specs: &[VolumeSpec],
        storage_mode: &str,
        fallback_to_local: bool,
        local_path: &str,
    ) -> Result<Vec<Volume>>;
}

// ============================================================================
// Standard Implementation
// ============================================================================

pub struct StandardVolumeService {
    repository: Arc<dyn VolumeRepository>,
    storage_provider: Arc<dyn StorageProvider>,
    event_bus: Arc<EventBus>,
    filer_endpoint: FilerEndpoint,
}

impl StandardVolumeService {
    pub fn new(
        repository: Arc<dyn VolumeRepository>,
        storage_provider: Arc<dyn StorageProvider>,
        event_bus: Arc<EventBus>,
        filer_url: String,
    ) -> Result<Self> {
        let filer_endpoint = FilerEndpoint::new(filer_url)
            .context("Invalid filer URL")?;
        
        Ok(Self {
            repository,
            storage_provider,
            event_bus,
            filer_endpoint,
        })
    }
}

#[async_trait]
impl VolumeService for StandardVolumeService {
    async fn create_volume(
        &self,
        name: String,
        tenant_id: TenantId,
        storage_class: StorageClass,
        size_limit_mb: u64,
        ownership: VolumeOwnership,
    ) -> Result<VolumeId> {
        info!(
            "Creating volume '{}' for tenant {} (storage_class: {:?}, size_limit: {}MB)",
            name, tenant_id, storage_class, size_limit_mb
        );

        // Create volume aggregate
        let size_limit_bytes = size_limit_mb * 1024 * 1024;
        let volume = Volume::new(
            name.clone(),
            tenant_id,
            storage_class.clone(),
            self.filer_endpoint.clone(),
            size_limit_bytes,
            ownership.clone(),
        )?;

        let volume_id = volume.id;
        let remote_path = volume.remote_path.clone();

        // Create directory on SeaweedFS
        self.storage_provider
            .create_directory(&remote_path)
            .await
            .context("Failed to create volume directory on storage backend")?;

        // Set quota on SeaweedFS
        self.storage_provider
            .set_quota(&remote_path, size_limit_bytes)
            .await
            .context("Failed to set volume quota on storage backend")?;

        // Transition volume from Creating → Available now that storage is provisioned
        let mut volume = volume;
        volume.mark_available().context("Failed to mark volume as available")?;

        // Persist to database
        self.repository
            .save(&volume)
            .await
            .context("Failed to save volume to repository")?;

        // Publish domain event
        self.event_bus.publish_volume_event(VolumeEvent::VolumeCreated {
            volume_id,
            execution_id: match &ownership {
                VolumeOwnership::Execution { execution_id } => Some(*execution_id),
                VolumeOwnership::WorkflowExecution { workflow_execution_id: _ } => None,
                VolumeOwnership::Persistent { owner: _ } => None,
            },
            storage_class: storage_class.clone(),
            remote_path: remote_path.clone(),
            size_limit_bytes,
            created_at: Utc::now(),
        });

        info!(
            "Volume '{}' created successfully (id: {}, remote_path: {})",
            name, volume_id, remote_path
        );

        Ok(volume_id)
    }

    async fn get_volume(&self, id: VolumeId) -> Result<Volume> {
        debug!("Fetching volume {}", id);
        self.repository
            .find_by_id(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Volume {} not found", id))
    }

    async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> Result<Vec<Volume>> {
        debug!("Listing volumes for tenant {}", tenant_id);
        self.repository
            .find_by_tenant(tenant_id)
            .await
            .context("Failed to list volumes by tenant")
    }

    async fn list_volumes_by_ownership(&self, ownership: &VolumeOwnership) -> Result<Vec<Volume>> {
        debug!("Listing volumes for ownership: {:?}", ownership);
        self.repository
            .find_by_ownership(ownership)
            .await
            .context("Failed to list volumes by ownership")
    }

    async fn attach_volume(
        &self,
        volume_id: VolumeId,
        instance_id: InstanceId,
        mount_point: PathBuf,
        access_mode: AccessMode,
    ) -> Result<VolumeMount> {
        info!(
            "Attaching volume {} to instance {:?} at {:?} (mode: {:?})",
            volume_id, instance_id, mount_point, access_mode
        );

        // Load volume aggregate
        let mut volume = self.get_volume(volume_id).await?;

        // Check if volume can be attached (domain invariant)
        if !volume.can_attach() {
            return Err(anyhow::anyhow!(
                "Volume {} cannot be attached in current state: {:?}",
                volume_id,
                volume.status
            ));
        }

        // Mark volume as attached (state transition)
        volume.mark_attached()?;

        // Persist state change
        self.repository
            .save(&volume)
            .await
            .context("Failed to save volume after attach")?;

        // Create VolumeMount value object
        let volume_mount = volume.to_mount(mount_point.clone(), access_mode);

        // Publish domain event
        self.event_bus.publish_volume_event(VolumeEvent::VolumeAttached {
            volume_id,
            instance_id: instance_id.clone(),
            mount_point: mount_point.to_string_lossy().to_string(),
            access_mode: format!("{:?}", access_mode),
            attached_at: Utc::now(),
        });

        info!(
            "Volume {} attached to instance {:?} successfully",
            volume_id, instance_id
        );

        Ok(volume_mount)
    }

    async fn detach_volume(
        &self,
        volume_id: VolumeId,
        instance_id: InstanceId,
    ) -> Result<()> {
        info!(
            "Detaching volume {} from instance {:?}",
            volume_id, instance_id
        );

        // Load volume aggregate
        let mut volume = self.get_volume(volume_id).await?;

        // Check if volume can be detached (domain invariant)
        if !volume.can_detach() {
            return Err(anyhow::anyhow!(
                "Volume {} cannot be detached in current state: {:?}",
                volume_id,
                volume.status
            ));
        }

        // Mark volume as detached (state transition)
        volume.mark_detached()?;

        // Persist state change
        self.repository
            .save(&volume)
            .await
            .context("Failed to save volume after detach")?;

        // Publish domain event
        self.event_bus.publish_volume_event(VolumeEvent::VolumeDetached {
            volume_id,
            instance_id: instance_id.clone(),
            detached_at: Utc::now(),
        });

        info!(
            "Volume {} detached from instance {:?} successfully",
            volume_id, instance_id
        );

        Ok(())
    }

    async fn delete_volume(&self, volume_id: VolumeId) -> Result<()> {
        info!("Deleting volume {}", volume_id);

        // Load volume aggregate
        let mut volume = self.get_volume(volume_id).await?;

        // Mark volume as deleting (state transition)
        volume.mark_deleting()?;

        // Persist state change (mark as deleting first for crash recovery)
        self.repository
            .save(&volume)
            .await
            .context("Failed to mark volume as deleting")?;

        // Delete directory from SeaweedFS
        let remote_path = volume.remote_path.clone();
        match self.storage_provider.delete_directory(&remote_path).await {
            Ok(_) => {
                debug!("Volume directory {} deleted from storage backend", remote_path);
            }
            Err(e) => {
                warn!(
                    "Failed to delete volume directory {} from storage backend: {}. Marking as deleted anyway.",
                    remote_path, e
                );
                // Continue even if storage deletion fails - volume is orphaned but marked deleted
            }
        }

        // Mark volume as deleted (final state)
        volume.mark_deleted()?;

        // Persist final state
        self.repository
            .save(&volume)
            .await
            .context("Failed to mark volume as deleted")?;

        // Publish domain event
        self.event_bus.publish_volume_event(VolumeEvent::VolumeDeleted {
            volume_id,
            deleted_at: Utc::now(),
        });

        info!("Volume {} deleted successfully", volume_id);

        Ok(())
    }

    async fn get_volume_usage(&self, volume_id: VolumeId) -> Result<u64> {
        debug!("Getting storage usage for volume {}", volume_id);

        // Load volume aggregate
        let volume = self.get_volume(volume_id).await?;

        // Query SeaweedFS for actual usage
        let remote_path = volume.remote_path.clone();
        let usage_bytes = self
            .storage_provider
            .get_usage(&remote_path)
            .await
            .context("Failed to get volume usage from storage backend")?;

        // Check if quota exceeded
        if usage_bytes > volume.size_limit_bytes {
            warn!(
                "Volume {} quota exceeded: {} bytes used, {} bytes limit",
                volume_id,
                usage_bytes,
                volume.size_limit_bytes
            );

            // Publish quota exceeded event
            self.event_bus.publish_volume_event(VolumeEvent::VolumeQuotaExceeded {
                volume_id,
                size_limit_bytes: volume.size_limit_bytes,
                actual_bytes: usage_bytes,
                exceeded_at: Utc::now(),
            });
        }

        Ok(usage_bytes)
    }

    async fn cleanup_expired_volumes(&self) -> Result<usize> {
        info!("Starting cleanup of expired ephemeral volumes");

        // Query for expired volumes
        let expired_volumes = self
            .repository
            .find_expired()
            .await
            .context("Failed to find expired volumes")?;

        let count = expired_volumes.len();
        info!("Found {} expired volumes to clean up", count);

        // Delete each expired volume
        let mut deleted_count = 0usize;
        for volume in expired_volumes {
            let volume_id = volume.id;
            
            // Publish expiration event
            self.event_bus.publish_volume_event(VolumeEvent::VolumeExpired {
                volume_id,
                expired_at: Utc::now(),
            });

            // Delete volume (includes storage cleanup)
            match self.delete_volume(volume_id).await {
                Ok(_) => {
                    info!("Expired volume {} cleaned up successfully", volume_id);
                    deleted_count += 1;
                }
                Err(e) => {
                    error!("Failed to clean up expired volume {}: {}", volume_id, e);
                    // Continue with other volumes even if one fails
                }
            }
        }

        info!("Cleanup completed: {}/{} expired volumes deleted", deleted_count, count);
        Ok(deleted_count)
    }
    
    async fn create_volumes_for_execution(
        &self,
        execution_id: ExecutionId,
        tenant_id: TenantId,
        volume_specs: &[VolumeSpec],
        storage_mode: &str,
        fallback_to_local: bool,
        _local_path: &str, // Prefixed with _ to suppress unused warning
    ) -> Result<Vec<Volume>> {
        if volume_specs.is_empty() {
            return Ok(Vec::new());
        }
        
        info!(
            "Creating {} volumes for execution {} (storage_mode: {}, fallback: {})",
            volume_specs.len(),
            execution_id,
            storage_mode,
            fallback_to_local
        );
        
        let mut volumes = Vec::new();
        
        for spec in volume_specs {
            // Parse size limit from string (e.g., "1Gi", "500Mi")
            let size_limit_bytes = parse_size_string(&spec.size_limit)
                .context(format!("Invalid size_limit '{}' in volume spec '{}'", spec.size_limit, spec.name))?;
            
            // Parse storage class
            let storage_class = match spec.storage_class.as_str() {
                "ephemeral" => {
                    let ttl_hours = spec.ttl_hours.unwrap_or(24) as i64;
                    StorageClass::ephemeral_hours(ttl_hours)
                }
                "persistent" => StorageClass::persistent(),
                other => {
                    return Err(anyhow::anyhow!(
                        "Invalid storage_class '{}' in volume spec '{}'. Expected 'ephemeral' or 'persistent'",
                        other,
                        spec.name
                    ));
                }
            };
            
            // Create volume ownership tied to execution
            let ownership = VolumeOwnership::execution(execution_id);
            
            // Attempt to create volume
            let volume_id = match storage_mode {
                "seaweedfs" => {
                    // Try SeaweedFS first
                    match self
                        .create_volume(
                            spec.name.clone(),
                            tenant_id,
                            storage_class.clone(),
                            size_limit_bytes / (1024 * 1024), // Convert bytes to MB
                            ownership.clone(),
                        )
                        .await
                    {
                        Ok(id) => id,
                        Err(e) if fallback_to_local => {
                            warn!(
                                "SeaweedFS volume creation failed for '{}', falling back to local storage: {}",
                                spec.name, e
                            );
                            // Create local volume instead (not using SeaweedFS adapter)
                            return Err(anyhow::anyhow!(
                                "Local storage fallback not yet implemented. SeaweedFS error: {}",
                                e
                            ));
                        }
                        Err(e) => {
                            return Err(anyhow::anyhow!(
                                "SeaweedFS volume creation failed for '{}' and fallback is disabled: {}",
                                spec.name,
                                e
                            ));
                        }
                    }
                }
                "local" => {
                    // Local storage mode (not using SeaweedFS)
                    warn!("Local storage mode not yet fully implemented, using SeaweedFS adapter anyway");
                    self.create_volume(
                        spec.name.clone(),
                        tenant_id,
                        storage_class.clone(),
                        size_limit_bytes / (1024 * 1024),
                        ownership.clone(),
                    )
                    .await?
                }
                other => {
                    return Err(anyhow::anyhow!(
                        "Invalid storage mode '{}'. Expected 'seaweedfs' or 'local'",
                        other
                    ));
                }
            };
            
            // Fetch created volume
            let volume = self.get_volume(volume_id).await?;
            volumes.push(volume);
            
            info!(
                "Volume '{}' created successfully (id: {}, size: {} bytes)",
                spec.name, volume_id, size_limit_bytes
            );
        }
        
        info!(
            "Successfully created {} volumes for execution {}",
            volumes.len(),
            execution_id
        );
        
        Ok(volumes)
    }
}

/// Parse size string like "1Gi", "500Mi", "100Ki" to bytes
fn parse_size_string(size: &str) -> Result<u64> {
    let size = size.trim();
    
    // Find where digits end and unit begins
    let (number_part, unit_part) = size
        .split_at(
            size
                .find(|c: char| c.is_alphabetic())
                .unwrap_or(size.len())
        );
    
    let number: u64 = number_part
        .trim()
        .parse()
        .context(format!("Invalid number in size '{}'", size))?;
    
    let multiplier: u64 = match unit_part.trim().to_lowercase().as_str() {
        "ki" => 1024,
        "mi" => 1024 * 1024,
        "gi" => 1024 * 1024 * 1024,
        "ti" => 1024 * 1024 * 1024 * 1024,
        "k" => 1000,
        "m" => 1000 * 1000,
        "g" => 1000 * 1000 * 1000,
        "t" => 1000 * 1000 * 1000 * 1000,
        "" => 1, // No unit = bytes
        unknown => {
            return Err(anyhow::anyhow!(
                "Unknown size unit '{}'. Supported: Ki, Mi, Gi, Ti (binary) or K, M, G, T (decimal)",
                unknown
            ));
        }
    };
    
    Ok(number * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::repository::RepositoryError;
    use crate::infrastructure::storage::MockStorageProvider;
    use crate::domain::execution::ExecutionId;
    use std::collections::HashMap;
    use tokio::sync::Mutex;
    use chrono::Duration;

    // Mock VolumeRepository for testing
    struct MockVolumeRepository {
        volumes: Arc<Mutex<HashMap<VolumeId, Volume>>>,
    }

    impl MockVolumeRepository {
        fn new() -> Self {
            Self {
                volumes: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    #[async_trait]
    impl VolumeRepository for MockVolumeRepository {
        async fn save(&self, volume: &Volume) -> Result<(), RepositoryError> {
            let mut volumes = self.volumes.lock().await;
            volumes.insert(volume.id, volume.clone());
            Ok(())
        }

        async fn find_by_id(&self, id: VolumeId) -> Result<Option<Volume>, RepositoryError> {
            let volumes = self.volumes.lock().await;
            Ok(volumes.get(&id).cloned())
        }

        async fn find_by_tenant(&self, tenant_id: TenantId) -> Result<Vec<Volume>, RepositoryError> {
            let volumes = self.volumes.lock().await;
            Ok(volumes
                .values()
                .filter(|v| v.tenant_id == tenant_id)
                .cloned()
                .collect())
        }

        async fn find_expired(&self) -> Result<Vec<Volume>, RepositoryError> {
            let volumes = self.volumes.lock().await;
            Ok(volumes
                .values()
                .filter(|v| v.is_expired())
                .cloned()
                .collect())
        }

        async fn find_by_ownership(&self, ownership: &VolumeOwnership) -> Result<Vec<Volume>, RepositoryError> {
            let volumes = self.volumes.lock().await;
            Ok(volumes
                .values()
                .filter(|v| &v.ownership == ownership)
                .cloned()
                .collect())
        }

        async fn delete(&self, id: VolumeId) -> Result<(), RepositoryError> {
            let mut volumes = self.volumes.lock().await;
            volumes.remove(&id);
            Ok(())
        }
    }

    fn create_test_service() -> (StandardVolumeService, Arc<MockVolumeRepository>, Arc<MockStorageProvider>) {
        let repository = Arc::new(MockVolumeRepository::new());
        let storage_provider = Arc::new(MockStorageProvider::new());
        let event_bus = Arc::new(EventBus::with_default_capacity());
        
        let service = StandardVolumeService::new(
            repository.clone(),
            storage_provider.clone(),
            event_bus,
            "http://localhost:8888".to_string(),
        ).expect("Failed to create test service");

        (service, repository, storage_provider)
    }

    #[tokio::test]
    async fn test_create_volume_success() {
        let (service, repository, storage_provider) = create_test_service();

        let tenant_id = TenantId::default();
        let volume_id = service
            .create_volume(
                "test-volume".to_string(),
                tenant_id,
                StorageClass::ephemeral_hours(6),
                100,
                VolumeOwnership::Execution {
                    execution_id: ExecutionId::new(),
                },
            )
            .await
            .expect("Failed to create volume");

        // Verify volume saved to repository
        let volume = repository
            .find_by_id(volume_id)
            .await
            .expect("Repository error")
            .expect("Volume not found");

        assert_eq!(volume.name, "test-volume");
        assert_eq!(volume.tenant_id, tenant_id);
        assert_eq!(volume.size_limit_bytes, 100 * 1024 * 1024);

        // Verify storage provider calls
        let directories = storage_provider.directories.lock().unwrap();
        let remote_path = format!("/aegis/volumes/{}/{}", tenant_id, volume_id);
        assert!(directories.contains_key(&remote_path));
    }

    #[tokio::test]
    async fn test_attach_detach_volume() {
        let (service, _repository, _storage_provider) = create_test_service();

        // Create volume
        let volume_id = service
            .create_volume(
                "test-volume".to_string(),
                TenantId::default(),
                StorageClass::persistent(),
                100,
                VolumeOwnership::Persistent {
                    owner: "test-owner".to_string(),
                },
            )
            .await
            .expect("Failed to create volume");

        // Attach volume
        let instance_id = InstanceId::new("test-instance-001".to_string());
        let mount_point = PathBuf::from("/workspace");
        let volume_mount = service
            .attach_volume(volume_id, instance_id.clone(), mount_point.clone(), AccessMode::ReadWrite)
            .await
            .expect("Failed to attach volume");

        assert_eq!(volume_mount.volume_id, volume_id);
        assert_eq!(volume_mount.mount_point, mount_point);
        assert_eq!(volume_mount.access_mode, AccessMode::ReadWrite);

        // Detach volume
        service
            .detach_volume(volume_id, instance_id)
            .await
            .expect("Failed to detach volume");

        // Verify volume state
        let volume = service.get_volume(volume_id).await.expect("Volume not found");
        assert!(volume.can_attach(), "Volume should be attachable after detach");
    }

    #[tokio::test]
    async fn test_delete_volume() {
        let (service, repository, storage_provider) = create_test_service();

        // Create volume
        let volume_id = service
            .create_volume(
                "test-volume".to_string(),
                TenantId::default(),
                StorageClass::persistent(),
                100,
                VolumeOwnership::Persistent {
                    owner: "test-owner".to_string(),
                },
            )
            .await
            .expect("Failed to create volume");

        let remote_path = format!("/aegis/volumes/{}/{}", TenantId::default(), volume_id);

        // Delete volume
        service
            .delete_volume(volume_id)
            .await
            .expect("Failed to delete volume");

        // Verify volume marked as deleted
        let volume = repository
            .find_by_id(volume_id)
            .await
            .expect("Repository error")
            .expect("Volume not found");
        
        // Volume should be in Deleted state
        assert!(!volume.can_attach(), "Deleted volume should not be attachable");

        // Verify storage provider deleted directory
        let directories = storage_provider.directories.lock().unwrap();
        assert!(!directories.contains_key(&remote_path), "Directory should be deleted");
    }

    #[tokio::test]
    async fn test_cleanup_expired_volumes() {
        let (service, _repository, _storage_provider) = create_test_service();

        // Create ephemeral volume with 1 second TTL
        let volume_id = service
            .create_volume(
                "ephemeral-volume".to_string(),
                TenantId::default(),
                StorageClass::Ephemeral {
                    ttl: Duration::seconds(1),
                },
                100,
                VolumeOwnership::Execution {
                    execution_id: ExecutionId::new(),
                },
            )
            .await
            .expect("Failed to create volume");

        // Wait for TTL to expire
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Run cleanup
        let count = service
            .cleanup_expired_volumes()
            .await
            .expect("Cleanup failed");

        assert_eq!(count, 1, "Should have cleaned up 1 expired volume");

        // Verify volume deleted
        let volume = service.get_volume(volume_id).await.expect("Volume not found");
        assert!(!volume.can_attach(), "Expired volume should be deleted");
    }
}
