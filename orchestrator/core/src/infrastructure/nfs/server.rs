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
//! ```ignore
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
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for server

use crate::domain::execution::ExecutionId;
use crate::domain::fsal::{AegisFSAL, AegisFileHandle, FsalAccessPolicy};
use crate::domain::volume::VolumeId;
use nfsserve::nfs::{fattr3, fileid3, filename3, ftype3, nfspath3, nfsstring, nfstime3, specdata3};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{self, NFSFileSystem};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::task::AbortHandle;
use tracing::{debug, error, info, warn};

/// Volume context for NFS export path routing
///
/// Encapsulates all execution-specific metadata needed for FSAL authorization.
#[derive(Debug, Clone)]
pub struct NfsVolumeContext {
    pub execution_id: ExecutionId,
    pub volume_id: VolumeId,
    pub container_uid: u32,
    pub container_gid: u32,
    pub policy: FsalAccessPolicy,
    /// Volume mount point in the agent container (e.g. `/workspace`).
    /// Used by FSAL tools to strip the container-absolute prefix from paths
    /// before forwarding to AegisFSAL (which operates on volume-relative paths).
    pub mount_point: PathBuf,
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
        self.forward
            .write()
            .insert(fileid, (handle.clone(), path.clone()));
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

    /// Get context for a registered volume.
    fn get_context(
        &self,
        volume_id: VolumeId,
    ) -> Result<NfsVolumeContext, nfsserve::nfs::nfsstat3> {
        self.volume_registry
            .read()
            .get(&volume_id)
            .cloned()
            .ok_or_else(|| {
                warn!("Volume {} not registered in NFS volume registry", volume_id);
                nfsserve::nfs::nfsstat3::NFS3ERR_STALE
            })
    }

    /// Decode NFS file handle to AegisFileHandle and path
    fn decode_handle(&self, id: fileid3) -> Result<(AegisFileHandle, String), NfsServerError> {
        self.handle_table
            .lookup(id)
            .ok_or(NfsServerError::InvalidHandle(id))
    }

    /// Encode AegisFileHandle to NFS file handle (fileid3)
    fn encode_handle(
        &self,
        handle: &AegisFileHandle,
        path: String,
    ) -> Result<fileid3, NfsServerError> {
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
        let context = match self.get_context(volume_id) {
            Ok(context) => context,
            Err(_) => return 0,
        };

        // Register new handle
        let handle = AegisFileHandle::new(context.execution_id, volume_id, path);
        self.handle_table.register(handle, path.to_string())
    }

    /// Convert FSAL FileAttributes to NFS fattr3
    fn convert_attrs(
        &self,
        attrs: crate::domain::storage::FileAttributes,
        volume_id: VolumeId,
    ) -> fattr3 {
        use crate::domain::storage::FileType;

        let context = match self.get_context(volume_id) {
            Ok(context) => context,
            Err(_) => {
                return fattr3 {
                    ftype: ftype3::NF3DIR,
                    mode: 0o755,
                    nlink: 2,
                    uid: 1000,
                    gid: 1000,
                    size: 4096,
                    used: 4096,
                    rdev: specdata3 {
                        specdata1: 0,
                        specdata2: 0,
                    },
                    fsid: 0,
                    fileid: 0,
                    atime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                    mtime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                    ctime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                };
            }
        };

        let ftype = match attrs.file_type {
            FileType::File => ftype3::NF3REG,
            FileType::Directory => ftype3::NF3DIR,
            FileType::Symlink => ftype3::NF3LNK,
        };

        fattr3 {
            ftype,
            mode: attrs.mode,
            nlink: attrs.nlink,
            uid: context.container_uid, // UID squashing per ADR-036
            gid: context.container_gid, // GID squashing per ADR-036
            size: attrs.size,
            used: attrs.size,
            rdev: specdata3 {
                specdata1: 0,
                specdata2: 0,
            },
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

    /// Record security-related metrics for FSAL errors.
    ///
    /// Inspects the error variant and increments the appropriate security counter:
    /// - `PathSanitization` (from `PathSanitizerError::PathTraversal`) -> `aegis_nfs_path_traversal_blocked_total`
    /// - `PolicyViolation` -> `aegis_nfs_policy_violations_total`
    /// - `UnauthorizedAccess` -> `aegis_nfs_unauthorized_access_attempts_total`
    #[allow(dead_code)] // TODO(BC-7): wire into NFS error paths
    fn record_fsal_security_metrics(e: &crate::domain::fsal::FsalError) {
        use crate::domain::fsal::FsalError;
        match e {
            FsalError::PathSanitization(ref inner) => {
                use crate::domain::path_sanitizer::PathSanitizerError;
                if matches!(inner, PathSanitizerError::PathTraversal(_)) {
                    metrics::counter!("aegis_nfs_path_traversal_blocked_total").increment(1);
                }
                metrics::counter!("aegis_nfs_policy_violations_total").increment(1);
            }
            FsalError::PolicyViolation(_) => {
                metrics::counter!("aegis_nfs_policy_violations_total").increment(1);
            }
            FsalError::UnauthorizedAccess { .. } => {
                metrics::counter!("aegis_nfs_unauthorized_access_attempts_total").increment(1);
            }
            _ => {}
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

    async fn lookup(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsserve::nfs::nfsstat3> {
        let start = std::time::Instant::now();
        let result: Result<fileid3, nfsserve::nfs::nfsstat3> = async {
            let name = std::str::from_utf8(filename)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

            debug!("NFS LOOKUP: dirid={}, filename={}", dirid, name);

            // Handle virtual root (dirid = 1)
            let (parent_handle, parent_path) = if dirid == 1 {
                (
                    AegisFileHandle::new(
                        crate::domain::execution::ExecutionId(uuid::Uuid::nil()),
                        VolumeId(uuid::Uuid::nil()),
                        "/",
                    ),
                    "/".to_string(),
                )
            } else {
                self.decode_handle(dirid).map_err(|e| {
                    warn!("Invalid parent handle: {}", e);
                    nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE
                })?
            };

            // If parent is a structural directory (identified by nil volume ID)
            if parent_handle.volume_id.0.is_nil() {
                let path_parts: Vec<&str> =
                    parent_path.split('/').filter(|s| !s.is_empty()).collect();

                // Expected path: /aegis/volumes/{tenant_id}/{volume_id}
                // Navigate down to depth 3: /aegis/volumes/{tenant_id}
                if path_parts.len() < 3 {
                    let synthetic_path = if parent_path == "/" {
                        format!("/{name}")
                    } else {
                        format!("{parent_path}/{name}")
                    };

                    let dummy_exec = crate::domain::execution::ExecutionId(uuid::Uuid::nil());
                    let dummy_vol = VolumeId(uuid::Uuid::nil());
                    let handle = AegisFileHandle::new(dummy_exec, dummy_vol, &synthetic_path);

                    let fileid = self
                        .encode_handle(&handle, synthetic_path)
                        .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_SERVERFAULT)?;
                    return Ok(fileid);
                }

                // At depth 3, the child name is {volume_id}
                if path_parts.len() == 3 {
                    let volume_id_str = name;
                    let volume_id =
                        uuid::Uuid::parse_str(volume_id_str)
                            .map(VolumeId)
                            .map_err(|_| {
                                warn!("Invalid volume ID format in lookup: {}", volume_id_str);
                                nfsserve::nfs::nfsstat3::NFS3ERR_NOENT
                            })?;

                    // Retrieve context ensuring it exists
                    let context = self.get_context(volume_id)?;

                    // Now we have a real volume! Create the proper root handle for it.
                    let child_path = "/".to_string();
                    let root_handle =
                        AegisFileHandle::new(context.execution_id, volume_id, &child_path);
                    let fileid = self
                        .encode_handle(&root_handle, child_path)
                        .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_SERVERFAULT)?;

                    return Ok(fileid);
                }
            }

            // We are inside a real volume.
            // Get context to ensure volume is valid
            let _context = self.get_context(parent_handle.volume_id)?;

            // Lookup via FSAL (for paths inside the volume)
            let child_handle = self
                .fsal
                .lookup(&parent_handle, &parent_path, name)
                .await
                .map_err(|e| {
                    warn!("FSAL lookup failed: {}", e);
                    nfsserve::nfs::nfsstat3::NFS3ERR_NOENT
                })?;

            // Build child path relative to volume root
            let child_path = if parent_path == "/" {
                format!("/{name}")
            } else {
                format!("{parent_path}/{name}")
            };

            // Encode child handle with path
            self.encode_handle(&child_handle, child_path)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_SERVERFAULT)
        }
        .await;

        match &result {
            Ok(_) => {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "lookup", "result" => "success").increment(1);
                metrics::histogram!("aegis_nfs_operation_duration_seconds", "operation" => "lookup").record(start.elapsed().as_secs_f64());
            }
            Err(_) => {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "lookup", "result" => "error").increment(1);
            }
        }
        result
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsserve::nfs::nfsstat3> {
        let start = std::time::Instant::now();
        let result: Result<fattr3, nfsserve::nfs::nfsstat3> = async {
            debug!("NFS GETATTR: id={}", id);
            // For root directory (dirid=1)
            if id == 1 {
                // Use defaults
                let default_context = self
                    .volume_registry
                    .read()
                    .values()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| NfsVolumeContext {
                        execution_id: ExecutionId::new(),
                        volume_id: VolumeId::new(),
                        container_uid: 1000,
                        container_gid: 1000,
                        policy: FsalAccessPolicy::default(),
                        mount_point: PathBuf::from("/workspace"),
                    });

                return Ok(fattr3 {
                    ftype: ftype3::NF3DIR,
                    mode: 0o755,
                    nlink: 2,
                    uid: default_context.container_uid,
                    gid: default_context.container_gid,
                    size: 4096,
                    used: 4096,
                    rdev: specdata3 {
                        specdata1: 0,
                        specdata2: 0,
                    },
                    fsid: 0,
                    fileid: 1,
                    atime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                    mtime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                    ctime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                });
            }

            // Decode handle to get path and volume
            let (handle, path) = self
                .decode_handle(id)
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

            // Handle synthetic attributes for structural synthetic directories (identified by nil volume ID)
            if handle.volume_id.0.is_nil() {
                // Return synthetic directory attributes
                return Ok(fattr3 {
                    ftype: ftype3::NF3DIR,
                    mode: 0o755,
                    nlink: 2,
                    uid: 1000,
                    gid: 1000,
                    size: 4096,
                    used: 4096,
                    rdev: specdata3 {
                        specdata1: 0,
                        specdata2: 0,
                    },
                    fsid: 0,
                    fileid: id,
                    atime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                    mtime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                    ctime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                });
            }

            // Get context for this volume
            let context = self.get_context(handle.volume_id)?;

            // For the volume root path "/", return synthetic directory attributes.
            // The volume's existence is already proven by the DB authorization in the FSAL.
            // SeaweedFS may not yet have the directory (it's created lazily on first write),
            // so avoid the stat call here — same pattern as for structural dirs above.
            if path == "/" {
                return Ok(fattr3 {
                    ftype: ftype3::NF3DIR,
                    mode: 0o755,
                    nlink: 2,
                    uid: context.container_uid,
                    gid: context.container_gid,
                    size: 4096,
                    used: 4096,
                    rdev: specdata3 {
                        specdata1: 0,
                        specdata2: 0,
                    },
                    fsid: 0,
                    fileid: id,
                    atime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                    mtime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                    ctime: nfstime3 {
                        seconds: 0,
                        nseconds: 0,
                    },
                });
            }

            // Get attributes via FSAL (with UID/GID squashing)
            let attrs = self
                .fsal
                .getattr(
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
        .await;

        match &result {
            Ok(_) => {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "getattr", "result" => "success").increment(1);
                metrics::histogram!("aegis_nfs_operation_duration_seconds", "operation" => "getattr").record(start.elapsed().as_secs_f64());
            }
            Err(_) => {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "getattr", "result" => "error").increment(1);
            }
        }
        result
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsserve::nfs::nfsstat3> {
        let start = std::time::Instant::now();
        debug!("NFS READ: id={}, offset={}, count={}", id, offset, count);

        let (handle, path) = self
            .decode_handle(id)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

        let context = self.get_context(handle.volume_id)?;

        match self
            .fsal
            .read(&handle, &path, &context.policy, offset, count as usize)
            .await
        {
            Ok(data) => {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "read", "result" => "success").increment(1);
                metrics::histogram!("aegis_nfs_operation_duration_seconds", "operation" => "read")
                    .record(start.elapsed().as_secs_f64());
                metrics::counter!("aegis_nfs_bytes_read_total").increment(data.len() as u64);
                let eof = data.len() < count as usize;
                Ok((data, eof))
            }
            Err(e) => {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "read", "result" => "error").increment(1);
                AegisFsalAdapter::record_fsal_security_metrics(&e);
                error!("FSAL read failed: {}", e);
                Err(nfsserve::nfs::nfsstat3::NFS3ERR_IO)
            }
        }
    }

    async fn write(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
    ) -> Result<fattr3, nfsserve::nfs::nfsstat3> {
        let start = std::time::Instant::now();
        let data_len = data.len();
        debug!("NFS WRITE: id={}, offset={}, len={}", id, offset, data_len);

        let (handle, path) = self
            .decode_handle(id)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

        let context = self.get_context(handle.volume_id)?;

        let _bytes_written = self
            .fsal
            .write(&handle, &path, &context.policy, offset, data)
            .await
            .map_err(|e| {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "write", "result" => "error").increment(1);
                AegisFsalAdapter::record_fsal_security_metrics(&e);
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

        metrics::counter!("aegis_nfs_operations_total", "operation" => "write", "result" => "success").increment(1);
        metrics::histogram!("aegis_nfs_operation_duration_seconds", "operation" => "write")
            .record(start.elapsed().as_secs_f64());
        metrics::counter!("aegis_nfs_bytes_written_total").increment(data_len as u64);

        // Return updated file attributes after write
        self.getattr(id).await
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<vfs::ReadDirResult, nfsserve::nfs::nfsstat3> {
        let start = std::time::Instant::now();
        debug!(
            "NFS READDIR: dirid={}, start_after={}, max={}",
            dirid, start_after, max_entries
        );
        // Decode directory handle to get path and volume
        let (dir_handle, dir_path) = self
            .decode_handle(dirid)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

        // Get context for this volume
        let context = self.get_context(dir_handle.volume_id)?;

        // Read directory via FSAL
        let entries = self
            .fsal
            .readdir(
                context.execution_id,
                context.volume_id,
                &dir_path,
                &context.policy,
            )
            .await
            .map_err(|e| {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "readdir", "result" => "error").increment(1);
                AegisFsalAdapter::record_fsal_security_metrics(&e);
                error!("FSAL readdir failed: {}", e);
                nfsserve::nfs::nfsstat3::NFS3ERR_IO
            })?;

        metrics::counter!("aegis_nfs_operations_total", "operation" => "readdir", "result" => "success").increment(1);
        metrics::histogram!("aegis_nfs_operation_duration_seconds", "operation" => "readdir")
            .record(start.elapsed().as_secs_f64());

        // Convert to NFS format
        let mut nfs_entries = Vec::new();
        for (_idx, entry) in entries
            .iter()
            .enumerate()
            .skip(start_after as usize)
            .take(max_entries)
        {
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
            let attr = self
                .getattr(fileid)
                .await
                .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_IO)?;

            nfs_entries.push(vfs::DirEntry { fileid, name, attr });
        }

        Ok(vfs::ReadDirResult {
            entries: nfs_entries,
            end: entries.len() <= start_after as usize + max_entries,
        })
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: nfsserve::nfs::sattr3,
    ) -> Result<(fileid3, fattr3), nfsserve::nfs::nfsstat3> {
        let start = std::time::Instant::now();
        debug!("NFS CREATE: dirid={}, filename={:?}", dirid, filename);
        // Decode parent directory handle
        let (parent_handle, parent_path) = self
            .decode_handle(dirid)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

        // Get context for this volume
        let context = self.get_context(parent_handle.volume_id)?;

        // Get filename
        let name =
            std::str::from_utf8(filename).map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

        // Build file path
        let file_path = if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };

        // Create file via FSAL
        let handle = self
            .fsal
            .create_file(
                context.execution_id,
                context.volume_id,
                &file_path,
                &context.policy,
            )
            .await
            .map_err(|e| {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "create", "result" => "error").increment(1);
                AegisFsalAdapter::record_fsal_security_metrics(&e);
                error!("FSAL create failed: {}", e);
                nfsserve::nfs::nfsstat3::NFS3ERR_IO
            })?;

        metrics::counter!("aegis_nfs_operations_total", "operation" => "create", "result" => "success").increment(1);
        metrics::histogram!("aegis_nfs_operation_duration_seconds", "operation" => "create")
            .record(start.elapsed().as_secs_f64());

        // Register and return fileid with attributes
        let fileid = self
            .encode_handle(&handle, file_path.clone())
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_SERVERFAULT)?;
        let attrs = self.getattr(fileid).await?;
        Ok((fileid, attrs))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsserve::nfs::nfsstat3> {
        debug!(
            "NFS CREATE_EXCLUSIVE: dirid={}, filename={:?}",
            dirid, filename
        );
        // Use same implementation as create, returning only fileid
        let (fileid, _attrs) = self
            .create(dirid, filename, nfsserve::nfs::sattr3::default())
            .await?;
        Ok(fileid)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsserve::nfs::nfsstat3> {
        let start = std::time::Instant::now();
        debug!("NFS MKDIR: dirid={}, dirname={:?}", dirid, dirname);
        // Decode parent directory handle
        let (parent_handle, parent_path) = self
            .decode_handle(dirid)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

        // Get context for this volume
        let context = self.get_context(parent_handle.volume_id)?;

        // Get directory name
        let name =
            std::str::from_utf8(dirname).map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

        // Build directory path
        let dir_path = if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };

        // Create directory via FSAL
        self.fsal
            .create_directory(
                context.execution_id,
                context.volume_id,
                &dir_path,
                &context.policy,
            )
            .await
            .map_err(|e| {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "create", "result" => "error").increment(1);
                AegisFsalAdapter::record_fsal_security_metrics(&e);
                error!("FSAL mkdir failed: {}", e);
                nfsserve::nfs::nfsstat3::NFS3ERR_IO
            })?;

        metrics::counter!("aegis_nfs_operations_total", "operation" => "create", "result" => "success").increment(1);
        metrics::histogram!("aegis_nfs_operation_duration_seconds", "operation" => "create")
            .record(start.elapsed().as_secs_f64());

        // Create a handle for the new directory
        let handle = crate::domain::fsal::AegisFileHandle::new(
            context.execution_id,
            context.volume_id,
            &dir_path,
        );

        // Register and return fileid with attributes
        let fileid = self
            .encode_handle(&handle, dir_path.clone())
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_SERVERFAULT)?;
        let attrs = self.getattr(fileid).await?;
        Ok((fileid, attrs))
    }

    async fn remove(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<(), nfsserve::nfs::nfsstat3> {
        let start = std::time::Instant::now();
        debug!("NFS REMOVE: dirid={}, filename={:?}", dirid, filename);
        // Decode parent directory handle
        let (parent_handle, parent_path) = self
            .decode_handle(dirid)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

        // Get context for this volume
        let context = self.get_context(parent_handle.volume_id)?;

        // Get filename
        let name =
            std::str::from_utf8(filename).map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

        // Build file path
        let file_path = if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };

        // Try to delete as file first, then as directory if file deletion fails
        let result = match self
            .fsal
            .delete_file(
                context.execution_id,
                context.volume_id,
                &file_path,
                &context.policy,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(file_err) => {
                // If file deletion fails, try directory deletion
                self.fsal
                    .delete_directory(
                        context.execution_id,
                        context.volume_id,
                        &file_path,
                        &context.policy,
                    )
                    .await
                    .map_err(|e| {
                        AegisFsalAdapter::record_fsal_security_metrics(&file_err);
                        AegisFsalAdapter::record_fsal_security_metrics(&e);
                        error!("FSAL remove failed: {}", e);
                        nfsserve::nfs::nfsstat3::NFS3ERR_IO
                    })
            }
        };

        match &result {
            Ok(()) => {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "delete", "result" => "success").increment(1);
                metrics::histogram!("aegis_nfs_operation_duration_seconds", "operation" => "delete").record(start.elapsed().as_secs_f64());
            }
            Err(_) => {
                metrics::counter!("aegis_nfs_operations_total", "operation" => "delete", "result" => "error").increment(1);
            }
        }
        result
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsserve::nfs::nfsstat3> {
        debug!(
            "NFS RENAME: from_dirid={}, from_filename={:?}, to_dirid={}, to_filename={:?}",
            from_dirid, from_filename, to_dirid, to_filename
        );
        // Decode parent directories
        let (from_parent, from_parent_path) = self
            .decode_handle(from_dirid)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;
        let (_to_parent, to_parent_path) = self
            .decode_handle(to_dirid)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_BADHANDLE)?;

        // Get context for this volume
        let context = self.get_context(from_parent.volume_id)?;

        // Get filenames
        let from_name = std::str::from_utf8(from_filename)
            .map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;
        let to_name =
            std::str::from_utf8(to_filename).map_err(|_| nfsserve::nfs::nfsstat3::NFS3ERR_INVAL)?;

        // Build full paths
        let from_path = if from_parent_path == "/" {
            format!("/{from_name}")
        } else {
            format!("{from_parent_path}/{from_name}")
        };
        let to_path = if to_parent_path == "/" {
            format!("/{to_name}")
        } else {
            format!("{to_parent_path}/{to_name}")
        };

        // Rename via FSAL
        self.fsal
            .rename(
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

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink_data: &nfspath3,
        _attr: &nfsserve::nfs::sattr3,
    ) -> Result<(fileid3, fattr3), nfsserve::nfs::nfsstat3> {
        debug!(
            "NFS SYMLINK: dirid={}, linkname={:?}, target={:?}",
            dirid, linkname, symlink_data
        );

        // Symlinks are explicitly unsupported per ADR-036: agent workspaces
        // are flat file trees; symlinks would enable escape from the sandbox.
        Err(nfsserve::nfs::nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsserve::nfs::nfsstat3> {
        debug!("NFS READLINK: id={}", id);

        // Symlink reading unsupported per ADR-036 — symlinks are not created.
        Err(nfsserve::nfs::nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn setattr(
        &self,
        id: fileid3,
        _setattr: nfsserve::nfs::sattr3,
    ) -> Result<fattr3, nfsserve::nfs::nfsstat3> {
        debug!("NFS SETATTR: id={}", id);

        // setattr is intentionally a no-op: UID/GID squashing (ADR-036) means
        // the orchestrator controls file ownership, not the agent container.
        // Return current attributes unchanged.
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
        let adapter = AegisFsalAdapter::new(self.fsal.clone(), self.volume_registry.clone());

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
        self.server_handle
            .lock()
            .as_ref()
            .is_some_and(|h| !h.is_finished())
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
        let module_marker = "nfs_server_creation";
        assert_eq!(module_marker, "nfs_server_creation");
    }
}
