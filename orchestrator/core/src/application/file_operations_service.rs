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
use crate::domain::repository::{AgentRepository, ExecutionRepository};
use crate::domain::storage::{FileType, OpenMode, StorageError};
use crate::domain::tenant::TenantId;
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
    /// The file's size as its storage stat gave it: the route's
    /// `Content-Length` (AEGIS ADR-005 I8).
    pub size_bytes: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct FileAttributes {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub created_at: Option<DateTime<Utc>>,
    pub modified_at: Option<DateTime<Utc>>,
}

/// File metadata for an attachment-shaped stat (ADR-113).
///
/// Returned by [`FileOperationsService::stat_attachment_for_user`] to back
/// the CLI's `--attachment <volume_id:path>` shorthand: the CLI issues this
/// stat per shorthand flag and constructs a full `AttachmentRef` from the
/// result. The fields mirror `AttachmentRef` minus `volume_id` / `path`,
/// which the caller already holds.
#[derive(Debug, serde::Serialize)]
pub struct AttachmentStat {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
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
    /// The execution records an execution's file is read through: its
    /// tenant, its produced files and its workspace (AEGIS ADR-005 I8).
    executions: Arc<dyn ExecutionRepository>,
    /// The agents whose manifest names the volume an execution mounted at
    /// `/workspace` (I8, P2).
    agents: Arc<dyn AgentRepository>,
}

impl FileOperationsService {
    pub fn new(
        fsal: Arc<AegisFSAL>,
        executions: Arc<dyn ExecutionRepository>,
        agents: Arc<dyn AgentRepository>,
    ) -> Self {
        Self {
            fsal,
            path_sanitizer: PathSanitizer::new(),
            executions,
            agents,
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
        tenant_id: &TenantId,
        owner: &str,
        path: &str,
    ) -> Result<Vec<DirEntry>, FileOperationsError> {
        let volume = self
            .fsal
            .authorize_for_user(tenant_id, owner, volume_id)
            .await?;
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
        tenant_id: &TenantId,
        owner: &str,
        path: &str,
    ) -> Result<FileContent, FileOperationsError> {
        let volume = self
            .fsal
            .authorize_for_user(tenant_id, owner, volume_id)
            .await?;
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
        Ok(FileContent {
            data,
            content_type,
            size_bytes: attrs.size,
        })
    }

    pub async fn write_file(
        &self,
        volume_id: &VolumeId,
        tenant_id: &TenantId,
        owner: &str,
        path: &str,
        data: &[u8],
        max_file_size_bytes: u64,
    ) -> Result<(), FileOperationsError> {
        if data.len() as u64 > max_file_size_bytes {
            return Err(FileOperationsError::FileTooLarge);
        }

        let volume = self
            .fsal
            .authorize_for_user(tenant_id, owner, volume_id)
            .await?;
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

    /// Write a file with size limits resolved from the caller's tier.
    pub async fn write_file_for_tier(
        &self,
        volume_id: &VolumeId,
        tenant_id: &TenantId,
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
        self.write_file(volume_id, tenant_id, owner, path, data, max_file_size)
            .await
    }

    pub async fn delete_path(
        &self,
        volume_id: &VolumeId,
        tenant_id: &TenantId,
        owner: &str,
        path: &str,
    ) -> Result<(), FileOperationsError> {
        let volume = self
            .fsal
            .authorize_for_user(tenant_id, owner, volume_id)
            .await?;
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
        tenant_id: &TenantId,
        owner: &str,
        path: &str,
    ) -> Result<(), FileOperationsError> {
        let volume = self
            .fsal
            .authorize_for_user(tenant_id, owner, volume_id)
            .await?;
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
        tenant_id: &TenantId,
        owner: &str,
        from: &str,
        to: &str,
    ) -> Result<(), FileOperationsError> {
        let volume = self
            .fsal
            .authorize_for_user(tenant_id, owner, volume_id)
            .await?;
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
        tenant_id: &TenantId,
        owner: &str,
        path: &str,
    ) -> Result<FileAttributes, FileOperationsError> {
        let volume = self
            .fsal
            .authorize_for_user(tenant_id, owner, volume_id)
            .await?;
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

    /// Stat a file as an attachment candidate (ADR-113).
    ///
    /// Authorizes by user (same path as `read_file`), reads the file's full
    /// contents to content-sniff its MIME type and compute a SHA-256 digest,
    /// and returns the size + display name. Backs the CLI's
    /// `--attachment <volume_id:path>` shorthand: the CLI issues one of these
    /// stats per flag and constructs a full `AttachmentRef` from the result.
    ///
    /// MIME sniffing reuses the same `infer::get` pattern as the upload
    /// handler, so the resulting AttachmentRef's `mime_type` is byte-accurate
    /// rather than extension-derived. Falls back to
    /// `application/octet-stream` for content `infer` cannot classify.
    pub async fn stat_attachment_for_user(
        &self,
        volume_id: &VolumeId,
        tenant_id: &TenantId,
        owner: &str,
        path: &str,
    ) -> Result<AttachmentStat, FileOperationsError> {
        let content = self.read_file(volume_id, tenant_id, owner, path).await?;
        let size = content.data.len() as u64;
        let mime_type = infer::get(&content.data)
            .map(|k| k.mime_type().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let sha256 = {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(&content.data);
            Some(format!("{:x}", hasher.finalize()))
        };
        let name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        Ok(AttachmentStat {
            name,
            mime_type,
            size,
            sha256,
        })
    }

    /// Read a file from a tenant-scoped persistent user volume, used by the
    /// `aegis.attachment.read` tool (ADR-113).
    ///
    /// Tenant isolation is enforced by the volume lookup: the call returns
    /// `Unauthorized` when the volume's `tenant_id` does not match the caller's,
    /// and `NotFound` when the volume does not exist or is non-persistent.
    /// Path sanitization reuses the same `sanitize_and_resolve` pipeline as
    /// every other read path.
    pub async fn read_attachment_for_tenant(
        &self,
        volume_id: &VolumeId,
        tenant_id: &crate::domain::tenant::TenantId,
        path: &str,
    ) -> Result<FileContent, FileOperationsError> {
        use crate::domain::volume::VolumeOwnership;

        let volume = self
            .fsal
            .volume_repository()
            .find_by_id(*volume_id)
            .await
            .map_err(|e| FileOperationsError::Repository(e.to_string()))?
            .ok_or_else(|| {
                FileOperationsError::NotFound(format!("volume {} not found", volume_id.0))
            })?;

        // Tenant-isolation check.
        if &volume.tenant_id != tenant_id {
            return Err(FileOperationsError::Unauthorized);
        }

        // Attachments live in persistent user volumes only.
        let owner = match &volume.ownership {
            VolumeOwnership::Persistent { owner } => owner.clone(),
            _ => return Err(FileOperationsError::Unauthorized),
        };

        // Delegate to the standard read_file path so sanitization and FSAL
        // semantics are identical to `aegis.file.read`. Tenant scoping flows
        // through `authorize_for_user`.
        self.read_file(volume_id, tenant_id, &owner, path).await
    }

    /// Read a file of an execution post-mortem: the route
    /// `GET /v1/executions/:id/files/*path` and `aegis.execution.file`
    /// (AEGIS ADR-005 I8, choices P2 and P3).
    ///
    /// `path` is relative to the container's `/workspace` (the route and the
    /// tool strip that prefix). The execution is loaded for `tenant_id`.
    /// A path its record lists among its produced files is read from the
    /// volume and the path the supervisor read it at; any other path is read
    /// from the volume mounted at `/workspace`: the workflow's workspace when
    /// the execution is a workflow step, otherwise the execution's volume the
    /// agent's current manifest mounts there. Reads go through the FSAL as
    /// the execution, then as its workflow execution when refused. Another
    /// tenant's execution, a missing execution, a missing volume and another
    /// tenant's volume are all answered as not found, never telling which.
    pub async fn read_file_for_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &crate::domain::tenant::TenantId,
        path: &str,
    ) -> Result<FileContent, FileOperationsError> {
        let no_workspace = || {
            FileOperationsError::NotFound(format!(
                "no workspace volume for execution {}",
                execution_id.0
            ))
        };
        let execution = self
            .executions
            .find_by_id_for_tenant(tenant_id, execution_id)
            .await
            .map_err(|e| FileOperationsError::Repository(e.to_string()))?
            .ok_or_else(no_workspace)?;

        let sanitized = self.sanitize(path)?;
        let container_path = if sanitized == "/" {
            "/workspace".to_string()
        } else {
            format!("/workspace{sanitized}")
        };

        let recorded = execution
            .produced_files()
            .iter()
            .find(|file| file.path == container_path)
            .and_then(|file| Some((file.volume_id?, file.path_in_volume.clone()?)));
        if let Some((volume_id, path_in_volume)) = recorded {
            let no_file = || {
                FileOperationsError::NotFound(format!(
                    "no file {container_path} in execution {}",
                    execution_id.0
                ))
            };
            return self
                .read_in_volume(&execution, tenant_id, volume_id, &path_in_volume, path)
                .await
                .map_err(|error| match error {
                    FileOperationsError::NotFound(_) | FileOperationsError::Unauthorized => {
                        no_file()
                    }
                    other => other,
                });
        }

        let volume_id = self
            .workspace_volume(&execution, tenant_id)
            .await?
            .ok_or_else(no_workspace)?;
        self.read_in_volume(&execution, tenant_id, volume_id, &sanitized, path)
            .await
            .map_err(|error| match error {
                FileOperationsError::Unauthorized => no_workspace(),
                other => other,
            })
    }

    /// The volume an execution mounted at `/workspace` (I8, P2): the
    /// workflow's workspace volume when the execution is a workflow step
    /// mounting it there; otherwise the execution's own volume that the
    /// agent's current manifest mounts at `/workspace`. `None` when there is
    /// none. The manifest read is the agent's current one, not necessarily
    /// the one the run used.
    async fn workspace_volume(
        &self,
        execution: &crate::domain::execution::Execution,
        tenant_id: &TenantId,
    ) -> Result<Option<VolumeId>, FileOperationsError> {
        use crate::domain::volume::VolumeOwnership;

        let at_workspace = |mount: &str| mount.trim_end_matches('/') == "/workspace";
        if let Some(workspace) = execution.input.workspace_volume_id {
            let mount = execution
                .input
                .workspace_volume_mount_path
                .as_ref()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_else(|| "/workspace".to_string());
            if at_workspace(&mount) {
                return Ok(Some(workspace));
            }
        }

        let Some(agent) = self
            .agents
            .find_by_id_for_tenant(tenant_id, execution.agent_id)
            .await
            .map_err(|e| FileOperationsError::Repository(e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(spec) = agent
            .manifest
            .spec
            .volumes
            .iter()
            .find(|spec| at_workspace(&spec.mount_path))
        else {
            return Ok(None);
        };
        let volumes = self
            .fsal
            .volume_repository()
            .find_by_ownership(&VolumeOwnership::execution(execution.id))
            .await
            .map_err(|e| FileOperationsError::Repository(e.to_string()))?;
        Ok(volumes
            .into_iter()
            .find(|volume| volume.name == spec.name)
            .map(|volume| volume.id))
    }

    /// Read `path_in_volume` from `volume_id` through the FSAL as
    /// `execution`, then as its workflow execution on `UnauthorizedAccess`
    /// (as `FsalOutputReader::read_head` reads a declared output). A volume
    /// that is missing or of another tenant is not found.
    async fn read_in_volume(
        &self,
        execution: &crate::domain::execution::Execution,
        tenant_id: &TenantId,
        volume_id: VolumeId,
        path_in_volume: &str,
        requested: &str,
    ) -> Result<FileContent, FileOperationsError> {
        use crate::domain::fsal::{AegisFileHandle, FsalAccessPolicy};

        let volume = self
            .fsal
            .volume_repository()
            .find_by_id(volume_id)
            .await
            .map_err(|e| FileOperationsError::Repository(e.to_string()))?
            .ok_or_else(|| FileOperationsError::NotFound(format!("volume {}", volume_id.0)))?;
        if &volume.tenant_id != tenant_id {
            return Err(FileOperationsError::NotFound(format!(
                "volume {}",
                volume_id.0
            )));
        }

        let workflow_execution_id = execution.input.workflow_execution_id;
        let attributes = self
            .fsal
            .getattr(
                execution.id,
                volume_id,
                path_in_volume,
                execution.container_uid,
                execution.container_gid,
                workflow_execution_id,
            )
            .await?;
        if attributes.file_type != FileType::File {
            return Err(FileOperationsError::NotFound(path_in_volume.to_string()));
        }
        let length =
            usize::try_from(attributes.size).map_err(|_| FileOperationsError::FileTooLarge)?;
        let policy = FsalAccessPolicy {
            read: vec!["/*".to_string()],
            write: Vec::new(),
        };
        let as_execution = AegisFileHandle::new(execution.id, volume_id, path_in_volume);
        let data = match self
            .fsal
            .read(&as_execution, path_in_volume, &policy, 0, length)
            .await
        {
            Ok(data) => data,
            Err(FsalError::UnauthorizedAccess { .. }) if workflow_execution_id.is_some() => {
                let as_workflow = AegisFileHandle::new_for_workflow(
                    workflow_execution_id.unwrap_or_default(),
                    volume_id,
                    path_in_volume,
                );
                self.fsal
                    .read(&as_workflow, path_in_volume, &policy, 0, length)
                    .await?
            }
            Err(error) => return Err(error.into()),
        };
        if data.len() as u64 != attributes.size {
            return Err(FileOperationsError::Fsal(format!(
                "{path_in_volume} changed while it was read: {} bytes of {}",
                data.len(),
                attributes.size
            )));
        }
        Ok(FileContent {
            data,
            content_type: guess_content_type(requested),
            size_bytes: attributes.size,
        })
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

pub(crate) fn guess_content_type(path: &str) -> String {
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
