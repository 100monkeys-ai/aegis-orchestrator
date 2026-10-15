// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # The volume file API's errors carry the caller's path, never storage's
//!
//! Regression (volume-files-listing-500-discloses-internal-path-2026-10-15):
//! `GET /v1/volumes/{id}/files?path=/undefined` answered 500 "fsal error:
//! Directory not found: /aegis/volumes/u-…/<id>/undefined", and the same
//! listing of a file answered 500 naming the filer's internal URL, because
//! `FileOperationsService` wrapped every storage error as `Fsal` with the
//! storage error's own text.
//!
//! These tests drive `FileOperationsService` as the dashboard's routes call
//! it, over a real FSAL: on the local host provider, a missing path is
//! `NotFound` with the caller's path, a file listed as a directory is
//! `NotADirectory`, a directory read as a file is `IsDirectory`; on the
//! SeaweedFS adapter, against a stand-in filer, a file listed as a directory
//! is `NotADirectory`, decided from the filer's entry metadata, and the file's
//! content is never fetched as a listing.
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** Error mapping of the volume file API's service

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use aegis_orchestrator_core::application::file_operations_service::{
    FileOperationsError, FileOperationsService,
};
use aegis_orchestrator_core::domain::events::StorageEvent;
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::repository::VolumeRepository;
use aegis_orchestrator_core::domain::storage::StorageProvider;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::volume::{
    FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeId, VolumeOwnership,
};
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
};
use aegis_orchestrator_core::infrastructure::storage::seaweedfs::SeaweedFSAdapter;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;

struct NoEvents;

#[async_trait]
impl EventPublisher for NoEvents {
    async fn publish_storage_event(&self, _: StorageEvent) {}
}

const OWNER: &str = "u-d7f8";

fn tenant() -> TenantId {
    TenantId::for_consumer_user(OWNER).unwrap()
}

fn service(
    storage: Arc<dyn StorageProvider>,
    volumes: Arc<InMemoryVolumeRepository>,
) -> FileOperationsService {
    let fsal = Arc::new(AegisFSAL::new(
        storage,
        volumes as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    ));
    FileOperationsService::new(
        fsal,
        Arc::new(InMemoryExecutionRepository::new()),
        Arc::new(InMemoryAgentRepository::new()),
    )
}

async fn persistent_volume(volumes: &InMemoryVolumeRepository, backend: VolumeBackend) -> Volume {
    let mut volume = Volume::new(
        "workspace".to_string(),
        tenant(),
        StorageClass::persistent(),
        backend,
        64 * 1024 * 1024,
        VolumeOwnership::persistent(OWNER),
    )
    .unwrap();
    volume.mark_available().unwrap();
    volumes.save(&volume).await.unwrap();
    volume
}

/// The service over a local host provider rooted at a scratch directory, with
/// one persistent volume of `OWNER` holding `/repo/README.md`.
struct HostWorld {
    _dir: tempfile::TempDir,
    storage_root: String,
    volume_id: VolumeId,
    service: FileOperationsService,
}

impl HostWorld {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let volumes = Arc::new(InMemoryVolumeRepository::new());
        let id = VolumeId::new();
        let volume = persistent_volume(
            &volumes,
            VolumeBackend::HostPath {
                path: PathBuf::from(format!("/hosts/{id}")),
            },
        )
        .await;
        let volume_root = root.join(format!("hosts/{id}"));
        std::fs::create_dir_all(volume_root.join("repo")).unwrap();
        std::fs::write(volume_root.join("repo/README.md"), b"# readme\n").unwrap();
        let service = service(
            Arc::new(LocalHostStorageProvider::new(&root).unwrap()),
            volumes,
        );
        Self {
            storage_root: root.to_string_lossy().into_owned(),
            _dir: dir,
            volume_id: volume.id,
            service,
        }
    }

    /// Nothing of the storage's side of the path is in `text`.
    fn assert_no_storage_path(&self, text: &str) {
        for leak in [self.storage_root.as_str(), "/hosts/"] {
            assert!(
                !text.contains(leak),
                "the error names the storage's path ({leak}): {text}"
            );
        }
    }
}

#[tokio::test]
async fn listing_a_missing_path_is_not_found_with_the_callers_path() {
    let world = HostWorld::new().await;

    let result = world
        .service
        .list_directory(&world.volume_id, &tenant(), OWNER, "/undefined")
        .await;

    match result {
        Err(FileOperationsError::NotFound(path)) => {
            assert_eq!(
                path, "/undefined",
                "NotFound does not carry the caller's path"
            );
            world.assert_no_storage_path(&path);
        }
        other => panic!("listing a missing path was not NotFound(\"/undefined\"): {other:?}"),
    }
}

#[tokio::test]
async fn listing_a_file_is_not_a_directory() {
    let world = HostWorld::new().await;

    let result = world
        .service
        .list_directory(&world.volume_id, &tenant(), OWNER, "/repo/README.md")
        .await;

    match result {
        Err(FileOperationsError::NotADirectory(path)) => {
            assert_eq!(path, "/repo/README.md");
            world.assert_no_storage_path(&path);
        }
        other => panic!("listing a file was not NotADirectory(\"/repo/README.md\"): {other:?}"),
    }
}

#[tokio::test]
async fn reading_a_directory_is_is_directory() {
    let world = HostWorld::new().await;

    let result = world
        .service
        .read_file(&world.volume_id, &tenant(), OWNER, "/repo")
        .await;

    match result {
        Err(FileOperationsError::IsDirectory(path)) => {
            assert_eq!(path, "/repo");
            world.assert_no_storage_path(&path);
        }
        Err(other) => panic!("reading a directory was not IsDirectory(\"/repo\"): {other:?}"),
        Ok(content) => panic!(
            "reading a directory was answered as a file of {} bytes",
            content.data.len()
        ),
    }
}

#[tokio::test]
async fn reading_a_missing_file_is_not_found_with_the_callers_path() {
    let world = HostWorld::new().await;

    let result = world
        .service
        .read_file(&world.volume_id, &tenant(), OWNER, "/repo/absent.md")
        .await;

    match result {
        Err(FileOperationsError::NotFound(path)) => {
            assert_eq!(path, "/repo/absent.md");
            world.assert_no_storage_path(&path);
        }
        Err(other) => panic!("reading a missing file was not NotFound: {other:?}"),
        Ok(_) => panic!("reading a missing file was answered"),
    }
}

#[tokio::test]
async fn deleting_a_missing_path_is_not_found_with_the_callers_path() {
    let world = HostWorld::new().await;

    let result = world
        .service
        .delete_path(&world.volume_id, &tenant(), OWNER, "/gone.txt")
        .await;

    match result {
        Err(FileOperationsError::NotFound(path)) => {
            assert_eq!(path, "/gone.txt");
            world.assert_no_storage_path(&path);
        }
        other => panic!("deleting a missing path was not NotFound(\"/gone.txt\"): {other:?}"),
    }
}

#[tokio::test]
async fn moving_a_missing_path_is_not_found_with_the_callers_path() {
    let world = HostWorld::new().await;

    let result = world
        .service
        .move_path(&world.volume_id, &tenant(), OWNER, "/gone.txt", "/here.txt")
        .await;

    match result {
        Err(FileOperationsError::NotFound(path)) => {
            assert_eq!(path, "/gone.txt");
            world.assert_no_storage_path(&path);
        }
        other => panic!("moving a missing path was not NotFound(\"/gone.txt\"): {other:?}"),
    }
}

#[tokio::test]
async fn writing_onto_a_directory_is_is_directory() {
    let world = HostWorld::new().await;

    let result = world
        .service
        .write_file(&world.volume_id, &tenant(), OWNER, "/repo", b"x", 1024)
        .await;

    match result {
        Err(FileOperationsError::IsDirectory(path)) => {
            assert_eq!(path, "/repo");
            world.assert_no_storage_path(&path);
        }
        other => panic!("writing onto a directory was not IsDirectory(\"/repo\"): {other:?}"),
    }
}

#[tokio::test]
async fn making_a_directory_where_a_file_is_is_not_a_directory() {
    let world = HostWorld::new().await;

    let result = world
        .service
        .create_directory(&world.volume_id, &tenant(), OWNER, "/repo/README.md")
        .await;

    match result {
        Err(FileOperationsError::NotADirectory(path)) => {
            assert_eq!(path, "/repo/README.md");
            world.assert_no_storage_path(&path);
        }
        other => panic!("mkdir over a file was not NotADirectory(\"/repo/README.md\"): {other:?}"),
    }
}

/// The filer's entry for a file: `Mode` 0o644, no `os.ModeDir`.
const FILE_METADATA: &str = r##"{"FullPath":"/aegis/volumes/u-d7f8/vol/repo/README.md","Mtime":"2026-10-15T00:30:00Z","Crtime":"2026-10-15T00:30:00Z","Mode":420,"Uid":0,"Gid":0,"Mime":"text/markdown","FileSize":9,"Extended":null,"chunks":[]}"##;

#[tokio::test]
async fn listing_a_file_on_seaweedfs_is_not_a_directory_and_fetches_no_content() {
    let mut filer = mockito::Server::new_async().await;
    let volumes = Arc::new(InMemoryVolumeRepository::new());
    let mut volume = persistent_volume(
        &volumes,
        VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new(filer.url()).unwrap(),
            remote_path: String::new(),
        },
    )
    .await;
    let remote_path = format!("/aegis/volumes/{OWNER}/{}", volume.id);
    volume.backend = VolumeBackend::SeaweedFS {
        filer_endpoint: FilerEndpoint::new(filer.url()).unwrap(),
        remote_path: remote_path.clone(),
    };
    volumes.save(&volume).await.unwrap();

    // As the filer answers for a file: its entry metadata marks a file, a
    // HEAD answers 200 with its length, and a plain GET answers the file's
    // own content, which is not a listing.
    let file = format!("{remote_path}/repo/README.md");
    filer
        .mock("GET", file.as_str())
        .match_query(mockito::Matcher::UrlEncoded(
            "metadata".into(),
            "true".into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(FILE_METADATA)
        .create_async()
        .await;
    filer
        .mock("HEAD", file.as_str())
        .with_status(200)
        .with_header("content-length", "9")
        .with_header("last-modified", "Thu, 15 Oct 2026 00:30:00 GMT")
        .create_async()
        .await;
    let content = filer
        .mock("GET", file.as_str())
        .match_query(mockito::Matcher::Missing)
        .with_status(200)
        .with_header("content-type", "text/markdown")
        .with_body("# readme\n")
        .expect(0)
        .create_async()
        .await;

    let service = service(Arc::new(SeaweedFSAdapter::new(filer.url())), volumes);
    let result = service
        .list_directory(&volume.id, &tenant(), OWNER, "/repo/README.md")
        .await;

    match &result {
        Err(FileOperationsError::NotADirectory(path)) => {
            assert_eq!(path, "/repo/README.md");
        }
        other => panic!("listing a file on SeaweedFS was not NotADirectory: {other:?}"),
    }
    let shown = format!("{:?}", result.err());
    for leak in ["http://", "/aegis/", remote_path.as_str()] {
        assert!(
            !shown.contains(leak),
            "the error names the storage ({leak}): {shown}"
        );
    }
    content.assert_async().await;
}
