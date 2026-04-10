// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! AegisFSAL - Transport-Agnostic File System Abstraction Layer
//!
//! Core security boundary for all file operations per ADR-036.
//! Acts as the orchestrator's semipermeable membrane for storage access.
//!
//! This entity enforces:
//! - Per-operation authorization (execution owns volume)
//! - Path canonicalization (prevent traversal attacks)
//! - Filesystem policy enforcement (read/write allowlists)
//! - File-level audit trail (StorageEvent publishing)
//! - UID/GID permission squashing (eliminate kernel checks)
//!
//! Used by:
//! - NFS Server Gateway (Phase 1, Docker)
//! - virtio-fs Gateway (Phase 2+, Firecracker)
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** Implements internal responsibilities for fsal

use crate::domain::{
    cluster::NodeId,
    events::StorageEvent,
    execution::ExecutionId,
    path_sanitizer::{PathSanitizer, PathSanitizerError},
    policy::FilesystemPolicy,
    repository::VolumeRepository,
    storage::{DirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider},
    volume::{Volume, VolumeId, VolumeStatus},
};

/// Transport-agnostic access policy for FSAL operations.
///
/// This is the FSAL's local representation of filesystem access constraints,
/// forming an Anti-Corruption Layer between BC-7 (Storage Gateway) and
/// BC-4 (Security Policy). The application layer converts
/// [`crate::domain::policy::FilesystemPolicy`] into this type before
/// passing it to FSAL methods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsalAccessPolicy {
    /// Path patterns the agent may **read** from mounted volumes.
    pub read: Vec<String>,
    /// Path patterns the agent may **write** to mounted volumes.
    pub write: Vec<String>,
}

impl Default for FsalAccessPolicy {
    fn default() -> Self {
        Self {
            read: vec!["/workspace/**".to_string()],
            write: vec!["/workspace/**".to_string()],
        }
    }
}

impl From<FilesystemPolicy> for FsalAccessPolicy {
    fn from(policy: FilesystemPolicy) -> Self {
        Self {
            read: policy.read,
            write: policy.write,
        }
    }
}
use async_trait::async_trait;
use chrono::Utc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

/// AegisFSAL errors
#[derive(Debug, Error)]
pub enum FsalError {
    #[error(
        "Unauthorized volume access: execution {execution_id} does not own volume {volume_id}"
    )]
    UnauthorizedAccess {
        execution_id: ExecutionId,
        volume_id: VolumeId,
    },

    #[error("Volume not found: {0}")]
    VolumeNotFound(VolumeId),

    #[error("Volume not attached: {0}")]
    VolumeNotAttached(VolumeId),

    #[error("Path sanitization error: {0}")]
    PathSanitization(#[from] PathSanitizerError),

    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("Filesystem policy violation: {0}")]
    PolicyViolation(String),

    #[error("Invalid file handle")]
    InvalidFileHandle,

    #[error("File handle deserialization error: {0}")]
    HandleDeserialization(String),

    #[error(
        "Volume quota exceeded: requested {requested_bytes} bytes, available {available_bytes} bytes"
    )]
    QuotaExceeded {
        requested_bytes: u64,
        available_bytes: u64,
    },
}

/// Compact execution context for NFSv3 file handles.
///
/// Serialized as: 1 byte tag + 16 bytes UUID = 17 bytes, compared to the previous
/// two `Option<Uuid>` fields which serialized to 18–34 bytes and pushed the total
/// handle size over the 64-byte NFSv3 hard limit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum HandleExecutionContext {
    /// Agent execution context.
    Agent(ExecutionId),
    /// Workflow execution context (ContainerStep FUSE path).
    Workflow(uuid::Uuid),
}

impl HandleExecutionContext {
    /// Returns the agent `ExecutionId` if this is an `Agent` context.
    pub fn execution_id(&self) -> Option<&ExecutionId> {
        match self {
            Self::Agent(id) => Some(id),
            Self::Workflow(_) => None,
        }
    }

    /// Returns the workflow execution UUID if this is a `Workflow` context.
    pub fn workflow_execution_id(&self) -> Option<uuid::Uuid> {
        match self {
            Self::Agent(_) => None,
            Self::Workflow(id) => Some(*id),
        }
    }
}

/// Aegis File Handle - encodes execution and volume ownership
///
/// Serialized with bincode to fit within NFSv3's 64-byte limit.
/// Uses a compact [`HandleExecutionContext`] enum (1-byte tag + 16-byte UUID = 17 bytes)
/// rather than two separate `Option<Uuid>` fields, which exceeded the 64-byte limit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct AegisFileHandle {
    /// Execution context that owns this file handle.
    pub execution_context: HandleExecutionContext,
    /// Volume containing this file
    pub volume_id: VolumeId,
    /// Hash of file path (for integrity check)
    pub path_hash: u64,
}

impl AegisFileHandle {
    /// Create a new file handle for an agent execution.
    pub fn new(execution_id: ExecutionId, volume_id: VolumeId, path: &str) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        execution_id.0.hash(&mut hasher);
        volume_id.0.hash(&mut hasher);
        path.hash(&mut hasher);
        let path_hash = hasher.finish();

        Self {
            execution_context: HandleExecutionContext::Agent(execution_id),
            volume_id,
            path_hash,
        }
    }

    /// Create a new file handle for a workflow execution (ContainerStep FUSE path).
    pub fn new_for_workflow(
        workflow_execution_id: uuid::Uuid,
        volume_id: VolumeId,
        path: &str,
    ) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        workflow_execution_id.hash(&mut hasher);
        volume_id.0.hash(&mut hasher);
        path.hash(&mut hasher);
        let path_hash = hasher.finish();

        Self {
            execution_context: HandleExecutionContext::Workflow(workflow_execution_id),
            volume_id,
            path_hash,
        }
    }

    /// Returns the agent `ExecutionId` if this handle belongs to an agent execution.
    pub fn execution_id(&self) -> Option<&ExecutionId> {
        self.execution_context.execution_id()
    }

    /// Returns the workflow execution UUID if this handle belongs to a workflow execution.
    pub fn workflow_execution_id(&self) -> Option<uuid::Uuid> {
        self.execution_context.workflow_execution_id()
    }

    /// Serialize to bytes (for NFS file handle)
    pub fn to_bytes(&self) -> Result<Vec<u8>, FsalError> {
        bincode::serialize(self).map_err(|e| FsalError::HandleDeserialization(e.to_string()))
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FsalError> {
        bincode::deserialize(bytes).map_err(|e| FsalError::HandleDeserialization(e.to_string()))
    }

    /// Validate handle size fits in NFSv3 limit (64 bytes)
    pub fn validate_size(&self) -> Result<(), FsalError> {
        let bytes = self.to_bytes()?;
        if bytes.len() > 64 {
            return Err(FsalError::HandleDeserialization(format!(
                "FileHandle too large: {} bytes (max 64)",
                bytes.len()
            )));
        }
        Ok(())
    }
}

/// Parameters for cross-node (SEAL) read and write operations on the FSAL.
///
/// Groups the provenance metadata required by [`AegisFSAL::read_for_node`] and
/// [`AegisFSAL::write_for_node`] to keep those signatures within Clippy's
/// function-argument-count limit.
pub struct NodeStorageRequest<'a> {
    pub execution_id: ExecutionId,
    pub volume_id: VolumeId,
    pub path: &'a str,
    pub caller_node_id: Option<NodeId>,
    pub host_node_id: Option<NodeId>,
}

/// Parameters for [`AegisFSAL::rename`].
///
/// Groups the provenance metadata and path parameters to keep the function
/// signature within Clippy's function-argument-count limit.
pub struct RenameFsalRequest<'a> {
    pub execution_id: ExecutionId,
    pub volume_id: VolumeId,
    pub from_path: &'a str,
    pub to_path: &'a str,
    pub policy: &'a FsalAccessPolicy,
    pub caller_node_id: Option<NodeId>,
    pub host_node_id: Option<NodeId>,
    pub workflow_execution_id: Option<uuid::Uuid>,
}

/// Parameters for [`AegisFSAL::create_file`].
///
/// Groups the provenance metadata and creation options to keep the function
/// signature within Clippy's function-argument-count limit.
pub struct CreateFsalFileRequest<'a> {
    pub execution_id: ExecutionId,
    pub volume_id: VolumeId,
    pub path: &'a str,
    pub policy: &'a FsalAccessPolicy,
    pub emit_event: bool,
    pub caller_node_id: Option<NodeId>,
    pub host_node_id: Option<NodeId>,
    pub workflow_execution_id: Option<uuid::Uuid>,
}

/// Provenance context for a filesystem policy violation event.
///
/// Groups the metadata required by [`AegisFSAL::emit_policy_violation`] to
/// keep that private helper within Clippy's function-argument-count limit.
struct PolicyViolationContext {
    execution_id: Option<ExecutionId>,
    workflow_execution_id: Option<uuid::Uuid>,
    volume_id: VolumeId,
    operation: &'static str,
    caller_node_id: Option<NodeId>,
    host_node_id: Option<NodeId>,
}

/// AegisFSAL - File System Abstraction Layer
///
/// Domain entity that enforces security policies and provides audit trail
/// for all file operations. Transport-agnostic: works with NFS, virtio-fs, etc.
pub struct AegisFSAL {
    /// Storage backend (SeaweedFS, Local, etc.)
    storage_provider: Arc<dyn StorageProvider>,
    /// Volume repository for ownership validation
    volume_repository: Arc<dyn VolumeRepository>,
    /// Read-only borrowed volume aliases keyed by the alias volume ID.
    borrowed_volumes: Arc<RwLock<HashMap<VolumeId, BorrowedVolumeAccess>>>,
    /// Path sanitizer for traversal prevention
    path_sanitizer: PathSanitizer,
    /// Event publisher (injected, not owned)
    event_publisher: Arc<dyn EventPublisher>,
    /// Optional volume context lookup for resolving `VolumeOwnership::WorkflowExecution`.
    /// When set, `authorize()` can match a requesting execution against the
    /// workflow_execution_id in the NFS volume registry, avoiding DB ownership
    /// mutations on every ContainerRun step.
    volume_context_lookup: Option<Arc<dyn VolumeContextLookup>>,
}

/// Event publisher trait (abstraction for event bus)
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish_storage_event(&self, event: StorageEvent);
}

/// Domain-layer abstraction for querying the NFS volume registry.
///
/// Used by `AegisFSAL::authorize()` to resolve `VolumeOwnership::WorkflowExecution`
/// without the domain layer depending on the application layer (`NfsVolumeRegistry`).
/// The application layer provides the concrete implementation.
pub trait VolumeContextLookup: Send + Sync {
    /// Returns the `workflow_execution_id` registered for the given volume, if any.
    fn lookup_workflow_execution_id(&self, volume_id: VolumeId) -> Option<uuid::Uuid>;
}

/// Borrowed read-only access to an existing volume, exposed under a distinct alias volume ID.
#[derive(Debug, Clone)]
pub struct BorrowedVolumeAccess {
    pub execution_id: ExecutionId,
    pub source_volume: Volume,
}

impl AegisFSAL {
    /// Create a new FSAL instance
    pub fn new(
        storage_provider: Arc<dyn StorageProvider>,
        volume_repository: Arc<dyn VolumeRepository>,
        borrowed_volumes: Arc<RwLock<HashMap<VolumeId, BorrowedVolumeAccess>>>,
        event_publisher: Arc<dyn EventPublisher>,
    ) -> Self {
        Self {
            storage_provider,
            volume_repository,
            borrowed_volumes,
            path_sanitizer: PathSanitizer::new(),
            event_publisher,
            volume_context_lookup: None,
        }
    }

    /// Set the volume context lookup for `WorkflowExecution` ownership resolution.
    pub fn with_volume_context_lookup(mut self, lookup: Arc<dyn VolumeContextLookup>) -> Self {
        self.volume_context_lookup = Some(lookup);
        self
    }

    /// Expose storage provider for direct use by application services (e.g. FileOperationsService)
    pub fn storage_provider(&self) -> &Arc<dyn crate::domain::storage::StorageProvider> {
        &self.storage_provider
    }

    fn routed_storage_path(&self, volume: &Volume, path: &str) -> String {
        match &volume.backend {
            crate::domain::volume::VolumeBackend::HostPath { path: host_path } => host_path
                .join(path.trim_start_matches('/'))
                .to_string_lossy()
                .to_string(),
            crate::domain::volume::VolumeBackend::SeaweedFS { remote_path, .. } => {
                format!("{}/{}", remote_path, path.trim_start_matches('/'))
            }
            crate::domain::volume::VolumeBackend::OpenDal { .. } => format!(
                "/aegis/opendal/volumes/{}/{}/{}",
                volume.tenant_id,
                volume.id,
                path.trim_start_matches('/')
            ),
            crate::domain::volume::VolumeBackend::Seal {
                node_id,
                remote_volume_id,
            } => format!(
                "/aegis/seal/{}/{}/{}",
                node_id,
                remote_volume_id,
                path.trim_start_matches('/')
            ),
        }
    }

    fn routed_usage_path(&self, volume: &Volume) -> String {
        match &volume.backend {
            crate::domain::volume::VolumeBackend::HostPath { path } => {
                path.to_string_lossy().to_string()
            }
            crate::domain::volume::VolumeBackend::SeaweedFS { remote_path, .. } => {
                remote_path.clone()
            }
            crate::domain::volume::VolumeBackend::OpenDal { .. } => {
                format!("/aegis/opendal/volumes/{}/{}", volume.tenant_id, volume.id)
            }
            crate::domain::volume::VolumeBackend::Seal {
                node_id,
                remote_volume_id,
            } => format!("/aegis/seal/{}/{}", node_id, remote_volume_id),
        }
    }

    /// Validate that a file handle's owner has access to the volume.
    ///
    /// Delegates to `authorize_inner` with the execution context extracted from
    /// the handle's `execution_id` / `workflow_execution_id` fields.
    async fn authorize_handle(&self, handle: &AegisFileHandle) -> Result<Volume, FsalError> {
        self.authorize_inner(
            handle.execution_id().copied(),
            handle.workflow_execution_id(),
            handle.volume_id,
        )
        .await
    }

    /// Validate that an execution owns the volume (agent execution path).
    async fn authorize(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
    ) -> Result<Volume, FsalError> {
        self.authorize_inner(Some(execution_id), None, volume_id)
            .await
    }

    /// Core volume authorization logic shared by `authorize` and `authorize_handle`.
    async fn authorize_inner(
        &self,
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
    ) -> Result<Volume, FsalError> {
        let borrowed = { self.borrowed_volumes.read().get(&volume_id).cloned() };
        if let Some(borrowed) = borrowed {
            let is_owner = execution_id
                .map(|eid| borrowed.execution_id == eid)
                .unwrap_or(false);
            if !is_owner {
                self.event_publisher
                    .publish_storage_event(StorageEvent::UnauthorizedVolumeAccess {
                        execution_id,
                        workflow_execution_id,
                        volume_id,
                        attempted_at: Utc::now(),
                        caller_node_id: None,
                        host_node_id: None,
                    })
                    .await;

                return Err(FsalError::UnauthorizedAccess {
                    execution_id: execution_id.unwrap_or_default(),
                    volume_id,
                });
            }

            let is_usable = matches!(
                borrowed.source_volume.status,
                VolumeStatus::Available | VolumeStatus::Attached
            );
            if !is_usable {
                return Err(FsalError::VolumeNotAttached(volume_id));
            }

            return Ok(borrowed.source_volume);
        }

        let volume = self
            .volume_repository
            .find_by_id(volume_id)
            .await
            .map_err(|_| FsalError::VolumeNotFound(volume_id))?
            .ok_or(FsalError::VolumeNotFound(volume_id))?;

        // Check volume is in a usable state.
        // Volumes created for an execution start as Available and are never individually
        // transitioned to Attached (that state is used for explicit attach_volume calls).
        // Real access control is the execution ownership check below.
        let is_usable = matches!(
            volume.status,
            VolumeStatus::Available | VolumeStatus::Attached
        );
        if !is_usable {
            return Err(FsalError::VolumeNotAttached(volume_id));
        }

        // Check execution owns volume
        let is_owner = match &volume.ownership {
            crate::domain::volume::VolumeOwnership::Execution {
                execution_id: exec_id,
            } => execution_id.map(|eid| *exec_id == eid).unwrap_or(false),
            crate::domain::volume::VolumeOwnership::WorkflowExecution {
                workflow_execution_id: wf_id,
            } => {
                if let Some(wf_exec_id) = workflow_execution_id {
                    // Direct match when the handle carries a workflow_execution_id
                    if wf_exec_id == *wf_id {
                        return Ok(volume);
                    }
                }
                // Fallback: check if the requesting execution is registered for this
                // volume in the NFS volume registry (legacy agent-execution path).
                self.volume_context_lookup
                    .as_ref()
                    .and_then(|lookup| lookup.lookup_workflow_execution_id(volume_id))
                    .map(|ctx_wf_id| ctx_wf_id == *wf_id)
                    .unwrap_or(false)
            }
            _ => false, // Persistent volumes require user-level auth
        };

        if !is_owner {
            self.event_publisher
                .publish_storage_event(StorageEvent::UnauthorizedVolumeAccess {
                    execution_id,
                    workflow_execution_id,
                    volume_id,
                    attempted_at: Utc::now(),
                    caller_node_id: None,
                    host_node_id: None,
                })
                .await;

            return Err(FsalError::UnauthorizedAccess {
                execution_id: execution_id.unwrap_or_default(),
                volume_id,
            });
        }

        Ok(volume)
    }

    /// Authorize a user (not an execution) to access a volume they own
    pub async fn authorize_for_user(
        &self,
        user_id: &str,
        volume_id: &VolumeId,
    ) -> Result<Volume, FsalError> {
        let volume = self
            .volume_repository
            .find_by_id(*volume_id)
            .await
            .map_err(|_| FsalError::VolumeNotFound(*volume_id))?
            .ok_or(FsalError::VolumeNotFound(*volume_id))?;

        match &volume.status {
            VolumeStatus::Available | VolumeStatus::Attached => {}
            _ => return Err(FsalError::VolumeNotAttached(*volume_id)),
        }

        match &volume.ownership {
            crate::domain::volume::VolumeOwnership::Persistent { owner } if owner == user_id => {}
            _ => {
                return Err(FsalError::UnauthorizedAccess {
                    execution_id: ExecutionId::new(),
                    volume_id: *volume_id,
                })
            }
        }

        Ok(volume)
    }

    /// Enforce filesystem policy for read operation
    fn enforce_read_policy(&self, policy: &FsalAccessPolicy, path: &str) -> Result<(), FsalError> {
        // Check if path matches any read allowlist pattern
        let allowed = policy.read.iter().any(|pattern| {
            // Wildcard matching: support both "/path/*" (single level) and "/path/**" (recursive)
            if pattern.ends_with("/**") {
                // Recursive glob pattern (/workspace/** matches /workspace and all nested)
                let prefix = &pattern[..pattern.len() - 3]; // Remove "/**"
                path.starts_with(prefix)
                    && (path == prefix || path.starts_with(&format!("{prefix}/")))
            } else if pattern.ends_with("/*") {
                // Single-level glob pattern (/workspace/* matches /workspace/file but not /workspace/dir/file)
                let prefix = &pattern[..pattern.len() - 2];
                path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
            } else {
                // Exact match
                path == pattern
            }
        });

        if !allowed {
            return Err(FsalError::PolicyViolation(format!(
                "Read not allowed for path: {path}"
            )));
        }

        Ok(())
    }

    /// Enforce filesystem policy for write operation
    fn enforce_write_policy(&self, policy: &FsalAccessPolicy, path: &str) -> Result<(), FsalError> {
        // Check if path matches any write allowlist pattern
        let allowed = policy.write.iter().any(|pattern| {
            // Wildcard matching: support both "/path/*" (single level) and "/path/**" (recursive)
            if pattern.ends_with("/**") {
                // Recursive glob pattern (/workspace/** matches /workspace and all nested)
                let prefix = &pattern[..pattern.len() - 3]; // Remove "/**"
                path.starts_with(prefix)
                    && (path == prefix || path.starts_with(&format!("{prefix}/")))
            } else if pattern.ends_with("/*") {
                // Single-level glob pattern (/workspace/* matches /workspace/file but not /workspace/dir/file)
                let prefix = &pattern[..pattern.len() - 2];
                path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
            } else {
                // Exact match
                path == pattern
            }
        });

        if !allowed {
            return Err(FsalError::PolicyViolation(format!(
                "Write not allowed for path: {path}"
            )));
        }

        Ok(())
    }

    /// Emit a `FilesystemPolicyViolation` storage event.
    ///
    /// Called by FSAL methods when `enforce_read_policy` or
    /// `enforce_write_policy` fails, providing a forensic audit trail
    /// for misconfigured or malicious access attempts.
    async fn emit_policy_violation(
        &self,
        ctx: PolicyViolationContext,
        path: &str,
        error: &FsalError,
    ) {
        self.event_publisher
            .publish_storage_event(StorageEvent::FilesystemPolicyViolation {
                execution_id: ctx.execution_id,
                workflow_execution_id: ctx.workflow_execution_id,
                volume_id: ctx.volume_id,
                operation: ctx.operation.to_string(),
                path: path.to_string(),
                policy_rule: error.to_string(),
                violated_at: Utc::now(),
                caller_node_id: ctx.caller_node_id,
                host_node_id: ctx.host_node_id,
            })
            .await;
    }

    /// Lookup a file/directory (NFS LOOKUP operation)
    pub async fn lookup(
        &self,
        handle: &AegisFileHandle,
        parent_path: &str,
        name: &str,
    ) -> Result<AegisFileHandle, FsalError> {
        // 1. Authorize
        let _volume = self.authorize_handle(handle).await?;

        // 2. Build child path
        let child_path = if parent_path == "/" || parent_path.is_empty() {
            format!("/{name}")
        } else {
            format!("{}/{}", parent_path.trim_end_matches('/'), name)
        };

        // 3. Sanitize path
        let canonical = self
            .path_sanitizer
            .canonicalize(&child_path, Some(parent_path))?;

        let canonical_str = canonical.to_str().unwrap().replace("\\", "/");

        // 4. Create new handle — propagate the same execution context as the parent
        let new_handle = match &handle.execution_context {
            HandleExecutionContext::Agent(exec_id) => {
                AegisFileHandle::new(*exec_id, handle.volume_id, &canonical_str)
            }
            HandleExecutionContext::Workflow(wf_id) => {
                AegisFileHandle::new_for_workflow(*wf_id, handle.volume_id, &canonical_str)
            }
        };

        Ok(new_handle)
    }

    /// Read from file at offset
    pub async fn read(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FsalAccessPolicy,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FsalError> {
        let start = std::time::Instant::now();

        // 1. Authorize
        let volume = self.authorize_handle(handle).await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_string = canonical.to_str().unwrap().replace("\\", "/");
        let path_str = path_string.as_str();
        if let Err(e) = self.enforce_read_policy(policy, path_str) {
            self.emit_policy_violation(
                PolicyViolationContext {
                    execution_id: handle.execution_id().copied(),
                    workflow_execution_id: handle.workflow_execution_id(),
                    volume_id: handle.volume_id,
                    operation: "read",
                    caller_node_id: None,
                    host_node_id: None,
                },
                path_str,
                &e,
            )
            .await;
            return Err(e);
        }
        let full_path = self.routed_storage_path(&volume, path_str);

        // 3. Read via storage provider
        let storage_handle = self
            .storage_provider
            .open_file(&full_path, OpenMode::ReadOnly)
            .await?;
        let data = self
            .storage_provider
            .read_at(&storage_handle, offset, length)
            .await?;
        let _ = self.storage_provider.close_file(&storage_handle).await;

        // 4. Publish event
        let duration_ms = start.elapsed().as_millis() as u64;
        self.event_publisher
            .publish_storage_event(StorageEvent::FileRead {
                execution_id: handle.execution_id().copied(),
                workflow_execution_id: handle.workflow_execution_id(),
                volume_id: handle.volume_id,
                path: path_str.to_string(),
                offset,
                bytes_read: data.len() as u64,
                duration_ms,
                read_at: Utc::now(),
                caller_node_id: None,
                host_node_id: None,
            })
            .await;

        Ok(data)
    }

    /// Write to file at offset
    pub async fn write(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FsalAccessPolicy,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, FsalError> {
        let start = std::time::Instant::now();

        // 1. Authorize and get volume for quota checking
        let volume = self.authorize_handle(handle).await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_string = canonical.to_str().unwrap().replace("\\", "/");
        let path_str = path_string.as_str();
        if let Err(e) = self.enforce_write_policy(policy, path_str) {
            self.emit_policy_violation(
                PolicyViolationContext {
                    execution_id: handle.execution_id().copied(),
                    workflow_execution_id: handle.workflow_execution_id(),
                    volume_id: handle.volume_id,
                    operation: "write",
                    caller_node_id: None,
                    host_node_id: None,
                },
                path_str,
                &e,
            )
            .await;
            return Err(e);
        }
        let full_path = self.routed_storage_path(&volume, path_str);
        let usage_path = self.routed_usage_path(&volume);

        // 3. Proactive quota enforcement (ADR-036)
        // Check if write would exceed volume quota before attempting write
        let current_usage = self.storage_provider.get_usage(&usage_path).await?;
        let requested_bytes = data.len() as u64;
        let projected_usage = current_usage.saturating_add(requested_bytes);

        if projected_usage > volume.size_limit_bytes {
            let available_bytes = volume.size_limit_bytes.saturating_sub(current_usage);

            // Publish quota exceeded event
            self.event_publisher
                .publish_storage_event(StorageEvent::QuotaExceeded {
                    execution_id: handle.execution_id().copied(),
                    workflow_execution_id: handle.workflow_execution_id(),
                    volume_id: handle.volume_id,
                    requested_bytes,
                    available_bytes,
                    exceeded_at: Utc::now(),
                    caller_node_id: None,
                    host_node_id: None,
                })
                .await;

            return Err(FsalError::QuotaExceeded {
                requested_bytes,
                available_bytes,
            });
        }

        // 4. Write via storage provider
        let storage_handle = self
            .storage_provider
            .open_file(&full_path, OpenMode::WriteOnly)
            .await?;
        let bytes_written = self
            .storage_provider
            .write_at(&storage_handle, offset, data)
            .await?;
        let _ = self.storage_provider.close_file(&storage_handle).await;

        // 5. Publish event
        let duration_ms = start.elapsed().as_millis() as u64;
        self.event_publisher
            .publish_storage_event(StorageEvent::FileWritten {
                execution_id: handle.execution_id().copied(),
                workflow_execution_id: handle.workflow_execution_id(),
                volume_id: handle.volume_id,
                path: path_str.to_string(),
                offset,
                bytes_written: bytes_written as u64,
                duration_ms,
                written_at: Utc::now(),
                caller_node_id: None,
                host_node_id: None,
            })
            .await;

        Ok(bytes_written)
    }

    /// Create a file
    pub async fn create_file(
        &self,
        req: CreateFsalFileRequest<'_>,
    ) -> Result<AegisFileHandle, FsalError> {
        let CreateFsalFileRequest {
            execution_id,
            volume_id,
            path,
            policy,
            emit_event,
            caller_node_id,
            host_node_id,
            workflow_execution_id,
        } = req;

        // 1. Authorize
        let volume = self
            .authorize_inner(Some(execution_id), workflow_execution_id, volume_id)
            .await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_string = canonical.to_str().unwrap().replace("\\", "/");
        let path_str = path_string.as_str();

        // 3. Enforce write policy
        if let Err(e) = self.enforce_write_policy(policy, path_str) {
            self.emit_policy_violation(
                PolicyViolationContext {
                    execution_id: Some(execution_id),
                    workflow_execution_id: None,
                    volume_id,
                    operation: "write",
                    caller_node_id,
                    host_node_id,
                },
                path_str,
                &e,
            )
            .await;
            return Err(e);
        }

        // 4. Build full remote path
        let full_path = self.routed_storage_path(&volume, path_str);

        // 5. Ensure parent directory exists (SeaweedFS does not auto-create parent dirs)
        if let Some(parent) = std::path::Path::new(&full_path).parent() {
            let parent_str = parent.to_string_lossy();
            if !parent_str.is_empty() && parent_str != "/" {
                self.storage_provider
                    .create_directory(&parent_str)
                    .await
                    .map_err(FsalError::Storage)?;
            }
        }

        // 6. Create file via storage provider (using default mode 0o644)
        let handle = self.storage_provider.create_file(&full_path, 0o644).await?;
        let _ = self.storage_provider.close_file(&handle).await; // Close immediately

        // 7. Create Aegis file handle
        let aegis_handle = AegisFileHandle::new(execution_id, volume_id, path_str);
        aegis_handle.validate_size()?;

        // 8. Publish event only when the caller confirms the overall write will not follow
        if emit_event {
            self.event_publisher
                .publish_storage_event(StorageEvent::FileCreated {
                    execution_id: Some(execution_id),
                    workflow_execution_id: None,
                    volume_id,
                    path: path_str.to_string(),
                    created_at: Utc::now(),
                    caller_node_id,
                    host_node_id,
                })
                .await;
        }

        Ok(aegis_handle)
    }

    /// Get file attributes (stat)
    pub async fn getattr(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        container_uid: u32,
        container_gid: u32,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<FileAttributes, FsalError> {
        // 1. Authorize
        let volume = self
            .authorize_inner(Some(execution_id), workflow_execution_id, volume_id)
            .await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_str = canonical.to_str().unwrap();

        // 4. Build full remote path
        let full_path = self.routed_storage_path(&volume, path_str);

        // 5. Get attributes from storage provider
        let mut attrs = self.storage_provider.stat(&full_path).await?;

        // 6. Override UID/GID (permission squashing per ADR-036)
        attrs.uid = container_uid;
        attrs.gid = container_gid;

        Ok(attrs)
    }

    /// List directory contents (readdir)
    #[allow(clippy::too_many_arguments)]
    pub async fn readdir(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        caller_node_id: Option<NodeId>,
        host_node_id: Option<NodeId>,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<Vec<DirEntry>, FsalError> {
        // 1. Authorize
        let volume = self
            .authorize_inner(Some(execution_id), workflow_execution_id, volume_id)
            .await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_str = canonical.to_str().unwrap();

        // 3. Enforce read policy
        if let Err(e) = self.enforce_read_policy(policy, path_str) {
            self.emit_policy_violation(
                PolicyViolationContext {
                    execution_id: Some(execution_id),
                    workflow_execution_id: None,
                    volume_id,
                    operation: "read",
                    caller_node_id,
                    host_node_id,
                },
                path_str,
                &e,
            )
            .await;
            return Err(e);
        }

        // 4. Build full remote path
        let full_path = self.routed_storage_path(&volume, path_str);

        // 5. List directory via storage provider
        let entries = self.storage_provider.readdir(&full_path).await?;

        // 6. Publish event
        self.event_publisher
            .publish_storage_event(StorageEvent::DirectoryListed {
                execution_id: Some(execution_id),
                workflow_execution_id: None,
                volume_id,
                path: path_str.to_string(),
                entry_count: entries.len(),
                listed_at: Utc::now(),
                caller_node_id,
                host_node_id,
            })
            .await;

        Ok(entries)
    }

    /// Create a directory
    #[allow(clippy::too_many_arguments)]
    pub async fn create_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        caller_node_id: Option<NodeId>,
        host_node_id: Option<NodeId>,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError> {
        // 1. Authorize
        let volume = self
            .authorize_inner(Some(execution_id), workflow_execution_id, volume_id)
            .await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_str = canonical.to_str().unwrap();

        // 3. Enforce write policy (directory creation is a write operation)
        if let Err(e) = self.enforce_write_policy(policy, path_str) {
            self.emit_policy_violation(
                PolicyViolationContext {
                    execution_id: Some(execution_id),
                    workflow_execution_id: None,
                    volume_id,
                    operation: "write",
                    caller_node_id,
                    host_node_id,
                },
                path_str,
                &e,
            )
            .await;
            return Err(e);
        }

        // 4. Build full remote path
        let full_path = self.routed_storage_path(&volume, path_str);

        // 5. Create directory via storage provider
        self.storage_provider.create_directory(&full_path).await?;

        // 6. Publish event
        self.event_publisher
            .publish_storage_event(StorageEvent::FileCreated {
                execution_id: Some(execution_id),
                workflow_execution_id: None,
                volume_id,
                path: path_str.to_string(),
                created_at: Utc::now(),
                caller_node_id,
                host_node_id,
            })
            .await;

        Ok(())
    }

    /// Delete a file
    #[allow(clippy::too_many_arguments)]
    pub async fn delete_file(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        caller_node_id: Option<NodeId>,
        host_node_id: Option<NodeId>,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError> {
        // 1. Authorize
        let volume = self
            .authorize_inner(Some(execution_id), workflow_execution_id, volume_id)
            .await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_str = canonical.to_str().unwrap();

        // 3. Enforce write policy
        if let Err(e) = self.enforce_write_policy(policy, path_str) {
            self.emit_policy_violation(
                PolicyViolationContext {
                    execution_id: Some(execution_id),
                    workflow_execution_id: None,
                    volume_id,
                    operation: "delete",
                    caller_node_id,
                    host_node_id,
                },
                path_str,
                &e,
            )
            .await;
            return Err(e);
        }

        // 4. Build full remote path
        let full_path = self.routed_storage_path(&volume, path_str);

        // 5. Delete file via storage provider
        self.storage_provider.delete_file(&full_path).await?;

        // 6. Publish event
        self.event_publisher
            .publish_storage_event(StorageEvent::FileDeleted {
                execution_id: Some(execution_id),
                workflow_execution_id: None,
                volume_id,
                path: path_str.to_string(),
                deleted_at: Utc::now(),
                caller_node_id,
                host_node_id,
            })
            .await;

        Ok(())
    }

    /// Delete a directory
    #[allow(clippy::too_many_arguments)]
    pub async fn delete_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        caller_node_id: Option<NodeId>,
        host_node_id: Option<NodeId>,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError> {
        // 1. Authorize
        let volume = self
            .authorize_inner(Some(execution_id), workflow_execution_id, volume_id)
            .await?;

        // 2. Sanitize path
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_str = canonical.to_str().unwrap();

        // 3. Enforce write policy
        if let Err(e) = self.enforce_write_policy(policy, path_str) {
            self.emit_policy_violation(
                PolicyViolationContext {
                    execution_id: Some(execution_id),
                    workflow_execution_id: None,
                    volume_id,
                    operation: "delete",
                    caller_node_id,
                    host_node_id,
                },
                path_str,
                &e,
            )
            .await;
            return Err(e);
        }

        // 4. Build full remote path
        let full_path = self.routed_storage_path(&volume, path_str);

        // 5. Delete directory via storage provider
        self.storage_provider.delete_directory(&full_path).await?;

        // 6. Publish event
        self.event_publisher
            .publish_storage_event(StorageEvent::FileDeleted {
                execution_id: Some(execution_id),
                workflow_execution_id: None,
                volume_id,
                path: path_str.to_string(),
                deleted_at: Utc::now(),
                caller_node_id,
                host_node_id,
            })
            .await;

        Ok(())
    }

    /// Rename a file or directory
    pub async fn rename(&self, req: RenameFsalRequest<'_>) -> Result<(), FsalError> {
        let RenameFsalRequest {
            execution_id,
            volume_id,
            from_path,
            to_path,
            policy,
            caller_node_id,
            host_node_id,
            workflow_execution_id,
        } = req;

        // 1. Authorize
        let volume = self
            .authorize_inner(Some(execution_id), workflow_execution_id, volume_id)
            .await?;

        // 2. Sanitize both paths
        let from_canonical = self.path_sanitizer.canonicalize(from_path, Some("/"))?;
        let to_canonical = self.path_sanitizer.canonicalize(to_path, Some("/"))?;
        let from_str = from_canonical.to_str().unwrap();
        let to_str = to_canonical.to_str().unwrap();

        // 3. Enforce write policy for both paths
        if let Err(e) = self.enforce_write_policy(policy, from_str) {
            self.emit_policy_violation(
                PolicyViolationContext {
                    execution_id: Some(execution_id),
                    workflow_execution_id: None,
                    volume_id,
                    operation: "write",
                    caller_node_id,
                    host_node_id,
                },
                from_str,
                &e,
            )
            .await;
            return Err(e);
        }
        if let Err(e) = self.enforce_write_policy(policy, to_str) {
            self.emit_policy_violation(
                PolicyViolationContext {
                    execution_id: Some(execution_id),
                    workflow_execution_id: None,
                    volume_id,
                    operation: "write",
                    caller_node_id,
                    host_node_id,
                },
                to_str,
                &e,
            )
            .await;
            return Err(e);
        }

        // 4. Build full remote paths
        let from_full = self.routed_storage_path(&volume, from_str);
        let to_full = self.routed_storage_path(&volume, to_str);

        // 5. Rename via storage provider
        self.storage_provider.rename(&from_full, &to_full).await?;

        // 6. Publish event (reuse FileCreated for rename target)
        self.event_publisher
            .publish_storage_event(StorageEvent::FileCreated {
                execution_id: Some(execution_id),
                workflow_execution_id: None,
                volume_id,
                path: to_str.to_string(),
                created_at: Utc::now(),
                caller_node_id,
                host_node_id,
            })
            .await;

        Ok(())
    }

    // ── Node-to-node FSAL methods (ADR-064 remote volume support) ────────

    /// Open a file with path sanitization, volume authorization, and event
    /// emission for cross-node (SEAL) operations.
    pub async fn open_file_for_node(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        mode: OpenMode,
        caller_node_id: Option<NodeId>,
        host_node_id: Option<NodeId>,
    ) -> Result<FileHandle, FsalError> {
        // 1. Authorize
        let volume = self.authorize(execution_id, volume_id).await?;

        // 2. Sanitize path
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_string = canonical.to_str().unwrap().replace('\\', "/");

        // 3. Route to storage backend
        let storage_path = self.routed_storage_path(&volume, &path_string);
        let handle = self.storage_provider.open_file(&storage_path, mode).await?;

        // 4. Emit event
        self.event_publisher
            .publish_storage_event(StorageEvent::FileOpened {
                execution_id: Some(execution_id),
                workflow_execution_id: None,
                volume_id,
                path: path_string,
                open_mode: format!("{mode:?}"),
                opened_at: Utc::now(),
                caller_node_id,
                host_node_id,
            })
            .await;

        Ok(handle)
    }

    /// Read from an open file handle with event emission for cross-node
    /// (SEAL) operations.
    ///
    /// Unlike [`Self::read`], this operates on an already-open
    /// [`FileHandle`] rather than opening/closing per call.
    pub async fn read_for_node(
        &self,
        handle: &FileHandle,
        req: NodeStorageRequest<'_>,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FsalError> {
        let NodeStorageRequest {
            execution_id,
            volume_id,
            path,
            caller_node_id,
            host_node_id,
        } = req;
        let start = std::time::Instant::now();

        let data = self
            .storage_provider
            .read_at(handle, offset, length)
            .await?;

        let duration_ms = start.elapsed().as_millis() as u64;
        self.event_publisher
            .publish_storage_event(StorageEvent::FileRead {
                execution_id: Some(execution_id),
                workflow_execution_id: None,
                volume_id,
                path: path.to_string(),
                offset,
                bytes_read: data.len() as u64,
                duration_ms,
                read_at: Utc::now(),
                caller_node_id,
                host_node_id,
            })
            .await;

        Ok(data)
    }

    /// Write to an open file handle with quota enforcement and event
    /// emission for cross-node (SEAL) operations.
    ///
    /// Unlike [`Self::write`], this operates on an already-open
    /// [`FileHandle`] rather than opening/closing per call.
    pub async fn write_for_node(
        &self,
        handle: &FileHandle,
        req: NodeStorageRequest<'_>,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, FsalError> {
        let NodeStorageRequest {
            execution_id,
            volume_id,
            path,
            caller_node_id,
            host_node_id,
        } = req;
        let start = std::time::Instant::now();

        // Quota enforcement
        let volume = self.authorize(execution_id, volume_id).await?;
        let usage_path = self.routed_usage_path(&volume);
        let current_usage = self.storage_provider.get_usage(&usage_path).await?;
        let requested_bytes = data.len() as u64;
        let projected_usage = current_usage.saturating_add(requested_bytes);

        if projected_usage > volume.size_limit_bytes {
            let available_bytes = volume.size_limit_bytes.saturating_sub(current_usage);
            self.event_publisher
                .publish_storage_event(StorageEvent::QuotaExceeded {
                    execution_id: Some(execution_id),
                    workflow_execution_id: None,
                    volume_id,
                    requested_bytes,
                    available_bytes,
                    exceeded_at: Utc::now(),
                    caller_node_id,
                    host_node_id,
                })
                .await;
            return Err(FsalError::QuotaExceeded {
                requested_bytes,
                available_bytes,
            });
        }

        let bytes_written = self.storage_provider.write_at(handle, offset, data).await?;

        let duration_ms = start.elapsed().as_millis() as u64;
        self.event_publisher
            .publish_storage_event(StorageEvent::FileWritten {
                execution_id: Some(execution_id),
                workflow_execution_id: None,
                volume_id,
                path: path.to_string(),
                offset,
                bytes_written: bytes_written as u64,
                duration_ms,
                written_at: Utc::now(),
                caller_node_id,
                host_node_id,
            })
            .await;

        Ok(bytes_written)
    }

    /// Close a file handle with event emission for cross-node (SEAL)
    /// operations.
    pub async fn close_for_node(
        &self,
        handle: &FileHandle,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        caller_node_id: Option<NodeId>,
        host_node_id: Option<NodeId>,
    ) -> Result<(), FsalError> {
        self.storage_provider.close_file(handle).await?;

        self.event_publisher
            .publish_storage_event(StorageEvent::FileClosed {
                execution_id: Some(execution_id),
                workflow_execution_id: None,
                volume_id,
                path: path.to_string(),
                closed_at: Utc::now(),
                caller_node_id,
                host_node_id,
            })
            .await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test: `uuid::Uuid`'s serde impl serializes as a 16-element u8 sequence;
    /// bincode adds an 8-byte length prefix per sequence. With `created_at` removed, the
    /// layout is: enum tag (4B) + UUID length+data (8+16=24B) + volume_id length+data (8+16=24B)
    /// + path_hash u64 (8B) = 60 bytes, well within the NFSv3 64-byte limit.
    #[test]
    fn test_aegis_file_handle_serializes_to_60_bytes() {
        let handle = AegisFileHandle::new(
            ExecutionId::new(),
            VolumeId::new(),
            "/workspace/test/file.txt",
        );

        let bytes = handle.to_bytes().unwrap();
        assert_eq!(
            bytes.len(),
            60,
            "AegisFileHandle must serialize to exactly 60 bytes; got {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn test_aegis_file_handle_size() {
        let handle = AegisFileHandle::new(
            ExecutionId::new(),
            VolumeId::new(),
            "/workspace/test/file.txt",
        );

        let bytes = handle.to_bytes().unwrap();
        // Verify NFSv3 64-byte limit per ADR-036
        assert!(
            bytes.len() <= 64,
            "FileHandle size {} bytes exceeds NFSv3 64-byte limit",
            bytes.len()
        );
    }

    #[test]
    fn test_aegis_file_handle_roundtrip() {
        let original =
            AegisFileHandle::new(ExecutionId::new(), VolumeId::new(), "/workspace/test.txt");

        let bytes = original.to_bytes().unwrap();
        let decoded = AegisFileHandle::from_bytes(&bytes).unwrap();

        assert_eq!(original, decoded);
    }

    #[test]
    fn test_aegis_file_handle_workflow_size() {
        // Workflow execution handles must also fit within the NFSv3 64-byte limit
        let handle = AegisFileHandle::new_for_workflow(
            uuid::Uuid::new_v4(),
            VolumeId::new(),
            "/workspace/test/file.txt",
        );

        let bytes = handle.to_bytes().unwrap();
        assert!(
            bytes.len() <= 64,
            "Workflow FileHandle size {} bytes exceeds NFSv3 64-byte limit",
            bytes.len()
        );
    }

    #[test]
    fn test_aegis_file_handle_workflow_roundtrip() {
        let original = AegisFileHandle::new_for_workflow(
            uuid::Uuid::new_v4(),
            VolumeId::new(),
            "/workspace/test.txt",
        );

        let bytes = original.to_bytes().unwrap();
        let decoded = AegisFileHandle::from_bytes(&bytes).unwrap();

        assert_eq!(original, decoded);
    }

    // ── authorize_for_user tests ───────────────────────────────────────────

    use crate::domain::events::StorageEvent;
    use crate::domain::storage::{
        DirEntry as SDirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider,
    };
    use crate::infrastructure::repositories::InMemoryVolumeRepository;
    use std::sync::Arc;

    struct NoopStorage;

    #[async_trait::async_trait]
    impl StorageProvider for NoopStorage {
        async fn create_directory(&self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn delete_directory(&self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn set_quota(&self, _: &str, _: u64) -> Result<(), StorageError> {
            Ok(())
        }
        async fn get_usage(&self, _: &str) -> Result<u64, StorageError> {
            Ok(0)
        }
        async fn health_check(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn open_file(&self, _: &str, _: OpenMode) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(vec![]))
        }
        async fn read_at(&self, _: &FileHandle, _: u64, _: usize) -> Result<Vec<u8>, StorageError> {
            Ok(vec![])
        }
        async fn write_at(
            &self,
            _: &FileHandle,
            _: u64,
            data: &[u8],
        ) -> Result<usize, StorageError> {
            Ok(data.len())
        }
        async fn close_file(&self, _: &FileHandle) -> Result<(), StorageError> {
            Ok(())
        }
        async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
            Err(StorageError::FileNotFound(path.to_string()))
        }
        async fn readdir(&self, _: &str) -> Result<Vec<SDirEntry>, StorageError> {
            Ok(vec![])
        }
        async fn create_file(&self, _: &str, _: u32) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(vec![]))
        }
        async fn delete_file(&self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn rename(&self, _: &str, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
    }

    struct NoopPublisher;

    #[async_trait::async_trait]
    impl EventPublisher for NoopPublisher {
        async fn publish_storage_event(&self, _: StorageEvent) {}
    }

    fn make_available_persistent_volume(owner: &str) -> crate::domain::volume::Volume {
        use crate::domain::volume::{FilerEndpoint, StorageClass, VolumeBackend, VolumeOwnership};
        use chrono::Utc;
        crate::domain::volume::Volume {
            id: VolumeId::new(),
            name: "test-vol".to_string(),
            tenant_id: crate::domain::volume::TenantId::consumer(),
            storage_class: StorageClass::persistent(),
            backend: VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
                remote_path: "/aegis/volumes/test/v1".to_string(),
            },
            size_limit_bytes: 1_000_000,
            status: VolumeStatus::Available,
            ownership: VolumeOwnership::persistent(owner),
            created_at: Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        }
    }

    async fn make_fsal_with_volume(vol: &crate::domain::volume::Volume) -> AegisFSAL {
        use parking_lot::RwLock;
        use std::collections::HashMap;

        let repo = Arc::new(InMemoryVolumeRepository::new());
        repo.save(vol).await.unwrap();

        AegisFSAL::new(
            Arc::new(NoopStorage),
            repo as Arc<dyn crate::domain::repository::VolumeRepository>,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(NoopPublisher),
        )
    }

    #[tokio::test]
    async fn authorize_for_user_correct_owner_passes() {
        let vol = make_available_persistent_volume("alice");
        let fsal = make_fsal_with_volume(&vol).await;
        let result = fsal.authorize_for_user("alice", &vol.id).await;
        assert!(result.is_ok(), "correct owner should pass: {:?}", result);
    }

    #[tokio::test]
    async fn authorize_for_user_wrong_owner_fails() {
        let vol = make_available_persistent_volume("alice");
        let fsal = make_fsal_with_volume(&vol).await;
        let result = fsal.authorize_for_user("bob", &vol.id).await;
        assert!(
            matches!(result, Err(FsalError::UnauthorizedAccess { .. })),
            "wrong owner should fail: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn authorize_for_user_workflow_ownership_fails() {
        use crate::domain::volume::{FilerEndpoint, StorageClass, VolumeBackend, VolumeOwnership};
        use chrono::Utc;
        use parking_lot::RwLock;
        use std::collections::HashMap;
        use uuid::Uuid;

        let vol = crate::domain::volume::Volume {
            id: VolumeId::new(),
            name: "wf-vol".to_string(),
            tenant_id: crate::domain::volume::TenantId::consumer(),
            storage_class: StorageClass::persistent(),
            backend: VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
                remote_path: "/aegis/volumes/test/wfv".to_string(),
            },
            size_limit_bytes: 1_000_000,
            status: VolumeStatus::Available,
            ownership: VolumeOwnership::WorkflowExecution {
                workflow_execution_id: Uuid::new_v4(),
            },
            created_at: Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        };

        let repo = Arc::new(InMemoryVolumeRepository::new());
        repo.save(&vol).await.unwrap();

        let fsal = AegisFSAL::new(
            Arc::new(NoopStorage),
            repo as Arc<dyn crate::domain::repository::VolumeRepository>,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(NoopPublisher),
        );

        let result = fsal.authorize_for_user("alice", &vol.id).await;
        assert!(
            matches!(result, Err(FsalError::UnauthorizedAccess { .. })),
            "workflow ownership should fail user auth: {:?}",
            result
        );
    }

    // ── VolumeOwnership::WorkflowExecution authorize tests ────────────────

    /// Stub implementation of VolumeContextLookup that returns a fixed
    /// workflow_execution_id for any volume.
    struct StubVolumeContextLookup {
        wf_id: Option<uuid::Uuid>,
    }

    impl VolumeContextLookup for StubVolumeContextLookup {
        fn lookup_workflow_execution_id(&self, _volume_id: VolumeId) -> Option<uuid::Uuid> {
            self.wf_id
        }
    }

    fn make_workflow_volume(wf_id: uuid::Uuid) -> crate::domain::volume::Volume {
        use crate::domain::volume::{FilerEndpoint, StorageClass, VolumeBackend, VolumeOwnership};
        use chrono::Utc;
        crate::domain::volume::Volume {
            id: VolumeId::new(),
            name: "wf-workspace".to_string(),
            tenant_id: crate::domain::volume::TenantId::consumer(),
            storage_class: StorageClass::persistent(),
            backend: VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
                remote_path: "/aegis/volumes/test/wf".to_string(),
            },
            size_limit_bytes: 1_000_000,
            status: VolumeStatus::Available,
            ownership: VolumeOwnership::WorkflowExecution {
                workflow_execution_id: wf_id,
            },
            created_at: Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        }
    }

    /// Regression: FSAL must authorize an execution accessing a volume with
    /// WorkflowExecution ownership when the volume context lookup reports
    /// a matching workflow_execution_id.
    #[tokio::test]
    async fn test_fsal_authorizes_workflow_execution_ownership() {
        use parking_lot::RwLock;
        use std::collections::HashMap;

        let wf_id = uuid::Uuid::new_v4();
        let vol = make_workflow_volume(wf_id);
        let vol_id = vol.id;

        let repo = Arc::new(InMemoryVolumeRepository::new());
        repo.save(&vol).await.unwrap();

        let lookup = Arc::new(StubVolumeContextLookup { wf_id: Some(wf_id) });
        let fsal = AegisFSAL::new(
            Arc::new(NoopStorage),
            repo as Arc<dyn crate::domain::repository::VolumeRepository>,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(NoopPublisher),
        )
        .with_volume_context_lookup(lookup);

        // Any execution_id should be authorized as long as the volume context
        // lookup returns a matching workflow_execution_id.
        let exec_id = ExecutionId::new();
        let result = fsal.authorize(exec_id, vol_id).await;
        assert!(
            result.is_ok(),
            "execution should be authorized for WorkflowExecution volume \
             when volume context lookup matches: {:?}",
            result
        );
    }

    /// Regression: FSAL must deny an execution accessing a volume with
    /// WorkflowExecution ownership when the volume context lookup reports
    /// a different workflow_execution_id.
    #[tokio::test]
    async fn test_fsal_denies_workflow_execution_ownership_mismatch() {
        use parking_lot::RwLock;
        use std::collections::HashMap;

        let wf_id = uuid::Uuid::new_v4();
        let vol = make_workflow_volume(wf_id);
        let vol_id = vol.id;

        let repo = Arc::new(InMemoryVolumeRepository::new());
        repo.save(&vol).await.unwrap();

        // Volume context reports a *different* workflow_execution_id
        let wrong_wf_id = uuid::Uuid::new_v4();
        let lookup = Arc::new(StubVolumeContextLookup {
            wf_id: Some(wrong_wf_id),
        });
        let fsal = AegisFSAL::new(
            Arc::new(NoopStorage),
            repo as Arc<dyn crate::domain::repository::VolumeRepository>,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(NoopPublisher),
        )
        .with_volume_context_lookup(lookup);

        let exec_id = ExecutionId::new();
        let result = fsal.authorize(exec_id, vol_id).await;
        assert!(
            matches!(result, Err(FsalError::UnauthorizedAccess { .. })),
            "mismatched workflow_execution_id should deny access: {:?}",
            result
        );
    }

    /// Regression: FSAL must deny when no volume context lookup is configured
    /// and the volume has WorkflowExecution ownership (the old behavior was
    /// `_ => false` which denied all workflow volumes).
    #[tokio::test]
    async fn test_fsal_denies_workflow_execution_without_lookup() {
        use parking_lot::RwLock;
        use std::collections::HashMap;

        let wf_id = uuid::Uuid::new_v4();
        let vol = make_workflow_volume(wf_id);
        let vol_id = vol.id;

        let repo = Arc::new(InMemoryVolumeRepository::new());
        repo.save(&vol).await.unwrap();

        // No volume context lookup configured
        let fsal = AegisFSAL::new(
            Arc::new(NoopStorage),
            repo as Arc<dyn crate::domain::repository::VolumeRepository>,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(NoopPublisher),
        );

        let exec_id = ExecutionId::new();
        let result = fsal.authorize(exec_id, vol_id).await;
        assert!(
            matches!(result, Err(FsalError::UnauthorizedAccess { .. })),
            "should deny without volume context lookup: {:?}",
            result
        );
    }

    /// Regression: getattr with workflow_execution_id set should authorize
    /// against a WorkflowExecution-owned volume via authorize_inner, not
    /// via the agent-only authorize() path that always passes None.
    /// This was the root cause of the FUSE daemon failing to access
    /// workflow-owned volumes through the FsalService gRPC RPCs.
    #[tokio::test]
    async fn test_getattr_with_workflow_execution_id_authorizes_workflow_volume() {
        use parking_lot::RwLock;
        use std::collections::HashMap;

        let wf_id = uuid::Uuid::new_v4();
        let vol = make_workflow_volume(wf_id);
        let vol_id = vol.id;

        let repo = Arc::new(InMemoryVolumeRepository::new());
        repo.save(&vol).await.unwrap();

        // No VolumeContextLookup needed — the workflow_execution_id is passed
        // directly through the getattr call, so authorize_inner gets it
        // without needing the NFS registry fallback.
        let fsal = AegisFSAL::new(
            Arc::new(NoopStorage),
            repo as Arc<dyn crate::domain::repository::VolumeRepository>,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(NoopPublisher),
        );

        // Without workflow_execution_id, this should fail (agent path).
        let exec_id = ExecutionId::new();
        let result = fsal.getattr(exec_id, vol_id, "/", 1000, 1000, None).await;
        assert!(
            matches!(result, Err(FsalError::UnauthorizedAccess { .. })),
            "getattr without workflow_execution_id should deny: {:?}",
            result
        );

        // With the correct workflow_execution_id, this should succeed.
        let result = fsal
            .getattr(exec_id, vol_id, "/", 1000, 1000, Some(wf_id))
            .await;
        // NoopStorage returns FileNotFound for stat, but we get past authorization.
        // The error proves authorize_inner succeeded — it's a Storage error, not Unauthorized.
        assert!(
            matches!(result, Err(FsalError::Storage(_))),
            "getattr with correct workflow_execution_id should pass authorization \
             (Storage error expected from NoopStorage stat): {:?}",
            result
        );
    }
}
