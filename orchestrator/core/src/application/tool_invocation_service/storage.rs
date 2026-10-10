// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Tool dispatch handlers for storage, volume, git, and script operations.

use serde_json::{json, Value};

use crate::application::file_operations_service::FileOperationsError;
use crate::application::git_repo_service::{
    GitRepoError, RepositoryActionAnswer, RepositoryActionError, LAND_IS_THE_WORKFLOWS,
    RUN_HOLDS_NO_REPOSITORY,
};
use crate::application::script_service::ScriptServiceError;
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::application::user_volume_service::UserVolumeError;
use crate::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use crate::domain::seal_session::{CallerAnswer, InternalFailure, SealSessionError};

use super::ToolInvocationService;

// ============================================================================
// Identity helpers
// ============================================================================

fn user_sub(identity: Option<&UserIdentity>) -> String {
    identity
        .map(|i| i.sub.clone())
        .unwrap_or_else(|| "anonymous".to_string())
}

fn user_tier(identity: Option<&UserIdentity>) -> ZaruTier {
    match identity.map(|i| &i.identity_kind) {
        Some(IdentityKind::ConsumerUser { zaru_tier, .. }) => zaru_tier.clone(),
        _ => ZaruTier::Enterprise,
    }
}

fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, SealSessionError> {
    args.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
        SealSessionError::InvalidArguments(format!(
            "required field '{key}' is missing or not a string"
        ))
    })
}

fn require_i64(args: &Value, key: &str) -> Result<i64, SealSessionError> {
    args.get(key).and_then(|v| v.as_i64()).ok_or_else(|| {
        SealSessionError::InvalidArguments(format!(
            "required field '{key}' is missing or not an integer"
        ))
    })
}

fn commit_author(identity: Option<&UserIdentity>) -> (String, String) {
    let name = identity
        .and_then(|i| i.name.clone())
        .unwrap_or_else(|| "User".to_string());
    let email = identity
        .and_then(|i| i.email.clone())
        .unwrap_or_else(|| "user@aegis.local".to_string());
    (name, email)
}

type ToolResult = Result<ToolInvocationResult, SealSessionError>;

/// A git call made inside a run (AEGIS ADR-136 G7a).
struct RunGitCall {
    run: uuid::Uuid,
    agent_id: crate::domain::agent::AgentId,
    entries: Vec<crate::domain::git_repo::RunRepository>,
}

fn ok_direct(value: Value) -> ToolResult {
    Ok(ToolInvocationResult::Direct(value))
}

// ============================================================================
// Refusals (AEGIS ADR-035, Update of 2026-10-04, R5, R6)
// ============================================================================
//
// A storage service's typed error is answered by its variant, where it is
// mapped: the caller is told their own business (not found, quota, conflict,
// invalid input), named as they named it; anything else is an internal
// failure. What the inner loop and the operator's log see is the error's
// text, as before (`shown`).

fn shown(e: &impl std::fmt::Display) -> SealSessionError {
    SealSessionError::InternalError(e.to_string())
}

fn internal(e: &impl std::fmt::Display) -> SealSessionError {
    shown(e).answered(CallerAnswer::Internal(InternalFailure::Server))
}

/// A file operation's error, answered by the caller's own `volume_id` and
/// `path`. A not-found's own text names the storage backend's path, so it is
/// never the answer.
fn file_refusal(e: FileOperationsError, volume_id: &str, path: &str) -> SealSessionError {
    let answer = match &e {
        // A volume of another tenant or owner is answered as a missing one:
        // one message, so existence is not told (R5, NOT_FOUND).
        FileOperationsError::NotFound(_) | FileOperationsError::Unauthorized => {
            CallerAnswer::NotFound(format!("Not found: '{path}' in volume '{volume_id}'."))
        }
        FileOperationsError::FileTooLarge => {
            CallerAnswer::QuotaExceeded("The file is larger than your tier allows.".to_string())
        }
        FileOperationsError::InvalidPath(_) => CallerAnswer::InvalidArguments(format!(
            "Invalid tool arguments: '{path}' is not a valid path in a volume."
        )),
        FileOperationsError::Fsal(_) | FileOperationsError::Repository(_) => {
            CallerAnswer::Internal(InternalFailure::Server)
        }
    };
    // What the inner loop shows an agent for another tenant's or owner's
    // volume is the text it shows for a volume that does not exist (AEGIS
    // ADR-035, Update T1: R7 yields for this one text). Only this warn line
    // tells the two apart, for the operator.
    let foreign = matches!(e, FileOperationsError::Unauthorized);
    let shown_error = match crate::domain::shared_kernel::VolumeId::from_string(volume_id) {
        Ok(id) if foreign => {
            tracing::warn!(
                volume_id,
                path,
                "aegis.file: volume of another tenant or owner; answered as not found"
            );
            FileOperationsError::from(crate::domain::fsal::FsalError::VolumeNotFound(id))
        }
        _ => e,
    };
    shown(&shown_error).answered(answer)
}

/// A storage service's typed error, answered by its variant.
trait IntoRefusal {
    fn into_refusal(self) -> SealSessionError;
}

impl IntoRefusal for UserVolumeError {
    fn into_refusal(self) -> SealSessionError {
        let answer = match &self {
            UserVolumeError::NotFound(_) => CallerAnswer::NotFound(format!("Not found: {self}.")),
            UserVolumeError::Unauthorized => {
                CallerAnswer::NotFound("Not found: the volume you named.".to_string())
            }
            UserVolumeError::VolumeCountQuotaExceeded | UserVolumeError::StorageQuotaExceeded => {
                CallerAnswer::QuotaExceeded(format!("Quota exceeded: {self}."))
            }
            UserVolumeError::DuplicateName(_) | UserVolumeError::VolumeAttached => {
                CallerAnswer::Conflict(format!("Conflict: {self}."))
            }
            UserVolumeError::UnknownTier
            | UserVolumeError::Repository(_)
            | UserVolumeError::VolumeService(_) => return internal(&self),
        };
        shown(&self).answered(answer)
    }
}

impl IntoRefusal for GitRepoError {
    fn into_refusal(self) -> SealSessionError {
        let answer = match &self {
            GitRepoError::TierLimitExceeded { .. } => {
                CallerAnswer::QuotaExceeded(format!("Quota exceeded: {self}."))
            }
            GitRepoError::BindingNotFound | GitRepoError::NotOwned => CallerAnswer::NotFound(
                "Not found: the git repository binding you named.".to_string(),
            ),
            GitRepoError::UrlValidationFailed(_) | GitRepoError::SshHostKeys(_) => {
                CallerAnswer::InvalidArguments(format!("Invalid tool arguments: {self}"))
            }
            GitRepoError::CredentialNotYours => CallerAnswer::InvalidArguments(self.to_string()),
            // AEGIS ADR-136 G4a: a binding a run holds, in the run's words.
            GitRepoError::HeldByRun { .. } => CallerAnswer::Conflict(self.to_string()),
            // AEGIS ADR-136 G7c, G8a: a clean tree and a push the remote
            // refused as not a fast-forward, each in its own sentence alone.
            GitRepoError::NothingToCommit | GitRepoError::RemoteAhead { .. } => {
                CallerAnswer::Conflict(self.to_string())
            }
            // AEGIS ADR-141 F8: a landing's ref moved on the remote, or is
            // a tag or a commit, each in its own sentence alone.
            GitRepoError::RefAhead { .. } | GitRepoError::NotABranch { .. } => {
                CallerAnswer::Conflict(self.to_string())
            }
            GitRepoError::BindingBusy(_) | GitRepoError::NoHeadBranch => {
                CallerAnswer::Conflict(format!("Conflict: {self}."))
            }
            GitRepoError::NotYetImplemented(_) => CallerAnswer::NotImplemented(self.to_string()),
            // The git library's text can carry the clone's local path: no
            // caller-facing message is built from the caller's own inputs at
            // this site, so it is answered as an upstream failure.
            GitRepoError::CloneFailed(_) | GitRepoError::GitFailed(_) => {
                CallerAnswer::Internal(InternalFailure::Upstream)
            }
            GitRepoError::Repository(_)
            | GitRepoError::SecretResolutionFailed(_)
            | GitRepoError::VolumeProvisioningFailed(_)
            | GitRepoError::WebhookRejected(_) => return internal(&self),
        };
        shown(&self).answered(answer)
    }
}

impl IntoRefusal for ScriptServiceError {
    fn into_refusal(self) -> SealSessionError {
        let answer = match &self {
            ScriptServiceError::NotFound => {
                CallerAnswer::NotFound("Not found: the script you named.".to_string())
            }
            ScriptServiceError::DuplicateName => {
                CallerAnswer::Conflict(format!("Conflict: {self}."))
            }
            ScriptServiceError::TierLimitExceeded { .. } => {
                CallerAnswer::QuotaExceeded(format!("Quota exceeded: {self}."))
            }
            ScriptServiceError::Domain(_) => {
                CallerAnswer::InvalidArguments(format!("Invalid tool arguments: {self}"))
            }
            ScriptServiceError::Repository(_) => return internal(&self),
        };
        shown(&self).answered(answer)
    }
}

fn not_configured(tool: &str, service: &str) -> ToolResult {
    Err(
        SealSessionError::InternalError(format!("{tool}: {service} not configured"))
            .answered(CallerAnswer::Internal(InternalFailure::Unavailable)),
    )
}

// ============================================================================
// File operations (aegis.file.*)
// ============================================================================

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_file_list(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.file_operations_service {
            Some(s) => s,
            None => return not_configured("aegis.file.list", "file operations service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;
        let volume_id = require_str(args, "volume_id")?;
        let path = require_str(args, "path")?;
        let owner = user_sub(caller);
        let vid = parse_volume_id(volume_id)?;

        let entries = svc
            .list_directory(&vid, &tenant_id, &owner, path)
            .await
            .map_err(|e| file_refusal(e, volume_id, path))?;

        ok_direct(serde_json::to_value(entries).unwrap_or(json!([])))
    }

    pub(super) async fn invoke_aegis_file_read(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.file_operations_service {
            Some(s) => s,
            None => return not_configured("aegis.file.read", "file operations service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;
        let volume_id = require_str(args, "volume_id")?;
        let path = require_str(args, "path")?;
        let owner = user_sub(caller);
        let vid = parse_volume_id(volume_id)?;

        let content = svc
            .read_file(&vid, &tenant_id, &owner, path)
            .await
            .map_err(|e| file_refusal(e, volume_id, path))?;

        // Return text content as JSON string; binary as base64
        let text = String::from_utf8(content.data.clone())
            .unwrap_or_else(|_| base64_encode(&content.data));

        ok_direct(json!({
            "content": text,
            "content_type": content.content_type,
            "size_bytes": content.data.len(),
        }))
    }

    pub(super) async fn invoke_aegis_file_write(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.file_operations_service {
            Some(s) => s,
            None => return not_configured("aegis.file.write", "file operations service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;
        let volume_id = require_str(args, "volume_id")?;
        let path = require_str(args, "path")?;
        let content = require_str(args, "content")?;
        let owner = user_sub(caller);
        let tier = user_tier(caller);
        let vid = parse_volume_id(volume_id)?;

        svc.write_file_for_tier(&vid, &tenant_id, &owner, path, content.as_bytes(), &tier)
            .await
            .map_err(|e| file_refusal(e, volume_id, path))?;

        ok_direct(json!({"success": true}))
    }

    pub(super) async fn invoke_aegis_file_delete(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.file_operations_service {
            Some(s) => s,
            None => return not_configured("aegis.file.delete", "file operations service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;
        let volume_id = require_str(args, "volume_id")?;
        let path = require_str(args, "path")?;
        let owner = user_sub(caller);
        let vid = parse_volume_id(volume_id)?;

        svc.delete_path(&vid, &tenant_id, &owner, path)
            .await
            .map_err(|e| file_refusal(e, volume_id, path))?;

        ok_direct(json!({"success": true}))
    }

    pub(super) async fn invoke_aegis_file_mkdir(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.file_operations_service {
            Some(s) => s,
            None => return not_configured("aegis.file.mkdir", "file operations service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;
        let volume_id = require_str(args, "volume_id")?;
        let path = require_str(args, "path")?;
        let owner = user_sub(caller);
        let vid = parse_volume_id(volume_id)?;

        svc.create_directory(&vid, &tenant_id, &owner, path)
            .await
            .map_err(|e| file_refusal(e, volume_id, path))?;

        ok_direct(json!({"success": true}))
    }

    // ========================================================================
    // Volume operations (aegis.volume.*)
    // ========================================================================

    pub(super) async fn invoke_aegis_volume_create(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.user_volume_service {
            Some(s) => s,
            None => return not_configured("aegis.volume.create", "user volume service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let label = require_str(args, "label")?;
        let size_limit_bytes = require_i64(args, "size_limit_bytes")? as u64;
        let owner = user_sub(caller);
        let tier = user_tier(caller);

        let cmd = crate::application::volume_manager::CreateUserVolumeCommand {
            tenant_id,
            owner_user_id: owner,
            label: label.to_string(),
            size_limit_bytes,
            zaru_tier: tier,
        };

        let vol = svc
            .create_volume(cmd)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(json!({
            "id": vol.id.to_string(),
            "name": vol.name,
            "status": format!("{:?}", vol.status),
            "size_limit_bytes": vol.size_limit_bytes,
            "created_at": vol.created_at,
        }))
    }

    pub(super) async fn invoke_aegis_volume_list(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.user_volume_service {
            Some(s) => s,
            None => return not_configured("aegis.volume.list", "user volume service"),
        };
        let owner = user_sub(caller);
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;

        let vols = svc
            .list_volumes(&tenant_id, &owner)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        let items: Vec<Value> = vols
            .into_iter()
            .map(|v| {
                json!({
                    "id": v.id.to_string(),
                    "name": v.name,
                    "status": format!("{:?}", v.status),
                    "size_limit_bytes": v.size_limit_bytes,
                    "created_at": v.created_at,
                })
            })
            .collect();

        ok_direct(json!(items))
    }

    pub(super) async fn invoke_aegis_volume_delete(
        &self,
        args: &Value,
        caller: Option<&UserIdentity>,
    ) -> ToolResult {
        let svc = match &self.user_volume_service {
            Some(s) => s,
            None => return not_configured("aegis.volume.delete", "user volume service"),
        };
        let volume_id = require_str(args, "volume_id")?;
        let owner = user_sub(caller);
        let vid = parse_volume_id(volume_id)?;

        svc.delete_volume(&vid, &owner)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(json!({"success": true}))
    }

    pub(super) async fn invoke_aegis_volume_quota(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.user_volume_service {
            Some(s) => s,
            None => return not_configured("aegis.volume.quota", "user volume service"),
        };
        let owner = user_sub(caller);
        let tier = user_tier(caller);
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;

        let usage = svc
            .get_quota_usage(&tenant_id, &owner, &tier)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(json!({
            "volume_count": usage.volume_count,
            "total_bytes_used": usage.total_bytes_used,
            "max_volumes": usage.tier_limit.max_volumes,
            "total_storage_bytes": usage.tier_limit.total_storage_bytes,
            "max_file_size_bytes": usage.tier_limit.max_file_size_bytes,
        }))
    }

    // ========================================================================
    // Git operations (aegis.git.*)
    // ========================================================================

    pub(super) async fn invoke_aegis_git_clone(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.clone", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let repo_url = require_str(args, "repo_url")?;
        let label = require_str(args, "label")?;
        let owner = user_sub(caller);
        let tier = user_tier(caller);

        let credential_binding_id = args
            .get("credential_binding_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .map(crate::domain::credential::CredentialBindingId);

        let git_ref = args
            .get("git_ref")
            .and_then(|v| {
                let kind = v.get("kind").and_then(|k| k.as_str())?;
                let value = v.get("value").and_then(|k| k.as_str())?;
                match kind {
                    "branch" => Some(crate::domain::git_repo::GitRef::Branch(value.to_string())),
                    "tag" => Some(crate::domain::git_repo::GitRef::Tag(value.to_string())),
                    "commit" => Some(crate::domain::git_repo::GitRef::Commit(value.to_string())),
                    _ => None,
                }
            })
            .unwrap_or_default();

        let sparse_paths = args.get("sparse_paths").and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
        });

        let auto_refresh = args
            .get("auto_refresh")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let shallow = args
            .get("shallow")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let ssh_host_keys: Vec<String> = args
            .get("ssh_host_keys")
            .and_then(|v| v.as_array())
            .map(|keys| {
                keys.iter()
                    .filter_map(|k| k.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();

        let cmd = crate::application::git_repo_service::CreateGitRepoCommand {
            tenant_id,
            owner,
            zaru_tier: tier,
            credential_binding_id,
            repo_url: repo_url.into(),
            git_ref,
            sparse_paths,
            label: label.to_string(),
            auto_refresh,
            shallow,
            ssh_host_keys,
        };

        let binding = svc
            .create_binding(cmd)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        // Spawn background clone
        let svc_bg = svc.clone();
        let binding_id = binding.id;
        tokio::spawn(async move {
            if let Err(e) = svc_bg.clone_repo(&binding_id).await {
                tracing::warn!(%binding_id, error = %e, "background clone failed");
            }
        });

        ok_direct(redacted_binding(&binding))
    }

    pub(super) async fn invoke_aegis_git_list(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.list", "git repo service"),
        };
        let owner = user_sub(caller);
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;

        let bindings = svc
            .list_bindings(&tenant_id, &owner)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        let items: Vec<Value> = bindings.iter().map(redacted_binding).collect();
        ok_direct(json!(items))
    }

    pub(super) async fn invoke_aegis_git_status(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.status", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let owner = user_sub(caller);
        if let Some(run) = self.run_of_git_call(execution_id).await {
            let (label, bid, branch) = self.run_repository(&run, args, &tenant_id, &owner).await?;
            let tree = svc
                .status_for_run(run.run, &bid, &tenant_id, &owner)
                .await
                .map_err(IntoRefusal::into_refusal)?;
            return ok_direct(json!({
                "repository": label,
                "branch": branch,
                "status": if tree.clean { "clean" } else { "changed" },
                "last_commit_sha": tree.head,
            }));
        }
        let binding_id = require_str(args, "binding_id")?;
        let bid = parse_binding_id(binding_id)?;

        let binding = svc
            .get_binding(&bid, &tenant_id, &owner)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(redacted_binding(&binding))
    }

    pub(super) async fn invoke_aegis_git_refresh(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.refresh", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let binding_id = require_str(args, "binding_id")?;
        let owner = user_sub(caller);
        let bid = parse_binding_id(binding_id)?;

        svc.refresh_repo(&bid, &tenant_id, &owner)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(json!({"success": true}))
    }

    pub(super) async fn invoke_aegis_git_delete(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.delete", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let binding_id = require_str(args, "binding_id")?;
        let owner = user_sub(caller);
        let bid = parse_binding_id(binding_id)?;

        svc.delete_binding(&bid, &tenant_id, &owner)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(json!({"success": true}))
    }

    pub(super) async fn invoke_aegis_git_commit(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.commit", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let owner = user_sub(caller);
        if let Some(run) = self.run_of_git_call(execution_id).await {
            let (label, bid, branch) = self.run_repository(&run, args, &tenant_id, &owner).await?;
            let message = require_str(args, "message")?;
            // The run's person, as stored in its entry when its repositories
            // were prepared; else the platform's author, as before (AEGIS
            // ADR-136 G5d).
            let (author_name, author_email) = match run
                .entries
                .iter()
                .find(|entry| entry.binding_id == bid)
                .and_then(|entry| entry.author.clone())
            {
                Some(author) => (author.name, author.email),
                None => commit_author(caller),
            };
            let commit_sha = svc
                .commit_for_run(
                    run.run,
                    &bid,
                    &tenant_id,
                    &owner,
                    message,
                    &author_name,
                    &author_email,
                )
                .await
                .map_err(IntoRefusal::into_refusal)?;
            self.event_bus.publish_execution_event(
                crate::domain::events::ExecutionEvent::RepositoryCommitted {
                    execution_id,
                    agent_id: run.agent_id,
                    label,
                    branch,
                    commit_sha: commit_sha.clone(),
                    author: author_name,
                    committed_at: chrono::Utc::now(),
                },
            );
            return ok_direct(json!({"commit_sha": commit_sha}));
        }
        let binding_id = require_str(args, "binding_id")?;
        let message = require_str(args, "message")?;
        let bid = parse_binding_id(binding_id)?;
        let (author_name, author_email) = commit_author(caller);

        let commit_sha = svc
            .commit(
                &bid,
                &tenant_id,
                &owner,
                message,
                &author_name,
                &author_email,
            )
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(json!({"commit_sha": commit_sha}))
    }

    pub(super) async fn invoke_aegis_git_push(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.push", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let owner = user_sub(caller);
        if let Some(run) = self.run_of_git_call(execution_id).await {
            // Only the work branch goes, to origin: no ref or remote is read
            // (AEGIS ADR-136 G8, G7a).
            let (label, bid, branch) = self.run_repository(&run, args, &tenant_id, &owner).await?;
            let pushed = svc
                .push_for_run(run.run, &bid, &tenant_id, &owner, &branch)
                .await
                .map_err(IntoRefusal::into_refusal)?;
            self.event_bus.publish_execution_event(
                crate::domain::events::ExecutionEvent::RepositoryPushed {
                    execution_id,
                    agent_id: run.agent_id,
                    label,
                    branch: pushed.branch.clone(),
                    remote_url: crate::domain::secrets::RedactedUrl::new(&pushed.remote_url),
                    branch_url: crate::domain::secrets::RedactedUrl::new(&pushed.branch_url),
                    pushed_at: chrono::Utc::now(),
                },
            );
            return ok_direct(json!({
                "branch": pushed.branch,
                "remote_url": pushed.remote_url,
            }));
        }
        let binding_id = require_str(args, "binding_id")?;
        let bid = parse_binding_id(binding_id)?;

        let remote = args.get("remote").and_then(|v| v.as_str());
        let ref_name = args.get("ref").and_then(|v| v.as_str());

        svc.push(&bid, &tenant_id, &owner, remote, ref_name)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(json!({"success": true}))
    }

    pub(super) async fn invoke_aegis_git_diff(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.diff", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let owner = user_sub(caller);
        let staged = args
            .get("staged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(run) = self.run_of_git_call(execution_id).await {
            let (_, bid, _) = self.run_repository(&run, args, &tenant_id, &owner).await?;
            let diff_text = svc
                .diff_for_run(run.run, &bid, &tenant_id, &owner, staged)
                .await
                .map_err(IntoRefusal::into_refusal)?;
            return ok_direct(json!({"diff": diff_text}));
        }
        let binding_id = require_str(args, "binding_id")?;
        let bid = parse_binding_id(binding_id)?;

        let diff_text = svc
            .diff(&bid, &tenant_id, &owner, staged)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(json!({"diff": diff_text}))
    }

    /// The run a git call is made in, when its execution has a record
    /// (AEGIS ADR-136 G7, G7a): the run (its root's workflow execution, or
    /// its root agent execution), the calling agent, and the repositories
    /// the run was given. A call with no execution record is the
    /// companion's, outside any run.
    async fn run_of_git_call(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> Option<RunGitCall> {
        let execution = self
            .execution_service
            .get_execution_unscoped(execution_id)
            .await
            .ok()?;
        let root_id = execution
            .hierarchy
            .path
            .first()
            .copied()
            .unwrap_or(execution.id);
        let root = if root_id == execution.id {
            None
        } else {
            self.execution_service
                .get_execution_unscoped(root_id)
                .await
                .ok()
        };
        let run = {
            let root = root.as_ref().unwrap_or(&execution);
            root.input.workflow_execution_id.unwrap_or(root.id.0)
        };
        let entries = execution
            .input
            .input
            .get(crate::domain::git_repo::REPOSITORIES_INPUT_KEY)
            .and_then(|value| crate::domain::git_repo::parse_run_repositories(value).ok())
            .unwrap_or_default();
        Some(RunGitCall {
            run,
            agent_id: execution.agent_id,
            entries,
        })
    }

    /// The binding the run mounted at the label `repository` names, with the
    /// label and the run's work branch; never an id the model typed (AEGIS
    /// ADR-136 G7, G7a).
    async fn run_repository(
        &self,
        run: &RunGitCall,
        args: &Value,
        tenant_id: &crate::domain::tenant::TenantId,
        owner: &str,
    ) -> Result<(String, crate::domain::git_repo::GitRepoBindingId, String), SealSessionError> {
        let label = args
            .get("repository")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if let Some(svc) = &self.git_repo_service {
            for entry in &run.entries {
                let Ok(binding) = svc.get_binding(&entry.binding_id, tenant_id, owner).await else {
                    continue;
                };
                if binding.label == label {
                    let branch = entry
                        .branch
                        .clone()
                        .unwrap_or_else(|| crate::domain::git_repo::default_work_branch(run.run));
                    return Ok((label, binding.id, branch));
                }
            }
        }
        Err(SealSessionError::InvalidArguments(format!(
            "repository '{label}' is not one of this run's repositories"
        )))
    }

    // ========================================================================
    // Script operations (aegis.script.*)
    // ========================================================================

    pub(super) async fn invoke_aegis_script_save(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.script_service {
            Some(s) => s,
            None => return not_configured("aegis.script.save", "script service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let name = require_str(args, "name")?;
        let code = require_str(args, "code")?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tags = args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        let owner = user_sub(caller);
        let tier = user_tier(caller);

        let cmd = crate::application::script_service::CreateScriptCommand {
            tenant_id,
            created_by: owner,
            zaru_tier: tier,
            name: name.to_string(),
            description,
            code: code.to_string(),
            tags,
        };

        let script = svc.create(cmd).await.map_err(IntoRefusal::into_refusal)?;
        ok_direct(script_dto(&script))
    }

    pub(super) async fn invoke_aegis_script_list(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.script_service {
            Some(s) => s,
            None => return not_configured("aegis.script.list", "script service"),
        };
        let owner = user_sub(caller);
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;

        let tag = args.get("tag").and_then(|v| v.as_str());
        let query = args.get("q").and_then(|v| v.as_str());

        let scripts = svc
            .list_filtered(&tenant_id, &owner, tag, query)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        let items: Vec<Value> = scripts.iter().map(script_dto).collect();
        ok_direct(json!(items))
    }

    pub(super) async fn invoke_aegis_script_get(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.script_service {
            Some(s) => s,
            None => return not_configured("aegis.script.get", "script service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let id = require_str(args, "id")?;
        let owner = user_sub(caller);
        let script_id = parse_script_id(id)?;

        let script = svc
            .get(&script_id, &tenant_id, &owner)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        let versions = svc
            .list_versions(&script_id, &tenant_id, &owner)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        let mut body = script_dto(&script);
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "versions".to_string(),
                json!(versions
                    .iter()
                    .map(|v| json!({
                        "version": v.version,
                        "updated_at": v.updated_at,
                        "updated_by": v.updated_by,
                    }))
                    .collect::<Vec<_>>()),
            );
        }

        ok_direct(body)
    }

    pub(super) async fn invoke_aegis_script_update(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.script_service {
            Some(s) => s,
            None => return not_configured("aegis.script.update", "script service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let id = require_str(args, "id")?;
        let name = require_str(args, "name")?;
        let code = require_str(args, "code")?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tags = args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        let owner = user_sub(caller);
        let script_id = parse_script_id(id)?;

        let cmd = crate::application::script_service::UpdateScriptCommand {
            name: name.to_string(),
            description,
            code: code.to_string(),
            tags,
        };

        let script = svc
            .update(&script_id, &tenant_id, &owner, cmd)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(script_dto(&script))
    }

    pub(super) async fn invoke_aegis_script_delete(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.script_service {
            Some(s) => s,
            None => return not_configured("aegis.script.delete", "script service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let id = require_str(args, "id")?;
        let owner = user_sub(caller);
        let script_id = parse_script_id(id)?;

        svc.delete(&script_id, &tenant_id, &owner)
            .await
            .map_err(IntoRefusal::into_refusal)?;

        ok_direct(json!({"success": true}))
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn parse_volume_id(s: &str) -> Result<crate::domain::volume::VolumeId, SealSessionError> {
    let uuid = uuid::Uuid::parse_str(s)
        .map_err(|e| SealSessionError::InvalidArguments(format!("invalid volume_id '{s}': {e}")))?;
    Ok(crate::domain::volume::VolumeId(uuid))
}

fn parse_binding_id(
    s: &str,
) -> Result<crate::domain::git_repo::GitRepoBindingId, SealSessionError> {
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| {
        SealSessionError::InvalidArguments(format!("invalid binding_id '{s}': {e}"))
    })?;
    Ok(crate::domain::git_repo::GitRepoBindingId(uuid))
}

fn parse_script_id(s: &str) -> Result<crate::domain::script::ScriptId, SealSessionError> {
    let uuid = uuid::Uuid::parse_str(s)
        .map_err(|e| SealSessionError::InvalidArguments(format!("invalid script id '{s}': {e}")))?;
    Ok(crate::domain::script::ScriptId(uuid))
}

/// The view of a binding that the git tools return to an agent. The agent's
/// model provider reads it, so the URL goes through [`RedactedUrl`]: the
/// repository's address with no user info and no secret query value. Clone,
/// fetch and push take the credential from the stored binding, so an agent
/// never needs it.
///
/// [`RedactedUrl`]: crate::domain::secrets::RedactedUrl
fn redacted_binding(b: &crate::domain::git_repo::GitRepoBinding) -> Value {
    json!({
        "id": b.id.0,
        "tenant_id": b.tenant_id,
        "credential_binding_id": b.credential_binding_id.map(|c| c.0),
        "repo_url": crate::domain::secrets::RedactedUrl::from(&b.repo_url),
        "git_ref": b.git_ref,
        "sparse_paths": b.sparse_paths,
        "volume_id": b.volume_id.0,
        "label": b.label,
        "status": b.status,
        "clone_strategy": b.clone_strategy,
        "last_cloned_at": b.last_cloned_at,
        "last_commit_sha": b.last_commit_sha,
        "auto_refresh": b.auto_refresh,
        // Audit 002 §4.37.13 — `webhook_secret` is transient (cleartext is
        // never stored). Probe the persisted ciphertext column instead.
        "webhook_secret_set": b.webhook_secret_ciphertext.is_some(),
        "created_at": b.created_at,
        "updated_at": b.updated_at,
    })
}

fn script_dto(s: &crate::domain::script::Script) -> Value {
    json!({
        "id": s.id.0,
        "tenant_id": s.tenant_id,
        "created_by": s.created_by,
        "name": s.name,
        "description": s.description,
        "code": s.code,
        "tags": s.tags,
        "visibility": s.visibility.as_str(),
        "version": s.version,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

// ============================================================================
// The workflow interpreter's repository steps (AEGIS ADR-141 F5, F8)
// ============================================================================

/// The security context a workflow's landing is stored at the approval gate
/// under: the interpreter's call carries none of an agent's, and the
/// context an identity-less interpreter call runs under elsewhere is this.
const LANDING_SECURITY_CONTEXT: &str = "aegis-system-operator";

/// How often a landing waiting at the gate reads its request's answer.
const LANDING_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// The repository a workflow run holds, as its steps act on it: its first,
/// as the blackboard names it (AEGIS ADR-141 F3).
struct WorkflowRunRepository {
    run: uuid::Uuid,
    person: String,
    label: String,
    binding_id: crate::domain::git_repo::GitRepoBindingId,
    branch: String,
    git_ref: String,
    /// The binding's ref as it was bound: a landing goes only to a branch.
    binding_ref: crate::domain::git_repo::GitRef,
    author: Option<crate::domain::git_repo::RunAuthor>,
}

/// The sentence a repository step answers for a refusal of the git path,
/// or `None` for a failure.
fn step_sentence(e: &GitRepoError) -> Option<String> {
    match e {
        GitRepoError::NothingToCommit
        | GitRepoError::RemoteAhead { .. }
        | GitRepoError::RefAhead { .. }
        | GitRepoError::NotABranch { .. }
        | GitRepoError::HeldByRun { .. }
        | GitRepoError::CredentialNotYours
        | GitRepoError::BindingBusy(_)
        | GitRepoError::NotYetImplemented(_) => Some(e.to_string()),
        GitRepoError::GitFailed(text)
            if text.contains(crate::application::git_clone_executor::DIFF_TOO_LARGE) =>
        {
            Some(crate::application::git_clone_executor::DIFF_TOO_LARGE.to_string())
        }
        _ => None,
    }
}

impl ToolInvocationService {
    /// The repository the workflow run `run` holds, read from its record in
    /// `tenant_id`; `Err` of `Ok` a refusal's sentence.
    async fn workflow_run_repository(
        &self,
        tenant_id: &crate::domain::tenant::TenantId,
        run: uuid::Uuid,
    ) -> Result<Result<WorkflowRunRepository, String>, RepositoryActionError> {
        let executions =
            self.workflow_execution_repo
                .as_ref()
                .ok_or(RepositoryActionError::NotConfigured(
                    "the workflow execution store",
                ))?;
        let svc = self
            .git_repo_service
            .as_ref()
            .ok_or(RepositoryActionError::NotConfigured(
                "the git repository service",
            ))?;
        let execution = executions
            .find_by_id_for_tenant(tenant_id, crate::domain::execution::ExecutionId(run))
            .await
            .map_err(|e| RepositoryActionError::Failed(e.to_string()))?
            .ok_or(RepositoryActionError::RunNotFound(run))?;
        let entries = match execution
            .input
            .get(crate::domain::git_repo::REPOSITORIES_INPUT_KEY)
        {
            None => Vec::new(),
            Some(value) => match crate::domain::git_repo::parse_run_repositories(value) {
                Ok(entries) => entries,
                Err(sentence) => return Ok(Err(sentence.to_string())),
            },
        };
        let Some(entry) = entries.into_iter().next() else {
            return Ok(Err(RUN_HOLDS_NO_REPOSITORY.to_string()));
        };
        let person = execution.initiating_user_sub.clone().unwrap_or_default();
        let binding = match svc.get_binding(&entry.binding_id, tenant_id, &person).await {
            Ok(binding) => binding,
            Err(GitRepoError::BindingNotFound | GitRepoError::NotOwned) => {
                return Ok(Err(RUN_HOLDS_NO_REPOSITORY.to_string()))
            }
            Err(e) => return Err(RepositoryActionError::Failed(e.to_string())),
        };
        let git_ref = match &binding.git_ref {
            crate::domain::git_repo::GitRef::Branch(name)
            | crate::domain::git_repo::GitRef::Tag(name)
            | crate::domain::git_repo::GitRef::Commit(name) => name.clone(),
        };
        Ok(Ok(WorkflowRunRepository {
            run,
            person,
            label: binding.label.clone(),
            binding_id: binding.id,
            branch: entry
                .branch
                .unwrap_or_else(|| crate::domain::git_repo::default_work_branch(run)),
            git_ref,
            binding_ref: binding.git_ref.clone(),
            author: entry.author,
        }))
    }

    /// One of the workflow interpreter's own repository steps on the
    /// repository the workflow run `run` holds (AEGIS ADR-141 F5): `diff`
    /// (unstaged changes, as `aegis.git.diff` answers them), `commit` (every
    /// change on the work branch, as the run's person) or `land` (through
    /// the person's approval, F8). A refusal answers its sentence; a run
    /// holding no repository answers "this run holds no repository".
    pub async fn run_repository_action(
        &self,
        tenant_id: &crate::domain::tenant::TenantId,
        run: uuid::Uuid,
        action: &str,
        message: Option<&str>,
    ) -> Result<RepositoryActionAnswer, RepositoryActionError> {
        if !matches!(action, "diff" | "commit" | "land") {
            return Err(RepositoryActionError::UnknownAction(action.to_string()));
        }
        let repository = match self.workflow_run_repository(tenant_id, run).await? {
            Ok(repository) => repository,
            Err(sentence) => return Ok(RepositoryActionAnswer::refused(sentence)),
        };
        let svc = self
            .git_repo_service
            .as_ref()
            .ok_or(RepositoryActionError::NotConfigured(
                "the git repository service",
            ))?;
        let answer = RepositoryActionAnswer {
            branch: repository.branch.clone(),
            git_ref: repository.git_ref.clone(),
            ..RepositoryActionAnswer::default()
        };
        let refused_or_failed =
            |e: GitRepoError, answer: RepositoryActionAnswer| match step_sentence(&e) {
                Some(sentence) => Ok(RepositoryActionAnswer {
                    sentence: Some(sentence),
                    ..answer
                }),
                None => Err(RepositoryActionError::Failed(e.to_string())),
            };
        match action {
            "diff" => match svc
                .diff_for_run(
                    repository.run,
                    &repository.binding_id,
                    tenant_id,
                    &repository.person,
                    false,
                )
                .await
            {
                Ok(diff) => Ok(RepositoryActionAnswer {
                    diff: Some(diff),
                    ..answer
                }),
                Err(e) => refused_or_failed(e, answer),
            },
            "commit" => {
                let message = message
                    .filter(|m| !m.trim().is_empty())
                    .ok_or(RepositoryActionError::NoMessage)?;
                let (author_name, author_email) = match &repository.author {
                    Some(author) => (author.name.clone(), author.email.clone()),
                    None => commit_author(None),
                };
                match svc
                    .commit_for_run(
                        repository.run,
                        &repository.binding_id,
                        tenant_id,
                        &repository.person,
                        message,
                        &author_name,
                        &author_email,
                    )
                    .await
                {
                    Ok(commit_sha) => Ok(RepositoryActionAnswer {
                        commit_sha: Some(commit_sha),
                        ..answer
                    }),
                    Err(e) => refused_or_failed(e, answer),
                }
            }
            _ => self.land_through_gate(tenant_id, repository, answer).await,
        }
    }

    /// A workflow's landing (AEGIS ADR-141 F8): a ref that is not a branch
    /// refused before anything; the call `aegis.git.land` asked of the run's
    /// person at the approval gate, in the run's own execution, as
    /// `aegis.git.push` is; answered with what the landing returned once the
    /// person approved, or the gate's refusal when they did not, nothing
    /// pushed.
    async fn land_through_gate(
        &self,
        tenant_id: &crate::domain::tenant::TenantId,
        repository: WorkflowRunRepository,
        answer: RepositoryActionAnswer,
    ) -> Result<RepositoryActionAnswer, RepositoryActionError> {
        use crate::application::tool_approval_service::{GateOutcome, GatedCall};
        use crate::domain::tool_approval::ToolApprovalStatus;

        if let Err(e) = crate::application::git_repo_service::landing_ref(&repository.binding_ref) {
            return Ok(RepositoryActionAnswer {
                sentence: Some(e.to_string()),
                ..answer
            });
        }
        let land_tool = crate::infrastructure::tool_router::LAND_TOOL;
        let arguments = json!({ "repository": repository.label });
        let execution_id = crate::domain::execution::ExecutionId(repository.run);
        let outcome = match &self.tool_approval_service {
            None => None,
            Some(approvals) if self.tool_router.requires_approval(land_tool) => Some(
                approvals
                    .gate(GatedCall {
                        tenant_id,
                        user_sub: Some(repository.person.as_str()).filter(|p| !p.is_empty()),
                        execution_id,
                        agent_id: crate::domain::agent::AgentId(uuid::Uuid::nil()),
                        tool_name: land_tool,
                        arguments: &arguments,
                        security_context_name: LANDING_SECURITY_CONTEXT,
                        conversation_id: None,
                        profile_id: None,
                        contract: self.tool_router.approval_contract(land_tool),
                    })
                    .await
                    .map_err(|e| RepositoryActionError::Failed(e.to_string()))?,
            ),
            Some(_) => None,
        };
        let landed = match outcome {
            // No gate on this node, or the tool is not gated: the landing
            // runs at once, as an ungated push does.
            None => {
                self.land_for_interpreter(tenant_id, execution_id, &repository.person)
                    .await
            }
            Some(GateOutcome::Proceed { approval_id }) => {
                let landed = self
                    .land_for_interpreter(tenant_id, execution_id, &repository.person)
                    .await;
                if let Some(approvals) = &self.tool_approval_service {
                    let recorded = landed.clone().map_err(|e| e.to_string());
                    if let Err(e) = approvals.record_outcome(approval_id, &recorded).await {
                        tracing::warn!(approval_id = %approval_id, error = %e, "Failed to record the outcome of an auto-allowed landing");
                    }
                }
                landed
            }
            Some(GateOutcome::Denied { .. }) => {
                return Ok(RepositoryActionAnswer {
                    sentence: Some(landing_not_approved(ToolApprovalStatus::AutoDenied)),
                    ..answer
                })
            }
            Some(GateOutcome::Pending { result }) => {
                let approval_id = result
                    .get("approval_id")
                    .and_then(Value::as_str)
                    .and_then(|id| {
                        crate::domain::tool_approval::ToolApprovalId::from_string(id).ok()
                    })
                    .ok_or_else(|| {
                        RepositoryActionError::Failed(
                            "the approval gate answered no approval id".to_string(),
                        )
                    })?;
                let approvals = self
                    .tool_approval_service
                    .as_ref()
                    .ok_or(RepositoryActionError::NotConfigured("the approval gate"))?;
                loop {
                    let request = approvals
                        .get_for_user(approval_id, tenant_id, &repository.person)
                        .await
                        .map_err(|e| RepositoryActionError::Failed(e.to_string()))?;
                    match request.status {
                        ToolApprovalStatus::Pending => {}
                        ToolApprovalStatus::ApprovedOnce
                        | ToolApprovalStatus::ApprovedAlways
                        | ToolApprovalStatus::AutoAllowed => {
                            if let Some(result) = request.result {
                                break Ok(result);
                            }
                            if let Some(error) = request.error {
                                break Err(RepositoryActionError::Failed(error));
                            }
                        }
                        status => {
                            return Ok(RepositoryActionAnswer {
                                sentence: Some(landing_not_approved(status)),
                                ..answer
                            })
                        }
                    }
                    tokio::time::sleep(LANDING_POLL).await;
                }
                .map(Ok)?
            }
        };
        let landed = landed.map_err(|e| RepositoryActionError::Failed(e.to_string()))?;
        Ok(RepositoryActionAnswer {
            commit_sha: landed
                .get("commit_sha")
                .and_then(Value::as_str)
                .map(str::to_string),
            sentence: landed
                .get("sentence")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..answer
        })
    }

    /// The landing itself, for the workflow execution `execution_id`'s own
    /// step and its person `person` only (AEGIS ADR-141 F8): what
    /// `aegis.git.land` runs once the person approved. Answers the landed
    /// commit, the branch, the ref and the ref's page; a refusal of the git
    /// path answers its sentence under `sentence`, nothing landed.
    pub async fn land_for_interpreter(
        &self,
        tenant_id: &crate::domain::tenant::TenantId,
        execution_id: crate::domain::execution::ExecutionId,
        person: &str,
    ) -> Result<Value, SealSessionError> {
        let not_the_workflows =
            || SealSessionError::InvalidArguments(LAND_IS_THE_WORKFLOWS.to_string());
        let repository = match self
            .workflow_run_repository(tenant_id, execution_id.0)
            .await
        {
            Ok(Ok(repository)) => repository,
            Ok(Err(sentence)) => return Ok(json!({ "sentence": sentence })),
            Err(RepositoryActionError::RunNotFound(_)) => return Err(not_the_workflows()),
            Err(e) => return Err(SealSessionError::InternalError(e.to_string())),
        };
        if repository.person.is_empty() || repository.person != person {
            return Err(not_the_workflows());
        }
        let svc = self.git_repo_service.as_ref().ok_or_else(|| {
            SealSessionError::InternalError("git repo service not configured".to_string())
        })?;
        match svc
            .land_for_run(
                repository.run,
                &repository.binding_id,
                tenant_id,
                &repository.person,
                &repository.branch,
            )
            .await
        {
            Ok(landed) => {
                self.event_bus.publish_execution_event(
                    crate::domain::events::ExecutionEvent::RepositoryLanded {
                        execution_id,
                        label: landed.label.clone(),
                        branch: landed.branch.clone(),
                        git_ref: landed.git_ref.clone(),
                        commit_sha: landed.commit_sha.clone(),
                        branch_url: crate::domain::secrets::RedactedUrl::new(&landed.branch_url),
                        landed_at: chrono::Utc::now(),
                    },
                );
                Ok(json!({
                    "commit_sha": landed.commit_sha,
                    "branch": landed.branch,
                    "ref": landed.git_ref,
                    "branch_url": landed.branch_url,
                }))
            }
            Err(e) => match step_sentence(&e) {
                Some(sentence) => Ok(json!({
                    "sentence": sentence,
                    "branch": repository.branch,
                    "ref": repository.git_ref,
                })),
                None => Err(e.into_refusal()),
            },
        }
    }

    /// `aegis.git.land` (AEGIS ADR-141 F8): answered only for a workflow's
    /// own landing step, which the approval gate runs in the workflow
    /// execution once its person approved; any other caller is refused.
    pub(super) async fn invoke_aegis_git_land(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        scope: &crate::domain::iam::TenantScope,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> ToolResult {
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;
        let person = caller.map(|c| c.sub.as_str()).unwrap_or_default();
        self.land_for_interpreter(&tenant_id, execution_id, person)
            .await
            .map(ToolInvocationResult::Direct)
    }
}

/// The gate's refusal of a landing its person did not approve: nothing was
/// pushed (AEGIS ADR-141 F8).
fn landing_not_approved(status: crate::domain::tool_approval::ToolApprovalStatus) -> String {
    format!("approval request {}; nothing was landed", status.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::git_repo::{CloneStrategy, GitRef, GitRepoBinding};
    use crate::domain::shared_kernel::{TenantId, VolumeId};

    /// `aegis.git.clone`, `aegis.git.list` and `aegis.git.status` answer an
    /// agent with this view of a binding, and the agent's model provider
    /// reads it. It holds the repository's address and nothing of a
    /// credential written into the stored URL: no user info and no secret
    /// query value.
    #[test]
    fn git_tool_answer_holds_the_repository_address_and_no_credential() {
        for (stored, address) in [
            (
                "https://Mk7-git-tool-user:Mk7-git-tool-token@git.example.invalid/o/r.git",
                "git.example.invalid/o/r.git",
            ),
            (
                "https://Mk7-git-tool-token@git.example.invalid/o/r.git?access_token=Mk7-git-tool-query",
                "git.example.invalid/o/r.git",
            ),
            (
                "Mk7-git-tool-user@git.example.invalid:o/r.git",
                "git.example.invalid:o/r.git",
            ),
        ] {
            let binding = GitRepoBinding::new(
                TenantId::consumer(),
                None,
                stored.to_string(),
                GitRef::default(),
                None,
                VolumeId::new(),
                "tool-view".to_string(),
                CloneStrategy::Libgit2,
                false,
                None,
                None,
                None,
            );
            let answer = serde_json::to_string(&redacted_binding(&binding)).unwrap();
            assert!(
                !answer.contains("Mk7-git-tool"),
                "the git tool answer carries a credential from the stored URL: {answer}"
            );
            assert!(
                answer.contains(address),
                "the git tool answer lost the repository's address {address}: {answer}"
            );
        }
    }

    /// A git binding naming a credential that is not the caller's active
    /// one is answered `INVALID_ARGUMENTS` with the sentence alone (AEGIS
    /// ADR-136 G2a).
    #[test]
    fn a_credential_not_the_callers_is_answered_invalid_arguments_with_the_sentence() {
        match GitRepoError::CredentialNotYours.into_refusal() {
            SealSessionError::Answered {
                answer: CallerAnswer::InvalidArguments(message),
                ..
            } => assert_eq!(
                message,
                "The credential named for this repository is not an active credential of yours."
            ),
            other => panic!(
                "a credential not the caller's was not answered INVALID_ARGUMENTS with the sentence: {other:?}"
            ),
        }
    }
}

#[cfg(test)]
#[path = "run_git_tools_tests.rs"]
mod run_git_tools_tests;
