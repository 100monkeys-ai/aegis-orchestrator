// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Builtin `aegis.execution.file` Tool
//!
//! Reads a single file of a completed execution post-mortem, through
//! `FileOperationsService::read_file_for_execution`: a produced file from the
//! volume the supervisor read it in, any other path from the volume mounted
//! at `/workspace`, both through the FSAL (AEGIS ADR-005 I8). Given a
//! `volume_id`, it reads the file from that volume of the caller's tenant
//! instead (`FileOperationsService::read_file_in_volume_for_tenant`): the
//! volume and path an `aegis.image.generate` answer names.

use crate::application::file_operations_service::{FileOperationsError, FileOperationsService};
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::seal_session::SealSessionError;
use crate::domain::tenant::TenantId;
use crate::domain::volume::VolumeId;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Returns `true` if the MIME content type represents text-like content that
/// can be safely rendered as a UTF-8 string.
fn is_text_content_type(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type == "application/json"
        || content_type == "application/xml"
}

/// Normalize a file path by stripping the `/workspace/` or `/workspace` prefix
/// that agents typically use as their working directory mount point.
fn normalize_path(path: &str) -> &str {
    let trimmed = path.trim_start_matches('/');
    if let Some(rest) = trimmed.strip_prefix("workspace/") {
        rest
    } else if trimmed == "workspace" {
        "/"
    } else {
        path
    }
}

/// An optional UUID argument: absent or null is `None`; anything else must
/// be a UUID string.
fn optional_uuid(args: &Value, field: &str) -> Result<Option<uuid::Uuid>, SealSessionError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => uuid::Uuid::parse_str(text).map(Some).map_err(|e| {
            SealSessionError::InvalidArguments(format!(
                "aegis.execution.file: invalid {field} UUID: {e}"
            ))
        }),
        Some(_) => Err(SealSessionError::MalformedPayload(format!(
            "aegis.execution.file: '{field}' must be a string"
        ))),
    }
}

/// Where the file is read from.
enum Source {
    /// The execution's own files (AEGIS ADR-005 I8).
    Execution(ExecutionId),
    /// A volume of the caller's tenant, named by the call; the execution,
    /// when the call also names one, only names the download URL.
    Volume {
        volume_id: VolumeId,
        execution_id: Option<ExecutionId>,
    },
}

/// Invoke the `aegis.execution.file` builtin tool.
///
/// Expected args, an execution's file or a volume's:
/// ```json
/// { "execution_id": "<uuid>", "path": "output.md" }
/// { "volume_id": "<uuid>", "path": "generated/<batch>/1.jpg" }
/// ```
pub async fn invoke_execution_file_tool(
    file_ops: &Arc<FileOperationsService>,
    tenant_id: &TenantId,
    args: &Value,
) -> Result<ToolInvocationResult, SealSessionError> {
    let execution_id = optional_uuid(args, "execution_id")?.map(ExecutionId);
    let volume_id = optional_uuid(args, "volume_id")?.map(VolumeId);
    let source = match (volume_id, execution_id) {
        (Some(volume_id), execution_id) => Source::Volume {
            volume_id,
            execution_id,
        },
        (None, Some(execution_id)) => Source::Execution(execution_id),
        (None, None) => {
            return Err(SealSessionError::MalformedPayload(
                "aegis.execution.file: missing required field 'execution_id' (string), or 'volume_id' (string) for a file in a volume".to_string(),
            ))
        }
    };

    let raw_path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
        SealSessionError::MalformedPayload(
            "aegis.execution.file: missing required field 'path' (string)".to_string(),
        )
    })?;

    let path = normalize_path(raw_path);

    info!(
        execution_id = ?execution_id,
        volume_id = ?volume_id,
        path,
        tenant_id = %tenant_id,
        "aegis.execution.file: reading file"
    );

    let read = match &source {
        Source::Execution(execution_id) => {
            file_ops
                .read_file_for_execution(*execution_id, tenant_id, path)
                .await
        }
        Source::Volume { volume_id, .. } => {
            file_ops
                .read_file_in_volume_for_tenant(volume_id, tenant_id, path)
                .await
        }
    };

    match read {
        Ok(content) => {
            let size_bytes = content.data.len();
            let is_binary = !is_text_content_type(&content.content_type);
            // The route reads by the volume its query names; with no
            // execution named, its id segment carries the volume's id and is
            // not read.
            let download_url = match &source {
                Source::Execution(execution_id) => {
                    format!("/v1/executions/{}/files/{}", execution_id.0, raw_path)
                }
                Source::Volume {
                    volume_id,
                    execution_id,
                } => format!(
                    "/v1/executions/{}/files/{}?volume_id={}",
                    execution_id.map_or(volume_id.0, |id| id.0),
                    raw_path,
                    volume_id.0
                ),
            };

            debug!(
                execution_id = ?execution_id,
                volume_id = ?volume_id,
                path,
                size_bytes,
                is_binary,
                content_type = %content.content_type,
                "aegis.execution.file: success"
            );

            let mut result = json!({
                "status": "success",
                "path": raw_path,
                "content_type": content.content_type,
                "is_binary": is_binary,
                "size_bytes": size_bytes,
                "download_url": download_url,
            });
            let fields = result.as_object_mut().expect("result is an object");
            if let Some(volume_id) = volume_id {
                fields.insert(
                    "volume_id".to_string(),
                    Value::String(volume_id.to_string()),
                );
            }
            if !is_binary {
                let text = String::from_utf8_lossy(&content.data).to_string();
                fields.insert("content".to_string(), Value::String(text));
            }

            Ok(ToolInvocationResult::Direct(result))
        }
        Err(FileOperationsError::NotFound(msg)) => {
            warn!(execution_id = ?execution_id, volume_id = ?volume_id, path, %msg, "aegis.execution.file: not found");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "not_found",
                "message": msg,
            })))
        }
        // Another tenant's execution is answered exactly as one with no
        // workspace volume (`read_file_for_execution`'s not-found text), and
        // another tenant's volume exactly as one that does not exist
        // (`read_file_in_volume_for_tenant`'s), so the answer never tells
        // that it exists (AEGIS ADR-035 R5, NOT_FOUND: "existence is not
        // told"). Only the operator's log tells them apart.
        Err(FileOperationsError::Unauthorized) => {
            warn!(
                execution_id = ?execution_id,
                volume_id = ?volume_id,
                path,
                caller_tenant = %tenant_id,
                "aegis.execution.file: execution or volume of another tenant; answered as not found"
            );
            let message = match &source {
                Source::Execution(execution_id) => {
                    format!("no workspace volume for execution {}", execution_id.0)
                }
                Source::Volume { volume_id, .. } => {
                    format!("no file {path} in volume {}", volume_id.0)
                }
            };
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "not_found",
                "message": message,
            })))
        }
        Err(
            FileOperationsError::InvalidPath(msg)
            | FileOperationsError::NotADirectory(msg)
            | FileOperationsError::IsDirectory(msg),
        ) => {
            warn!(execution_id = ?execution_id, volume_id = ?volume_id, path, %msg, "aegis.execution.file: invalid path");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "invalid_path",
                "message": msg,
            })))
        }
        Err(FileOperationsError::FileTooLarge) => {
            warn!(execution_id = ?execution_id, volume_id = ?volume_id, path, "aegis.execution.file: file too large");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "file_too_large",
                "message": "file exceeds the maximum allowed size",
            })))
        }
        Err(FileOperationsError::Fsal(msg)) => {
            warn!(execution_id = ?execution_id, volume_id = ?volume_id, path, %msg, "aegis.execution.file: fsal error");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "storage_error",
                "message": format!("storage backend error: {msg}"),
            })))
        }
        Err(FileOperationsError::Repository(msg)) => {
            warn!(execution_id = ?execution_id, volume_id = ?volume_id, path, %msg, "aegis.execution.file: repository error");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "internal_error",
                "message": format!("repository error: {msg}"),
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path_strips_workspace_prefix() {
        assert_eq!(normalize_path("/workspace/output.md"), "output.md");
        assert_eq!(normalize_path("workspace/output.md"), "output.md");
        assert_eq!(normalize_path("/workspace/deep/file.txt"), "deep/file.txt");
    }

    #[test]
    fn test_normalize_path_bare_workspace() {
        assert_eq!(normalize_path("/workspace"), "/");
        assert_eq!(normalize_path("workspace"), "/");
    }

    #[test]
    fn test_normalize_path_no_workspace_prefix() {
        assert_eq!(normalize_path("output.md"), "output.md");
        assert_eq!(normalize_path("/output.md"), "/output.md");
        assert_eq!(normalize_path("deep/file.txt"), "deep/file.txt");
    }

    // ---------------------------------------------------------------------
    // A file read by the volume the call names: the volume an
    // `aegis.image.generate` answer names (row
    // `image-generate-file-unfetchable`). Out of a run, the image is written
    // to the person's `chat-attachments` volume under `generated/<batch>/`,
    // and no execution has that volume, so the execution's lookup answered
    // "no workspace volume for execution <batch>".
    // ---------------------------------------------------------------------

    use crate::domain::events::StorageEvent;
    use crate::domain::fsal::{AegisFSAL, EventPublisher};
    use crate::domain::repository::{ExecutionRepository, VolumeRepository};
    use crate::domain::volume::{StorageClass, Volume, VolumeBackend, VolumeId, VolumeOwnership};
    use crate::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
    };
    use crate::infrastructure::storage::LocalHostStorageProvider;
    use std::collections::HashMap;

    /// The first bytes of a JPEG file, then a few more.
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIFgenerated-image";

    struct NoStorageEvents;

    #[async_trait::async_trait]
    impl EventPublisher for NoStorageEvents {
        async fn publish_storage_event(&self, _event: StorageEvent) {}
    }

    /// A file service over a scratch root, and the volume repository behind
    /// it.
    struct Files {
        root: tempfile::TempDir,
        volumes: Arc<InMemoryVolumeRepository>,
        service: Arc<FileOperationsService>,
    }

    fn files() -> Files {
        let root = tempfile::tempdir().expect("scratch root");
        let volumes = Arc::new(InMemoryVolumeRepository::new());
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(root.path()).expect("storage root")),
            volumes.clone() as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoStorageEvents),
        ));
        let service = Arc::new(FileOperationsService::new(
            fsal,
            Arc::new(InMemoryExecutionRepository::new()) as Arc<dyn ExecutionRepository>,
            Arc::new(InMemoryAgentRepository::new()),
        ));
        Files {
            root,
            volumes,
            service,
        }
    }

    impl Files {
        /// A person's `chat-attachments` volume of `tenant`, as the image
        /// tool writes into out of a run, holding `files` at their paths.
        async fn person_volume(&self, tenant: &TenantId, files: &[(&str, &[u8])]) -> VolumeId {
            let mut volume = Volume::new(
                "chat-attachments".to_string(),
                tenant.clone(),
                StorageClass::persistent(),
                VolumeBackend::HostPath {
                    path: std::path::PathBuf::from("/placeholder"),
                },
                64 * 1024 * 1024,
                VolumeOwnership::persistent("image-person"),
            )
            .expect("volume");
            volume.backend = VolumeBackend::HostPath {
                path: std::path::PathBuf::from(format!("/hosts/{}", volume.id)),
            };
            volume.mark_available().expect("available");
            self.volumes.save(&volume).await.expect("saved");
            for (path, bytes) in files {
                let on_disk = self
                    .root
                    .path()
                    .join(format!("hosts/{}", volume.id))
                    .join(path);
                std::fs::create_dir_all(on_disk.parent().expect("a parent")).expect("dirs");
                std::fs::write(on_disk, bytes).expect("written");
            }
            volume.id
        }
    }

    async fn call(files: &Files, tenant: &TenantId, args: Value) -> Value {
        match invoke_execution_file_tool(&files.service, tenant, &args)
            .await
            .expect("the tool answers")
        {
            ToolInvocationResult::Direct(answer) => answer,
            _ => panic!("aegis.execution.file answers directly"),
        }
    }

    /// The call the image answer names: `volume_id` and `path`, here with
    /// the batch's id as `execution_id`, as the caller who found the row
    /// called it. The file is read from that volume: a text file's bytes
    /// as `content`, an image's metadata and a download URL naming the
    /// volume (binary bytes never ride the tool's answer; the route serves
    /// them). Red before the fix: `no workspace volume for execution
    /// <batch>`.
    #[tokio::test]
    async fn a_file_is_read_from_the_volume_the_call_names() {
        let files = files();
        let tenant = TenantId::for_consumer_user("image-person").expect("tenant");
        let batch = uuid::Uuid::new_v4();
        let image = format!("generated/{batch}/1.jpg");
        let caption = format!("generated/{batch}/caption.txt");
        let volume = files
            .person_volume(
                &tenant,
                &[(&image, JPEG), (&caption, b"a grey monkey under a tree")],
            )
            .await;

        let text = call(
            &files,
            &tenant,
            json!({"execution_id": batch.to_string(), "volume_id": volume.to_string(), "path": caption}),
        )
        .await;
        println!("aegis.execution.file, volume_id and a text path: {text}");
        assert_eq!(text["status"], "success", "answer: {text}");
        assert_eq!(text["content"], "a grey monkey under a tree");
        assert_eq!(text["size_bytes"], 26);

        let picture = call(
            &files,
            &tenant,
            json!({"volume_id": volume.to_string(), "path": image}),
        )
        .await;
        println!("aegis.execution.file, volume_id and an image path: {picture}");
        assert_eq!(picture["status"], "success", "answer: {picture}");
        assert_eq!(picture["content_type"], "image/jpeg");
        assert_eq!(picture["is_binary"], true);
        assert_eq!(picture["size_bytes"], JPEG.len());
        assert_eq!(picture["volume_id"], volume.to_string());
        assert!(picture.get("content").is_none(), "answer: {picture}");
        assert_eq!(
            picture["download_url"],
            format!("/v1/executions/{volume}/files/{image}?volume_id={volume}")
        );
    }

    /// Another tenant's volume is answered exactly as a volume that does
    /// not exist: the same body, built from the id the caller sent, and no
    /// byte of the file (AEGIS ADR-035 R5: existence is not told).
    #[tokio::test]
    async fn a_volume_of_another_tenant_is_answered_as_one_that_does_not_exist() {
        let files = files();
        let owner = TenantId::for_consumer_user("image-person").expect("tenant");
        let caller = TenantId::for_consumer_user("intruder").expect("tenant");
        let path = "generated/batch/1.jpg";
        let foreign = files.person_volume(&owner, &[(path, JPEG)]).await;
        let missing = VolumeId::new();

        let answer = |volume: VolumeId| json!({"volume_id": volume.to_string(), "path": path});
        let refused = call(&files, &caller, answer(foreign)).await;
        let absent = call(&files, &caller, answer(missing)).await;
        println!("another tenant's volume: {refused}");
        println!("a volume that does not exist: {absent}");

        let not_found = |volume: VolumeId| {
            json!({
                "status": "error",
                "error": "not_found",
                "message": format!("no file {path} in volume {volume}"),
            })
        };
        assert_eq!(refused, not_found(foreign));
        assert_eq!(absent, not_found(missing));

        // The owner reads it, so the refusal is the tenant's and not the path's.
        let owned = call(&files, &owner, answer(foreign)).await;
        assert_eq!(owned["status"], "success", "answer: {owned}");
    }
}
