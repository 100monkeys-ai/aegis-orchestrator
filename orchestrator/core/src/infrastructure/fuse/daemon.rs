// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! FUSE Daemon — maps POSIX filesystem operations to AegisFSAL calls (ADR-107)
//!
//! Implements `fuser::Filesystem` to present a FUSE mountpoint that delegates
//! all I/O to the shared `AegisFSAL` domain entity. Each mount corresponds to
//! a single (execution_id, volume_id) pair with its own access policy.
//!
//! ## Async Bridging
//! FSAL methods are async but `fuser::Filesystem` callbacks are synchronous.
//! We use `tokio::runtime::Handle::current().block_on()` to bridge, which is
//! safe because FUSE callbacks run on dedicated `fuser` threads — not on the
//! tokio executor.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** FUSE transport frontend for the FSAL domain entity

use crate::domain::execution::ExecutionId;
use crate::domain::fsal::{AegisFSAL, AegisFileHandle, CreateFsalFileRequest, FsalAccessPolicy};
use crate::domain::storage::FileType;
use crate::domain::volume::VolumeId;

use super::inode_table::{InodeTable, ROOT_INODE};

use fuser::{
    FileAttr, FileType as FuseFileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEntry, ReplyWrite, Request,
};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::{debug, error, warn};

/// TTL for FUSE attribute/entry caches.
/// Short TTL ensures agents see fresh data from the storage backend.
const FUSE_TTL: Duration = Duration::from_secs(1);

/// FUSE daemon errors
#[derive(Debug, Error)]
pub enum FuseDaemonError {
    #[error("Failed to create mountpoint directory {path}: {error}")]
    MountpointCreation { path: String, error: String },

    #[error("FUSE mount failed at {path}: {error}")]
    MountFailed { path: String, error: String },

    #[error("FUSE session error: {0}")]
    SessionError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Volume context for a FUSE mount — analogous to `NfsVolumeContext`.
#[derive(Debug, Clone)]
pub struct FuseVolumeContext {
    pub execution_id: ExecutionId,
    pub volume_id: VolumeId,
    pub workflow_execution_id: Option<uuid::Uuid>,
    pub container_uid: u32,
    pub container_gid: u32,
    pub policy: FsalAccessPolicy,
}

/// Wrapper to make `fuser::BackgroundSession` `Send + Sync`.
///
/// # Safety
///
/// `BackgroundSession` contains a `*mut c_void` (the libfuse session handle).
/// The orchestrator only creates, holds, and drops sessions sequentially —
/// the handle is never accessed concurrently from multiple threads. The
/// underlying libfuse mount/unmount operations are thread-safe.
#[expect(dead_code, reason = "held for RAII drop — unmounts on drop")]
struct SendSyncSession(fuser::BackgroundSession);

// SAFETY: see `SendSyncSession` doc comment above.
unsafe impl Send for SendSyncSession {}
unsafe impl Sync for SendSyncSession {}

/// Handle to an active FUSE mount. Dropping this handle unmounts the filesystem.
pub struct FuseMountHandle {
    /// The `fuser::BackgroundSession` keeps the FUSE filesystem mounted.
    /// Dropping it triggers unmount.
    _session: SendSyncSession,
    mountpoint: String,
}

impl FuseMountHandle {
    /// Returns the host path where the FUSE filesystem is mounted.
    pub fn mountpoint(&self) -> &str {
        &self.mountpoint
    }
}

impl Drop for FuseMountHandle {
    fn drop(&mut self) {
        debug!(mountpoint = %self.mountpoint, "FUSE mount handle dropped — unmounting");
        // BackgroundSession::drop() sends FUSE_DESTROY and unmounts automatically.
    }
}

/// FUSE-based FSAL transport daemon.
///
/// Creates per-volume FUSE mountpoints on the host that agent containers can
/// access via bind mounts. Shares the same `AegisFSAL` instance as the NFS
/// transport, preserving identical security semantics.
pub struct FuseFsalDaemon {
    fsal: Arc<AegisFSAL>,
}

impl FuseFsalDaemon {
    /// Create a new FUSE daemon backed by the given FSAL instance.
    pub fn new(fsal: Arc<AegisFSAL>) -> Self {
        Self { fsal }
    }

    /// Mount a FUSE filesystem at the given host path for a specific volume.
    ///
    /// The returned `FuseMountHandle` keeps the mount alive. Dropping the handle
    /// triggers an automatic unmount. The mountpoint directory is created if it
    /// does not exist.
    pub fn mount(
        &self,
        mountpoint: &Path,
        context: FuseVolumeContext,
    ) -> Result<FuseMountHandle, FuseDaemonError> {
        // Ensure mountpoint directory exists
        std::fs::create_dir_all(mountpoint).map_err(|e| FuseDaemonError::MountpointCreation {
            path: mountpoint.display().to_string(),
            error: e.to_string(),
        })?;

        let fs = FuseFsal {
            fsal: self.fsal.clone(),
            context: context.clone(),
            inode_table: Arc::new(InodeTable::new()),
        };

        // Mount options:
        // - allow_other: let the container user access the mount (requires /etc/fuse.conf)
        // - default_permissions: let the kernel enforce basic permission checks
        // - fsname: identifies this mount in /proc/mounts
        let options = vec![
            fuser::MountOption::FSName(format!("aegis-fsal-{}", context.volume_id)),
            fuser::MountOption::AllowOther,
            fuser::MountOption::DefaultPermissions,
            fuser::MountOption::RW,
        ];

        let session = fuser::spawn_mount2(fs, mountpoint, &options).map_err(|e| {
            FuseDaemonError::MountFailed {
                path: mountpoint.display().to_string(),
                error: e.to_string(),
            }
        })?;

        let mountpoint_str = mountpoint.display().to_string();
        tracing::info!(
            mountpoint = %mountpoint_str,
            volume_id = %context.volume_id,
            execution_id = %context.execution_id,
            "FUSE filesystem mounted"
        );

        Ok(FuseMountHandle {
            _session: SendSyncSession(session),
            mountpoint: mountpoint_str,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// fuser::Filesystem implementation
// ─────────────────────────────────────────────────────────────────────────────

/// Internal FUSE filesystem implementation that delegates to AegisFSAL.
///
/// Each instance is bound to a single (execution_id, volume_id) pair —
/// unlike the NFS server which multiplexes all volumes on a single port.
struct FuseFsal {
    fsal: Arc<AegisFSAL>,
    context: FuseVolumeContext,
    inode_table: Arc<InodeTable>,
}

impl FuseFsal {
    /// Get or create the root file handle for this volume.
    fn root_handle(&self) -> AegisFileHandle {
        AegisFileHandle::new(self.context.execution_id, self.context.volume_id, "/")
    }

    /// Resolve an inode to its (AegisFileHandle, path) pair.
    /// Returns `None` for unknown inodes except ROOT_INODE which is synthesized.
    fn resolve_inode(&self, ino: u64) -> Option<(AegisFileHandle, String)> {
        if ino == ROOT_INODE {
            Some((self.root_handle(), "/".to_string()))
        } else {
            self.inode_table.lookup(ino)
        }
    }

    /// Convert FSAL `FileAttributes` to `fuser::FileAttr`, applying UID/GID squashing.
    fn convert_attrs(&self, attrs: &crate::domain::storage::FileAttributes, ino: u64) -> FileAttr {
        let kind = match attrs.file_type {
            FileType::File => FuseFileType::RegularFile,
            FileType::Directory => FuseFileType::Directory,
            FileType::Symlink => FuseFileType::Symlink,
        };

        FileAttr {
            ino,
            size: attrs.size,
            blocks: attrs.size.div_ceil(512),
            atime: UNIX_EPOCH + Duration::from_secs(attrs.atime.max(0) as u64),
            mtime: UNIX_EPOCH + Duration::from_secs(attrs.mtime.max(0) as u64),
            ctime: UNIX_EPOCH + Duration::from_secs(attrs.ctime.max(0) as u64),
            crtime: UNIX_EPOCH, // creation time not tracked
            kind,
            perm: attrs.mode as u16,
            nlink: attrs.nlink,
            uid: self.context.container_uid, // UID squashing per ADR-036
            gid: self.context.container_gid, // GID squashing per ADR-036
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Synthesize directory attributes for the volume root.
    fn root_attr(&self) -> FileAttr {
        FileAttr {
            ino: ROOT_INODE,
            size: 4096,
            blocks: 8,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FuseFileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: self.context.container_uid,
            gid: self.context.container_gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Build a child path from a parent path and child name.
    fn child_path(parent_path: &str, name: &str) -> String {
        if parent_path == "/" || parent_path.is_empty() {
            format!("/{name}")
        } else {
            format!("{}/{}", parent_path.trim_end_matches('/'), name)
        }
    }

    /// Bridge async FSAL calls into the synchronous FUSE callback context.
    ///
    /// FUSE callbacks run on dedicated `fuser` threads, not on the tokio
    /// executor, so `block_on` is safe and does not risk deadlock.
    fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        tokio::runtime::Handle::current().block_on(f)
    }
}

impl Filesystem for FuseFsal {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        debug!(parent = parent, name = %name_str, "FUSE LOOKUP");

        let (parent_handle, parent_path) = match self.resolve_inode(parent) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Perform FSAL lookup to get a child handle
        let child_handle =
            match self.block_on(self.fsal.lookup(&parent_handle, &parent_path, name_str)) {
                Ok(h) => h,
                Err(e) => {
                    debug!(error = %e, "FUSE lookup failed");
                    reply.error(libc::ENOENT);
                    return;
                }
            };

        let child_path = Self::child_path(&parent_path, name_str);
        let child_ino = self.inode_table.allocate(child_handle, child_path.clone());

        // Get attributes for the child
        match self.block_on(self.fsal.getattr(
            self.context.execution_id,
            self.context.volume_id,
            &child_path,
            self.context.container_uid,
            self.context.container_gid,
        )) {
            Ok(attrs) => {
                let fuse_attr = self.convert_attrs(&attrs, child_ino);
                metrics::counter!("aegis_fuse_operations_total", "operation" => "lookup", "result" => "success").increment(1);
                reply.entry(&FUSE_TTL, &fuse_attr, 0);
            }
            Err(e) => {
                // Path exists (lookup succeeded) but stat failed — may be a race
                // or the storage backend is lazily creating entries. Return ENOENT
                // rather than EIO to allow retries.
                debug!(error = %e, path = %child_path, "FUSE lookup getattr failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "lookup", "result" => "error").increment(1);
                reply.error(libc::ENOENT);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        debug!(ino = ino, "FUSE GETATTR");

        if ino == ROOT_INODE {
            // For the volume root, return synthetic attributes (same as NFS server pattern).
            // SeaweedFS may not have the directory yet (created lazily on first write).
            reply.attr(&FUSE_TTL, &self.root_attr());
            return;
        }

        let (_handle, path) = match self.resolve_inode(ino) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        match self.block_on(self.fsal.getattr(
            self.context.execution_id,
            self.context.volume_id,
            &path,
            self.context.container_uid,
            self.context.container_gid,
        )) {
            Ok(attrs) => {
                let fuse_attr = self.convert_attrs(&attrs, ino);
                metrics::counter!("aegis_fuse_operations_total", "operation" => "getattr", "result" => "success").increment(1);
                reply.attr(&FUSE_TTL, &fuse_attr);
            }
            Err(e) => {
                warn!(error = %e, ino = ino, path = %path, "FUSE getattr failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "getattr", "result" => "error").increment(1);
                reply.error(libc::ENOENT);
            }
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        debug!(ino = ino, offset = offset, size = size, "FUSE READ");

        let (handle, path) = match self.resolve_inode(ino) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        match self.block_on(self.fsal.read(
            &handle,
            &path,
            &self.context.policy,
            offset.max(0) as u64,
            size as usize,
        )) {
            Ok(data) => {
                metrics::counter!("aegis_fuse_operations_total", "operation" => "read", "result" => "success").increment(1);
                metrics::counter!("aegis_fuse_bytes_read_total").increment(data.len() as u64);
                reply.data(&data);
            }
            Err(e) => {
                error!(error = %e, ino = ino, "FUSE read failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "read", "result" => "error").increment(1);
                reply.error(fsal_error_to_errno(&e));
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        debug!(ino = ino, offset = offset, len = data.len(), "FUSE WRITE");

        let (handle, path) = match self.resolve_inode(ino) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        match self.block_on(self.fsal.write(
            &handle,
            &path,
            &self.context.policy,
            offset.max(0) as u64,
            data,
        )) {
            Ok(bytes_written) => {
                metrics::counter!("aegis_fuse_operations_total", "operation" => "write", "result" => "success").increment(1);
                metrics::counter!("aegis_fuse_bytes_written_total").increment(bytes_written as u64);
                reply.written(bytes_written as u32);
            }
            Err(e) => {
                error!(error = %e, ino = ino, "FUSE write failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "write", "result" => "error").increment(1);
                reply.error(fsal_error_to_errno(&e));
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!(ino = ino, offset = offset, "FUSE READDIR");

        let (_handle, dir_path) = match self.resolve_inode(ino) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let entries = match self.block_on(self.fsal.readdir(
            self.context.execution_id,
            self.context.volume_id,
            &dir_path,
            &self.context.policy,
            None,
            None,
        )) {
            Ok(e) => e,
            Err(e) => {
                error!(error = %e, ino = ino, "FUSE readdir failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "readdir", "result" => "error").increment(1);
                reply.error(fsal_error_to_errno(&e));
                return;
            }
        };

        metrics::counter!("aegis_fuse_operations_total", "operation" => "readdir", "result" => "success").increment(1);

        // Build the full entry list: ".", "..", then directory contents
        let mut full_entries: Vec<(u64, FuseFileType, String)> =
            Vec::with_capacity(entries.len() + 2);
        full_entries.push((ino, FuseFileType::Directory, ".".to_string()));
        // Parent is self for root, otherwise we'd need parent tracking.
        // Using ino for ".." is a safe approximation — the kernel handles it.
        full_entries.push((ino, FuseFileType::Directory, "..".to_string()));

        for entry in &entries {
            let entry_path = Self::child_path(&dir_path, &entry.name);
            let child_handle = AegisFileHandle::new(
                self.context.execution_id,
                self.context.volume_id,
                &entry_path,
            );
            let child_ino = self.inode_table.allocate(child_handle, entry_path);
            let kind = match entry.file_type {
                FileType::File => FuseFileType::RegularFile,
                FileType::Directory => FuseFileType::Directory,
                FileType::Symlink => FuseFileType::Symlink,
            };
            full_entries.push((child_ino, kind, entry.name.clone()));
        }

        // Skip entries before offset and add remaining
        for (i, (child_ino, kind, name)) in full_entries.iter().enumerate().skip(offset as usize) {
            // reply.add returns true when the buffer is full
            if reply.add(*child_ino, (i + 1) as i64, *kind, name) {
                break;
            }
        }

        reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        debug!(parent = parent, name = %name_str, "FUSE CREATE");

        let (_parent_handle, parent_path) = match self.resolve_inode(parent) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let file_path = Self::child_path(&parent_path, name_str);

        match self.block_on(self.fsal.create_file(CreateFsalFileRequest {
            execution_id: self.context.execution_id,
            volume_id: self.context.volume_id,
            path: &file_path,
            policy: &self.context.policy,
            emit_event: true,
            caller_node_id: None,
            host_node_id: None,
        })) {
            Ok(handle) => {
                let child_ino = self.inode_table.allocate(handle, file_path.clone());

                // Get attrs for the newly created file
                match self.block_on(self.fsal.getattr(
                    self.context.execution_id,
                    self.context.volume_id,
                    &file_path,
                    self.context.container_uid,
                    self.context.container_gid,
                )) {
                    Ok(attrs) => {
                        let fuse_attr = self.convert_attrs(&attrs, child_ino);
                        metrics::counter!("aegis_fuse_operations_total", "operation" => "create", "result" => "success").increment(1);
                        reply.created(&FUSE_TTL, &fuse_attr, 0, 0, 0);
                    }
                    Err(e) => {
                        // File was created but stat failed — unlikely but handle gracefully.
                        // Return a synthetic file attr rather than failing.
                        warn!(error = %e, path = %file_path, "FUSE create getattr failed — using synthetic attrs");
                        let attr = FileAttr {
                            ino: child_ino,
                            size: 0,
                            blocks: 0,
                            atime: SystemTime::now(),
                            mtime: SystemTime::now(),
                            ctime: SystemTime::now(),
                            crtime: SystemTime::now(),
                            kind: FuseFileType::RegularFile,
                            perm: 0o644,
                            nlink: 1,
                            uid: self.context.container_uid,
                            gid: self.context.container_gid,
                            rdev: 0,
                            blksize: 4096,
                            flags: 0,
                        };
                        metrics::counter!("aegis_fuse_operations_total", "operation" => "create", "result" => "success").increment(1);
                        reply.created(&FUSE_TTL, &attr, 0, 0, 0);
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "FUSE create failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "create", "result" => "error").increment(1);
                reply.error(fsal_error_to_errno(&e));
            }
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        debug!(parent = parent, name = %name_str, "FUSE MKDIR");

        let (_parent_handle, parent_path) = match self.resolve_inode(parent) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let dir_path = Self::child_path(&parent_path, name_str);

        match self.block_on(self.fsal.create_directory(
            self.context.execution_id,
            self.context.volume_id,
            &dir_path,
            &self.context.policy,
            None,
            None,
        )) {
            Ok(()) => {
                let handle = AegisFileHandle::new(
                    self.context.execution_id,
                    self.context.volume_id,
                    &dir_path,
                );
                let child_ino = self.inode_table.allocate(handle, dir_path.clone());

                let attr = FileAttr {
                    ino: child_ino,
                    size: 4096,
                    blocks: 8,
                    atime: SystemTime::now(),
                    mtime: SystemTime::now(),
                    ctime: SystemTime::now(),
                    crtime: SystemTime::now(),
                    kind: FuseFileType::Directory,
                    perm: 0o755,
                    nlink: 2,
                    uid: self.context.container_uid,
                    gid: self.context.container_gid,
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                };
                metrics::counter!("aegis_fuse_operations_total", "operation" => "mkdir", "result" => "success").increment(1);
                reply.entry(&FUSE_TTL, &attr, 0);
            }
            Err(e) => {
                error!(error = %e, "FUSE mkdir failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "mkdir", "result" => "error").increment(1);
                reply.error(fsal_error_to_errno(&e));
            }
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        debug!(parent = parent, name = %name_str, "FUSE UNLINK");

        let (_parent_handle, parent_path) = match self.resolve_inode(parent) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let file_path = Self::child_path(&parent_path, name_str);

        match self.block_on(self.fsal.delete_file(
            self.context.execution_id,
            self.context.volume_id,
            &file_path,
            &self.context.policy,
            None,
            None,
        )) {
            Ok(()) => {
                metrics::counter!("aegis_fuse_operations_total", "operation" => "unlink", "result" => "success").increment(1);
                reply.ok();
            }
            Err(e) => {
                error!(error = %e, "FUSE unlink failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "unlink", "result" => "error").increment(1);
                reply.error(fsal_error_to_errno(&e));
            }
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        debug!(parent = parent, name = %name_str, "FUSE RMDIR");

        let (_parent_handle, parent_path) = match self.resolve_inode(parent) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let dir_path = Self::child_path(&parent_path, name_str);

        match self.block_on(self.fsal.delete_directory(
            self.context.execution_id,
            self.context.volume_id,
            &dir_path,
            &self.context.policy,
            None,
            None,
        )) {
            Ok(()) => {
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rmdir", "result" => "success").increment(1);
                reply.ok();
            }
            Err(e) => {
                error!(error = %e, "FUSE rmdir failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rmdir", "result" => "error").increment(1);
                reply.error(fsal_error_to_errno(&e));
            }
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let newname_str = match newname.to_str() {
            Some(n) => n,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        debug!(
            parent = parent,
            name = %name_str,
            newparent = newparent,
            newname = %newname_str,
            "FUSE RENAME"
        );

        let (_from_handle, from_parent_path) = match self.resolve_inode(parent) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let (_to_handle, to_parent_path) = match self.resolve_inode(newparent) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let from_path = Self::child_path(&from_parent_path, name_str);
        let to_path = Self::child_path(&to_parent_path, newname_str);

        match self.block_on(self.fsal.rename(crate::domain::fsal::RenameFsalRequest {
            execution_id: self.context.execution_id,
            volume_id: self.context.volume_id,
            from_path: &from_path,
            to_path: &to_path,
            policy: &self.context.policy,
            caller_node_id: None,
            host_node_id: None,
        })) {
            Ok(()) => {
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rename", "result" => "success").increment(1);
                reply.ok();
            }
            Err(e) => {
                error!(error = %e, "FUSE rename failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rename", "result" => "error").increment(1);
                reply.error(fsal_error_to_errno(&e));
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // FSAL does not support chmod/chown/truncate as discrete operations.
        // Return current attributes unchanged (UID/GID squashing makes this a no-op).
        debug!(ino = ino, "FUSE SETATTR (no-op, returning current attrs)");

        if ino == ROOT_INODE {
            reply.attr(&FUSE_TTL, &self.root_attr());
            return;
        }

        let (_handle, path) = match self.resolve_inode(ino) {
            Some(v) => v,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        match self.block_on(self.fsal.getattr(
            self.context.execution_id,
            self.context.volume_id,
            &path,
            self.context.container_uid,
            self.context.container_gid,
        )) {
            Ok(attrs) => {
                reply.attr(&FUSE_TTL, &self.convert_attrs(&attrs, ino));
            }
            Err(_) => {
                reply.error(libc::ENOENT);
            }
        }
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        // Stateless open — all I/O goes through read/write with inode lookup.
        // Return fh=0, no special flags.
        debug!(ino = ino, "FUSE OPEN (stateless)");
        reply.opened(0, 0);
    }

    fn opendir(&mut self, _req: &Request, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        debug!(ino = ino, "FUSE OPENDIR (stateless)");
        reply.opened(0, 0);
    }

    fn releasedir(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: fuser::ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        reply.ok();
    }

    fn access(&mut self, _req: &Request, _ino: u64, _mask: i32, reply: fuser::ReplyEmpty) {
        // All access control is handled by FSAL policy enforcement.
        // DefaultPermissions mount option handles basic kernel checks.
        reply.ok();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Error mapping
// ─────────────────────────────────────────────────────────────────────────────

/// Map FSAL errors to POSIX errno values for FUSE responses.
fn fsal_error_to_errno(e: &crate::domain::fsal::FsalError) -> i32 {
    use crate::domain::fsal::FsalError;
    match e {
        FsalError::UnauthorizedAccess { .. } => libc::EACCES,
        FsalError::VolumeNotFound(_) => libc::ENOENT,
        FsalError::VolumeNotAttached(_) => libc::ENOENT,
        FsalError::PathSanitization(_) => libc::EINVAL,
        FsalError::Storage(_) => libc::EIO,
        FsalError::PolicyViolation(_) => libc::EACCES,
        FsalError::InvalidFileHandle => libc::ESTALE,
        FsalError::HandleDeserialization(_) => libc::EIO,
        FsalError::QuotaExceeded { .. } => libc::ENOSPC,
    }
}
