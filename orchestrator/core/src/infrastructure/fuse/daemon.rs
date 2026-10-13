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
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

/// TTL for FUSE attribute/entry caches.
/// Short TTL ensures agents see fresh data from the storage backend.
const FUSE_TTL: Duration = Duration::from_secs(1);

/// Threshold past which a single FUSE mount with in-flight ops that haven't
/// completed is considered degraded and starts returning EIO immediately.
/// Belt-and-suspenders against backend wedges that pin kernel callback threads.
const STUCK_OP_THRESHOLD_MS: u64 = 30_000;

/// Cadence at which the per-mount supervisor task checks for stuck ops.
const SUPERVISOR_TICK_SECS: u64 = 5;

/// Why a mount was marked degraded. Surfaced by the daemon's `Health` RPC.
#[derive(Debug, Clone)]
pub struct DegradedReason {
    pub reason: String,
    pub since: SystemTime,
}

/// Daemon-wide registry of currently-degraded mounts. Keyed by
/// `"{execution_id}/{volume_id}"`, mirroring the gRPC `MountHandleMap` keys
/// used by the host-side daemon. The supervisor task per mount inserts an
/// entry on transition; entries are removed on unmount.
pub type DegradedMountRegistry = Arc<RwLock<HashMap<String, DegradedReason>>>;

/// Build a fresh, empty degraded-mount registry.
pub fn new_degraded_mount_registry() -> DegradedMountRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Returns the canonical mount key used by both the gRPC `MountHandleMap` and
/// the [`DegradedMountRegistry`].
pub fn mount_key(execution_id: &str, volume_id: &str) -> String {
    format!("{execution_id}/{volume_id}")
}

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
    /// The identity the container's process has inside its own namespace,
    /// as the mount request names it. The mount reports it on the host as
    /// [`MountOwner::for_container`] translates it.
    pub container_uid: u32,
    pub container_gid: u32,
    pub policy: FsalAccessPolicy,
}

/// The owner a FUSE mount reports on every inode (ADR-107 D8, "UID/GID
/// squashing"): the identity the container's process has on the host.
///
/// The kernel reads a FUSE daemon's attributes in the daemon's own user
/// namespace, and with `default_permissions` it checks every access against
/// them, so the owner must be the host identity of the process that uses the
/// mount. A mount request names that process's identity inside its container.
/// A container's root is the user that runs the container engine: under
/// rootless Podman, the unprivileged user that runs both Podman and this
/// daemon (ADR-107 D11; `aegis` in production); under a rootful engine, root,
/// as this daemon then is (and root passes the check in any case). In both it
/// is this daemon's own identity, so root is reported as that. Any other
/// identity is reported as the request names it: under a rootful engine that
/// is its host identity; under rootless Podman it maps to a subordinate id the
/// daemon cannot see, as before.
///
/// The storage layer's policy (the request's read and write paths) stays the
/// authority on what may be written: every write is still proxied to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountOwner {
    pub uid: u32,
    pub gid: u32,
}

impl MountOwner {
    /// The owner reported for a container process `container_uid:container_gid`
    /// by a daemon running as `daemon_uid:daemon_gid`.
    pub fn for_container(
        container_uid: u32,
        container_gid: u32,
        daemon_uid: u32,
        daemon_gid: u32,
    ) -> Self {
        Self {
            uid: if container_uid == 0 {
                daemon_uid
            } else {
                container_uid
            },
            gid: if container_gid == 0 {
                daemon_gid
            } else {
                container_gid
            },
        }
    }

    /// The owner reported for `context` by this process.
    fn for_context(context: &FuseVolumeContext) -> Self {
        // SAFETY: geteuid and getegid have no preconditions and cannot fail.
        let (daemon_uid, daemon_gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self::for_container(
            context.container_uid,
            context.container_gid,
            daemon_uid,
            daemon_gid,
        )
    }
}

/// Wrapper to make `fuser::BackgroundSession` `Send + Sync`.
///
/// # Safety
///
/// `fuser::BackgroundSession` is not `Send + Sync` upstream because it
/// stores a raw `*mut c_void` pointer to the libfuse session handle. The
/// audit (002 finding 4.37.17) flagged this as a manually-asserted invariant
/// rather than a compile-checked one; this comment block documents WHY the
/// assertion is sound so a future maintainer cannot silently invalidate it.
///
/// **Why the invariant holds:**
///
/// 1. **Exclusive ownership.** The wrapped `BackgroundSession` is owned by a
///    single [`FuseMountHandle`] and is never cloned, never aliased, never
///    handed to another task. The handle moves through:
///    create → store in `MountHandleMap` (behind an `Arc<Mutex<…>>`) → drop.
/// 2. **No concurrent access to the inner pointer.** The only operation
///    performed on the session after creation is `Drop`, which calls into
///    libfuse's `fuse_session_destroy` / unmount path. Tokio cannot spuriously
///    move a `BackgroundSession` between threads while a method is in flight
///    because there are no methods called on it after construction —
///    everything else operates on `mountpoint` / `mount_key` / metadata.
/// 3. **libfuse mount/unmount thread-safety.** Per `libfuse` documentation,
///    `fuse_session_destroy` is safe to call from a thread other than the
///    one that created the session, provided no other thread is concurrently
///    using the session — which point (1) guarantees.
///
/// **Why we do not wrap in `Mutex<…>` instead.** A `Mutex` would make the
/// type `Send + Sync` via the type system but adds a runtime lock around a
/// drop-only resource. The lock would never be contended (there is no
/// concurrent reader), so the cost is pure ceremony for a value whose only
/// use is RAII unmount. The `unsafe impl` is the more honest expression of
/// the actual invariant.
///
/// **Invariants a future maintainer MUST preserve:**
///
/// - Do not add methods that mutate the inner `BackgroundSession` from
///   shared references.
/// - Do not clone or duplicate `SendSyncSession`.
/// - Do not expose `&BackgroundSession` across an `await` point in a
///   multi-threaded runtime without re-validating thread-safety.
#[expect(dead_code, reason = "held for RAII drop — unmounts on drop")]
struct SendSyncSession(fuser::BackgroundSession);

// SAFETY: see `SendSyncSession` doc comment above. The inner
// `BackgroundSession` is exclusively owned, never aliased, never accessed
// concurrently, and only touched by `Drop`. libfuse `fuse_session_destroy`
// is documented as thread-safe under exclusive-ownership conditions.
unsafe impl Send for SendSyncSession {}
// SAFETY: same justification as `Send` above. There are no shared-reference
// methods on `SendSyncSession`, so `&Self` carries no concurrent-access
// hazard — `Sync` is vacuous in practice for this type.
unsafe impl Sync for SendSyncSession {}

/// Handle to an active FUSE mount. Dropping this handle unmounts the filesystem.
pub struct FuseMountHandle {
    /// The `fuser::BackgroundSession` keeps the FUSE filesystem mounted.
    /// Dropping it triggers unmount.
    _session: SendSyncSession,
    mountpoint: String,
    /// Mount-key (`"{execution_id}/{volume_id}"`) for cross-referencing the
    /// daemon-wide degraded registry from the gRPC `Health` handler.
    mount_key: String,
    /// Wall-clock instant the mount was created. Surfaced by the
    /// `Health` / heartbeat RPC as `oldest_mount_age_secs`.
    created_at: Instant,
    /// Shutdown signal for the per-mount supervisor task. Triggered on Drop
    /// so the supervisor doesn't keep ticking against a freed mount.
    supervisor_shutdown: Option<tokio::sync::watch::Sender<bool>>,
    /// Daemon-wide degraded registry — entry is removed on Drop so a fresh
    /// re-mount of the same key starts clean.
    degraded_registry: DegradedMountRegistry,
}

impl FuseMountHandle {
    /// Returns the host path where the FUSE filesystem is mounted.
    pub fn mountpoint(&self) -> &str {
        &self.mountpoint
    }

    /// Returns the canonical mount key (`"{execution_id}/{volume_id}"`).
    pub fn mount_key(&self) -> &str {
        &self.mount_key
    }

    /// Returns the [`Instant`] this mount was created. Used by the daemon's
    /// `Health` RPC and periodic heartbeat to compute `oldest_mount_age_secs`.
    pub fn created_at(&self) -> Instant {
        self.created_at
    }
}

impl Drop for FuseMountHandle {
    fn drop(&mut self) {
        debug!(mountpoint = %self.mountpoint, "FUSE mount handle dropped — flushing and unmounting");
        // Signal the per-mount supervisor task to shut down so it doesn't
        // keep ticking against a freed FuseFsal.
        if let Some(tx) = self.supervisor_shutdown.take() {
            let _ = tx.send(true);
        }
        // Drop any degraded-registry entry so a fresh re-mount starts clean.
        let registry = self.degraded_registry.clone();
        let key = self.mount_key.clone();
        // Best-effort fire-and-forget; we're in Drop so we can't .await.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                registry.write().await.remove(&key);
            });
        }
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
    /// Daemon-wide registry of degraded mounts. Shared with each `FuseFsal`'s
    /// per-mount supervisor task and exposed to the gRPC `Health` handler.
    degraded_registry: DegradedMountRegistry,
}

impl FuseFsalDaemon {
    /// Create a new FUSE daemon backed by the given FSAL instance (in-process).
    pub fn new(fsal: Arc<AegisFSAL>) -> Self {
        Self {
            backend: Arc::new(DirectFsalBackend(fsal)),
            degraded_registry: new_degraded_mount_registry(),
        }
    }

    /// Create a new FUSE daemon backed by a `FsalBackend` trait object.
    ///
    /// Use this constructor for the host-side daemon with a `GrpcFsalBackend`,
    /// or any other backend implementation.
    pub fn with_backend(backend: Arc<dyn FsalBackend>) -> Self {
        Self {
            backend,
            degraded_registry: new_degraded_mount_registry(),
        }
    }

    /// Returns the daemon-wide degraded-mount registry. Used by the gRPC
    /// `Health` handler to report `degraded_mount_count` and by tests.
    pub fn degraded_registry(&self) -> DegradedMountRegistry {
        self.degraded_registry.clone()
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

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last_completed_op_at = Arc::new(AtomicU64::new(now_ms));
        let in_flight_ops = Arc::new(AtomicU32::new(0));
        let degraded = Arc::new(AtomicBool::new(false));
        let key = mount_key(
            &context.execution_id.0.to_string(),
            &context.volume_id.0.to_string(),
        );

        let fs = FuseFsal {
            backend: self.backend.clone(),
            context: context.clone(),
            inode_table: Arc::new(InodeTable::new()),
            runtime: tokio::runtime::Handle::current(),
            last_completed_op_at: last_completed_op_at.clone(),
            in_flight_ops: in_flight_ops.clone(),
            degraded: degraded.clone(),
        };

        // Spawn per-mount supervisor task. Detects stuck ops and marks the
        // mount degraded so subsequent FUSE callbacks return EIO immediately
        // — preventing the kernel-callback pile-up that pinned the orphan PID
        // in the incident this commit fixes.
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let supervisor_key = key.clone();
        let supervisor_registry = self.degraded_registry.clone();
        let supervisor_last = last_completed_op_at.clone();
        let supervisor_in_flight = in_flight_ops.clone();
        let supervisor_degraded = degraded.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(SUPERVISOR_TICK_SECS));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if supervisor_degraded.load(Ordering::SeqCst) {
                            continue;
                        }
                        let in_flight = supervisor_in_flight.load(Ordering::SeqCst);
                        if in_flight == 0 {
                            continue;
                        }
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let last = supervisor_last.load(Ordering::SeqCst);
                        let age_ms = now.saturating_sub(last);
                        if age_ms > STUCK_OP_THRESHOLD_MS {
                            supervisor_degraded.store(true, Ordering::SeqCst);
                            supervisor_registry.write().await.insert(
                                supervisor_key.clone(),
                                DegradedReason {
                                    reason: format!(
                                        "stuck FUSE op: in_flight={in_flight} age_ms={age_ms}"
                                    ),
                                    since: SystemTime::now(),
                                },
                            );
                            warn!(
                                target: "fuse_health",
                                mount_key = %supervisor_key,
                                in_flight = in_flight,
                                age_ms = age_ms,
                                "FUSE mount degraded; subsequent ops will return EIO"
                            );
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        // Mount options:
        // - allow_other: let the container user access the mount (requires /etc/fuse.conf)
        // - default_permissions: let the kernel enforce basic permission checks,
        //   against the owner `MountOwner` reports (the container's process on
        //   the host); writes are then still refused by the FSAL's policy
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
            mount_key: key,
            created_at: Instant::now(),
            supervisor_shutdown: Some(shutdown_tx),
            degraded_registry: self.degraded_registry.clone(),
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
    /// Unix-millis timestamp of the most recently completed FUSE callback.
    /// The per-mount supervisor task uses this to detect stuck callbacks.
    last_completed_op_at: Arc<AtomicU64>,
    /// Number of FUSE callbacks currently executing (incremented at entry,
    /// decremented at exit via [`OpGuard`]).
    in_flight_ops: Arc<AtomicU32>,
    /// Once `true`, every callback short-circuits to EIO. Set by the supervisor
    /// task when ops have been stuck longer than [`STUCK_OP_THRESHOLD_MS`].
    degraded: Arc<AtomicBool>,
}

/// RAII guard that increments `in_flight_ops` on construction and updates
/// `last_completed_op_at` + decrements `in_flight_ops` on Drop. Used to wrap
/// every FUSE callback so the per-mount supervisor task always sees accurate
/// counters even on early returns.
struct OpGuard {
    in_flight_ops: Arc<AtomicU32>,
    last_completed_op_at: Arc<AtomicU64>,
}

impl OpGuard {
    fn enter(fs: &FuseFsal) -> Self {
        fs.in_flight_ops.fetch_add(1, Ordering::SeqCst);
        Self {
            in_flight_ops: fs.in_flight_ops.clone(),
            last_completed_op_at: fs.last_completed_op_at.clone(),
        }
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_completed_op_at.store(now_ms, Ordering::SeqCst);
        self.in_flight_ops.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FuseFsal {
    /// Returns `true` if the mount is degraded — callbacks should short-circuit
    /// to EIO without enqueuing additional work behind a wedged backend.
    fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }
}

impl FuseFsal {
    /// Get or create the root file handle for this volume.
    fn root_handle(&self) -> AegisFileHandle {
        self.handle_for("/")
    }

    /// The FSAL handle this mount allocates for `path`: it names the
    /// execution the mount serves, as the handle `AegisFSAL::create_file`
    /// returns does.
    ///
    /// The FSAL authorises a handle by the one execution context it carries.
    /// An execution-named handle is authorised on a volume its execution owns
    /// and, through the registered volume context, on a workflow's workspace
    /// the execution was registered for; a workflow-named one only on a volume
    /// the workflow owns. A workflow's Agent state mounts its own volume, owned
    /// by its execution, with the workflow's id in the context, so a
    /// workflow-named handle was refused and every LOOKUP of a file the mount
    /// did not create answered ENOENT (`clerk-fs-write-not-visible-to-cmd-run`).
    /// The workflow's id still travels with every call that takes one.
    fn handle_for(&self, path: &str) -> AegisFileHandle {
        AegisFileHandle::new(self.context.execution_id, self.context.volume_id, path)
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

    /// The owner this mount reports on every inode.
    fn owner(&self) -> MountOwner {
        MountOwner::for_context(&self.context)
    }

    /// Convert FSAL `FileAttributes` to `fuser::FileAttr`, applying UID/GID squashing.
    fn convert_attrs(&self, attrs: &crate::domain::storage::FileAttributes, ino: u64) -> FileAttr {
        let kind = match attrs.file_type {
            FileType::File => FuseFileType::RegularFile,
            FileType::Directory => FuseFileType::Directory,
            FileType::Symlink => FuseFileType::Symlink,
        };
        let owner = self.owner();

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
            uid: owner.uid, // UID squashing per ADR-036
            gid: owner.gid, // GID squashing per ADR-036
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Synthesize directory attributes for the volume root.
    fn root_attr(&self) -> FileAttr {
        let owner = self.owner();
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
            uid: owner.uid,
            gid: owner.gid,
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

    /// LOOKUP of `name` under the inode `parent`: the child's inode is
    /// allocated with the handle the FSAL returns, and its attributes read.
    fn lookup_entry(&self, parent: u64, name: &str) -> Result<FileAttr, Errno> {
        debug!(parent = parent, name = %name, "FUSE LOOKUP");

        let (parent_handle, parent_path) = self.resolve_inode(parent).ok_or(Errno::ENOENT)?;

        // Perform FSAL lookup to get a child handle
        let child_handle = self
            .block_on(self.backend.lookup(&parent_handle, &parent_path, name))
            .map_err(|e| {
                debug!(error = %e, "FUSE lookup failed");
                Errno::ENOENT
            })?;

        let child_path = Self::child_path(&parent_path, name);
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
                metrics::counter!("aegis_fuse_operations_total", "operation" => "lookup", "result" => "success").increment(1);
                Ok(self.convert_attrs(&attrs, child_ino))
            }
            Err(e) => {
                // Path exists (lookup succeeded) but stat failed — may be a race
                // or the storage backend is lazily creating entries. Return ENOENT
                // rather than EIO to allow retries.
                debug!(error = %e, path = %child_path, "FUSE lookup getattr failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "lookup", "result" => "error").increment(1);
                Err(Errno::ENOENT)
            }
        }
    }

    /// MKDIR of `name` under the inode `parent`: the directory's inode is
    /// allocated with this mount's handle for its path.
    fn make_directory(&self, parent: u64, name: &str) -> Result<FileAttr, Errno> {
        debug!(parent = parent, name = %name, "FUSE MKDIR");

        let (_parent_handle, parent_path) = self.resolve_inode(parent).ok_or(Errno::ENOENT)?;

        let dir_path = Self::child_path(&parent_path, name);

        match self.block_on(self.backend.create_directory(
            self.context.execution_id,
            self.context.volume_id,
            &dir_path,
            &self.context.policy,
            self.context.workflow_execution_id,
        )) {
            Ok(()) => {
                let handle = self.handle_for(&dir_path);
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
                    uid: self.owner().uid,
                    gid: self.owner().gid,
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                };
                metrics::counter!("aegis_fuse_operations_total", "operation" => "mkdir", "result" => "success").increment(1);
                Ok(attr)
            }
            Err(e) => {
                error!(error = %e, "FUSE mkdir failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "mkdir", "result" => "error").increment(1);
                Err(fsal_error_to_errno(&e))
            }
        }
    }

    /// READ of `size` bytes at `offset` from the inode `ino`, by the handle
    /// the inode was allocated with.
    fn read_data(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        debug!(ino = ino, offset = offset, size = size, "FUSE READ");

        let (handle, path) = self.resolve_inode(ino).ok_or(Errno::ENOENT)?;

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
                Ok(data)
            }
            Err(e) => {
                error!(error = %e, ino = ino, "FUSE read failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "read", "result" => "error").increment(1);
                Err(fsal_error_to_errno(&e))
            }
        }
    }

    /// READDIR of the inode `ino`: ".", "..", then each entry with the inode
    /// allocated for it.
    fn list_directory(&self, ino: u64) -> Result<Vec<(u64, FuseFileType, String)>, Errno> {
        debug!(ino = ino, "FUSE READDIR");

        let (_handle, dir_path) = self.resolve_inode(ino).ok_or(Errno::ENOENT)?;

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
                return Err(fsal_error_to_errno(&e));
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
            let child_handle = self.handle_for(&entry_path);
            let child_ino = self.inode_table.allocate(child_handle, entry_path);
            let kind = match entry.file_type {
                FileType::File => FuseFileType::RegularFile,
                FileType::Directory => FuseFileType::Directory,
                FileType::Symlink => FuseFileType::Symlink,
            };
            full_entries.push((child_ino, kind, entry.name.clone()));
        }

        Ok(full_entries)
    }

    /// GETATTR of the inode `ino`: the volume root's synthetic attributes, or
    /// the attributes of the path the inode names.
    fn attributes(&self, ino: u64) -> Result<FileAttr, Errno> {
        debug!(ino = ino, "FUSE GETATTR");

        if ino == ROOT_INODE {
            // For the volume root, return synthetic attributes (same as NFS server pattern).
            // SeaweedFS may not have the directory yet (created lazily on first write).
            return Ok(self.root_attr());
        }

        let (_handle, path) = self.resolve_inode(ino).ok_or(Errno::ENOENT)?;

        match self.block_on(self.backend.getattr(
            self.context.execution_id,
            self.context.volume_id,
            &path,
            self.context.container_uid,
            self.context.container_gid,
            self.context.workflow_execution_id,
        )) {
            Ok(attrs) => {
                metrics::counter!("aegis_fuse_operations_total", "operation" => "getattr", "result" => "success").increment(1);
                Ok(self.convert_attrs(&attrs, ino))
            }
            Err(e) => {
                warn!(error = %e, ino = ino, path = %path, "FUSE getattr failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "getattr", "result" => "error").increment(1);
                Err(Errno::ENOENT)
            }
        }
    }

    /// WRITE of `data` at `offset` into the inode `ino`, by the handle the
    /// inode was allocated with.
    fn write_data(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        debug!(ino = ino, offset = offset, len = data.len(), "FUSE WRITE");

        let (handle, path) = self.resolve_inode(ino).ok_or(Errno::ENOENT)?;

        match self.block_on(
            self.backend
                .write(&handle, &path, &self.context.policy, offset, data),
        ) {
            Ok(bytes_written) => {
                metrics::counter!("aegis_fuse_operations_total", "operation" => "write", "result" => "success").increment(1);
                metrics::counter!("aegis_fuse_bytes_written_total").increment(bytes_written as u64);
                Ok(bytes_written as u32)
            }
            Err(e) => {
                error!(error = %e, ino = ino, "FUSE write failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "write", "result" => "error").increment(1);
                Err(fsal_error_to_errno(&e))
            }
        }
    }

    /// SETATTR of the inode `ino`: a `size` sets the file's length through
    /// the FSAL's truncate (an `O_TRUNC` open and `truncate(2)` arrive here
    /// as a size), then the attributes are read back. Mode, owner and times
    /// are not stored: UID/GID squashing makes chmod and chown no-ops.
    fn set_attributes(&self, ino: u64, size: Option<u64>) -> Result<FileAttr, Errno> {
        debug!(ino = ino, size = ?size, "FUSE SETATTR");

        if ino == ROOT_INODE {
            return if size.is_some() {
                Err(Errno::EISDIR)
            } else {
                Ok(self.root_attr())
            };
        }

        let (handle, path) = self.resolve_inode(ino).ok_or(Errno::ENOENT)?;

        if let Some(size) = size {
            if let Err(e) =
                self.block_on(
                    self.backend
                        .truncate(&handle, &path, &self.context.policy, size),
                )
            {
                error!(error = %e, ino = ino, size = size, "FUSE truncate failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "truncate", "result" => "error").increment(1);
                return Err(fsal_error_to_errno(&e));
            }
            metrics::counter!("aegis_fuse_operations_total", "operation" => "truncate", "result" => "success").increment(1);
        }

        match self.block_on(self.backend.getattr(
            self.context.execution_id,
            self.context.volume_id,
            &path,
            self.context.container_uid,
            self.context.container_gid,
            self.context.workflow_execution_id,
        )) {
            Ok(attrs) => Ok(self.convert_attrs(&attrs, ino)),
            Err(_) => Err(Errno::ENOENT),
        }
    }

    /// CREATE of the file `name` under the inode `parent`: the file's inode
    /// is allocated with the handle the FSAL returns, and its attributes read.
    fn create_entry(&self, parent: u64, name: &str) -> Result<FileAttr, Errno> {
        debug!(parent = parent, name = %name, "FUSE CREATE");

        let (_parent_handle, parent_path) = self.resolve_inode(parent).ok_or(Errno::ENOENT)?;

        let file_path = Self::child_path(&parent_path, name);

        let handle = match self.block_on(self.backend.create_file(
            self.context.execution_id,
            self.context.volume_id,
            &file_path,
            &self.context.policy,
            self.context.workflow_execution_id,
        )) {
            Ok(handle) => handle,
            Err(e) => {
                error!(error = %e, "FUSE create failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "create", "result" => "error").increment(1);
                return Err(fsal_error_to_errno(&e));
            }
        };

        let child_ino = self.inode_table.allocate(handle, file_path.clone());

        // Get attrs for the newly created file
        let attr = match self.block_on(self.backend.getattr(
            self.context.execution_id,
            self.context.volume_id,
            &file_path,
            self.context.container_uid,
            self.context.container_gid,
            self.context.workflow_execution_id,
        )) {
            Ok(attrs) => self.convert_attrs(&attrs, child_ino),
            Err(e) => {
                // File was created but stat failed — unlikely but handle gracefully.
                // Return a synthetic file attr rather than failing.
                warn!(error = %e, path = %file_path, "FUSE create getattr failed — using synthetic attrs");
                FileAttr {
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
                    uid: self.owner().uid,
                    gid: self.owner().gid,
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                }
            }
        };
        metrics::counter!("aegis_fuse_operations_total", "operation" => "create", "result" => "success").increment(1);
        Ok(attr)
    }

    /// UNLINK of the file `name` under the inode `parent`.
    fn remove_file(&self, parent: u64, name: &str) -> Result<(), Errno> {
        debug!(parent = parent, name = %name, "FUSE UNLINK");

        let (_parent_handle, parent_path) = self.resolve_inode(parent).ok_or(Errno::ENOENT)?;

        let file_path = Self::child_path(&parent_path, name);

        match self.block_on(self.backend.delete_file(
            self.context.execution_id,
            self.context.volume_id,
            &file_path,
            &self.context.policy,
            self.context.workflow_execution_id,
        )) {
            Ok(()) => {
                self.inode_table.forget(&file_path);
                metrics::counter!("aegis_fuse_operations_total", "operation" => "unlink", "result" => "success").increment(1);
                Ok(())
            }
            Err(e) => {
                error!(error = %e, "FUSE unlink failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "unlink", "result" => "error").increment(1);
                Err(fsal_error_to_errno(&e))
            }
        }
    }

    /// RMDIR of the directory `name` under the inode `parent`.
    fn remove_directory(&self, parent: u64, name: &str) -> Result<(), Errno> {
        debug!(parent = parent, name = %name, "FUSE RMDIR");

        let (_parent_handle, parent_path) = self.resolve_inode(parent).ok_or(Errno::ENOENT)?;

        let dir_path = Self::child_path(&parent_path, name);

        // The storage deletes a directory with everything in it; RMDIR
        // removes only an empty one. git counts on ENOTEMPTY: it removes a
        // file, then tries `rmdir` of each directory above it.
        match self.block_on(self.backend.readdir(
            self.context.execution_id,
            self.context.volume_id,
            &dir_path,
            &self.context.policy,
            self.context.workflow_execution_id,
        )) {
            Ok(entries) if !entries.is_empty() => {
                debug!(path = %dir_path, entries = entries.len(), "FUSE RMDIR of a directory that is not empty");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rmdir", "result" => "error").increment(1);
                return Err(Errno::ENOTEMPTY);
            }
            Ok(_) => {}
            Err(e) => {
                error!(error = %e, "FUSE rmdir failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rmdir", "result" => "error").increment(1);
                return Err(fsal_error_to_errno(&e));
            }
        }

        match self.block_on(self.backend.delete_directory(
            self.context.execution_id,
            self.context.volume_id,
            &dir_path,
            &self.context.policy,
            self.context.workflow_execution_id,
        )) {
            Ok(()) => {
                self.inode_table.forget(&dir_path);
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rmdir", "result" => "success").increment(1);
                Ok(())
            }
            Err(e) => {
                error!(error = %e, "FUSE rmdir failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rmdir", "result" => "error").increment(1);
                Err(fsal_error_to_errno(&e))
            }
        }
    }

    /// RENAME of `name` under the inode `parent` to `newname` under the
    /// inode `newparent`.
    fn rename_entry(
        &self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
    ) -> Result<(), Errno> {
        debug!(
            parent = parent,
            name = %name,
            newparent = newparent,
            newname = %newname,
            "FUSE RENAME"
        );

        let (_from_handle, from_parent_path) = self.resolve_inode(parent).ok_or(Errno::ENOENT)?;
        let (_to_handle, to_parent_path) = self.resolve_inode(newparent).ok_or(Errno::ENOENT)?;

        let from_path = Self::child_path(&from_parent_path, name);
        let to_path = Self::child_path(&to_parent_path, newname);

        match self.block_on(self.backend.rename(
            self.context.execution_id,
            self.context.volume_id,
            &from_path,
            &to_path,
            &self.context.policy,
            self.context.workflow_execution_id,
        )) {
            Ok(()) => {
                // The kernel keeps the moved entry's inode under its new
                // name: the inode names the new path from now on.
                self.inode_table
                    .rename(&from_path, &to_path, |path| self.handle_for(path));
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rename", "result" => "success").increment(1);
                Ok(())
            }
            Err(e) => {
                error!(error = %e, "FUSE rename failed");
                metrics::counter!("aegis_fuse_operations_total", "operation" => "rename", "result" => "error").increment(1);
                Err(fsal_error_to_errno(&e))
            }
        }
    }
}

impl Filesystem for FuseFsal {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        match self.lookup_entry(parent.into(), name_str) {
            Ok(fuse_attr) => reply.entry(&FUSE_TTL, &fuse_attr, Generation(0)),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        match self.attributes(ino.into()) {
            Ok(attr) => reply.attr(&FUSE_TTL, &attr),
            Err(errno) => reply.error(errno),
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
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        match self.read_data(ino.into(), offset, size) {
            Ok(data) => reply.data(&data),
            Err(errno) => reply.error(errno),
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
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        match self.write_data(ino.into(), offset, data) {
            Ok(written) => reply.written(written),
            Err(errno) => reply.error(errno),
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
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        let ino: u64 = ino.into();
        let full_entries = match self.list_directory(ino) {
            Ok(entries) => entries,
            Err(errno) => {
                reply.error(errno);
                return;
            }
        };

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
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        match self.create_entry(parent.into(), name_str) {
            Ok(attr) => reply.created(
                &FUSE_TTL,
                &attr,
                Generation(0),
                FileHandle(0),
                FopenFlags::empty(),
            ),
            Err(errno) => reply.error(errno),
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
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        match self.make_directory(parent.into(), name_str) {
            Ok(attr) => reply.entry(&FUSE_TTL, &attr, Generation(0)),
            Err(errno) => reply.error(errno),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        match self.remove_file(parent.into(), name_str) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        match self.remove_directory(parent.into(), name_str) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
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
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
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

        match self.rename_entry(parent.into(), name_str, newparent.into(), newname_str) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
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
        if self.is_degraded() {
            reply.error(Errno::EIO);
            return;
        }
        let _guard = OpGuard::enter(self);
        match self.set_attributes(ino.into(), size) {
            Ok(attr) => reply.attr(&FUSE_TTL, &attr),
            Err(errno) => reply.error(errno),
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

        async fn truncate(
            &self,
            _: &AegisFileHandle,
            _: &str,
            _: &FsalAccessPolicy,
            _: u64,
        ) -> Result<(), FsalError> {
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
        make_fuse_fsal_for(runtime, 1000, 1000)
    }

    /// Build a `FuseFsal` for a mount request naming `container_uid:container_gid`.
    fn make_fuse_fsal_for(
        runtime: tokio::runtime::Handle,
        container_uid: u32,
        container_gid: u32,
    ) -> FuseFsal {
        FuseFsal {
            backend: Arc::new(StubBackend),
            context: FuseVolumeContext {
                execution_id: ExecutionId::new(),
                volume_id: VolumeId::new(),
                workflow_execution_id: None,
                container_uid,
                container_gid,
                policy: FsalAccessPolicy::default(),
            },
            inode_table: Arc::new(InodeTable::new()),
            runtime,
            last_completed_op_at: Arc::new(AtomicU64::new(0)),
            in_flight_ops: Arc::new(AtomicU32::new(0)),
            degraded: Arc::new(AtomicBool::new(false)),
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

    /// Regression: `OpGuard::enter` must increment `in_flight_ops` on
    /// construction and `Drop` must update `last_completed_op_at` and
    /// decrement `in_flight_ops` even on early returns. This is what allows
    /// the per-mount supervisor task to reliably detect stuck callbacks
    /// without missing decrements on error paths.
    #[test]
    fn op_guard_increments_and_drops_correctly() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let fs = make_fuse_fsal(rt.handle().clone());
        assert_eq!(fs.in_flight_ops.load(Ordering::SeqCst), 0);
        let before = fs.last_completed_op_at.load(Ordering::SeqCst);

        {
            let _g = OpGuard::enter(&fs);
            assert_eq!(fs.in_flight_ops.load(Ordering::SeqCst), 1);
        }

        assert_eq!(fs.in_flight_ops.load(Ordering::SeqCst), 0);
        let after = fs.last_completed_op_at.load(Ordering::SeqCst);
        assert!(
            after >= before,
            "Drop must update last_completed_op_at (before={before}, after={after})"
        );
    }

    /// Regression: when `degraded` is true, `is_degraded()` must return true
    /// so callbacks can short-circuit to EIO instead of enqueuing more work
    /// behind a wedged backend. The supervisor task drives this transition;
    /// here we set it synthetically to test the readback path that every
    /// FUSE callback uses.
    #[test]
    fn degraded_flag_is_observable_by_callbacks() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let fs = make_fuse_fsal(rt.handle().clone());
        assert!(!fs.is_degraded(), "fresh mount must not be degraded");
        fs.degraded.store(true, Ordering::SeqCst);
        assert!(
            fs.is_degraded(),
            "callbacks must see the degraded flag and short-circuit to EIO"
        );
    }

    /// Regression (eval-long-doc-agent, execution ff93baf9): a mount whose
    /// request names the container's root reports every inode as owned by the
    /// identity that root has on the host, this daemon's own, so the kernel's
    /// `default_permissions` check lets the container's root write a
    /// read-write volume. Before the fix the mount reported the request's
    /// identity unchanged, an owner the container's root is not on the host.
    #[test]
    fn a_root_containers_mount_is_owned_by_the_daemons_own_identity() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let fs = make_fuse_fsal_for(rt.handle().clone(), 0, 0);
        // SAFETY: geteuid and getegid have no preconditions and cannot fail.
        let (daemon_uid, daemon_gid) = unsafe { (libc::geteuid(), libc::getegid()) };

        let root = fs.root_attr();
        assert_eq!(
            (root.uid, root.gid, root.perm),
            (daemon_uid, daemon_gid, 0o755),
            "the volume root is owned by the daemon's identity, mode 0755"
        );

        let file = fs.convert_attrs(
            &FileAttributes {
                file_type: FileType::File,
                size: 4,
                mtime: 0,
                atime: 0,
                ctime: 0,
                mode: 0o644,
                uid: 1000,
                gid: 1000,
                nlink: 1,
            },
            7,
        );
        assert_eq!(
            (file.uid, file.gid, file.perm),
            (daemon_uid, daemon_gid, 0o644),
            "a file is owned by the daemon's identity and keeps the storage layer's mode"
        );
    }

    /// The rule for a rootless and a rootful runtime. Under rootless Podman
    /// the daemon runs as the unprivileged user that runs Podman (production:
    /// `aegis`, uid 1003), and the container's root is that user on the host;
    /// under a rootful runtime the daemon and the container's root are both
    /// root. A non-root identity is reported as the request names it.
    #[test]
    fn mount_owner_follows_the_container_root_under_rootless_and_rootful_runtimes() {
        // Rootless Podman: the agent path's request names root, 0:0.
        assert_eq!(
            MountOwner::for_container(0, 0, 1003, 1003),
            MountOwner {
                uid: 1003,
                gid: 1003
            }
        );
        // Rootful runtime: the daemon runs as root.
        assert_eq!(
            MountOwner::for_container(0, 0, 0, 0),
            MountOwner { uid: 0, gid: 0 }
        );
        // A container process with its own non-root identity, either runtime.
        assert_eq!(
            MountOwner::for_container(65534, 65534, 1003, 1003),
            MountOwner {
                uid: 65534,
                gid: 65534
            }
        );
        assert_eq!(
            MountOwner::for_container(1000, 0, 0, 0),
            MountOwner { uid: 1000, gid: 0 }
        );
    }

    /// A request naming a non-root identity is reported as it names it.
    #[test]
    fn a_non_root_identity_is_reported_as_the_request_names_it() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let fs = make_fuse_fsal_for(rt.handle().clone(), 65534, 65534);
        let root = fs.root_attr();
        assert_eq!((root.uid, root.gid), (65534, 65534));
    }

    /// Regression: the supervisor task must mark a mount degraded when ops
    /// have been in-flight longer than `STUCK_OP_THRESHOLD_MS`. We simulate
    /// this directly against the registry rather than spawning the real
    /// supervisor (which would require coordinating with the FUSE session
    /// lifecycle).
    #[tokio::test]
    async fn degraded_registry_records_stuck_mount() {
        let registry = new_degraded_mount_registry();
        let key = mount_key("exec-stuck", "vol-stuck");
        assert_eq!(registry.read().await.len(), 0);

        registry.write().await.insert(
            key.clone(),
            DegradedReason {
                reason: "test stuck".into(),
                since: SystemTime::now(),
            },
        );

        assert_eq!(registry.read().await.len(), 1);
        assert!(registry.read().await.contains_key(&key));
    }

    /// Regression (clone-work-tree-io-error): on a SeaweedFS volume the
    /// LOOKUP of a directory the shell made (`git clone` making `.git`)
    /// answered a regular file, because the filer's `stat` reported every
    /// path as one; the kernel then failed git's next `mkdir` under it with
    /// EIO. The fake filer answers as SeaweedFS does: MKDIR as its own mkdir
    /// (`POST <dir>/`), a HEAD of the directory with 200, and its metadata
    /// with `os.ModeDir` in `Mode`.
    #[test]
    fn a_directory_made_through_the_mount_looks_up_as_a_directory_on_seaweedfs() {
        use crate::domain::events::StorageEvent;
        use crate::domain::fsal::EventPublisher;
        use crate::domain::repository::VolumeRepository;
        use crate::domain::volume::{
            FilerEndpoint, StorageClass, TenantId, Volume, VolumeBackend, VolumeOwnership,
            VolumeStatus,
        };
        use crate::infrastructure::repositories::InMemoryVolumeRepository;
        use crate::infrastructure::storage::seaweedfs::SeaweedFSAdapter;

        struct NoEvents;

        #[async_trait]
        impl EventPublisher for NoEvents {
            async fn publish_storage_event(&self, _: StorageEvent) {}
        }

        let volume_id = VolumeId::new();
        let remote_path = format!("/volumes/{volume_id}");
        let git_dir = format!("{remote_path}/.git");

        let mut filer = mockito::Server::new();
        filer
            .mock("POST", format!("{git_dir}/").as_str())
            .with_status(201)
            .with_body(r#"{"name":".git"}"#)
            .create();
        filer
            .mock("HEAD", git_dir.as_str())
            .with_status(200)
            .create();
        filer
            .mock("GET", git_dir.as_str())
            .match_query(mockito::Matcher::UrlEncoded(
                "metadata".into(),
                "true".into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"FullPath":"{git_dir}","Mtime":"2026-10-11T14:27:33Z","Mode":2147484141,"FileSize":0}}"#
            ))
            .create();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");

        let step = ExecutionId::new();
        let volume = Volume {
            id: volume_id,
            name: "repo".to_string(),
            tenant_id: TenantId::consumer(),
            storage_class: StorageClass::persistent(),
            backend: VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new(filer.url()).unwrap(),
                remote_path,
            },
            size_limit_bytes: 1_000_000,
            status: VolumeStatus::Available,
            ownership: VolumeOwnership::execution(step),
            created_at: chrono::Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        };
        let repo = Arc::new(InMemoryVolumeRepository::new());
        rt.block_on(repo.save(&volume)).expect("volume saved");
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(SeaweedFSAdapter::new(filer.url())),
            repo as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoEvents),
        ));

        let mount = FuseFsal {
            backend: Arc::new(DirectFsalBackend(fsal)),
            context: FuseVolumeContext {
                execution_id: step,
                volume_id,
                workflow_execution_id: None,
                container_uid: 0,
                container_gid: 0,
                policy: FsalAccessPolicy {
                    read: vec!["/*".to_string()],
                    write: vec!["/*".to_string()],
                },
            },
            inode_table: Arc::new(InodeTable::new()),
            runtime: rt.handle().clone(),
            last_completed_op_at: Arc::new(AtomicU64::new(0)),
            in_flight_ops: Arc::new(AtomicU32::new(0)),
            degraded: Arc::new(AtomicBool::new(false)),
        };

        mount
            .make_directory(ROOT_INODE, ".git")
            .expect("mkdir .git");
        let looked_up = mount
            .lookup_entry(ROOT_INODE, ".git")
            .map(|attr| attr.kind)
            .map_err(|errno| format!("{errno:?}"));

        assert_eq!(
            looked_up,
            Ok(FuseFileType::Directory),
            "LOOKUP of the directory the shell made did not answer a directory"
        );
    }

    /// Regression (`forge-clone-index-pack-failed`): SETATTR ignored its
    /// size, so an `O_TRUNC` open or `truncate(2)` through the mount left the
    /// file as it was. A SETATTR with a size sets the file's length on the
    /// filer: size 3 of a 10-byte file PUTs its first 3 bytes.
    #[test]
    fn setattr_with_a_size_truncates_the_file_on_seaweedfs() {
        use crate::domain::events::StorageEvent;
        use crate::domain::fsal::EventPublisher;
        use crate::domain::repository::VolumeRepository;
        use crate::domain::volume::{
            FilerEndpoint, StorageClass, TenantId, Volume, VolumeBackend, VolumeOwnership,
            VolumeStatus,
        };
        use crate::infrastructure::repositories::InMemoryVolumeRepository;
        use crate::infrastructure::storage::seaweedfs::SeaweedFSAdapter;

        struct NoEvents;

        #[async_trait]
        impl EventPublisher for NoEvents {
            async fn publish_storage_event(&self, _: StorageEvent) {}
        }

        let volume_id = VolumeId::new();
        let remote_path = format!("/volumes/{volume_id}");
        let file = format!("{remote_path}/FETCH_HEAD");

        let mut filer = mockito::Server::new();
        filer
            .mock("GET", file.as_str())
            .match_query(mockito::Matcher::UrlEncoded(
                "metadata".into(),
                "true".into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"FullPath":"{file}","Mtime":"2026-10-12T20:02:15Z","Mode":420,"FileSize":10}}"#
            ))
            .create();
        filer
            .mock("HEAD", file.as_str())
            .with_status(200)
            .with_header("content-length", "10")
            .create();
        filer
            .mock("GET", file.as_str())
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_body("0123456789")
            .create();
        let put = filer
            .mock("PUT", file.as_str())
            .match_body(mockito::Matcher::Exact("012".to_string()))
            .with_status(201)
            .expect(1)
            .create();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");

        let step = ExecutionId::new();
        let volume = Volume {
            id: volume_id,
            name: "repo".to_string(),
            tenant_id: TenantId::consumer(),
            storage_class: StorageClass::persistent(),
            backend: VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new(filer.url()).unwrap(),
                remote_path,
            },
            size_limit_bytes: 1_000_000,
            status: VolumeStatus::Available,
            ownership: VolumeOwnership::execution(step),
            created_at: chrono::Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        };
        let repo = Arc::new(InMemoryVolumeRepository::new());
        rt.block_on(repo.save(&volume)).expect("volume saved");
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(SeaweedFSAdapter::new(filer.url())),
            repo as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoEvents),
        ));

        let mount = FuseFsal {
            backend: Arc::new(DirectFsalBackend(fsal)),
            context: FuseVolumeContext {
                execution_id: step,
                volume_id,
                workflow_execution_id: None,
                container_uid: 0,
                container_gid: 0,
                policy: FsalAccessPolicy {
                    read: vec!["/*".to_string()],
                    write: vec!["/*".to_string()],
                },
            },
            inode_table: Arc::new(InodeTable::new()),
            runtime: rt.handle().clone(),
            last_completed_op_at: Arc::new(AtomicU64::new(0)),
            in_flight_ops: Arc::new(AtomicU32::new(0)),
            degraded: Arc::new(AtomicBool::new(false)),
        };

        let ino: u64 = mount
            .lookup_entry(ROOT_INODE, "FETCH_HEAD")
            .expect("lookup FETCH_HEAD")
            .ino
            .into();
        let set = mount
            .set_attributes(ino, Some(3))
            .map(|_| ())
            .map_err(|errno| format!("{errno:?}"));

        assert_eq!(set, Ok(()), "SETATTR with a size failed");
        put.assert();
    }
}

/// Regression (`clerk-fs-write-not-visible-to-cmd-run`, the triage run's
/// RECORD clerk `c9995957` on 2026-10-09): a workflow's Agent state mounts its
/// own volume, owned by its execution, with the workflow's id in the mount's
/// context. The mount's handles named the workflow, which the FSAL authorises
/// only against a volume the workflow owns, so every LOOKUP of a file the
/// mount did not create was refused and answered ENOENT: the shell could not
/// see what `fs.write` wrote to the same volume.
#[cfg(test)]
mod handle_tests {
    use super::*;
    use crate::domain::events::StorageEvent;
    use crate::domain::fsal::{CreateFsalFileRequest, EventPublisher, VolumeContextLookup};
    use crate::domain::repository::VolumeRepository;
    use crate::domain::volume::{
        FilerEndpoint, StorageClass, TenantId, Volume, VolumeBackend, VolumeOwnership, VolumeStatus,
    };
    use crate::infrastructure::repositories::InMemoryVolumeRepository;
    use crate::infrastructure::storage::local_host_provider::LocalHostStorageProvider;

    const THREAD_IDS: &[u8] = b"<a@example.com>\n<b@example.com>\n";

    struct NoEvents;

    #[async_trait::async_trait]
    impl EventPublisher for NoEvents {
        async fn publish_storage_event(&self, _: StorageEvent) {}
    }

    /// The orchestrator's volume registry as the FSAL reads it: the workflow
    /// a volume was registered for.
    struct Registered(Option<(VolumeId, uuid::Uuid)>);

    impl VolumeContextLookup for Registered {
        fn lookup_workflow_execution_id(&self, volume_id: VolumeId) -> Option<uuid::Uuid> {
            self.0
                .filter(|(registered, _)| *registered == volume_id)
                .map(|(_, workflow)| workflow)
        }

        fn holds_registration(&self, _volume_id: VolumeId, _execution_id: ExecutionId) -> bool {
            false
        }
    }

    fn all_paths() -> FsalAccessPolicy {
        FsalAccessPolicy {
            read: vec!["/*".to_string()],
            write: vec!["/*".to_string()],
        }
    }

    /// One volume on a scratch directory, the FSAL over it, and a mount of it.
    struct Fixture {
        _root: tempfile::TempDir,
        storage_root: std::path::PathBuf,
        rt: tokio::runtime::Runtime,
        fsal: Arc<AegisFSAL>,
        volume: Volume,
        mount: FuseFsal,
    }

    impl Fixture {
        /// `ownership` owns the volume; the mount serves `execution_id` in
        /// `workflow_execution_id`, as the runtime's mount request names them.
        fn new(
            ownership: VolumeOwnership,
            registered: Option<uuid::Uuid>,
            execution_id: ExecutionId,
            workflow_execution_id: Option<uuid::Uuid>,
        ) -> Self {
            let root = tempfile::tempdir().expect("scratch root");
            let storage_root = root.path().to_path_buf();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime");

            let volume_id = VolumeId::new();
            let volume = Volume {
                id: volume_id,
                name: "workspace".to_string(),
                tenant_id: TenantId::consumer(),
                storage_class: StorageClass::persistent(),
                backend: VolumeBackend::SeaweedFS {
                    filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
                    remote_path: format!("/volumes/{volume_id}"),
                },
                size_limit_bytes: 1_000_000,
                status: VolumeStatus::Available,
                ownership,
                created_at: chrono::Utc::now(),
                attached_at: None,
                detached_at: None,
                expires_at: None,
                host_node_id: None,
            };

            let repo = Arc::new(InMemoryVolumeRepository::new());
            rt.block_on(repo.save(&volume)).expect("volume saved");
            let fsal = Arc::new(
                AegisFSAL::new(
                    Arc::new(LocalHostStorageProvider::new(&storage_root).expect("storage")),
                    repo as Arc<dyn VolumeRepository>,
                    Arc::new(parking_lot::RwLock::new(HashMap::new())),
                    Arc::new(NoEvents),
                )
                .with_volume_context_lookup(Arc::new(Registered(
                    registered.map(|workflow| (volume_id, workflow)),
                ))),
            );

            let mount = FuseFsal {
                backend: Arc::new(DirectFsalBackend(fsal.clone())),
                context: FuseVolumeContext {
                    execution_id,
                    volume_id,
                    workflow_execution_id,
                    container_uid: 0,
                    container_gid: 0,
                    policy: all_paths(),
                },
                inode_table: Arc::new(InodeTable::new()),
                runtime: rt.handle().clone(),
                last_completed_op_at: Arc::new(AtomicU64::new(0)),
                in_flight_ops: Arc::new(AtomicU32::new(0)),
                degraded: Arc::new(AtomicBool::new(false)),
            };

            Self {
                _root: root,
                storage_root,
                rt,
                fsal,
                volume,
                mount,
            }
        }

        /// What the orchestrator's `fs.write` does for `execution_id`
        /// (`builtin_fsal::invoke_fs_tool`): create, then write through an
        /// execution handle, outside the mount.
        fn fs_write(&self, execution_id: ExecutionId, path: &str, content: &[u8]) {
            let policy = all_paths();
            self.rt
                .block_on(async {
                    self.fsal
                        .create_file(CreateFsalFileRequest {
                            execution_id,
                            volume_id: self.volume.id,
                            path,
                            policy: &policy,
                            emit_event: false,
                            caller_node_id: None,
                            host_node_id: None,
                            workflow_execution_id: None,
                        })
                        .await?;
                    self.fsal
                        .write(
                            &AegisFileHandle::new(execution_id, self.volume.id, "/"),
                            path,
                            &policy,
                            0,
                            content,
                        )
                        .await
                })
                .expect("fs.write");
        }

        /// The shell's `cat <path>` through the mount: LOOKUP of each
        /// component from the root, then READ.
        fn cat(&self, path: &str) -> Result<Vec<u8>, String> {
            let mut ino = ROOT_INODE;
            for name in path.trim_start_matches('/').split('/') {
                ino = self
                    .mount
                    .lookup_entry(ino, name)
                    .map_err(|errno| format!("LOOKUP {name}: {errno:?}"))?
                    .ino
                    .into();
            }
            self.mount
                .read_data(ino, 0, 4096)
                .map_err(|errno| format!("READ {path}: {errno:?}"))
        }
    }

    /// RECORD's case: its own volume, owned by its execution, mounted with the
    /// workflow's id. What `fs.write` wrote is found and read through the
    /// mount.
    #[test]
    fn a_workflow_steps_mount_reads_what_fs_write_wrote_to_the_steps_own_volume() {
        let step = ExecutionId::new();
        let workflow = uuid::Uuid::new_v4();
        let fixture = Fixture::new(VolumeOwnership::execution(step), None, step, Some(workflow));

        fixture.fs_write(step, "/thread-ids.txt", THREAD_IDS);

        assert_eq!(
            fixture.cat("/thread-ids.txt"),
            Ok(THREAD_IDS.to_vec()),
            "the step's mount did not find or read the file fs.write wrote to its own volume"
        );
    }

    /// The workflow's workspace, owned by the workflow and registered for it,
    /// still reads through a step's mount.
    #[test]
    fn a_workflow_steps_mount_reads_what_fs_write_wrote_to_the_workflows_workspace() {
        let step = ExecutionId::new();
        let workflow = uuid::Uuid::new_v4();
        let fixture = Fixture::new(
            VolumeOwnership::workflow(workflow),
            Some(workflow),
            step,
            Some(workflow),
        );

        fixture.fs_write(step, "/thread-ids.txt", THREAD_IDS);

        assert_eq!(
            fixture.cat("/thread-ids.txt"),
            Ok(THREAD_IDS.to_vec()),
            "the step's mount did not find or read the file fs.write wrote to the workflow's workspace"
        );
    }

    /// A directory the shell made through the mount (`mkdir out`): a file
    /// written into it outside the mount is found under the directory's own
    /// inode, which the kernel keeps from the MKDIR reply.
    #[test]
    fn a_file_written_into_a_directory_the_shell_made_reads_through_the_directorys_inode() {
        let step = ExecutionId::new();
        let workflow = uuid::Uuid::new_v4();
        let fixture = Fixture::new(VolumeOwnership::execution(step), None, step, Some(workflow));

        let out: u64 = fixture
            .mount
            .make_directory(ROOT_INODE, "out")
            .expect("mkdir out")
            .ino
            .into();
        let on_disk = fixture
            .storage_root
            .join(format!("volumes/{}/out/report.txt", fixture.volume.id));
        std::fs::write(&on_disk, THREAD_IDS).expect("report written");

        let report = fixture
            .mount
            .lookup_entry(out, "report.txt")
            .map(|attr| u64::from(attr.ino));
        assert!(
            report.is_ok(),
            "LOOKUP of report.txt under the directory the shell made was refused: {report:?}"
        );
        assert_eq!(
            fixture
                .mount
                .read_data(report.unwrap(), 0, 4096)
                .map_err(|errno| format!("{errno:?}")),
            Ok(THREAD_IDS.to_vec())
        );
    }
}

/// The inode a kernel holds for a file names that file across a rename and
/// is let go on unlink: git's lock-file protocol (`config.lock` renamed over
/// `config`, `index.lock` over `index`) depends on both.
#[cfg(test)]
mod inode_tests {
    use super::*;
    use crate::domain::fsal::FsalError;
    use crate::domain::storage::{DirEntry, FileAttributes, StorageError};
    use async_trait::async_trait;
    use std::collections::{BTreeMap, BTreeSet};

    const THREAD_IDS: &[u8] = b"<a@example.com>\n<b@example.com>\n";

    /// A volume held in memory, answering as a POSIX file system does: a
    /// rename moves a file or a directory with everything under it and
    /// replaces a file at its destination.
    #[derive(Default)]
    struct MemoryVolume {
        files: parking_lot::Mutex<BTreeMap<String, Vec<u8>>>,
        directories: parking_lot::Mutex<BTreeSet<String>>,
    }

    fn missing(path: &str) -> FsalError {
        FsalError::Storage(StorageError::FileNotFound(path.to_string()))
    }

    fn join(parent: &str, name: &str) -> String {
        FuseFsal::child_path(parent, name)
    }

    #[async_trait]
    impl FsalBackend for MemoryVolume {
        async fn getattr(
            &self,
            _: ExecutionId,
            _: VolumeId,
            path: &str,
            _: u32,
            _: u32,
            _: Option<uuid::Uuid>,
        ) -> Result<FileAttributes, FsalError> {
            let (file_type, size, mode, nlink) = if self.directories.lock().contains(path) {
                (FileType::Directory, 4096, 0o755, 2)
            } else {
                let files = self.files.lock();
                let data = files.get(path).ok_or_else(|| missing(path))?;
                (FileType::File, data.len() as u64, 0o644, 1)
            };
            Ok(FileAttributes {
                file_type,
                size,
                mtime: 0,
                atime: 0,
                ctime: 0,
                mode,
                uid: 1000,
                gid: 1000,
                nlink,
            })
        }

        async fn lookup(
            &self,
            parent: &AegisFileHandle,
            parent_path: &str,
            name: &str,
        ) -> Result<AegisFileHandle, FsalError> {
            let path = join(parent_path, name);
            if !self.files.lock().contains_key(&path) && !self.directories.lock().contains(&path) {
                return Err(missing(&path));
            }
            let execution_id = *parent
                .execution_context
                .execution_id()
                .expect("the mount's handles name its execution");
            Ok(AegisFileHandle::new(execution_id, parent.volume_id, &path))
        }

        async fn readdir(
            &self,
            _: ExecutionId,
            _: VolumeId,
            path: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<Vec<DirEntry>, FsalError> {
            if !self.directories.lock().contains(path) {
                return Err(missing(path));
            }
            let child = |held: &str| -> Option<String> {
                let rest = held.strip_prefix(path)?.strip_prefix('/')?;
                (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_string())
            };
            let mut entries: Vec<DirEntry> = self
                .files
                .lock()
                .keys()
                .filter_map(|held| child(held))
                .map(|name| DirEntry {
                    name,
                    file_type: FileType::File,
                })
                .collect();
            entries.extend(
                self.directories
                    .lock()
                    .iter()
                    .filter_map(|held| child(held))
                    .map(|name| DirEntry {
                        name,
                        file_type: FileType::Directory,
                    }),
            );
            Ok(entries)
        }

        async fn read(
            &self,
            _: &AegisFileHandle,
            path: &str,
            _: &FsalAccessPolicy,
            offset: u64,
            length: usize,
        ) -> Result<Vec<u8>, FsalError> {
            let files = self.files.lock();
            let data = files.get(path).ok_or_else(|| missing(path))?;
            let start = (offset as usize).min(data.len());
            let end = (start + length).min(data.len());
            Ok(data[start..end].to_vec())
        }

        async fn write(
            &self,
            _: &AegisFileHandle,
            path: &str,
            _: &FsalAccessPolicy,
            offset: u64,
            bytes: &[u8],
        ) -> Result<usize, FsalError> {
            let mut files = self.files.lock();
            let data = files.get_mut(path).ok_or_else(|| missing(path))?;
            let end = offset as usize + bytes.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[offset as usize..end].copy_from_slice(bytes);
            Ok(bytes.len())
        }

        async fn truncate(
            &self,
            _: &AegisFileHandle,
            path: &str,
            _: &FsalAccessPolicy,
            size: u64,
        ) -> Result<(), FsalError> {
            let mut files = self.files.lock();
            let data = files.get_mut(path).ok_or_else(|| missing(path))?;
            data.resize(size as usize, 0);
            Ok(())
        }

        async fn create_file(
            &self,
            execution_id: ExecutionId,
            volume_id: VolumeId,
            path: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<AegisFileHandle, FsalError> {
            self.files.lock().insert(path.to_string(), Vec::new());
            Ok(AegisFileHandle::new(execution_id, volume_id, path))
        }

        async fn create_directory(
            &self,
            _: ExecutionId,
            _: VolumeId,
            path: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<(), FsalError> {
            self.directories.lock().insert(path.to_string());
            Ok(())
        }

        async fn delete_file(
            &self,
            _: ExecutionId,
            _: VolumeId,
            path: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<(), FsalError> {
            self.files
                .lock()
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| missing(path))
        }

        async fn delete_directory(
            &self,
            _: ExecutionId,
            _: VolumeId,
            path: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<(), FsalError> {
            // The storage's delete of a directory takes everything in it.
            let under = |held: &str| {
                held.strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
            };
            if !self.directories.lock().remove(path) {
                return Err(missing(path));
            }
            self.files.lock().retain(|held, _| !under(held));
            self.directories.lock().retain(|held| !under(held));
            Ok(())
        }

        async fn rename(
            &self,
            _: ExecutionId,
            _: VolumeId,
            from: &str,
            to: &str,
            _: &FsalAccessPolicy,
            _: Option<uuid::Uuid>,
        ) -> Result<(), FsalError> {
            let moved = |path: &str| -> Option<String> {
                if path == from {
                    Some(to.to_string())
                } else {
                    path.strip_prefix(from)
                        .filter(|rest| rest.starts_with('/'))
                        .map(|rest| format!("{to}{rest}"))
                }
            };
            let mut files = self.files.lock();
            let mut directories = self.directories.lock();
            if !files.contains_key(from) && !directories.contains(from) {
                return Err(missing(from));
            }
            let entries: Vec<(String, Vec<u8>)> = std::mem::take(&mut *files).into_iter().collect();
            for (path, data) in entries {
                files.insert(moved(&path).unwrap_or(path), data);
            }
            let dirs: Vec<String> = std::mem::take(&mut *directories).into_iter().collect();
            for path in dirs {
                directories.insert(moved(&path).unwrap_or(path));
            }
            Ok(())
        }
    }

    /// A mount of a [`MemoryVolume`].
    struct Fixture {
        _rt: tokio::runtime::Runtime,
        mount: FuseFsal,
    }

    impl Fixture {
        fn new() -> Self {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime");
            let mount = FuseFsal {
                backend: Arc::new(MemoryVolume::default()),
                context: FuseVolumeContext {
                    execution_id: ExecutionId::new(),
                    volume_id: VolumeId::new(),
                    workflow_execution_id: None,
                    container_uid: 0,
                    container_gid: 0,
                    policy: FsalAccessPolicy::default(),
                },
                inode_table: Arc::new(InodeTable::new()),
                runtime: rt.handle().clone(),
                last_completed_op_at: Arc::new(AtomicU64::new(0)),
                in_flight_ops: Arc::new(AtomicU32::new(0)),
                degraded: Arc::new(AtomicBool::new(false)),
            };
            Self { _rt: rt, mount }
        }
    }

    /// Regression (`clone-work-tree-io-error`, read on the box on
    /// 2026-10-11): git writes `.git/config` as `config.lock` and renames it
    /// over `config`. The kernel keeps the moved file's inode under its new
    /// name, and the mount went on naming that inode by the old path, so the
    /// GETATTR of `config` answered ENOENT and git died with "unknown error
    /// occurred while reading the configuration files"; the next
    /// `config.lock` was handed the moved file's inode as well.
    #[test]
    fn a_file_renamed_over_its_name_reads_through_the_inode_it_moved_with() {
        let fixture = Fixture::new();
        let mount = &fixture.mount;

        let lock: u64 = mount
            .create_entry(ROOT_INODE, "a.lock")
            .expect("create a.lock")
            .ino
            .into();
        mount.write_data(lock, 0, THREAD_IDS).expect("write a.lock");
        mount
            .rename_entry(ROOT_INODE, "a.lock", ROOT_INODE, "a")
            .expect("rename a.lock to a");

        assert_eq!(
            mount
                .attributes(lock)
                .map(|attr| attr.size)
                .map_err(|errno| format!("{errno:?}")),
            Ok(THREAD_IDS.len() as u64),
            "GETATTR of the moved file's inode did not answer the moved file"
        );
        assert_eq!(
            mount
                .read_data(lock, 0, 4096)
                .map_err(|errno| format!("{errno:?}")),
            Ok(THREAD_IDS.to_vec()),
            "READ of the moved file's inode did not answer the moved bytes"
        );

        let next_lock: u64 = mount
            .create_entry(ROOT_INODE, "a.lock")
            .expect("create a.lock again")
            .ino
            .into();
        assert_ne!(
            next_lock, lock,
            "a new a.lock was handed the inode of the file moved to a"
        );
    }

    /// A directory renamed with a file inside it: the file's inode names the
    /// file under the directory's new name.
    #[test]
    fn a_file_inside_a_renamed_directory_reads_through_its_own_inode() {
        let fixture = Fixture::new();
        let mount = &fixture.mount;

        let dir: u64 = mount
            .make_directory(ROOT_INODE, "tmp_pack")
            .expect("mkdir tmp_pack")
            .ino
            .into();
        let file: u64 = mount
            .create_entry(dir, "pack")
            .expect("create tmp_pack/pack")
            .ino
            .into();
        mount.write_data(file, 0, THREAD_IDS).expect("write pack");
        mount
            .rename_entry(ROOT_INODE, "tmp_pack", ROOT_INODE, "pack")
            .expect("rename tmp_pack to pack");

        assert_eq!(
            mount
                .read_data(file, 0, 4096)
                .map_err(|errno| format!("{errno:?}")),
            Ok(THREAD_IDS.to_vec()),
            "READ of a file inside the renamed directory did not answer its bytes"
        );
    }

    /// A file removed and made again under the same name is a new file with
    /// an inode of its own.
    #[test]
    fn a_file_made_again_after_unlink_is_given_a_new_inode() {
        let fixture = Fixture::new();
        let mount = &fixture.mount;

        let first: u64 = mount
            .create_entry(ROOT_INODE, "index.lock")
            .expect("create index.lock")
            .ino
            .into();
        mount
            .remove_file(ROOT_INODE, "index.lock")
            .expect("unlink index.lock");
        let second: u64 = mount
            .create_entry(ROOT_INODE, "index.lock")
            .expect("create index.lock again")
            .ino
            .into();

        assert_ne!(
            second, first,
            "a file made again after its unlink was handed the removed file's inode"
        );
    }

    /// Regression (clone-work-tree-io-error, the end-to-end harness against a
    /// real filer on 2026-10-11): git removes a file and then tries `rmdir`
    /// of its directory, counting on ENOTEMPTY while the directory holds
    /// other files (`dir.c`, `remove_path`). The mount's RMDIR is the
    /// storage's delete of a directory with everything in it, so `git rm
    /// data/000.txt` took the other 149 files of `data` with it. RMDIR of a
    /// directory that is not empty is refused and removes nothing.
    #[test]
    fn rmdir_of_a_directory_that_holds_a_file_is_refused_and_removes_nothing() {
        let fixture = Fixture::new();
        let mount = &fixture.mount;

        let data: u64 = mount
            .make_directory(ROOT_INODE, "data")
            .expect("mkdir data")
            .ino
            .into();
        let kept: u64 = mount
            .create_entry(data, "001.txt")
            .expect("create data/001.txt")
            .ino
            .into();
        mount
            .write_data(kept, 0, THREAD_IDS)
            .expect("write data/001.txt");

        assert_eq!(
            mount
                .remove_directory(ROOT_INODE, "data")
                .map_err(|errno| format!("{errno:?}")),
            Err(format!("{:?}", Errno::ENOTEMPTY)),
            "RMDIR of a directory holding a file was not refused with ENOTEMPTY"
        );
        assert_eq!(
            mount
                .read_data(kept, 0, 4096)
                .map_err(|errno| format!("{errno:?}")),
            Ok(THREAD_IDS.to_vec()),
            "the refused RMDIR removed the file inside the directory"
        );
    }

    /// RMDIR of an empty directory removes it.
    #[test]
    fn rmdir_of_an_empty_directory_removes_it() {
        let fixture = Fixture::new();
        let mount = &fixture.mount;

        mount.make_directory(ROOT_INODE, "lib").expect("mkdir lib");

        assert_eq!(
            mount
                .remove_directory(ROOT_INODE, "lib")
                .map_err(|errno| format!("{errno:?}")),
            Ok(())
        );
        assert!(
            mount.lookup_entry(ROOT_INODE, "lib").is_err(),
            "lib is still there after its RMDIR"
        );
    }
}
