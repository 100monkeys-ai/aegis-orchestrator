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
use crate::domain::fsal::{AegisFSAL, AegisFileHandle, FsalAccessPolicy};
use crate::domain::storage::FileType;
use crate::domain::volume::VolumeId;

use super::fsal_backend::{DirectFsalBackend, FsalBackend};

use super::inode_table::{InodeTable, ROOT_INODE};

use fuser::{
    AccessFlags, BsdFileFlags, Config as FuseConfig, Errno, FileAttr, FileHandle,
    FileType as FuseFileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags,
    RenameFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyWrite, Request, SessionACL,
    WriteFlags,
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
        debug!(mountpoint = %self.mountpoint, "FUSE mount handle dropped — flushing and unmounting");
        // Flush kernel page cache before BackgroundSession::drop() unmounts.
        if let Ok(fd) = std::fs::File::open(&self.mountpoint) {
            use std::os::unix::io::AsRawFd;
            // SAFETY: fd is a valid open file descriptor.
            let _ = unsafe { libc::syncfs(fd.as_raw_fd()) };
        }
        // BackgroundSession::drop() sends FUSE_DESTROY and unmounts automatically.
    }
}

/// FUSE-based FSAL transport daemon.
///
/// Creates per-volume FUSE mountpoints on the host that agent containers can
/// access via bind mounts. The daemon delegates all FSAL operations through the
/// `FsalBackend` trait, enabling both in-process (direct) and out-of-process
/// (gRPC) operation.
pub struct FuseFsalDaemon {
    backend: Arc<dyn FsalBackend>,
}

impl FuseFsalDaemon {
    /// Create a new FUSE daemon backed by the given FSAL instance (in-process).
    pub fn new(fsal: Arc<AegisFSAL>) -> Self {
        Self {
            backend: Arc::new(DirectFsalBackend(fsal)),
        }
    }

    /// Create a new FUSE daemon backed by a `FsalBackend` trait object.
    ///
    /// Use this constructor for the host-side daemon with a `GrpcFsalBackend`,
    /// or any other backend implementation.
    pub fn with_backend(backend: Arc<dyn FsalBackend>) -> Self {
        Self { backend }
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
            backend: self.backend.clone(),
            context: context.clone(),
            inode_table: Arc::new(InodeTable::new()),
            runtime: tokio::runtime::Handle::current(),
        };

        // Mount options:
        // - allow_other: let the container user access the mount (requires /etc/fuse.conf)
        // - default_permissions: let the kernel enforce basic permission checks
        // - fsname: identifies this mount in /proc/mounts
        let mut config = FuseConfig::default();
        config.mount_options = vec![
            fuser::MountOption::FSName(format!("aegis-fsal-{}", context.volume_id)),
            fuser::MountOption::DefaultPermissions,
            fuser::MountOption::RW,
        ];
        config.acl = SessionACL::All; // allow_other: let the container user access the mount

        let session = fuser::spawn_mount2(fs, mountpoint, &config).map_err(|e| {
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

/// Internal FUSE filesystem implementation that delegates to `FsalBackend`.
///
/// Each instance is bound to a single (execution_id, volume_id) pair —
/// unlike the NFS server which multiplexes all volumes on a single port.
struct FuseFsal {
    backend: Arc<dyn FsalBackend>,
    context: FuseVolumeContext,
    inode_table: Arc<InodeTable>,
    /// Tokio runtime handle captured before `fuser::spawn_mount2()` is called.
    ///
    /// FUSE callbacks run on OS threads spawned by `fuser` that have no Tokio
    /// runtime context. `Handle::current()` would panic in those threads, so the
    /// handle is captured in the Tokio context and stored here for use by
    /// `block_on`.
    runtime: tokio::runtime::Handle,
}

impl FuseFsal {
    /// Get or create the root file handle for this volume.
    fn root_handle(&self) -> AegisFileHandle {
        if let Some(wf_id) = self.context.workflow_execution_id {
            AegisFileHandle::new_for_workflow(wf_id, self.context.volume_id, "/")
        } else {
            AegisFileHandle::new(self.context.execution_id, self.context.volume_id, "/")
        }
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
            ino: INodeNo(ino),
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
            ino: INodeNo(ROOT_INODE),
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
    /// Uses the Tokio runtime handle captured before `fuser::spawn_mount2()` was
    /// called. FUSE callbacks run on OS threads that have no Tokio runtime
    /// context, so `Handle::current()` would panic — the stored handle avoids
    /// that. `block_on` is safe here because FUSE callbacks never run on the
    /// Tokio executor threads themselves.
    fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        self.runtime.block_on(f)
    }
}

impl Filesystem for FuseFsal {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_u64: u64 = parent.into();
        debug!(parent = parent_u64, name = %name_str, "FUSE LOOKUP");

        let (parent_handle, parent_path) = match self.resolve_inode(parent_u64) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Perform FSAL lookup to get a child handle
        let child_handle =
            match self.block_on(self.backend.lookup(&parent_handle, &parent_path, name_str)) {
                Ok(h) => h,
                Err(e) => {
                    debug!(error = %e, "FUSE lookup failed");
                    reply.error(Errno::ENOENT);
                    return;
                }
            };

        let child_path = Self::child_path(&parent_path, name_str);
        let child_ino = self.inode_table.allocate(child_handle, child_path.clone());

        // Get attributes for the child
        match self.block_on(self.backend.getattr(
            self.context.execution_id,
            self.context.volume_id,
            &child_path,
            self.context.container_uid,
            self.context.container_gid,
            self.context.workflow_execution_id,
        )) {
            Ok(attrs) => {
                let fuse_attr = self.convert_attrs(&attrs, child_ino);
                metrics::counter!("aegis_fuse_operations_total", "operation" => "lookup", "result" => "success").increment(1);
                reply.entry(&FUSE_TTL, &fuse_attr, Generation(0));
            }
            Err(e) => {
                // Path exists (lookup succeeded) but stat failed — may be a race
                // or the storage backend is lazily creating entries. Return ENOENT
                // rather than EIO to allow retries.
                debug!(error = %e, path = %child_path, "FUSE lookup getattr failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "lookup", "result" => "error").increment(1);
                reply.error(Errno::ENOENT);
            }
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino: u64 = ino.into();
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
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match self.block_on(self.backend.getattr(
            self.context.execution_id,
            self.context.volume_id,
            &path,
            self.context.container_uid,
            self.context.container_gid,
            self.context.workflow_execution_id,
        )) {
            Ok(attrs) => {
                let fuse_attr = self.convert_attrs(&attrs, ino);
                metrics::counter!("aegis_fuse_operations_total", "operation" => "getattr", "result" => "success").increment(1);
                reply.attr(&FUSE_TTL, &fuse_attr);
            }
            Err(e) => {
                warn!(error = %e, ino = ino, path = %path, "FUSE getattr failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "getattr", "result" => "error").increment(1);
                reply.error(Errno::ENOENT);
            }
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let ino: u64 = ino.into();
        debug!(ino = ino, offset = offset, size = size, "FUSE READ");

        let (handle, path) = match self.resolve_inode(ino) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match self.block_on(self.backend.read(
            &handle,
            &path,
            &self.context.policy,
            offset,
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
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let ino: u64 = ino.into();
        debug!(ino = ino, offset = offset, len = data.len(), "FUSE WRITE");

        let (handle, path) = match self.resolve_inode(ino) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match self.block_on(
            self.backend
                .write(&handle, &path, &self.context.policy, offset, data),
        ) {
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
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino: u64 = ino.into();
        debug!(ino = ino, offset = offset, "FUSE READDIR");

        let (_handle, dir_path) = match self.resolve_inode(ino) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let entries = match self.block_on(self.backend.readdir(
            self.context.execution_id,
            self.context.volume_id,
            &dir_path,
            &self.context.policy,
            self.context.workflow_execution_id,
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
            let child_handle = if let Some(wf_id) = self.context.workflow_execution_id {
                AegisFileHandle::new_for_workflow(wf_id, self.context.volume_id, &entry_path)
            } else {
                AegisFileHandle::new(
                    self.context.execution_id,
                    self.context.volume_id,
                    &entry_path,
                )
            };
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
            if reply.add(INodeNo(*child_ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }

        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_u64: u64 = parent.into();
        debug!(parent = parent_u64, name = %name_str, "FUSE CREATE");

        let (_parent_handle, parent_path) = match self.resolve_inode(parent_u64) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let file_path = Self::child_path(&parent_path, name_str);

        match self.block_on(self.backend.create_file(
            self.context.execution_id,
            self.context.volume_id,
            &file_path,
            &self.context.policy,
            self.context.workflow_execution_id,
        )) {
            Ok(handle) => {
                let child_ino = self.inode_table.allocate(handle, file_path.clone());

                // Get attrs for the newly created file
                match self.block_on(self.backend.getattr(
                    self.context.execution_id,
                    self.context.volume_id,
                    &file_path,
                    self.context.container_uid,
                    self.context.container_gid,
                    self.context.workflow_execution_id,
                )) {
                    Ok(attrs) => {
                        let fuse_attr = self.convert_attrs(&attrs, child_ino);
                        metrics::counter!("aegis_fuse_operations_total", "operation" => "create", "result" => "success").increment(1);
                        reply.created(
                            &FUSE_TTL,
                            &fuse_attr,
                            Generation(0),
                            FileHandle(0),
                            FopenFlags::empty(),
                        );
                    }
                    Err(e) => {
                        // File was created but stat failed — unlikely but handle gracefully.
                        // Return a synthetic file attr rather than failing.
                        warn!(error = %e, path = %file_path, "FUSE create getattr failed — using synthetic attrs");
                        let attr = FileAttr {
                            ino: INodeNo(child_ino),
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
                        reply.created(
                            &FUSE_TTL,
                            &attr,
                            Generation(0),
                            FileHandle(0),
                            FopenFlags::empty(),
                        );
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
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_u64: u64 = parent.into();
        debug!(parent = parent_u64, name = %name_str, "FUSE MKDIR");

        let (_parent_handle, parent_path) = match self.resolve_inode(parent_u64) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let dir_path = Self::child_path(&parent_path, name_str);

        match self.block_on(self.backend.create_directory(
            self.context.execution_id,
            self.context.volume_id,
            &dir_path,
            &self.context.policy,
            self.context.workflow_execution_id,
        )) {
            Ok(()) => {
                let handle = if let Some(wf_id) = self.context.workflow_execution_id {
                    AegisFileHandle::new_for_workflow(wf_id, self.context.volume_id, &dir_path)
                } else {
                    AegisFileHandle::new(
                        self.context.execution_id,
                        self.context.volume_id,
                        &dir_path,
                    )
                };
                let child_ino = self.inode_table.allocate(handle, dir_path.clone());

                let attr = FileAttr {
                    ino: INodeNo(child_ino),
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
                reply.entry(&FUSE_TTL, &attr, Generation(0));
            }
            Err(e) => {
                error!(error = %e, "FUSE mkdir failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "mkdir", "result" => "error").increment(1);
                reply.error(fsal_error_to_errno(&e));
            }
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_u64: u64 = parent.into();
        debug!(parent = parent_u64, name = %name_str, "FUSE UNLINK");

        let (_parent_handle, parent_path) = match self.resolve_inode(parent_u64) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let file_path = Self::child_path(&parent_path, name_str);

        match self.block_on(self.backend.delete_file(
            self.context.execution_id,
            self.context.volume_id,
            &file_path,
            &self.context.policy,
            self.context.workflow_execution_id,
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

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_u64: u64 = parent.into();
        debug!(parent = parent_u64, name = %name_str, "FUSE RMDIR");

        let (_parent_handle, parent_path) = match self.resolve_inode(parent_u64) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let dir_path = Self::child_path(&parent_path, name_str);

        match self.block_on(self.backend.delete_directory(
            self.context.execution_id,
            self.context.volume_id,
            &dir_path,
            &self.context.policy,
            self.context.workflow_execution_id,
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
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: fuser::ReplyEmpty,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let newname_str = match newname.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let parent_u64: u64 = parent.into();
        let newparent_u64: u64 = newparent.into();
        debug!(
            parent = parent_u64,
            name = %name_str,
            newparent = newparent_u64,
            newname = %newname_str,
            "FUSE RENAME"
        );

        let (_from_handle, from_parent_path) = match self.resolve_inode(parent_u64) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let (_to_handle, to_parent_path) = match self.resolve_inode(newparent_u64) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let from_path = Self::child_path(&from_parent_path, name_str);
        let to_path = Self::child_path(&to_parent_path, newname_str);

        match self.block_on(self.backend.rename(
            self.context.execution_id,
            self.context.volume_id,
            &from_path,
            &to_path,
            &self.context.policy,
            self.context.workflow_execution_id,
        )) {
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
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // FSAL does not support chmod/chown/truncate as discrete operations.
        // Return current attributes unchanged (UID/GID squashing makes this a no-op).
        let ino: u64 = ino.into();
        debug!(ino = ino, "FUSE SETATTR (no-op, returning current attrs)");

        if ino == ROOT_INODE {
            reply.attr(&FUSE_TTL, &self.root_attr());
            return;
        }

        let (_handle, path) = match self.resolve_inode(ino) {
            Some(v) => v,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match self.block_on(self.backend.getattr(
            self.context.execution_id,
            self.context.volume_id,
            &path,
            self.context.container_uid,
            self.context.container_gid,
            self.context.workflow_execution_id,
        )) {
            Ok(attrs) => {
                reply.attr(&FUSE_TTL, &self.convert_attrs(&attrs, ino));
            }
            Err(_) => {
                reply.error(Errno::ENOENT);
            }
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: fuser::ReplyOpen) {
        // Stateless open — all I/O goes through read/write with inode lookup.
        // Return fh=0, no special flags.
        let ino: u64 = ino.into();
        debug!(ino = ino, "FUSE OPEN (stateless)");
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: fuser::ReplyOpen) {
        let ino: u64 = ino.into();
        debug!(ino = ino, "FUSE OPENDIR (stateless)");
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: fuser::ReplyEmpty,
    ) {
        reply.ok();
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: fuser::ReplyEmpty,
    ) {
        // All FSAL writes are synchronous — the gRPC call to the storage
        // backend completes before the FUSE write callback returns. There are
        // no pending writes to drain, so flush is a no-op success.
        let ino: u64 = ino.into();
        debug!(ino = ino, "FUSE FLUSH (synchronous write path — no-op)");
        metrics::counter!("aegis_fuse_operations_total", "operation" => "flush", "result" => "success").increment(1);
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        reply.ok();
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: fuser::ReplyEmpty) {
        // All access control is handled by FSAL policy enforcement.
        // DefaultPermissions mount option handles basic kernel checks.
        reply.ok();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Error mapping
// ─────────────────────────────────────────────────────────────────────────────

/// Map FSAL errors to fuser `Errno` values for FUSE responses.
fn fsal_error_to_errno(e: &crate::domain::fsal::FsalError) -> Errno {
    use crate::domain::fsal::FsalError;
    match e {
        FsalError::UnauthorizedAccess { .. } => Errno::EACCES,
        FsalError::VolumeNotFound(_) => Errno::ENOENT,
        FsalError::VolumeNotAttached(_) => Errno::ENOENT,
        FsalError::PathSanitization(_) => Errno::EINVAL,
        FsalError::Storage(_) => Errno::EIO,
        FsalError::PolicyViolation(_) => Errno::EACCES,
        FsalError::InvalidFileHandle => Errno::ESTALE,
        FsalError::HandleDeserialization(_) => Errno::EIO,
        FsalError::QuotaExceeded { .. } => Errno::ENOSPC,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::execution::ExecutionId;
    use crate::domain::fsal::{AegisFileHandle, FsalAccessPolicy, FsalError};
    use crate::domain::storage::{DirEntry, FileAttributes};
    use crate::domain::volume::VolumeId;
    use async_trait::async_trait;

    // ── Stub backend ──────────────────────────────────────────────────────────

    /// Minimal no-op `FsalBackend` used only to construct `FuseFsal` in tests.
    struct StubBackend;

    #[async_trait]
    impl FsalBackend for StubBackend {
        async fn getattr(
            &self,
            _: ExecutionId,
            _: VolumeId,
            _: &str,
            _: u32,
            _: u32,
            _: Option<uuid::Uuid>,
        ) -> Result<FileAttributes, FsalError> {
            unimplemented!("stub")
        }

        async fn lookup(
            &self,
            _: &AegisFileHandle,
            _: &str,
            _: &str,
        ) -> Result<AegisFileHandle, FsalError> {
            unimplemented!("stub")
        }

        async fn readdir(
            &self,
            _: ExecutionId,
            _: VolumeId,
            _: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<Vec<DirEntry>, FsalError> {
            unimplemented!("stub")
        }

        async fn read(
            &self,
            _: &AegisFileHandle,
            _: &str,
            _: &FsalAccessPolicy,
            _: u64,
            _: usize,
        ) -> Result<Vec<u8>, FsalError> {
            unimplemented!("stub")
        }

        async fn write(
            &self,
            _: &AegisFileHandle,
            _: &str,
            _: &FsalAccessPolicy,
            _: u64,
            _: &[u8],
        ) -> Result<usize, FsalError> {
            unimplemented!("stub")
        }

        async fn create_file(
            &self,
            _: ExecutionId,
            _: VolumeId,
            _: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<AegisFileHandle, FsalError> {
            unimplemented!("stub")
        }

        async fn create_directory(
            &self,
            _: ExecutionId,
            _: VolumeId,
            _: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<(), FsalError> {
            unimplemented!("stub")
        }

        async fn delete_file(
            &self,
            _: ExecutionId,
            _: VolumeId,
            _: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<(), FsalError> {
            unimplemented!("stub")
        }

        async fn delete_directory(
            &self,
            _: ExecutionId,
            _: VolumeId,
            _: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<(), FsalError> {
            unimplemented!("stub")
        }

        async fn rename(
            &self,
            _: ExecutionId,
            _: VolumeId,
            _: &str,
            _: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<(), FsalError> {
            unimplemented!("stub")
        }
    }

    // ── Helper ────────────────────────────────────────────────────────────────

    /// Build a `FuseFsal` with the given runtime handle and a stub backend.
    fn make_fuse_fsal(runtime: tokio::runtime::Handle) -> FuseFsal {
        FuseFsal {
            backend: Arc::new(StubBackend),
            context: FuseVolumeContext {
                execution_id: ExecutionId::new(),
                volume_id: VolumeId::new(),
                workflow_execution_id: None,
                container_uid: 1000,
                container_gid: 1000,
                policy: FsalAccessPolicy::default(),
            },
            inode_table: Arc::new(InodeTable::new()),
            runtime,
        }
    }

    // ── Regression test ───────────────────────────────────────────────────────

    /// Regression: `FuseFsal::block_on` must work on OS threads that have no
    /// Tokio runtime context (as is the case for threads spawned by
    /// `fuser::spawn_mount2`).
    ///
    /// Before the fix, `block_on` called `Handle::current()` which panics on
    /// any thread without a Tokio runtime. After the fix, the handle is captured
    /// before mounting and stored in `FuseFsal::runtime`, so FUSE callback
    /// threads can safely use it.
    #[test]
    fn block_on_works_from_non_tokio_thread() {
        // Build a multi-thread Tokio runtime to act as the "outer" runtime that
        // would normally be running the orchestrator (i.e. the runtime context
        // that exists *before* spawn_mount2 is called).
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("failed to build tokio runtime");

        // Capture the handle while we're inside the runtime context — this
        // mirrors exactly what `FuseFsalDaemon::mount()` now does.
        let handle = rt.handle().clone();

        // Construct FuseFsal with the captured handle (simulates post-fix code).
        let fsal = make_fuse_fsal(handle);

        // Spawn a plain OS thread (no Tokio context) — this simulates a FUSE
        // callback thread created by fuser::spawn_mount2.
        let result = std::thread::spawn(move || {
            // This would panic with "there is no reactor running" before the fix.
            fsal.block_on(async { 42_u32 })
        })
        .join()
        .expect("thread panicked — block_on failed on non-Tokio thread");

        assert_eq!(result, 42, "block_on must return the future's output");

        rt.shutdown_background();
    }
}
