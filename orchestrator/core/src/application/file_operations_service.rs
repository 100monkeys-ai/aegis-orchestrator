// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # File Operations Service (Gap 079-8)
//!
//! Mediates REST-facing file operations against user-owned persistent volumes.
//! Authorization uses `AegisFSAL::authorize_for_user`; path sanitization reuses
//! the domain `PathSanitizer`.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};

use crate::domain::fsal::{AegisFSAL, FsalError};
use crate::domain::path_sanitizer::PathSanitizer;
use crate::domain::storage::{FileType, OpenMode, StorageError};
use crate::domain::volume::VolumeId;

// ============================================================================
// Value types
// ============================================================================

#[derive(Debug, serde::Serialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub modified_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub struct FileContent {
    pub data: Vec<u8>,
    pub content_type: String,
}

#[derive(Debug, serde::Serialize)]
pub struct FileAttributes {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub created_at: Option<DateTime<Utc>>,
    pub modified_at: Option<DateTime<Utc>>,
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum FileOperationsError {
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("file size exceeds limit")]
    FileTooLarge,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("fsal error: {0}")]
    Fsal(String),
    #[error("repository error: {0}")]
    Repository(String),
}

impl From<FsalError> for FileOperationsError {
    fn from(e: FsalError) -> Self {
        match e {
            FsalError::VolumeNotFound(_) => FileOperationsError::NotFound(e.to_string()),
            FsalError::UnauthorizedAccess { .. } => FileOperationsError::Unauthorized,
            FsalError::PathSanitization(inner) => {
                FileOperationsError::InvalidPath(inner.to_string())
            }
            FsalError::Storage(StorageError::FileNotFound(p)) => FileOperationsError::NotFound(p),
            FsalError::Storage(StorageError::NotFound(p)) => FileOperationsError::NotFound(p),
            _ => FileOperationsError::Fsal(e.to_string()),
        }
    }
}

// ============================================================================
// Service
// ============================================================================

pub struct FileOperationsService {
    fsal: Arc<AegisFSAL>,
    path_sanitizer: PathSanitizer,
}

impl FileOperationsService {
    pub fn new(fsal: Arc<AegisFSAL>) -> Self {
        Self {
            fsal,
            path_sanitizer: PathSanitizer::new(),
        }
    }

    fn sanitize(&self, path: &str) -> Result<String, FileOperationsError> {
        let canonical = self
            .path_sanitizer
            .canonicalize(path, Some("/"))
            .map_err(|e| FileOperationsError::InvalidPath(e.to_string()))?;
        Ok(canonical.to_string_lossy().replace('\\', "/"))
    }

    fn sanitize_and_resolve(
        &self,
        path: &str,
        volume: &crate::domain::volume::Volume,
    ) -> Result<String, FileOperationsError> {
        let canonical_str = self.sanitize(path)?;
        Ok(routed_path(volume, &canonical_str))
    }

    pub async fn list_directory(
        &self,
        volume_id: &VolumeId,
        owner: &str,
        path: &str,
    ) -> Result<Vec<DirEntry>, FileOperationsError> {
        let volume = self.fsal.authorize_for_user(owner, volume_id).await?;
        let full_path = self.sanitize_and_resolve(path, &volume)?;

        let entries = self
            .fsal
            .storage_provider()
            .readdir(&full_path)
            .await
            .map_err(|e| FileOperationsError::Fsal(e.to_string()))?;

        let result = entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.name,
                is_dir: e.file_type == FileType::Directory,
                size_bytes: 0,
                modified_at: None,
            })
            .collect();
        Ok(result)
    }

    pub async fn read_file(
        &self,
        volume_id: &VolumeId,
        owner: &str,
        path: &str,
    ) -> Result<FileContent, FileOperationsError> {
        let volume = self.fsal.authorize_for_user(owner, volume_id).await?;
        let full_path = self.sanitize_and_resolve(path, &volume)?;

        let handle = self
            .fsal
            .storage_provider()
            .open_file(&full_path, OpenMode::ReadOnly)
            .await
            .map_err(|e| match e {
                StorageError::FileNotFound(p) => FileOperationsError::NotFound(p),
                StorageError::NotFound(p) => FileOperationsError::NotFound(p),
                other => FileOperationsError::Fsal(other.to_string()),
            })?;

        let attrs = self
            .fsal
            .storage_provider()
            .stat(&full_path)
            .await
            .map_err(|e| FileOperationsError::Fsal(e.to_string()))?;

        let data = self
            .fsal
            .storage_provider()
            .read_at(&handle, 0, attrs.size as usize)
            .await
            .map_err(|e| FileOperationsError::Fsal(e.to_string()))?;

        let _ = self.fsal.storage_provider().close_file(&handle).await;

        let content_type = guess_content_type(path);
        Ok(FileContent { data, content_type })
    }

    pub async fn write_file(
        &self,
        volume_id: &VolumeId,
        owner: &str,
        path: &str,
        data: &[u8],
        max_file_size_bytes: u64,
    ) -> Result<(), FileOperationsError> {
        if data.len() as u64 > max_file_size_bytes {
            return Err(FileOperationsError::FileTooLarge);
        }

        let volume = self.fsal.authorize_for_user(owner, volume_id).await?;
        let full_path = self.sanitize_and_resolve(path, &volume)?;

        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(&full_path).parent() {
            let parent_str = parent.to_string_lossy();
            if !parent_str.is_empty() && parent_str != "/" {
                let _ = self
                    .fsal
                    .storage_provider()
                    .create_directory(&parent_str)
                    .await;
            }
        }

        let handle = self
            .fsal
            .storage_provider()
            .create_file(&full_path, 0o644)
            .await
            .map_err(|e| FileOperationsError::Fsal(e.to_string()))?;

        self.fsal
            .storage_provider()
            .write_at(&handle, 0, data)
            .await
            .map_err(|e| FileOperationsError::Fsal(e.to_string()))?;

        let _ = self.fsal.storage_provider().close_file(&handle).await;

        Ok(())
    }

    /// Write a file with size limits resolved from the caller's [`ZaruTier`].
    pub async fn write_file_for_tier(
        &self,
        volume_id: &VolumeId,
        owner: &str,
        path: &str,
        data: &[u8],
        tier: &crate::domain::iam::ZaruTier,
    ) -> Result<(), FileOperationsError> {
        let tier_limits = crate::domain::volume::StorageTierLimits::default();
        let max_file_size = tier_limits
            .limits
            .get(tier)
            .map(|l| l.max_file_size_bytes)
            .unwrap_or(50 * 1024 * 1024);
        self.write_file(volume_id, owner, path, data, max_file_size)
            .await
    }

    pub async fn delete_path(
        &self,
        volume_id: &VolumeId,
        owner: &str,
        path: &str,
    ) -> Result<(), FileOperationsError> {
        let volume = self.fsal.authorize_for_user(owner, volume_id).await?;
        let full_path = self.sanitize_and_resolve(path, &volume)?;

        // Try file first, then directory
        let file_result = self.fsal.storage_provider().delete_file(&full_path).await;
        if let Err(StorageError::FileNotFound(_)) | Err(StorageError::NotFound(_)) = &file_result {
            self.fsal
                .storage_provider()
                .delete_directory(&full_path)
                .await
                .map_err(|e| match e {
                    StorageError::NotFound(p) => FileOperationsError::NotFound(p),
                    other => FileOperationsError::Fsal(other.to_string()),
                })?;
        } else {
            file_result.map_err(|e| FileOperationsError::Fsal(e.to_string()))?;
        }

        Ok(())
    }

    pub async fn create_directory(
        &self,
        volume_id: &VolumeId,
        owner: &str,
        path: &str,
    ) -> Result<(), FileOperationsError> {
        let volume = self.fsal.authorize_for_user(owner, volume_id).await?;
        let full_path = self.sanitize_and_resolve(path, &volume)?;

        self.fsal
            .storage_provider()
            .create_directory(&full_path)
            .await
            .map_err(|e| FileOperationsError::Fsal(e.to_string()))?;

        Ok(())
    }

    pub async fn move_path(
        &self,
        volume_id: &VolumeId,
        owner: &str,
        from: &str,
        to: &str,
    ) -> Result<(), FileOperationsError> {
        let volume = self.fsal.authorize_for_user(owner, volume_id).await?;
        let from_full = self.sanitize_and_resolve(from, &volume)?;
        let to_full = self.sanitize_and_resolve(to, &volume)?;

        self.fsal
            .storage_provider()
            .rename(&from_full, &to_full)
            .await
            .map_err(|e| match e {
                StorageError::FileNotFound(p) | StorageError::NotFound(p) => {
                    FileOperationsError::NotFound(p)
                }
                other => FileOperationsError::Fsal(other.to_string()),
            })?;

        Ok(())
    }

    pub async fn get_attributes(
        &self,
        volume_id: &VolumeId,
        owner: &str,
        path: &str,
    ) -> Result<FileAttributes, FileOperationsError> {
        let volume = self.fsal.authorize_for_user(owner, volume_id).await?;
        let full_path = self.sanitize_and_resolve(path, &volume)?;

        let attrs = self
            .fsal
            .storage_provider()
            .stat(&full_path)
            .await
            .map_err(|e| match e {
                StorageError::FileNotFound(p) | StorageError::NotFound(p) => {
                    FileOperationsError::NotFound(p)
                }
                other => FileOperationsError::Fsal(other.to_string()),
            })?;

        let name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());

        Ok(FileAttributes {
            name,
            is_dir: attrs.file_type == FileType::Directory,
            size_bytes: attrs.size,
            created_at: Some(
                Utc.timestamp_opt(attrs.ctime, 0)
                    .single()
                    .unwrap_or_else(Utc::now),
            ),
            modified_at: Some(
                Utc.timestamp_opt(attrs.mtime, 0)
                    .single()
                    .unwrap_or_else(Utc::now),
            ),
        })
    }

    /// Read a file from an execution-owned workspace volume (post-mortem).
    ///
    /// Looks up the volume by `VolumeOwnership::Execution { execution_id }`,
    /// verifies the volume belongs to the given tenant, sanitizes the path,
    /// and returns the file content.
    pub async fn read_file_for_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &crate::domain::tenant::TenantId,
        path: &str,
    ) -> Result<FileContent, FileOperationsError> {
        use crate::domain::volume::VolumeOwnership;

        let ownership = VolumeOwnership::execution(execution_id);
        let volumes = self
            .fsal
            .volume_repository()
            .find_by_ownership(&ownership)
            .await
            .map_err(|e| FileOperationsError::Repository(e.to_string()))?;

        let volume = volumes.into_iter().next().ok_or_else(|| {
            FileOperationsError::NotFound(format!(
                "no workspace volume for execution {}",
                execution_id.0
            ))
        })?;

        // Tenant isolation check
        if &volume.tenant_id != tenant_id {
            return Err(FileOperationsError::Unauthorized);
        }

        let sanitized = self.sanitize(path)?;
        let full_path = routed_path(&volume, &sanitized);

        let handle = self
            .fsal
            .storage_provider()
            .open_file(&full_path, OpenMode::ReadOnly)
            .await
            .map_err(|e| match e {
                StorageError::FileNotFound(p) => FileOperationsError::NotFound(p),
                StorageError::NotFound(p) => FileOperationsError::NotFound(p),
                other => FileOperationsError::Fsal(other.to_string()),
            })?;

        let attrs = self
            .fsal
            .storage_provider()
            .stat(&full_path)
            .await
            .map_err(|e| FileOperationsError::Fsal(e.to_string()))?;

        let data = self
            .fsal
            .storage_provider()
            .read_at(&handle, 0, attrs.size as usize)
            .await
            .map_err(|e| FileOperationsError::Fsal(e.to_string()))?;

        let _ = self.fsal.storage_provider().close_file(&handle).await;

        let content_type = guess_content_type(path);
        Ok(FileContent { data, content_type })
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn routed_path(volume: &crate::domain::volume::Volume, path: &str) -> String {
    match &volume.backend {
        crate::domain::volume::VolumeBackend::SeaweedFS { remote_path, .. } => {
            format!("{}/{}", remote_path, path.trim_start_matches('/'))
        }
        crate::domain::volume::VolumeBackend::HostPath { path: host_path } => host_path
            .join(path.trim_start_matches('/'))
            .to_string_lossy()
            .to_string(),
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

fn guess_content_type(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "json" => "application/json",
        "txt" | "log" | "md" | "rs" | "toml" | "yaml" | "yml" => "text/plain",
        "html" | "htm" => "text/html",
        "js" => "text/javascript",
        "css" => "text/css",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
    .to_string()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use crate::domain::path_sanitizer::PathSanitizer;

    #[test]
    fn path_sanitizer_rejects_traversal() {
        let s = PathSanitizer::new();
        assert!(s.canonicalize("../foo", Some("/")).is_err());
        assert!(s.canonicalize("/etc/passwd", Some("/workspace")).is_err());
        assert!(s.canonicalize("foo/../../bar", Some("/")).is_err());
    }
}
