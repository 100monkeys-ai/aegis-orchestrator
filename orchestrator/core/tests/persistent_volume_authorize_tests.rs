// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A person's persistent volume and the execution registered for it
//!
//! A git repository binding's clone volume is a person's persistent volume.
//! The clone step, and a run that mounts the binding, register their own
//! execution for that volume in the gateway's volume registry, and the FUSE
//! gateway then asks the FSAL to authorise that execution. These tests hold
//! the FSAL to admitting exactly the execution the registry holds for a
//! persistent volume, and only while that registration holds; every other
//! execution, and every unregistered persistent volume, is refused.
//!
//! The calls go through `AegisFSAL::create_directory`, the path a FUSE
//! `mkdir` takes (the clone's `could not create work tree dir` was this call
//! refused).
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** Regression tests for persistent volume authorisation

use aegis_orchestrator_core::application::nfs_gateway::{NfsVolumeRegistry, VolumeRegistration};
use aegis_orchestrator_core::domain::events::StorageEvent;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::fsal::{
    AegisFSAL, EventPublisher, FsalAccessPolicy, FsalError,
};
use aegis_orchestrator_core::domain::repository::VolumeRepository;
use aegis_orchestrator_core::domain::volume::{
    FilerEndpoint, StorageClass, TenantId, Volume, VolumeBackend, VolumeId, VolumeOwnership,
};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

struct NoEvents;

#[async_trait]
impl EventPublisher for NoEvents {
    async fn publish_storage_event(&self, _: StorageEvent) {}
}

fn all_paths() -> FsalAccessPolicy {
    FsalAccessPolicy {
        read: vec!["/*".to_string()],
        write: vec!["/*".to_string()],
    }
}

/// A person's persistent volume, the registry, and the FSAL reading it.
struct Fixture {
    _root: tempfile::TempDir,
    volume_id: VolumeId,
    registry: NfsVolumeRegistry,
    fsal: AegisFSAL,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut volume = Volume::new(
            "git-repo-binding".to_string(),
            TenantId::default(),
            StorageClass::persistent(),
            VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
                remote_path: "/binding".to_string(),
            },
            500 * 1024 * 1024,
            VolumeOwnership::persistent("alice"),
        )
        .unwrap();
        volume.mark_available().unwrap();
        let volume_id = volume.id;

        let repository = Arc::new(InMemoryVolumeRepository::new());
        repository.save(&volume).await.unwrap();

        let registry = NfsVolumeRegistry::new();
        let fsal = AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(root.path()).unwrap()),
            repository as Arc<dyn VolumeRepository>,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(NoEvents),
        )
        .with_volume_context_lookup(Arc::new(registry.clone()));

        Self {
            _root: root,
            volume_id,
            registry,
            fsal,
        }
    }

    /// Registers `execution_id` for the volume, as the clone step does.
    fn register(&self, execution_id: ExecutionId) {
        self.registry.register(VolumeRegistration {
            volume_id: self.volume_id,
            execution_id,
            workflow_execution_id: None,
            container_uid: 0,
            container_gid: 0,
            policy: FsalAccessPolicy::default(),
            mount_point: PathBuf::from("/workspace"),
            remote_path: "/binding".to_string(),
        });
    }

    /// The FUSE gateway's `mkdir` of `/repo` for `execution_id`.
    async fn mkdir(&self, execution_id: ExecutionId) -> Result<(), FsalError> {
        self.fsal
            .create_directory(
                execution_id,
                self.volume_id,
                "/repo",
                &all_paths(),
                None,
                None,
                None,
            )
            .await
    }
}

/// The execution the registry holds for a persistent volume is admitted.
#[tokio::test]
async fn registered_execution_is_admitted_to_a_persistent_volume() {
    let fixture = Fixture::new().await;
    let clone_step = ExecutionId::new();
    fixture.register(clone_step);

    let result = fixture.mkdir(clone_step).await;
    assert!(
        result.is_ok(),
        "the execution registered for the persistent volume was refused: {result:?}"
    );
}

/// Any other execution is refused while the volume is registered to one.
#[tokio::test]
async fn another_execution_is_refused_a_registered_persistent_volume() {
    let fixture = Fixture::new().await;
    fixture.register(ExecutionId::new());

    let result = fixture.mkdir(ExecutionId::new()).await;
    assert!(
        matches!(result, Err(FsalError::UnauthorizedAccess { .. })),
        "an execution not registered for the persistent volume was admitted: {result:?}"
    );
}

/// A persistent volume no execution is registered for is refused.
#[tokio::test]
async fn unregistered_persistent_volume_is_refused() {
    let fixture = Fixture::new().await;

    let result = fixture.mkdir(ExecutionId::new()).await;
    assert!(
        matches!(result, Err(FsalError::UnauthorizedAccess { .. })),
        "an unregistered persistent volume was admitted: {result:?}"
    );
}

/// The admission ends with the registration.
#[tokio::test]
async fn admission_ends_when_the_registration_is_removed() {
    let fixture = Fixture::new().await;
    let clone_step = ExecutionId::new();
    fixture.register(clone_step);
    fixture.registry.deregister(fixture.volume_id);

    let result = fixture.mkdir(clone_step).await;
    assert!(
        matches!(result, Err(FsalError::UnauthorizedAccess { .. })),
        "a persistent volume was admitted after its registration was removed: {result:?}"
    );
}
