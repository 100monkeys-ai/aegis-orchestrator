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
    execution::ExecutionId,
    volume::{VolumeId, Volume, VolumeStatus},
    repository::VolumeRepository,
    storage::{StorageProvider, FileAttributes, DirEntry, OpenMode, StorageError},
    events::StorageEvent,
    policy::FilesystemPolicy,
    path_sanitizer::{PathSanitizer, PathSanitizerError},
};
use async_trait::async_trait;
use chrono::Utc;
use std::sync::Arc;
use thiserror::Error;
use serde::{Serialize, Deserialize};


/// AegisFSAL errors
#[derive(Debug, Error)]
pub enum FsalError {
    #[error("Unauthorized volume access: execution {execution_id} does not own volume {volume_id}")]
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

    #[error("Volume quota exceeded: requested {requested_bytes} bytes, available {available_bytes} bytes")]
    QuotaExceeded {
        requested_bytes: u64,
        available_bytes: u64,
    },
}

/// Aegis File Handle - encodes execution and volume ownership
///
/// Serialized with bincode to fit within NFSv3's 64-byte limit.
/// Current size: 48 bytes raw + ~4 bytes bincode overhead = 52 bytes (safe)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct AegisFileHandle {
    /// Execution that opened this file
    pub execution_id: ExecutionId,
    /// Volume containing this file
    pub volume_id: VolumeId,
    /// Hash of file path (for integrity check)
    pub path_hash: u64,
    /// Handle creation timestamp (Unix timestamp)
    pub created_at: i64,
}

impl AegisFileHandle {
    /// Create a new file handle
    pub fn new(execution_id: ExecutionId, volume_id: VolumeId, path: &str) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        let path_hash = hasher.finish();

        Self {
            execution_id,
            volume_id,
            path_hash,
            created_at: Utc::now().timestamp(),
        }
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

/// AegisFSAL - File System Abstraction Layer
///
/// Domain entity that enforces security policies and provides audit trail
/// for all file operations. Transport-agnostic: works with NFS, virtio-fs, etc.
pub struct AegisFSAL {
    /// Storage backend (SeaweedFS, Local, etc.)
    storage_provider: Arc<dyn StorageProvider>,
    /// Volume repository for ownership validation
    volume_repository: Arc<dyn VolumeRepository>,
    /// Path sanitizer for traversal prevention
    path_sanitizer: PathSanitizer,
    /// Event publisher (injected, not owned)
    event_publisher: Arc<dyn EventPublisher>,
}

/// Event publisher trait (abstraction for event bus)
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish_storage_event(&self, event: StorageEvent);
}

impl AegisFSAL {
    /// Create a new FSAL instance
    pub fn new(
        storage_provider: Arc<dyn StorageProvider>,
        volume_repository: Arc<dyn VolumeRepository>,
        event_publisher: Arc<dyn EventPublisher>,
    ) -> Self {
        Self {
            storage_provider,
            volume_repository,
            path_sanitizer: PathSanitizer::new(),
            event_publisher,
        }
    }

    /// Validate that execution owns the volume
    async fn authorize(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
    ) -> Result<Volume, FsalError> {
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
            crate::domain::volume::VolumeOwnership::Execution { execution_id: exec_id } => *exec_id == execution_id,
            _ => false, // WorkflowExecution or Persistent volumes require different auth
        };

        if !is_owner {
            self.event_publisher
                .publish_storage_event(StorageEvent::UnauthorizedVolumeAccess {
                    execution_id,
                    volume_id,
                    attempted_at: Utc::now(),
                })
                .await;

            return Err(FsalError::UnauthorizedAccess {
                execution_id,
                volume_id,
            });
        }

        Ok(volume)
    }

    /// Enforce filesystem policy for read operation
    fn enforce_read_policy(
        &self,
        policy: &FilesystemPolicy,
        path: &str,
    ) -> Result<(), FsalError> {
        // Check if path matches any read allowlist pattern
        let allowed = policy.read.iter().any(|pattern| {
            // Wildcard matching: support both "/path/*" (single level) and "/path/**" (recursive)
            if pattern.ends_with("/**") {
                // Recursive glob pattern (/workspace/** matches /workspace and all nested)
                let prefix = &pattern[..pattern.len() - 3]; // Remove "/**"
                path.starts_with(prefix) && (path == prefix || path.starts_with(&format!("{}/", prefix)))
            } else if pattern.ends_with("/*") {
                // Single-level glob pattern (/workspace/* matches /workspace/file but not /workspace/dir/file)
                let prefix = &pattern[..pattern.len() - 2];
                path.starts_with(prefix)
                    && path.as_bytes().get(prefix.len()) == Some(&b'/')
            } else {
                // Exact match
                path == pattern
            }
        });

        if !allowed {
            return Err(FsalError::PolicyViolation(format!(
                "Read not allowed for path: {}",
                path
            )));
        }

        Ok(())
    }

    /// Enforce filesystem policy for write operation
    fn enforce_write_policy(
        &self,
        policy: &FilesystemPolicy,
        path: &str,
    ) -> Result<(), FsalError> {
        // Check if path matches any write allowlist pattern
        let allowed = policy.write.iter().any(|pattern| {
            // Wildcard matching: support both "/path/*" (single level) and "/path/**" (recursive)
            if pattern.ends_with("/**") {
                // Recursive glob pattern (/workspace/** matches /workspace and all nested)
                let prefix = &pattern[..pattern.len() - 3]; // Remove "/**"
                path.starts_with(prefix) && (path == prefix || path.starts_with(&format!("{}/", prefix)))
            } else if pattern.ends_with("/*") {
                // Single-level glob pattern (/workspace/* matches /workspace/file but not /workspace/dir/file)
                let prefix = &pattern[..pattern.len() - 2];
                path.starts_with(prefix)
                    && path.as_bytes().get(prefix.len()) == Some(&b'/')
            } else {
                // Exact match
                path == pattern
            }
        });

        if !allowed {
            return Err(FsalError::PolicyViolation(format!(
                "Write not allowed for path: {}",
                path
            )));
        }

        Ok(())
    }

    /// Lookup a file/directory (NFS LOOKUP operation)
    pub async fn lookup(
        &self,
        handle: &AegisFileHandle,
        parent_path: &str,
        name: &str,
    ) -> Result<AegisFileHandle, FsalError> {
        // 1. Authorize
        let _volume = self
            .authorize(handle.execution_id, handle.volume_id)
            .await?;

        // 2. Build child path
        let child_path = if parent_path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", parent_path, name)
        };

        // 3. Sanitize path
        let canonical = self
            .path_sanitizer
            .canonicalize(&child_path, Some(parent_path))?;

        let canonical_str = canonical.to_str().unwrap().replace("\\", "/");

        // 4. Create new handle
        let new_handle = AegisFileHandle::new(
            handle.execution_id,
            handle.volume_id,
            &canonical_str,
        );

        Ok(new_handle)
    }

    /// Read from file at offset
    pub async fn read(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FilesystemPolicy,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, FsalError> {
        let start = std::time::Instant::now();

        // 1. Authorize
        let volume = self.authorize(handle.execution_id, handle.volume_id).await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_string = canonical.to_str().unwrap().replace("\\", "/");
        let path_str = path_string.as_str();
        self.enforce_read_policy(policy, path_str)?;
        let full_path = format!("{}/{}", volume.remote_path, path_str.trim_start_matches('/'));

        // 3. Read via storage provider
        let storage_handle = self.storage_provider.open_file(&full_path, OpenMode::ReadOnly).await?;
        let data = self
            .storage_provider
            .read_at(&storage_handle, offset, length)
            .await?;
        let _ = self.storage_provider.close_file(&storage_handle).await;

        // 4. Publish event
        let duration_ms = start.elapsed().as_millis() as u64;
        self.event_publisher
            .publish_storage_event(StorageEvent::FileRead {
                execution_id: handle.execution_id,
                volume_id: handle.volume_id,
                path: path_str.to_string(),
                offset,
                bytes_read: data.len() as u64,
                duration_ms,
                read_at: Utc::now(),
            })
            .await;

        Ok(data)
    }

    /// Write to file at offset
    pub async fn write(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FilesystemPolicy,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, FsalError> {
        let start = std::time::Instant::now();

        // 1. Authorize and get volume for quota checking
        let volume = self.authorize(handle.execution_id, handle.volume_id).await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_string = canonical.to_str().unwrap().replace("\\", "/");
        let path_str = path_string.as_str();
        self.enforce_write_policy(policy, path_str)?;
        let full_path = format!("{}/{}", volume.remote_path, path_str.trim_start_matches('/'));

        // 3. Proactive quota enforcement (ADR-036)
        // Check if write would exceed volume quota before attempting write
        let current_usage = self.storage_provider.get_usage(&volume.remote_path).await?;
        let requested_bytes = data.len() as u64;
        let projected_usage = current_usage.saturating_add(requested_bytes);
        
        if projected_usage > volume.size_limit_bytes {
            let available_bytes = volume.size_limit_bytes.saturating_sub(current_usage);
            
            // Publish quota exceeded event
            self.event_publisher
                .publish_storage_event(StorageEvent::QuotaExceeded {
                    execution_id: handle.execution_id,
                    volume_id: handle.volume_id,
                    requested_bytes,
                    available_bytes,
                    exceeded_at: Utc::now(),
                })
                .await;
            
            return Err(FsalError::QuotaExceeded {
                requested_bytes,
                available_bytes,
            });
        }

        // 4. Write via storage provider
        let storage_handle = self.storage_provider.open_file(&full_path, OpenMode::WriteOnly).await?;
        let bytes_written = self
            .storage_provider
            .write_at(&storage_handle, offset, data)
            .await?;
        let _ = self.storage_provider.close_file(&storage_handle).await;

        // 5. Publish event
        let duration_ms = start.elapsed().as_millis() as u64;
        self.event_publisher
            .publish_storage_event(StorageEvent::FileWritten {
                execution_id: handle.execution_id,
                volume_id: handle.volume_id,
                path: path_str.to_string(),
                offset,
                bytes_written: bytes_written as u64,
                duration_ms,
                written_at: Utc::now(),
            })
            .await;

        Ok(bytes_written)
    }

    /// Create a file
    pub async fn create_file(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FilesystemPolicy,
    ) -> Result<AegisFileHandle, FsalError> {
        // 1. Authorize
        let volume = self.authorize(execution_id, volume_id).await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_string = canonical.to_str().unwrap().replace("\\", "/");
        let path_str = path_string.as_str();

        // 3. Enforce write policy
        self.enforce_write_policy(policy, path_str)?;

        // 4. Build full remote path
        let full_path = format!("{}/{}", volume.remote_path, path_str.trim_start_matches('/'));

        // 5. Create file via storage provider (using default mode 0o644)
        let handle = self.storage_provider.create_file(&full_path, 0o644).await?;
        let _ = self.storage_provider.close_file(&handle).await; // Close immediately

        // 6. Create Aegis file handle
        let aegis_handle = AegisFileHandle::new(execution_id, volume_id, path_str);
        aegis_handle.validate_size()?;

        // 7. Publish event
        self.event_publisher
            .publish_storage_event(StorageEvent::FileCreated {
                execution_id,
                volume_id,
                path: path_str.to_string(),
                created_at: Utc::now(),
            })
            .await;

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
    ) -> Result<FileAttributes, FsalError> {
        // 1. Authorize
        let volume = self.authorize(execution_id, volume_id).await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_str = canonical.to_str().unwrap();

        // 4. Build full remote path
        let full_path = format!("{}/{}", volume.remote_path, path_str.trim_start_matches('/'));

        // 5. Get attributes from storage provider
        let mut attrs = self.storage_provider.stat(&full_path).await?;

        // 6. Override UID/GID (permission squashing per ADR-036)
        attrs.uid = container_uid;
        attrs.gid = container_gid;

        Ok(attrs)
    }

    /// List directory contents (readdir)
    pub async fn readdir(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FilesystemPolicy,
    ) -> Result<Vec<DirEntry>, FsalError> {
        // 1. Authorize
        let volume = self.authorize(execution_id, volume_id).await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_str = canonical.to_str().unwrap();

        // 3. Enforce read policy
        self.enforce_read_policy(policy, path_str)?;

        // 4. Build full remote path
        let full_path = format!("{}/{}", volume.remote_path, path_str.trim_start_matches('/'));

        // 5. List directory via storage provider
        let entries = self.storage_provider.readdir(&full_path).await?;

        // 6. Publish event
        self.event_publisher
            .publish_storage_event(StorageEvent::DirectoryListed {
                execution_id,
                volume_id,
                path: path_str.to_string(),
                entry_count: entries.len(),
                listed_at: Utc::now(),
            })
            .await;

        Ok(entries)
    }

    /// Create a directory
    pub async fn create_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FilesystemPolicy,
    ) -> Result<(), FsalError> {
        // 1. Authorize
        let volume = self.authorize(execution_id, volume_id).await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_str = canonical.to_str().unwrap();

        // 3. Enforce write policy (directory creation is a write operation)
        self.enforce_write_policy(policy, path_str)?;

        // 4. Build full remote path
        let full_path = format!("{}/{}", volume.remote_path, path_str.trim_start_matches('/'));

        // 5. Create directory via storage provider
        self.storage_provider.create_directory(&full_path).await?;

        // 6. Publish event
        self.event_publisher
            .publish_storage_event(StorageEvent::FileCreated {
                execution_id,
                volume_id,
                path: path_str.to_string(),
                created_at: Utc::now(),
            })
            .await;

        Ok(())
    }

    /// Delete a file
    pub async fn delete_file(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FilesystemPolicy,
    ) -> Result<(), FsalError> {
        // 1. Authorize
        let volume = self.authorize(execution_id, volume_id).await?;

        // 2. Sanitize path — NFS paths are volume-local (root = "/")
        let canonical = self.path_sanitizer.canonicalize(path, Some("/"))?;
        let path_str = canonical.to_str().unwrap();

        // 3. Enforce write policy
        self.enforce_write_policy(policy, path_str)?;

        // 4. Build full remote path
        let full_path = format!("{}/{}", volume.remote_path, path_str.trim_start_matches('/'));

        // 5. Delete file via storage provider
        self.storage_provider.delete_file(&full_path).await?;

        // 6. Publish event
        self.event_publisher
            .publish_storage_event(StorageEvent::FileDeleted {
                execution_id,
                volume_id,
                path: path_str.to_string(),
                deleted_at: Utc::now(),
            })
            .await;

        Ok(())
    }

    /// Delete a directory
    pub async fn delete_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FilesystemPolicy,
    ) -> Result<(), FsalError> {
        // 1. Authorize
        let volume = self.authorize(execution_id, volume_id).await?;

        // 2. Sanitize path
        let canonical = self.path_sanitizer.canonicalize(path, Some("/workspace"))?;
        let path_str = canonical.to_str().unwrap();

        // 3. Enforce write policy
        self.enforce_write_policy(policy, path_str)?;

        // 4. Build full remote path
        let full_path = format!("{}/{}", volume.remote_path, path_str.trim_start_matches('/'));

        // 5. Delete directory via storage provider
        self.storage_provider.delete_directory(&full_path).await?;

        // 6. Publish event
        self.event_publisher
            .publish_storage_event(StorageEvent::FileDeleted {
                execution_id,
                volume_id,
                path: path_str.to_string(),
                deleted_at: Utc::now(),
            })
            .await;

        Ok(())
    }

    /// Rename a file or directory
    pub async fn rename(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        from_path: &str,
        to_path: &str,
        policy: &FilesystemPolicy,
    ) -> Result<(), FsalError> {
        // 1. Authorize
        let volume = self.authorize(execution_id, volume_id).await?;

        // 2. Sanitize both paths
        let from_canonical = self.path_sanitizer.canonicalize(from_path, Some("/workspace"))?;
        let to_canonical = self.path_sanitizer.canonicalize(to_path, Some("/workspace"))?;
        let from_str = from_canonical.to_str().unwrap();
        let to_str = to_canonical.to_str().unwrap();

        // 3. Enforce write policy for both paths
        self.enforce_write_policy(policy, from_str)?;
        self.enforce_write_policy(policy, to_str)?;

        // 4. Build full remote paths
        let from_full = format!("{}/{}", volume.remote_path, from_str.trim_start_matches('/'));
        let to_full = format!("{}/{}", volume.remote_path, to_str.trim_start_matches('/'));

        // 5. Rename via storage provider
        self.storage_provider.rename(&from_full, &to_full).await?;

        // 6. Publish event (reuse FileCreated for rename target)
        self.event_publisher
            .publish_storage_event(StorageEvent::FileCreated {
                execution_id,
                volume_id,
                path: to_str.to_string(),
                created_at: Utc::now(),
            })
            .await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aegis_file_handle_size() {
        let handle = AegisFileHandle::new(
            ExecutionId::new(),
            VolumeId::new(),
            "/workspace/test/file.txt",
        );

        let bytes = handle.to_bytes().unwrap();
        println!("FileHandle size: {} bytes", bytes.len());
        assert!(bytes.len() <= 64, "FileHandle exceeds NFSv3 64-byte limit");
    }

    #[test]
    fn test_aegis_file_handle_roundtrip() {
        let original = AegisFileHandle::new(
            ExecutionId::new(),
            VolumeId::new(),
            "/workspace/test.txt",
        );

        let bytes = original.to_bytes().unwrap();
        let decoded = AegisFileHandle::from_bytes(&bytes).unwrap();

        assert_eq!(original, decoded);
    }
}
