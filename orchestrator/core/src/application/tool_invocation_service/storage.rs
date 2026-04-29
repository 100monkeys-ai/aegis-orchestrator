// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Tool dispatch handlers for storage, volume, git, and script operations.

use serde_json::{json, Value};

use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use crate::domain::seal_session::SealSessionError;

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

fn ok_direct(value: Value) -> ToolResult {
    Ok(ToolInvocationResult::Direct(value))
}

fn internal_err(msg: impl std::fmt::Display) -> SealSessionError {
    SealSessionError::InternalError(msg.to_string())
}

fn not_configured(tool: &str, service: &str) -> ToolResult {
    Err(SealSessionError::InternalError(format!(
        "{tool}: {service} not configured"
    )))
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
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

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

        let vol = svc.create_volume(cmd).await.map_err(internal_err)?;

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
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

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

        let cmd = crate::application::git_repo_service::CreateGitRepoCommand {
            tenant_id,
            owner,
            zaru_tier: tier,
            credential_binding_id,
            repo_url: repo_url.to_string(),
            git_ref,
            sparse_paths,
            label: label.to_string(),
            auto_refresh,
            shallow,
        };

        let binding = svc.create_binding(cmd).await.map_err(internal_err)?;

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
            .map_err(internal_err)?;

        let items: Vec<Value> = bindings.iter().map(redacted_binding).collect();
        ok_direct(json!(items))
    }

    pub(super) async fn invoke_aegis_git_status(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.status", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let binding_id = require_str(args, "binding_id")?;
        let owner = user_sub(caller);
        let bid = parse_binding_id(binding_id)?;

        let binding = svc
            .get_binding(&bid, &tenant_id, &owner)
            .await
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

        ok_direct(json!({"success": true}))
    }

    pub(super) async fn invoke_aegis_git_commit(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.commit", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let binding_id = require_str(args, "binding_id")?;
        let message = require_str(args, "message")?;
        let owner = user_sub(caller);
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
            .map_err(internal_err)?;

        ok_direct(json!({"commit_sha": commit_sha}))
    }

    pub(super) async fn invoke_aegis_git_push(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.push", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let binding_id = require_str(args, "binding_id")?;
        let owner = user_sub(caller);
        let bid = parse_binding_id(binding_id)?;

        let remote = args.get("remote").and_then(|v| v.as_str());
        let ref_name = args.get("ref").and_then(|v| v.as_str());

        svc.push(&bid, &tenant_id, &owner, remote, ref_name)
            .await
            .map_err(internal_err)?;

        ok_direct(json!({"success": true}))
    }

    pub(super) async fn invoke_aegis_git_diff(
        &self,
        args: &mut Value,
        caller: Option<&UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> ToolResult {
        let svc = match &self.git_repo_service {
            Some(s) => s,
            None => return not_configured("aegis.git.diff", "git repo service"),
        };
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let binding_id = require_str(args, "binding_id")?;
        let owner = user_sub(caller);
        let bid = parse_binding_id(binding_id)?;

        let staged = args
            .get("staged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let diff_text = svc
            .diff(&bid, &tenant_id, &owner, staged)
            .await
            .map_err(internal_err)?;

        ok_direct(json!({"diff": diff_text}))
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

        let script = svc.create(cmd).await.map_err(internal_err)?;
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
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

        let versions = svc
            .list_versions(&script_id, &tenant_id, &owner)
            .await
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

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
            .map_err(internal_err)?;

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

fn redacted_binding(b: &crate::domain::git_repo::GitRepoBinding) -> Value {
    json!({
        "id": b.id.0,
        "tenant_id": b.tenant_id,
        "credential_binding_id": b.credential_binding_id.map(|c| c.0),
        "repo_url": b.repo_url,
        "git_ref": b.git_ref,
        "sparse_paths": b.sparse_paths,
        "volume_id": b.volume_id.0,
        "label": b.label,
        "status": b.status,
        "clone_strategy": b.clone_strategy,
        "last_cloned_at": b.last_cloned_at,
        "last_commit_sha": b.last_commit_sha,
        "auto_refresh": b.auto_refresh,
        "webhook_secret_set": b.webhook_secret.is_some(),
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
