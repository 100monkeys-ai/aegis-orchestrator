// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Host-side FUSE daemon CLI subcommand (ADR-107)
//!
//! Runs the FUSE daemon as a standalone process that connects to the
//! orchestrator's FsalService over gRPC and exposes a FuseMountService
//! for the orchestrator to request mount/unmount operations.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** CLI entry point for the host-side FUSE daemon

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use aegis_orchestrator_core::infrastructure::aegis_runtime_proto::fuse_mount_service_server::{
    FuseMountService, FuseMountServiceServer,
};
use aegis_orchestrator_core::infrastructure::aegis_runtime_proto::{
    FuseMountRequest, FuseMountResponse, FuseUnmountRequest, FuseUnmountResponse,
};
use aegis_orchestrator_core::infrastructure::fuse::daemon::{
    FuseFsalDaemon, FuseMountHandle, FuseVolumeContext,
};
use aegis_orchestrator_core::infrastructure::fuse::grpc_backend::GrpcFsalBackend;

use crate::output::OutputFormat;

#[derive(Subcommand)]
pub enum FuseDaemonCommand {
    /// Start the FUSE daemon process
    Start {
        /// gRPC endpoint of the orchestrator's FsalService
        #[arg(long, default_value = "grpc://127.0.0.1:50051")]
        orchestrator_url: String,

        /// Host directory prefix for FUSE mountpoints
        #[arg(long, default_value = "/tmp/aegis-fuse-mounts")]
        mount_prefix: String,

        /// Address to listen on for FuseMountService gRPC
        #[arg(long, default_value = "0.0.0.0:50053")]
        listen_addr: String,
    },

    /// Stop a running FUSE daemon
    Stop,

    /// Check FUSE daemon status
    Status,
}

/// Active mount handles keyed by `"{execution_id}/{volume_id}"`.
type MountHandleMap = Arc<RwLock<HashMap<String, FuseMountHandle>>>;

/// FuseMountService gRPC implementation — receives mount/unmount commands
/// from the orchestrator and creates/destroys FUSE mounts on the host.
struct FuseMountServiceImpl {
    daemon: Arc<FuseFsalDaemon>,
    mount_prefix: String,
    handles: MountHandleMap,
}

fn mount_key(execution_id: &str, volume_id: &str) -> String {
    format!("{execution_id}/{volume_id}")
}

#[tonic::async_trait]
impl FuseMountService for FuseMountServiceImpl {
    async fn mount(
        &self,
        request: Request<FuseMountRequest>,
    ) -> Result<Response<FuseMountResponse>, Status> {
        let req = request.into_inner();

        let execution_id = uuid::Uuid::parse_str(&req.execution_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid execution_id: {e}")))?;
        let volume_id = uuid::Uuid::parse_str(&req.volume_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid volume_id: {e}")))?;

        let workflow_execution_id = if req.workflow_execution_id.is_empty() {
            None
        } else {
            Some(
                uuid::Uuid::parse_str(&req.workflow_execution_id).map_err(|e| {
                    Status::invalid_argument(format!("Invalid workflow_execution_id: {e}"))
                })?,
            )
        };

        let mountpoint = if req.mount_point.is_empty() {
            format!("{}/{}", self.mount_prefix, req.volume_id)
        } else {
            req.mount_point.clone()
        };

        let context = FuseVolumeContext {
            execution_id: aegis_orchestrator_core::domain::execution::ExecutionId(execution_id),
            volume_id: aegis_orchestrator_core::domain::volume::VolumeId(volume_id),
            workflow_execution_id,
            container_uid: req.container_uid,
            container_gid: req.container_gid,
            policy: aegis_orchestrator_core::domain::fsal::FsalAccessPolicy {
                read: req.read_paths,
                write: req.write_paths,
            },
        };

        let handle = self
            .daemon
            .mount(Path::new(&mountpoint), context)
            .map_err(|e| Status::internal(format!("FUSE mount failed: {e}")))?;

        let key = mount_key(&req.execution_id, &req.volume_id);
        let actual_mountpoint = handle.mountpoint().to_string();
        self.handles.write().await.insert(key, handle);

        info!(
            mountpoint = %actual_mountpoint,
            volume_id = %req.volume_id,
            execution_id = %req.execution_id,
            "FUSE mount created via gRPC"
        );

        Ok(Response::new(FuseMountResponse {
            mountpoint: actual_mountpoint,
        }))
    }

    async fn unmount(
        &self,
        request: Request<FuseUnmountRequest>,
    ) -> Result<Response<FuseUnmountResponse>, Status> {
        let req = request.into_inner();

        if req.execution_id.is_empty() {
            // Wildcard unmount: remove ALL mounts for this volume_id regardless
            // of execution_id. Used by DestroyWorkspaceVolume to ensure no stale
            // FUSE mountpoints survive after volume destruction.
            let suffix = format!("/{}", req.volume_id);
            let mut handles = self.handles.write().await;
            let keys_to_remove: Vec<String> = handles
                .keys()
                .filter(|k| k.ends_with(&suffix))
                .cloned()
                .collect();
            let count = keys_to_remove.len();
            // Collect mountpoints before dropping handles
            let mountpoints: Vec<String> = keys_to_remove
                .iter()
                .filter_map(|k| handles.get(k).map(|h| h.mountpoint().to_string()))
                .collect();
            for key in &keys_to_remove {
                // Dropping the FuseMountHandle triggers the actual unmount.
                handles.remove(key);
            }
            // Clean up empty mount directories
            for mp in &mountpoints {
                let _ = std::fs::remove_dir(mp);
            }
            if count > 0 {
                info!(
                    volume_id = %req.volume_id,
                    count,
                    "FUSE mount(s) removed via gRPC wildcard unmount"
                );
            } else {
                warn!(
                    volume_id = %req.volume_id,
                    "No active FUSE mounts found for wildcard unmount request"
                );
            }
            return Ok(Response::new(FuseUnmountResponse {
                unmounted: count > 0,
            }));
        }

        let key = mount_key(&req.execution_id, &req.volume_id);

        let mut handles = self.handles.write().await;
        let mountpoint = handles.get(&key).map(|h| h.mountpoint().to_string());
        let removed = handles.remove(&key);
        drop(handles);
        let unmounted = removed.is_some();

        if unmounted {
            info!(
                volume_id = %req.volume_id,
                execution_id = %req.execution_id,
                "FUSE mount removed via gRPC"
            );
            // Dropping the FuseMountHandle triggers the actual unmount.
            // Clean up the empty mount directory.
            drop(removed);
            if let Some(mp) = mountpoint {
                let _ = std::fs::remove_dir(&mp);
            }
        } else {
            warn!(
                volume_id = %req.volume_id,
                execution_id = %req.execution_id,
                "No active FUSE mount found for unmount request"
            );
        }

        Ok(Response::new(FuseUnmountResponse { unmounted }))
    }
}

pub async fn handle_command(command: FuseDaemonCommand, _output: OutputFormat) -> Result<()> {
    match command {
        FuseDaemonCommand::Start {
            orchestrator_url,
            mount_prefix,
            listen_addr,
        } => {
            info!(
                orchestrator_url = %orchestrator_url,
                mount_prefix = %mount_prefix,
                listen_addr = %listen_addr,
                "Starting AEGIS FUSE daemon (ADR-107)"
            );

            // Connect to orchestrator FsalService
            let backend = GrpcFsalBackend::connect(&orchestrator_url)
                .await
                .context("Failed to connect to orchestrator FsalService")?;

            // Create FuseFsalDaemon with the gRPC backend
            let daemon = Arc::new(FuseFsalDaemon::with_backend(Arc::new(backend)));

            // Ensure mount prefix directory exists
            std::fs::create_dir_all(&mount_prefix)
                .context("Failed to create mount prefix directory")?;

            // Clean up stale FUSE mounts from previous daemon instances
            info!("Cleaning up stale FUSE mounts in {}", mount_prefix);
            if let Ok(entries) = std::fs::read_dir(&mount_prefix) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        // Lazy unmount (best-effort) — try fusermount first, fall back to umount
                        let fusermount_result = std::process::Command::new("fusermount")
                            .args(["-uz", &path.to_string_lossy()])
                            .output();
                        if fusermount_result.is_err() {
                            let _ = std::process::Command::new("umount")
                                .args(["-l", &path.to_string_lossy()])
                                .output();
                        }
                        // Remove empty directory
                        match std::fs::remove_dir(&path) {
                            Ok(()) => {
                                info!(path = %path.display(), "Reaped stale FUSE mount directory")
                            }
                            Err(_) => warn!(
                                path = %path.display(),
                                "Could not remove FUSE mount directory (may still be in use)"
                            ),
                        }
                    }
                }
            }

            // Start FuseMountService gRPC server
            let addr = listen_addr.parse().context("Invalid listen address")?;

            let handles: MountHandleMap = Arc::new(RwLock::new(HashMap::new()));

            let service = FuseMountServiceImpl {
                daemon,
                mount_prefix: mount_prefix.clone(),
                handles: handles.clone(),
            };

            // Spawn periodic mount reaper — cleans up orphaned FUSE mount
            // directories that have no corresponding active handle (e.g. after
            // a crash or missed unmount RPC).
            let reaper_handles = handles.clone();
            let reaper_prefix = mount_prefix.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    if let Ok(entries) = std::fs::read_dir(&reaper_prefix) {
                        let active = reaper_handles.read().await;
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.is_dir() {
                                let path_str = path.to_string_lossy().to_string();
                                // A directory is active if any handle's mountpoint matches it
                                let is_active = active.values().any(|h| h.mountpoint() == path_str);
                                if !is_active {
                                    let _ = std::process::Command::new("fusermount")
                                        .args(["-uz", &path_str])
                                        .output();
                                    if let Ok(()) = std::fs::remove_dir(&path) {
                                        info!(
                                            path = %path.display(),
                                            "Reaped orphaned FUSE mount directory"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            });

            println!(
                "{}",
                format!("AEGIS FUSE daemon listening on {listen_addr}").green()
            );

            Server::builder()
                .add_service(FuseMountServiceServer::new(service))
                .serve(addr)
                .await
                .context("FUSE daemon gRPC server failed")?;

            Ok(())
        }
        FuseDaemonCommand::Stop => {
            // TODO: Implement graceful shutdown via PID file or signal
            eprintln!(
                "{}",
                "FUSE daemon stop not yet implemented — use SIGTERM".yellow()
            );
            Ok(())
        }
        FuseDaemonCommand::Status => {
            // TODO: Implement status check via health endpoint
            eprintln!("{}", "FUSE daemon status not yet implemented".yellow());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mount_key;

    /// Regression: wildcard unmount must correctly identify all mount keys
    /// matching a given volume_id, regardless of execution_id. Before this fix,
    /// gRPC FUSE mounts created during container step execution were never
    /// unmounted — DestroyWorkspaceVolume destroyed the volume but left stale
    /// FUSE mountpoints generating endless DirectoryListed storage events.
    ///
    /// The fix uses a suffix-matching pattern: when execution_id is empty,
    /// all keys ending with "/{volume_id}" are removed.
    #[test]
    fn test_wildcard_unmount_matches_all_executions_for_volume() {
        let volume_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let exec_1 = "11111111-1111-1111-1111-111111111111";
        let exec_2 = "22222222-2222-2222-2222-222222222222";
        let other_volume = "ffffffff-ffff-ffff-ffff-ffffffffffff";

        let key_1 = mount_key(exec_1, volume_id);
        let key_2 = mount_key(exec_2, volume_id);
        let key_other = mount_key(exec_1, other_volume);

        let suffix = format!("/{volume_id}");

        assert!(
            key_1.ends_with(&suffix),
            "key for exec_1/volume must match wildcard"
        );
        assert!(
            key_2.ends_with(&suffix),
            "key for exec_2/volume must match wildcard"
        );
        assert!(
            !key_other.ends_with(&suffix),
            "key for different volume must NOT match wildcard"
        );
    }

    /// Regression: exact unmount (with both execution_id and volume_id) must
    /// only match the specific (execution_id, volume_id) pair — not other
    /// executions of the same volume.
    #[test]
    fn test_exact_unmount_matches_only_specific_key() {
        let volume_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let exec_1 = "11111111-1111-1111-1111-111111111111";
        let exec_2 = "22222222-2222-2222-2222-222222222222";

        let key_1 = mount_key(exec_1, volume_id);
        let key_2 = mount_key(exec_2, volume_id);

        assert_ne!(
            key_1, key_2,
            "different execution_ids must produce different keys"
        );
        assert_eq!(
            key_1,
            mount_key(exec_1, volume_id),
            "same inputs must produce identical keys"
        );
    }
}
