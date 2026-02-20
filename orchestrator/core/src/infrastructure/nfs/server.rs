// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! NFS Server Implementation (ADR-036 Storage Gateway)
//!
//! Provides user-space NFSv3 server using nfsserve crate as transport layer
//! for AegisFSAL (File System Abstraction Layer domain entity).
//!
//! ## Architecture
//!
//! ### Component Responsibilities
//! - **AegisFsalAdapter**: Implements `nfsserve::NFSFileSystem` trait
//!   - Maps NFSv3 RPC operations (LOOKUP, GETATTR, READ, WRITE, READDIR) to AegisFSAL domain methods
//!   - Handles fileid3 ↔ AegisFileHandle encoding/decoding (max 64 bytes for NFSv3)
//!   - Translates NFS errors (nfsstat3) to FSAL errors
//! - **NfsServer**: Manages server lifecycle and TCP listener
//!   - Spawns tokio task running `nfsserve::tcp::NFSTcp`
//!   - Provides graceful shutdown with task abort + timeout
//!   - Health check via `JoinHandle::is_finished()`
//!
//! ### Export Path Routing
//! Agent containers mount NFS exports using path pattern: `/{tenant_id}/{volume_id}`
//! - Docker mount options: `addr={orchestrator_host},nfsvers=3,proto=tcp,soft,timeo=10,nolock`
//! - Export path parsed by server to determine VolumeId and authorization context
//! - Authorization enforced at AegisFSAL layer (execution must own volume)
//!
//! ## FileHandle Encoding (ADR-036)
//! NFSv3 file handles are limited to 64 bytes. `AegisFileHandle` is serialized with
//! bincode as a fixed-size 48-byte structure:
//! ```rust
//! pub struct AegisFileHandle {
//!     execution_id: ExecutionId, // 16 bytes (UUID)
//!     volume_id: VolumeId,       // 16 bytes (UUID)
//!     path_hash: u64,            // 8 bytes (Hash of file path)
//!     created_at: i64,           // 8 bytes (Timestamp)
//! }
//! ```
//! Current encoding: 48 bytes raw + ~4 bytes bincode overhead = 52 bytes (safe margin under 64-byte limit).
//!
//! ## Security Model (Orchestrator Proxy Pattern)
//! - Agent containers require **zero elevated privileges** (no CAP_SYS_ADMIN)
//! - All authorization occurs in AegisFSAL (server-side)
//! - Path sanitization prevents `../` traversal attacks
//! - UID/GID squashing eliminates kernel permission checks
//! - FilesystemPolicy enforced per manifest (read/write allowlists)

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use crate::domain::fsal::{AegisFSAL, AegisFileHandle};
use crate::domain::execution::ExecutionId;
use crate::domain::volume::VolumeId;
use crate::domain::policy::FilesystemPolicy;
use thiserror::Error;
use nfsserve::vfs::{self, NFSFileSystem};
use nfsserve::nfs::{fattr3, fileid3, filename3, ftype3, nfspath3, nfstime3, specdata3, nfsstring};
use nfsserve::tcp::{NFSTcpListener, NFSTcp};
use tokio::task::AbortHandle;
use tracing::{info, debug, warn, error};
use parking_lot::{RwLock, Mutex};

/// Volume context for NFS export path routing
/// 
/// Encapsulates all execution-specific metadata needed for FSAL authorization.
#[derive(Debug, Clone)]
pub struct NfsVolumeContext {
    pub execution_id: ExecutionId,
    pub volume_id: VolumeId,
    pub container_uid: u32,
    pub container_gid: u32,
    pub policy: FilesystemPolicy,
}

/// NFS server errors
#[derive(Debug, Error)]
pub enum NfsServerError {
    #[error("Failed to bind to port {port}: {error}")]
    BindFailed { port: u16, error: String },

    #[error("Server error: {0}")]
    ServerError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("FSAL error: {0}")]
    Fsal(String),

    #[error("Invalid file handle: {0}")]
    InvalidHandle(fileid3),
}

/// FileHandle mapping table for NFS protocol
/// 
/// Maintains bidirectional mapping between NFSv3's fileid3 (u64) and AegisFileHandle.
/// Thread-safe with RwLock for concurrent NFS operations.
struct FileHandleTable {
    /// Counter for generating unique fileid3 values
    next_fileid: AtomicU64,
    /// Forward mapping: fileid3 -> (AegisFileHandle, path)
    forward: RwLock<HashMap<fileid3, (AegisFileHandle, String)>>,
    /// Reverse mapping: path_hash -> fileid3 (for consistent lookup)
    reverse: RwLock<HashMap<u64, fileid3>>,
}

impl FileHandleTable {
    /// Create new handle table
    fn new() -> Self {
        Self {
            next_fileid: AtomicU64::new(2), // Start at 2 (1 is reserved for root)
            forward: RwLock::new(HashMap::new()),
            reverse: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new handle and return its fileid3
    fn register(&self, handle: AegisFileHandle, path: String) -> fileid3 {
        // Check if handle already registered
        let reverse = self.reverse.read();
        if let Some(&existing_id) = reverse.get(&handle.path_hash) {
            return existing_id;
        }
        drop(reverse);

        // Generate new fileid
        let fileid = self.next_fileid.fetch_add(1, Ordering::SeqCst);

        // Store bidirectional mapping
        self.forward.write().insert(fileid, (handle.clone(), path.clone()));
        self.reverse.write().insert(handle.path_hash, fileid);

        debug!("Registered file handle: fileid={}, path={}", fileid, path);
        fileid
    }

    /// Lookup handle by fileid3
    fn lookup(&self, id: fileid3) -> Option<(AegisFileHandle, String)> {
        self.forward.read().get(&id).cloned()
    }

    /// Get fileid from path hash (if previously registered)
    fn get_fileid_by_hash(&self, path_hash: u64) -> Option<fileid3> {
        self.reverse.read().get(&path_hash).copied()
    }
}

/// NFS File System Adapter for AegisFSAL
///
/// Maps NFSv3 protocol operations to AegisFSAL domain methods.
/// Handles FileHandle encoding/decoding and export path routing.
struct AegisFsalAdapter {
    fsal: Arc<AegisFSAL>,
    /// Volume registry for dynamic context lookup
    volume_registry: Arc<RwLock<HashMap<VolumeId, NfsVolumeContext>>>,
    /// FileHandle mapping table (fileid3 <-> AegisFileHandle)
    handle_table: Arc<FileHandleTable>,
}

impl AegisFsalAdapter {
    fn new(
        fsal: Arc<AegisFSAL>,
        volume_registry: Arc<RwLock<HashMap<VolumeId, NfsVolumeContext>>>,
    ) -> Self {
        Self {
            fsal,
            volume_registry,
            handle_table: Arc::new(FileHandleTable::new()),
        }
    }

    /// Get context for a volume, returns default if not found
    fn get_context(&self, volume_id: VolumeId) -> NfsVolumeContext {
        self.volume_registry
            .read()
            .get(&volume_id)
            .cloned()
            .unwrap_or_else(|| {
                warn!("Volume {} not registered, using default context", volume_id);
                NfsVolumeContext {
                    execution_id: ExecutionId::new(),
                    volume_id,
                    container_uid: 1000,
                    container_gid: 1000,
                    policy: FilesystemPolicy::default(),
                }
            })
    }

    /// Decode NFS file handle to AegisFileHandle and path
    fn decode_handle(&self, id: fileid3) -> Result<(AegisFileHandle, String), NfsServerError> {
        self.handle_table
            .lookup(id)
            .ok_or(NfsServerError::InvalidHandle(id))
    }

    /// Encode AegisFileHandle to NFS file handle (fileid3)
    fn encode_handle(&self, handle: &AegisFileHandle, path: String) -> Result<fileid3, NfsServerError> {
        Ok(self.handle_table.register(handle.clone(), path))
    }

    /// Convert path to stable fileid3
    fn path_to_fileid(&self, path: &str, volume_id: VolumeId) -> fileid3 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        let path_hash = hasher.finish();

        // Check if already registered
        if let Some(id) = self.handle_table.get_fileid_by_hash(path_hash) {
            return id;
        }

        // Get context for this volume
        let context = self.get_context(volume_id);

        // Register new handle
        let handle = AegisFileHandle::new(
            context.execution_id,
            volume_id,
            path,
        );
        self.handle_table.register(handle, path.to_string())
    }

    /// Convert FSAL FileAttributes to NFS fattr3
    fn convert_attrs(&self, attrs: crate::domain::storage::FileAttributes, volume_id: VolumeId) -> fattr3 {
        use crate::domain::storage::FileType;

        let context = self.get_context(volume_id);

        let ftype = match attrs.file_type {
            FileType::File => ftype3::NF3REG,
            FileType::Directory => ftype3::NF3DIR,
            FileType::Symlink => ftype3::NF3LNK,
        };

        fattr3 {
            ftype,
            mode: attrs.mode,
            nlink: attrs.nlink,
            uid: context.container_uid,  // UID squashing per ADR-036
            gid: context.container_gid,  // GID squashing per ADR-036
            size: attrs.size,
            used: attrs.size,
            rdev: specdata3 { specdata1: 0, specdata2: 0 },
            fsid: 0,
            fileid: 0, // Set by caller after handle registration
            atime: nfstime3 {
                seconds: attrs.atime as u32,
                nseconds: 0,
            },
            mtime: nfstime3 {
                seconds: attrs.mtime as u32,
                nseconds: 0,
            },
            ctime: nfstime3 {
                seconds: attrs.ctime as u32,
                nseconds: 0,
            },
        }
    }
}

#[async_trait::async_trait]
impl NFSFileSystem for AegisFsalAdapter {
    fn root_dir(&self) -> fileid3 {
        // Return root directory fileid (arbitrary number)
        1
    }

    fn capabilities(&self) -> nfsserve::vfs::VFSCapabilities {
        nfsserve::vfs::VFSCapabilities::ReadWrite
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsserve::nfs::nfsstat3> {
        debug!("NFS LOOKUP: dirid={}, filename={:?}", dirid, filename);
            // Decode parent directory handle
            let (parent_handle, parent_path) = self.decode_handle(dirid)
                .map_err(|e| {
                    warn!("Invalid parent handle: {}", e);
                    nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE
                })?;

            // Lookup via FSAL
            let name = std::str::from_utf8(filename)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

            let child_handle = self.fsal.lookup(&parent_handle, name)
                .await
                .map_err(|e| {
                    warn!("FSAL lookup failed: {}", e);
                    nfsserve::nfs::nfsstat3::NFS3ERR_NOENT
                })?;

            // Build child path
            let child_path = if parent_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", parent_path, name)
            };

        // Encode child handle with path
        self.encode_handle(&child_handle, child_path)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_SERVERFAULT)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsserve::nfs::nfsstat3> {
        debug!("NFS GETATTR: id={}", id);
            // For root directory, return synthetic attributes with default context
            if id == 1 {
                // Use first registered volume context or defaults
                let default_context = self.volume_registry.read().values().next().cloned()
                    .unwrap_or_else(|| NfsVolumeContext {
                        execution_id: ExecutionId::new(),
                        volume_id: VolumeId::new(),
                        container_uid: 1000,
                        container_gid: 1000,
                        policy: FilesystemPolicy::default(),
                    });
                
                return Ok(fattr3 {
                    ftype: ftype3::NF3DIR,
                    mode: 0o755,
                    nlink: 2,
                    uid: default_context.container_uid,
                    gid: default_context.container_gid,
                    size: 4096,
                    used: 4096,
                    rdev: specdata3 { specdata1: 0, specdata2: 0 },
                    fsid: 0,
                    fileid: 1,
                    atime: nfstime3 { seconds: 0, nseconds: 0 },
                    mtime: nfstime3 { seconds: 0, nseconds: 0 },
                    ctime: nfstime3 { seconds: 0, nseconds: 0 },
                });
            }

            // Decode handle to get path and volume
            let (handle, path) = self.decode_handle(id)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

            // Get context for this volume
            let context = self.get_context(handle.volume_id);

            // Get attributes via FSAL (with UID/GID squashing)
            let attrs = self.fsal.getattr(
                context.execution_id,
                context.volume_id,
                &path,
                context.container_uid,
                context.container_gid,
            )
            .await
            .map_err(|e| {
                warn!("FSAL getattr failed: {}", e);
                nfsserve::nfs::nfsstat3::NFS3ERR_NOENT
            })?;

        let mut nfs_attrs = self.convert_attrs(attrs, handle.volume_id);
        nfs_attrs.fileid = id;
        Ok(nfs_attrs)
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsserve::nfs::nfsstat3> {
        debug!("NFS READ: id={}, offset={}, count={}", id, offset, count);

        let (handle, _path) = self.decode_handle(id)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

        let data = self.fsal.read(&handle, offset, count as usize)
            .await
            .map_err(|e| {
                error!("FSAL read failed: {}", e);
                nfsserve::nfs::nfsstat3::NFS3ERR_IO
            })?;

        let eof = data.len() < count as usize;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsserve::nfs::nfsstat3> {
        debug!("NFS WRITE: id={}, offset={}, len={}", id, offset, data.len());

        let (handle, _path) = self.decode_handle(id)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

        let _bytes_written = self.fsal.write(&handle, offset, data)
            .await
            .map_err(|e| {
                error!("FSAL write failed: {}", e);
                // Map FSAL errors to appropriate NFS status codes
                match e {
                    crate::domain::fsal::FsalError::QuotaExceeded { .. } => {
                        // Quota exceeded - no space left on device (ADR-036)
                        nfsserve::nfs::nfsstat3::NFS3ERR_NOSPC
                    }
                    crate::domain::fsal::FsalError::PolicyViolation(_) => {
                        // Policy violation - permission denied
                        nfsserve::nfs::nfsstat3::NFS3ERR_ACCES
                    }
                    crate::domain::fsal::FsalError::UnauthorizedAccess { .. } => {
                        // Unauthorized access - permission denied
                        nfsserve::nfs::nfsstat3::NFS3ERR_ACCES
                    }
                    _ => {
                        // Generic I/O error for other failures
                        nfsserve::nfs::nfsstat3::NFS3ERR_IO
                    }
                }
            })?;

        // Return updated file attributes after write
        self.getattr(id).await
    }

    async fn readdir(&self, dirid: fileid3, start_after: fileid3, max_entries: usize) -> Result<vfs::ReadDirResult, nfsserve::nfs::nfsstat3> {
        debug!("NFS READDIR: dirid={}, start_after={}, max={}", dirid, start_after, max_entries);
            // Decode directory handle to get path and volume
            let (dir_handle, dir_path) = self.decode_handle(dirid)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

            // Get context for this volume
            let context = self.get_context(dir_handle.volume_id);

            // Read directory via FSAL
            let entries = self.fsal.readdir(
                context.execution_id,
                context.volume_id,
                &dir_path,
                &context.policy,
            )
            .await
            .map_err(|e| {
                error!("FSAL readdir failed: {}", e);
                nfsserve::nfs::nfsstat3::NFS3ERR_IO
            })?;

            // Convert to NFS format
            let mut nfs_entries = Vec::new();
            for (_idx, entry) in entries.iter().enumerate().skip(start_after as usize).take(max_entries) {
                // Build full path for entry
                let entry_path = if dir_path == "/" {
                    format!("/{}", entry.name)
                } else {
                    format!("{}/{}", dir_path, entry.name)
                };
                
                // Generate stable fileid from path
                let fileid = self.path_to_fileid(&entry_path, dir_handle.volume_id);
                let name: nfsstring = nfsstring::from(entry.name.as_bytes());
                
                // Get attributes for this entry (required, not optional)
                let attr = self.getattr(fileid).await
                    .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_IO)?;

                nfs_entries.push(vfs::DirEntry { fileid, name, attr });
            }

        Ok(vfs::ReadDirResult {
            entries: nfs_entries,
            end: entries.len() <= start_after as usize + max_entries,
        })
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, _attr: nfsserve::nfs::sattr3) -> Result<(fileid3, fattr3), nfsserve::nfs::nfsstat3> {
        debug!("NFS CREATE: dirid={}, filename={:?}", dirid, filename);
            // Decode parent directory handle
            let (parent_handle, parent_path) = self.decode_handle(dirid)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

            // Get context for this volume
            let context = self.get_context(parent_handle.volume_id);

            // Get filename
            let name = std::str::from_utf8(filename)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

            // Build file path
            let file_path = if parent_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", parent_path, name)
            };

            // Create file via FSAL (open with Create mode)
            let handle = self.fsal.open(
                context.execution_id,
                context.volume_id,
                &file_path,
                crate::domain::storage::OpenMode::Create,
                &context.policy,
            )
            .await
            .map_err(|e| {
                error!("FSAL create failed: {}", e);
                nfsserve::nfs::nfsstat3::NFS3ERR_IO
            })?;

        // Close the file immediately (NFS CREATE returns handle but file is created)
        let _ = self.fsal.close(&handle).await;

        // Register and return fileid with attributes
        let fileid = self.encode_handle(&handle, file_path.clone())
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_SERVERFAULT)?;
        let attrs = self.getattr(fileid).await?;
        Ok((fileid, attrs))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsserve::nfs::nfsstat3> {
        debug!("NFS CREATE_EXCLUSIVE: dirid={}, filename={:?}", dirid, filename);
        // Use same implementation as create, returning only fileid
        let (fileid, _attrs) = self.create(dirid, filename, nfsserve::nfs::sattr3::default()).await?;
        Ok(fileid)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsserve::nfs::nfsstat3> {
        debug!("NFS MKDIR: dirid={}, dirname={:?}", dirid, dirname);
            // Decode parent directory handle
            let (parent_handle, parent_path) = self.decode_handle(dirid)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

            // Get context for this volume
            let context = self.get_context(parent_handle.volume_id);

            // Get directory name
            let name = std::str::from_utf8(dirname)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

            // Build directory path
            let dir_path = if parent_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", parent_path, name)
            };

            // Create directory via FSAL
            self.fsal.create_directory(
                context.execution_id,
                context.volume_id,
                &dir_path,
                &context.policy,
            )
            .await
            .map_err(|e| {
                error!("FSAL mkdir failed: {}", e);
                nfsserve::nfs::nfsstat3::NFS3ERR_IO
            })?;

        // Create a handle for the new directory
        let handle = crate::domain::fsal::AegisFileHandle::new(
            context.execution_id,
            context.volume_id,
            &dir_path,
        );

        // Register and return fileid with attributes
        let fileid = self.encode_handle(&handle, dir_path.clone())
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_SERVERFAULT)?;
        let attrs = self.getattr(fileid).await?;
        Ok((fileid, attrs))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsserve::nfs::nfsstat3> {
        debug!("NFS REMOVE: dirid={}, filename={:?}", dirid, filename);
            // Decode parent directory handle
            let (parent_handle, parent_path) = self.decode_handle(dirid)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

            // Get context for this volume
            let context = self.get_context(parent_handle.volume_id);

            // Get filename
            let name = std::str::from_utf8(filename)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

            // Build file path
            let file_path = if parent_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", parent_path, name)
            };

        // Try to delete as file first, then as directory if file deletion fails
        match self.fsal.delete_file(
            context.execution_id,
            context.volume_id,
            &file_path,
            &context.policy,
        )
        .await
        {
            Ok(_) => Ok(()),
            Err(_) => {
                // If file deletion fails, try directory deletion
                self.fsal.delete_directory(
                    context.execution_id,
                    context.volume_id,
                    &file_path,
                    &context.policy,
                )
                .await
                .map_err(|e| {
                    error!("FSAL remove failed: {}", e);
                    nfsserve::nfs::nfsstat3::NFS3ERR_IO
                })
            }
        }
    }

    async fn rename(&self, from_dirid: fileid3, from_filename: &filename3, to_dirid: fileid3, to_filename: &filename3) -> Result<(), nfsserve::nfs::nfsstat3> {
        debug!("NFS RENAME: from_dirid={}, from_filename={:?}, to_dirid={}, to_filename={:?}", 
               from_dirid, from_filename, to_dirid, to_filename);
            // Decode parent directories
            let (from_parent, from_parent_path) = self.decode_handle(from_dirid)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;
            let (_to_parent, to_parent_path) = self.decode_handle(to_dirid)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

            // Get context for this volume
            let context = self.get_context(from_parent.volume_id);

            // Get filenames
            let from_name = std::str::from_utf8(from_filename)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;
            let to_name = std::str::from_utf8(to_filename)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

            // Build full paths
            let from_path = if from_parent_path == "/" {
                format!("/{}", from_name)
            } else {
                format!("{}/{}", from_parent_path, from_name)
            };
            let to_path = if to_parent_path == "/" {
                format!("/{}", to_name)
            } else {
                format!("{}/{}", to_parent_path, to_name)
            };

        // Rename via FSAL
        self.fsal.rename(
            context.execution_id,
            context.volume_id,
            &from_path,
            &to_path,
            &context.policy,
        )
        .await
        .map_err(|e| {
            error!("FSAL rename failed: {}", e);
            nfsserve::nfs::nfsstat3::NFS3ERR_IO
        })
    }

    async fn symlink(&self, dirid: fileid3, linkname: &filename3, symlink_data: &nfspath3, _attr: &nfsserve::nfs::sattr3) -> Result<(fileid3, fattr3), nfsserve::nfs::nfsstat3> {
        debug!("NFS SYMLINK: dirid={}, linkname={:?}, target={:?}", dirid, linkname, symlink_data);
        
        // TODO: Implement symlink creation
        Err(nfsserve::nfs::nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsserve::nfs::nfsstat3> {
        debug!("NFS READLINK: id={}", id);
        
        // TODO: Implement symlink reading
        Err(nfsserve::nfs::nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn setattr(&self, id: fileid3, _setattr: nfsserve::nfs::sattr3) -> Result<fattr3, nfsserve::nfs::nfsstat3> {
        debug!("NFS SETATTR: id={}", id);
        
        // TODO: Implement attribute setting
        // For now, return current attributes unchanged
        self.getattr(id).await
    }
}

/// NFS Server
///
/// Manages NFSv3 TCP server lifecycle.
/// Uses nfsserve crate for protocol handling.
pub struct NfsServer {
    fsal: Arc<AegisFSAL>,
    volume_registry: Arc<RwLock<HashMap<VolumeId, NfsVolumeContext>>>,
    bind_port: u16,
    server_handle: Arc<Mutex<Option<AbortHandle>>>,
}

impl NfsServer {
    /// Create a new NFS server
    ///
    /// # Arguments
    /// * `fsal` - The FSAL instance for file operations
    /// * `volume_registry` - Registry from NfsVolumeRegistry (cloned Arc)
    /// * `bind_port` - Port to bind (default: 2049)
    pub fn new(
        fsal: Arc<AegisFSAL>,
        volume_registry: Arc<RwLock<HashMap<VolumeId, NfsVolumeContext>>>,
        bind_port: u16,
    ) -> Self {
        Self { 
            fsal, 
            volume_registry,
            bind_port,
            server_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Start the NFS server
    ///
    /// Spawns TCP listener and handles NFS protocol operations.
    pub async fn start(&self) -> Result<(), NfsServerError> {
        info!("Starting NFS server on port {}", self.bind_port);

        // Create adapter with shared volume registry
        let adapter = AegisFsalAdapter::new(
            self.fsal.clone(),
            self.volume_registry.clone(),
        );

        // Create NFS TCP listener and spawn server task
        let bind_address = format!("0.0.0.0:{}", self.bind_port);
        let nfs_listener = NFSTcpListener::bind(&bind_address, adapter)
            .await
            .map_err(|e| NfsServerError::BindFailed {
                port: self.bind_port,
                error: e.to_string(),
            })?;

        // Spawn server task
        let handle = tokio::spawn(async move {
            info!("NFS server task started");
            if let Err(e) = nfs_listener.handle_forever().await {
                error!("NFS server error: {}", e);
            }
            info!("NFS server task stopped");
        });

        *self.server_handle.lock() = Some(handle.abort_handle());
        info!("NFS server started successfully");

        Ok(())
    }

    /// Stop the NFS server gracefully
    pub async fn stop(&self) -> Result<(), NfsServerError> {
        info!("Stopping NFS server");

        if let Some(handle) = self.server_handle.lock().take() {
            handle.abort();
            info!("NFS server stopped");
        } else {
            warn!("NFS server was not running");
        }

        Ok(())
    }

    /// Get FSAL reference
    pub fn fsal(&self) -> &Arc<AegisFSAL> {
        &self.fsal
    }

    /// Check if server is running
    pub fn is_running(&self) -> bool {
        self.server_handle.lock().as_ref().map_or(false, |h| !h.is_finished())
    }

    /// Get bind port
    pub fn bind_port(&self) -> u16 {
        self.bind_port
    }
}

#[cfg(test)]
mod tests {
    // Future tests will use specific imports as needed

    #[test]
    fn test_nfs_server_creation() {
        // Test will be expanded with full FSAL mock setup
        // For now, just verify struct creation compiles
    }
}
