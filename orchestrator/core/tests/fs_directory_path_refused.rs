// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # `fs.read` of a directory on a SeaweedFS volume, end to end
//!
//! Regression (fs-read-of-volume-root-returns-filer-html-2026-10-14): in a
//! Forge run the coder's `fs.read /workspace` was answered, as a success, with
//! the SeaweedFS filer's HTML directory page, which named the volume's
//! internal path. The read passed the provider's existence check (a HEAD,
//! which the filer answers 200 for a directory) and then fetched the listing
//! as the file's content.
//!
//! This test runs the tool as a run calls it: `invoke_fs_tool` over an
//! `AegisFSAL` on the `SeaweedFSAdapter`, the volume registered at
//! `/workspace`, against a stand-in filer that answers as the filer does for a
//! directory: its entry metadata marks it a directory, a HEAD answers 200, and
//! a plain GET answers the HTML listing.
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** End-to-end test of the `fs.*` directory refusal on the
//!   SeaweedFS storage adapter

use aegis_orchestrator_core::application::nfs_gateway::{NfsVolumeRegistry, VolumeRegistration};
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationResult;
use aegis_orchestrator_core::application::tools::builtin_fsal::invoke_fs_tool;
use aegis_orchestrator_core::domain::events::StorageEvent;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher, FsalAccessPolicy};
use aegis_orchestrator_core::domain::repository::VolumeRepository;
use aegis_orchestrator_core::domain::volume::{
    FilerEndpoint, StorageClass, TenantId, Volume, VolumeBackend, VolumeOwnership,
};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::storage::seaweedfs::SeaweedFSAdapter;
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

struct NoEvents;

#[async_trait]
impl EventPublisher for NoEvents {
    async fn publish_storage_event(&self, _: StorageEvent) {}
}

/// The filer's entry for a directory: `Mode` carries Go's `os.ModeDir`
/// (`1 << 31`), here `ModeDir | 0o755`.
const DIRECTORY_METADATA: &str = r#"{"FullPath":"/aegis/volumes/u-t4/root","Mtime":"2026-10-14T20:00:00Z","Crtime":"2026-10-14T20:00:00Z","Mode":2147484141,"Uid":0,"Gid":0,"Mime":"","FileSize":0,"Extended":null}"#;

#[tokio::test]
async fn fs_read_of_a_seaweedfs_volumes_root_is_refused_and_names_no_storage_path() {
    let mut filer = mockito::Server::new_async().await;
    let execution = ExecutionId::new();
    let mut volume = Volume::new(
        "workspace".to_string(),
        TenantId::default(),
        StorageClass::persistent(),
        VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new(filer.url()).unwrap(),
            remote_path: String::new(),
        },
        64 * 1024 * 1024,
        VolumeOwnership::execution(execution),
    )
    .unwrap();
    let remote_path = format!("/aegis/volumes/u-t4/{}", volume.id);
    volume.backend = VolumeBackend::SeaweedFS {
        filer_endpoint: FilerEndpoint::new(filer.url()).unwrap(),
        remote_path: remote_path.clone(),
    };
    volume.mark_available().unwrap();

    // The volume's root as the FSAL names it: `<remote_path>/`.
    let root = format!("{remote_path}/");
    filer
        .mock("GET", root.as_str())
        .match_query(mockito::Matcher::UrlEncoded(
            "metadata".into(),
            "true".into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(DIRECTORY_METADATA)
        .create_async()
        .await;
    filer
        .mock("HEAD", root.as_str())
        .with_status(200)
        .create_async()
        .await;
    let listing = filer
        .mock("GET", root.as_str())
        .match_query(mockito::Matcher::Missing)
        .with_status(200)
        .with_header("content-type", "text/html")
        .with_body(format!(
            "<html><head><title>SeaweedFS Filer</title></head><body>{root}</body></html>"
        ))
        .expect(0)
        .create_async()
        .await;

    let volumes = Arc::new(InMemoryVolumeRepository::new());
    volumes.save(&volume).await.unwrap();
    let registry = NfsVolumeRegistry::new();
    let policy = FsalAccessPolicy {
        read: vec!["/**".to_string()],
        write: vec!["/**".to_string()],
    };
    registry.register(VolumeRegistration {
        volume_id: volume.id,
        execution_id: execution,
        workflow_execution_id: None,
        run_repository: None,
        container_uid: 1000,
        container_gid: 1000,
        policy,
        mount_point: PathBuf::from("/workspace"),
        remote_path: remote_path.clone(),
    });
    let fsal = Arc::new(
        AegisFSAL::new(
            Arc::new(SeaweedFSAdapter::new(filer.url())),
            volumes as Arc<dyn VolumeRepository>,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(NoEvents),
        )
        .with_volume_context_lookup(Arc::new(registry.clone())),
    );

    let answer = invoke_fs_tool(
        "fs.read",
        &json!({"path": "/workspace"}),
        execution,
        &fsal,
        &registry,
    )
    .await;

    let sentence =
        "[AEGIS] fs.read was not run: /workspace is a directory, not a file; fs.list lists a directory.";
    let refused = match answer {
        Err(refused) => refused,
        Ok(ToolInvocationResult::Direct(ran)) => {
            panic!("fs.read of the volume's root was run: {ran}")
        }
        Ok(_) => panic!("fs.read of the volume's root was run"),
    };
    let shown = refused.to_string();
    let answered = refused.refusal().message;
    assert_eq!(
        (shown.as_str(), answered.as_str()),
        (
            format!("Invalid tool arguments: {sentence}").as_str(),
            sentence
        ),
        "fs.read of the volume's root was not refused naming fs.list"
    );
    for text in [&shown, &answered] {
        assert!(
            !text.contains("SeaweedFS") && !text.contains("/aegis/volumes/"),
            "the refusal names the storage: {text}"
        );
    }
    listing.assert_async().await;
}
