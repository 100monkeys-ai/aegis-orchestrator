// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Integration tests for NFS Server Gateway (ADR-036)
//!
//! These tests verify:
//! 1. NFS server lifecycle (start/stop)
//! 2. Health check functionality
//! 3. FSAL security enforcement (path sanitization, mode validation)
//! 4. File operation audit trail (StorageEvent publishing)
//!
//! Note: Full NFS protocol testing requires mounting volumes and is beyond
//! the scope of these unit/integration tests. These focus on service layer.
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** Implements internal responsibilities for nfs gateway integration tests

use aegis_orchestrator_core::application::nfs_gateway::{EventBusPublisher, NfsGatewayService};
use aegis_orchestrator_core::domain::events::StorageEvent;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::fsal::{
    AegisFSAL, AegisFileHandle, BorrowedVolumeAccess, EventPublisher, FsalAccessPolicy, FsalError,
};
use aegis_orchestrator_core::domain::repository::{RepositoryError, VolumeRepository};
use aegis_orchestrator_core::domain::storage::{
    DirEntry, FileAttributes, FileHandle, FileType, OpenMode, StorageError, StorageProvider,
};
use aegis_orchestrator_core::domain::volume::{
    FilerEndpoint, StorageClass, TenantId, Volume, VolumeBackend, VolumeId, VolumeOwnership,
};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use async_trait::async_trait;
use chrono::Utc;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

// ── Test helper ─────────────────────────────────────────────────────────────

/// Create an Attached volume owned by the given execution and persist it in the repository.
/// Returns `(volume_id, execution_id)` ready for use with `AegisFSAL`.
async fn create_attached_test_volume(
    repository: &Arc<dyn VolumeRepository>,
    execution_id: ExecutionId,
    quota_bytes: u64,
) -> VolumeId {
    let mut volume = Volume::new(
        "test-volume".to_string(),
        TenantId::default(),
        StorageClass::persistent(),
        VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
            remote_path: "/volume-path".to_string(),
        },
        quota_bytes,
        VolumeOwnership::Execution { execution_id },
    )
    .unwrap();
    volume.mark_available().unwrap();
    volume.mark_attached().unwrap();
    let volume_id = volume.id;
    repository.save(&volume).await.unwrap();
    volume_id
}

// Test Storage Provider for testing
struct TestStorageProvider;

#[async_trait]
impl StorageProvider for TestStorageProvider {
    async fn open_file(&self, path: &str, _mode: OpenMode) -> Result<FileHandle, StorageError> {
        Ok(FileHandle(path.as_bytes().to_vec()))
    }

    async fn read_at(
        &self,
        _handle: &FileHandle,
        _offset: u64,
        count: usize,
    ) -> Result<Vec<u8>, StorageError> {
        Ok(vec![0u8; count])
    }

    async fn write_at(
        &self,
        _handle: &FileHandle,
        _offset: u64,
        data: &[u8],
    ) -> Result<usize, StorageError> {
        Ok(data.len())
    }

    async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
        Ok(())
    }

    async fn stat(&self, _path: &str) -> Result<FileAttributes, StorageError> {
        Ok(FileAttributes {
            file_type: FileType::File,
            size: 1024,
            mtime: Utc::now().timestamp(),
            atime: Utc::now().timestamp(),
            ctime: Utc::now().timestamp(),
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            nlink: 1,
        })
    }

    async fn readdir(&self, _path: &str) -> Result<Vec<DirEntry>, StorageError> {
        Ok(vec![
            DirEntry {
                name: "file1.txt".to_string(),
                file_type: FileType::File,
            },
            DirEntry {
                name: "file2.txt".to_string(),
                file_type: FileType::File,
            },
        ])
    }

    async fn create_file(&self, path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
        Ok(FileHandle(path.as_bytes().to_vec()))
    }

    async fn delete_file(&self, _path: &str) -> Result<(), StorageError> {
        Ok(())
    }

    async fn create_directory(&self, _path: &str) -> Result<(), StorageError> {
        Ok(())
    }

    async fn delete_directory(&self, _path: &str) -> Result<(), StorageError> {
        Ok(())
    }

    async fn set_quota(&self, _path: &str, _limit_bytes: u64) -> Result<(), StorageError> {
        Ok(())
    }

    async fn get_usage(&self, _path: &str) -> Result<u64, StorageError> {
        Ok(5120)
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn rename(&self, _from: &str, _to: &str) -> Result<(), StorageError> {
        Ok(())
    }
}

// Test Volume Repository
struct TestVolumeRepository {
    volumes: Arc<tokio::sync::RwLock<Vec<Volume>>>,
}

impl TestVolumeRepository {
    fn new() -> Self {
        Self {
            volumes: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        }
    }

    async fn add_volume(&self, volume: Volume) {
        self.volumes.write().await.push(volume);
    }
}

fn empty_borrowed_volume_registry() -> Arc<RwLock<HashMap<VolumeId, BorrowedVolumeAccess>>> {
    Arc::new(RwLock::new(HashMap::new()))
}

#[async_trait]
impl VolumeRepository for TestVolumeRepository {
    async fn save(&self, volume: &Volume) -> Result<(), RepositoryError> {
        self.add_volume(volume.clone()).await;
        Ok(())
    }

    async fn find_by_id(&self, id: VolumeId) -> Result<Option<Volume>, RepositoryError> {
        let volumes = self.volumes.read().await;
        Ok(volumes.iter().find(|v| v.id == id).cloned())
    }

    async fn find_by_tenant(&self, _tenant_id: TenantId) -> Result<Vec<Volume>, RepositoryError> {
        Ok(self.volumes.read().await.clone())
    }

    async fn find_expired(&self) -> Result<Vec<Volume>, RepositoryError> {
        Ok(Vec::new())
    }

    async fn find_by_ownership(
        &self,
        _ownership: &VolumeOwnership,
    ) -> Result<Vec<Volume>, RepositoryError> {
        Ok(Vec::new())
    }

    async fn delete(&self, id: VolumeId) -> Result<(), RepositoryError> {
        let mut volumes = self.volumes.write().await;
        volumes.retain(|v| v.id != id);
        Ok(())
    }
}

#[tokio::test]
async fn test_nfs_gateway_lifecycle() {
    // Create test dependencies
    let storage_provider = Arc::new(TestStorageProvider) as Arc<dyn StorageProvider>;
    let volume_repository = Arc::new(TestVolumeRepository::new()) as Arc<dyn VolumeRepository>;
    let event_bus = Arc::new(EventBus::new(1000));
    let event_publisher =
        Arc::new(EventBusPublisher::new(event_bus.clone())) as Arc<dyn EventPublisher>;

    // Create NFS gateway service (use non-standard port for testing)
    let gateway = NfsGatewayService::new(
        storage_provider,
        volume_repository,
        event_publisher,
        Some(12049), // Test port (not standard 2049)
    );

    // Initially not running
    assert!(!gateway.is_running());
    assert_eq!(gateway.bind_port(), 12049);

    // Start server
    let result = gateway.start_server().await;
    // Note: This may fail in CI without network permissions; in that case we still
    // assert that the error is reported, while in local runs we exercise the full
    // lifecycle (start -> health_check -> stop).
    match result {
        Ok(_) => {
            // On successful start, the gateway must report as running.
            assert!(
                gateway.is_running(),
                "gateway should be running after start_server()"
            );

            // Health check should be callable and succeed for a running gateway.
            let health = gateway.health_check().await;
            assert!(
                health.is_ok(),
                "expected health_check() to succeed for running gateway, got {:?}",
                health
            );

            // Stop server should succeed.
            let stop_result = gateway.stop_server().await;
            assert!(
                stop_result.is_ok(),
                "expected stop_server() to succeed, got {:?}",
                stop_result
            );
        }
        Err(e) => {
            // In constrained environments (e.g., CI without network permissions),
            // we at least assert that start_server reports a failure instead of
            // silently ignoring it.
            eprintln!(
                "start_server() failed in test_nfs_gateway_lifecycle: {:?}",
                e
            );
            assert!(
                !gateway.is_running(),
                "gateway should not report running when start_server() fails"
            );
        }
    }
}

#[tokio::test]
async fn test_fsal_mode_validation() {
    // Create FSAL with test dependencies
    let storage_provider = Arc::new(TestStorageProvider) as Arc<dyn StorageProvider>;
    let volume_repository = Arc::new(TestVolumeRepository::new()) as Arc<dyn VolumeRepository>;
    let event_bus = Arc::new(EventBus::new(1000));
    let event_publisher =
        Arc::new(EventBusPublisher::new(event_bus.clone())) as Arc<dyn EventPublisher>;

    let fsal = AegisFSAL::new(
        storage_provider.clone(),
        volume_repository.clone(),
        empty_borrowed_volume_registry(),
        event_publisher.clone(),
    );

    // Create NFS gateway service to mirror production export registration
    let gateway = NfsGatewayService::new(
        storage_provider,
        volume_repository.clone(),
        event_publisher,
        None, // Use default or ephemeral port for test; server is not started
    );

    let execution_id = ExecutionId::new();
    let volume_id =
        create_attached_test_volume(&volume_repository, execution_id, 1024 * 1024).await;

    let policy = FsalAccessPolicy {
        read: vec!["/workspace/*".to_string()],
        write: vec![], // Read-only policy
    };

    // Register the volume with the NFS gateway to establish export context
    gateway.register_volume(
        volume_id,
        execution_id,
        0,
        0,
        policy.clone(),
        std::path::PathBuf::from("/workspace"),
        String::new(),
    );

    let path = "/workspace/test.txt";
    let handle =
        aegis_orchestrator_core::domain::fsal::AegisFileHandle::new(execution_id, volume_id, path);

    // Test: Read from allowed file (should succeed)
    let read_result = fsal.read(&handle, path, &policy, 0, 100).await;
    assert!(read_result.is_ok());

    // Test: Write to read-only file (should fail)
    let write_result = fsal.write(&handle, path, &policy, 0, b"test").await;
    assert!(write_result.is_err());
    if let Err(FsalError::PolicyViolation(msg)) = write_result {
        assert!(msg.contains("Write not allowed"));
    }
}

#[tokio::test]
async fn test_fsal_path_traversal_prevention() {
    // Create FSAL with test dependencies
    let storage_provider = Arc::new(TestStorageProvider) as Arc<dyn StorageProvider>;
    let volume_repository = Arc::new(TestVolumeRepository::new()) as Arc<dyn VolumeRepository>;
    let event_bus = Arc::new(EventBus::new(1000));
    let event_publisher =
        Arc::new(EventBusPublisher::new(event_bus.clone())) as Arc<dyn EventPublisher>;

    let fsal = AegisFSAL::new(
        storage_provider.clone(),
        volume_repository.clone(),
        empty_borrowed_volume_registry(),
        event_publisher.clone(),
    );

    // Create NFS gateway service and register the test volume to mirror production lifecycle
    let nfs_gateway = NfsGatewayService::new(
        storage_provider,
        volume_repository.clone(),
        event_publisher,
        None,
    );

    // Create test volume (path traversal check occurs after authorization, so volume must be attached)
    let execution_id = ExecutionId::new();
    let volume_id =
        create_attached_test_volume(&volume_repository, execution_id, 1024 * 1024).await;

    let policy = FsalAccessPolicy {
        read: vec!["/workspace/*".to_string()],
        write: vec!["/workspace/*".to_string()],
    };

    // Register the attached volume with the NFS gateway before performing FSAL operations
    nfs_gateway.register_volume(
        volume_id,
        execution_id,
        0,
        0,
        policy.clone(),
        std::path::PathBuf::from("/workspace"),
        String::new(),
    );

    // Test: Attempt path traversal attack
    let handle = aegis_orchestrator_core::domain::fsal::AegisFileHandle::new(
        execution_id,
        volume_id,
        "/workspace/../etc/passwd",
    );
    let traversal_result = fsal
        .read(
            &handle,
            "/workspace/../etc/passwd", // Path traversal attempt
            &policy,
            0,
            100,
        )
        .await;

    // Should be rejected by path sanitizer
    assert!(traversal_result.is_err());
    if let Err(FsalError::PathSanitization(err)) = traversal_result {
        assert!(err.to_string().contains("traversal"));
    }
}

#[tokio::test]
async fn test_fsal_policy_enforcement() {
    // Create FSAL with test dependencies
    let storage_provider = Arc::new(TestStorageProvider) as Arc<dyn StorageProvider>;
    let volume_repository = Arc::new(TestVolumeRepository::new()) as Arc<dyn VolumeRepository>;
    let event_bus = Arc::new(EventBus::new(1000));
    let event_publisher =
        Arc::new(EventBusPublisher::new(event_bus.clone())) as Arc<dyn EventPublisher>;

    let fsal = AegisFSAL::new(
        storage_provider.clone(),
        volume_repository.clone(),
        empty_borrowed_volume_registry(),
        event_publisher.clone(),
    );

    let execution_id = ExecutionId::new();
    let volume_id =
        create_attached_test_volume(&volume_repository, execution_id, 1024 * 1024).await;

    // Policy: Only allow /workspace/data/* files
    let policy = FsalAccessPolicy {
        read: vec!["/workspace/data/*".to_string()],
        write: vec!["/workspace/data/*".to_string()],
    };

    // Test: Attempt to write outside allowlist
    let handle1 = aegis_orchestrator_core::domain::fsal::AegisFileHandle::new(
        execution_id,
        volume_id,
        "/workspace/config.yaml",
    );
    let denied_result = fsal
        .read(
            &handle1,
            "/workspace/config.yaml", // Not in /workspace/data/*
            &policy,
            0,
            100,
        )
        .await;

    // Should be rejected by policy enforcement
    assert!(denied_result.is_err());
    if let Err(FsalError::PolicyViolation(msg)) = denied_result {
        assert!(msg.contains("Read not allowed"));
    }

    // Test: Open file within allowlist
    let handle2 = aegis_orchestrator_core::domain::fsal::AegisFileHandle::new(
        execution_id,
        volume_id,
        "/workspace/data/file.txt",
    );
    let allowed_result = fsal
        .read(&handle2, "/workspace/data/file.txt", &policy, 0, 100)
        .await;

    assert!(allowed_result.is_ok());
}

#[tokio::test]
async fn test_fsal_audit_events() {
    // Create FSAL with event bus
    let storage_provider = Arc::new(TestStorageProvider) as Arc<dyn StorageProvider>;
    let volume_repository = Arc::new(TestVolumeRepository::new()) as Arc<dyn VolumeRepository>;
    let event_bus = Arc::new(EventBus::new(1000));
    let event_publisher =
        Arc::new(EventBusPublisher::new(event_bus.clone())) as Arc<dyn EventPublisher>;

    let fsal = AegisFSAL::new(
        storage_provider.clone(),
        volume_repository.clone(),
        empty_borrowed_volume_registry(),
        event_publisher.clone(),
    );

    let execution_id = ExecutionId::new();
    let volume_id =
        create_attached_test_volume(&volume_repository, execution_id, 1024 * 1024).await;

    let policy = FsalAccessPolicy {
        read: vec!["/workspace/*".to_string()],
        write: vec!["/workspace/*".to_string()],
    };

    // Subscribe to domain events (filter for storage events)
    let mut event_rx = event_bus.subscribe();

    // Create, write file
    let path = "/workspace/test.txt";
    let handle = fsal
        .create_file(execution_id, volume_id, path, &policy, true)
        .await
        .unwrap();

    // Give event bus time to process
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Check for FileCreated event
    if let Ok(DomainEvent::Storage(StorageEvent::FileCreated { path, .. })) = event_rx.try_recv() {
        assert!(path.contains("test.txt"));
    }

    // Write file
    let _ = fsal
        .write(&handle, path, &policy, 0, b"data")
        .await
        .unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Check for FileWritten event
    if let Ok(DomainEvent::Storage(StorageEvent::FileWritten { path, .. })) = event_rx.try_recv() {
        assert!(path.contains("test.txt"));
    }
}

#[tokio::test]
async fn test_fsal_quota_enforcement() {
    // Create FSAL with test dependencies
    let storage_provider = Arc::new(TestStorageProvider) as Arc<dyn StorageProvider>;
    let volume_repository = Arc::new(TestVolumeRepository::new()) as Arc<dyn VolumeRepository>;
    let event_bus = Arc::new(EventBus::new(1000));
    let event_publisher =
        Arc::new(EventBusPublisher::new(event_bus.clone())) as Arc<dyn EventPublisher>;

    let fsal = AegisFSAL::new(
        storage_provider,
        volume_repository.clone(),
        empty_borrowed_volume_registry(),
        event_publisher,
    );

    // Create test volume with a very small quota (1KB)
    let execution_id = ExecutionId::new();
    let volume_id = create_attached_test_volume(&volume_repository, execution_id, 1024).await;

    let policy = FsalAccessPolicy {
        read: vec!["/workspace/*".to_string()],
        write: vec!["/workspace/*".to_string()],
    };

    // Subscribe to domain events to verify QuotaExceeded event is published
    let mut event_rx = event_bus.subscribe();

    // Open file for writing
    let path = "/workspace/large_file.txt";
    let handle =
        aegis_orchestrator_core::domain::fsal::AegisFileHandle::new(execution_id, volume_id, path);

    // TestStorageProvider.get_usage() returns 5120 bytes (5KB)
    // Volume quota is 1024 bytes (1KB)
    // Any write attempt should fail with QuotaExceeded

    // Attempt to write 1KB of data (should fail due to quota)
    let large_data = vec![0u8; 1024];
    let write_result = fsal.write(&handle, path, &policy, 0, &large_data).await;

    // Verify write was rejected with QuotaExceeded error
    assert!(write_result.is_err());
    match write_result {
        Err(FsalError::QuotaExceeded {
            requested_bytes,
            available_bytes,
        }) => {
            // Current usage: 5120 bytes
            // Quota: 1024 bytes
            // Available should be 0 (quota already exceeded by existing usage)
            assert_eq!(requested_bytes, 1024);
            assert_eq!(available_bytes, 0); // Quota already exceeded (5120 > 1024)
        }
        other => {
            panic!("Expected QuotaExceeded error, got: {other:?}");
        }
    }

    // Verify QuotaExceeded event was published
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let mut found_quota_event = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(
            event,
            DomainEvent::Storage(StorageEvent::QuotaExceeded { .. })
        ) {
            found_quota_event = true;
            break;
        }
    }
    assert!(
        found_quota_event,
        "QuotaExceeded event should have been published"
    );
}

#[tokio::test]
async fn test_gateway_borrowed_volume_alias_allows_judge_read_only_access() {
    let storage_provider = Arc::new(TestStorageProvider) as Arc<dyn StorageProvider>;
    let volume_repository = Arc::new(TestVolumeRepository::new()) as Arc<dyn VolumeRepository>;
    let event_bus = Arc::new(EventBus::new(1000));
    let event_publisher =
        Arc::new(EventBusPublisher::new(event_bus.clone())) as Arc<dyn EventPublisher>;
    let gateway = NfsGatewayService::new(
        storage_provider,
        volume_repository.clone(),
        event_publisher,
        Some(12050),
    );

    let worker_execution_id = ExecutionId::new();
    let judge_execution_id = ExecutionId::new();
    let mut source_volume = Volume::new(
        "worker-workspace".to_string(),
        TenantId::default(),
        StorageClass::persistent(),
        VolumeBackend::OpenDal {
            provider: "memory".to_string(),
            config: None,
            cache_path: None,
        },
        1024 * 1024,
        VolumeOwnership::Execution {
            execution_id: worker_execution_id,
        },
    )
    .unwrap();
    source_volume.mark_available().unwrap();
    source_volume.mark_attached().unwrap();
    volume_repository.save(&source_volume).await.unwrap();
    let borrowed_alias_id = VolumeId::new();

    gateway.register_borrowed_volume(borrowed_alias_id, judge_execution_id, source_volume.clone());
    gateway.register_volume(
        borrowed_alias_id,
        judge_execution_id,
        1000,
        1000,
        FsalAccessPolicy {
            read: vec!["/workspace/**".to_string()],
            write: vec![],
        },
        "/workspace".into(),
        String::new(),
    );

    let policy = FsalAccessPolicy {
        read: vec!["/workspace/**".to_string()],
        write: vec![],
    };
    let handle = AegisFileHandle::new(
        judge_execution_id,
        borrowed_alias_id,
        "/workspace/review.txt",
    );

    let read_result = gateway
        .fsal()
        .read(&handle, "/workspace/review.txt", &policy, 0, 64)
        .await;
    assert!(
        read_result.is_ok(),
        "judge should read inherited worker volume: {:?}",
        read_result
    );

    let write_result = gateway
        .fsal()
        .write(&handle, "/workspace/review.txt", &policy, 0, b"mutate")
        .await;
    assert!(matches!(write_result, Err(FsalError::PolicyViolation(_))));
}

#[tokio::test]
async fn test_gateway_borrowed_volume_alias_rejects_wrong_execution() {
    let storage_provider = Arc::new(TestStorageProvider) as Arc<dyn StorageProvider>;
    let volume_repository = Arc::new(TestVolumeRepository::new()) as Arc<dyn VolumeRepository>;
    let event_bus = Arc::new(EventBus::new(1000));
    let event_publisher =
        Arc::new(EventBusPublisher::new(event_bus.clone())) as Arc<dyn EventPublisher>;
    let gateway = NfsGatewayService::new(
        storage_provider,
        volume_repository.clone(),
        event_publisher,
        Some(12051),
    );

    let worker_execution_id = ExecutionId::new();
    let judge_execution_id = ExecutionId::new();
    let wrong_execution_id = ExecutionId::new();
    let source_volume_id =
        create_attached_test_volume(&volume_repository, worker_execution_id, 1024 * 1024).await;
    let source_volume = volume_repository
        .find_by_id(source_volume_id)
        .await
        .unwrap()
        .expect("source volume should exist");
    let borrowed_alias_id = VolumeId::new();

    gateway.register_borrowed_volume(borrowed_alias_id, judge_execution_id, source_volume);
    gateway.register_volume(
        borrowed_alias_id,
        judge_execution_id,
        1000,
        1000,
        FsalAccessPolicy {
            read: vec!["/workspace/**".to_string()],
            write: vec![],
        },
        "/workspace".into(),
        String::new(),
    );

    let handle = AegisFileHandle::new(
        wrong_execution_id,
        borrowed_alias_id,
        "/workspace/review.txt",
    );
    let read_result = gateway
        .fsal()
        .read(
            &handle,
            "/workspace/review.txt",
            &FsalAccessPolicy {
                read: vec!["/workspace/**".to_string()],
                write: vec![],
            },
            0,
            64,
        )
        .await;

    assert!(matches!(
        read_result,
        Err(FsalError::UnauthorizedAccess {
            execution_id,
            volume_id
        }) if execution_id == wrong_execution_id && volume_id == borrowed_alias_id
    ));
}
