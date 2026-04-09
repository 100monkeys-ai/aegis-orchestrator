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
use tracing::{error, info, warn};

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
        #[arg(long, default_value = "0.0.0.0:50052")]
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
        let key = mount_key(&req.execution_id, &req.volume_id);

        let removed = self.handles.write().await.remove(&key);
        let unmounted = removed.is_some();

        if unmounted {
            info!(
                volume_id = %req.volume_id,
                execution_id = %req.execution_id,
                "FUSE mount removed via gRPC"
            );
        } else {
            warn!(
                volume_id = %req.volume_id,
                execution_id = %req.execution_id,
                "No active FUSE mount found for unmount request"
            );
        }

        // Dropping the FuseMountHandle triggers the actual unmount.
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

            // Start FuseMountService gRPC server
            let addr = listen_addr.parse().context("Invalid listen address")?;

            let service = FuseMountServiceImpl {
                daemon,
                mount_prefix,
                handles: Arc::new(RwLock::new(HashMap::new())),
            };

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
