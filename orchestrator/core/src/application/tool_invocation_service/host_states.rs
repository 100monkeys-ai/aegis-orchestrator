// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `aegis.host_state.*` tools (ADR-157 D2): host state through MCP, for
//! Zaru, agents and workflows, over the same HostStateService the
//! `/v1/host-states` routes use.

use super::*;
use crate::application::host_state_service::{HostStatePrincipal, HostStateService};
use crate::domain::iam::{TenantScope, UserIdentity};
use serde_json::json;

pub(super) const HOST_STATE_TOOLS: [&str; 8] = [
    "aegis.host_state.create",
    "aegis.host_state.update",
    "aegis.host_state.get",
    "aegis.host_state.list",
    "aegis.host_state.validate",
    "aegis.host_state.plan",
    "aegis.host_state.apply",
    "aegis.host_state.archive",
];

const NO_HOST_STATES: &str = "Host state is not available on this node.";

fn view(s: &crate::domain::host_state::HostState) -> Value {
    json!({
        "name": s.name,
        "version": s.version,
        "status": s.status.as_str(),
        "description": s.manifest.metadata.description,
        "target": s.manifest.spec.target,
        "resources": s.manifest.spec.resources.len(),
        "manifest_yaml": s.manifest_yaml,
        "created_by": s.created_by,
        "created_at": s.created_at.to_rfc3339(),
    })
}

fn text_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("`{key}` is required."))
}

impl ToolInvocationService {
    /// The host state service the `aegis.host_state.*` tools use, the one
    /// the `/v1/host-states` routes use (ADR-157 D2).
    pub fn with_host_state_service(mut self, service: Arc<HostStateService>) -> Self {
        self.host_state_service = Some(service);
        self
    }

    /// Run one `aegis.host_state.*` tool in the call's tenant. A plan or an
    /// apply is attested as the caller's, through this MCP call (D12).
    pub(super) async fn invoke_aegis_host_state_tool(
        &self,
        tool_name: &str,
        args: &mut Value,
        execution_id: crate::domain::execution::ExecutionId,
        caller_identity: Option<&UserIdentity>,
        scope: &TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant = Self::enforce_tenant_arg(args, scope)?;
        let answer = self
            .host_state_answer(tool_name, args, execution_id, caller_identity, &tenant)
            .await;
        Ok(ToolInvocationResult::Direct(match answer {
            Ok(body) => body,
            Err(sentence) => json!({ "tool": tool_name, "error": sentence }),
        }))
    }

    async fn host_state_answer(
        &self,
        tool_name: &str,
        args: &Value,
        execution_id: crate::domain::execution::ExecutionId,
        caller_identity: Option<&UserIdentity>,
        tenant: &crate::domain::shared_kernel::TenantId,
    ) -> Result<Value, String> {
        if tool_name == "aegis.host_state.validate" {
            let problems = HostStateService::validate(text_arg(args, "manifest_yaml")?);
            return Ok(json!({ "valid": problems.is_empty(), "problems": problems }));
        }
        let service = self
            .host_state_service
            .as_ref()
            .ok_or_else(|| NO_HOST_STATES.to_string())?;
        let actor = caller_identity
            .map(|i| i.sub.clone())
            .unwrap_or_else(|| format!("execution:{execution_id}"));
        let err = |e: crate::application::host_state_service::HostStateError| e.to_string();
        match tool_name {
            "aegis.host_state.create" => {
                let saved = service
                    .register(tenant, text_arg(args, "manifest_yaml")?, &actor)
                    .await
                    .map_err(err)?;
                Ok(json!({ "host_state": view(&saved) }))
            }
            "aegis.host_state.update" => {
                let name = text_arg(args, "name")?;
                let yaml = text_arg(args, "manifest_yaml")?;
                let manifest = crate::domain::host_state::HostStateManifest::parse(yaml)
                    .map_err(|p| format!("the host state is not valid: {}", p.join("; ")))?;
                if manifest.metadata.name != name {
                    return Err(format!(
                        "the manifest names {:?}, not {name:?}",
                        manifest.metadata.name
                    ));
                }
                service.get(tenant, name).await.map_err(err)?;
                let saved = service.register(tenant, yaml, &actor).await.map_err(err)?;
                Ok(json!({ "host_state": view(&saved) }))
            }
            "aegis.host_state.get" => {
                let s = service
                    .get(tenant, text_arg(args, "name")?)
                    .await
                    .map_err(err)?;
                Ok(json!({ "host_state": view(&s) }))
            }
            "aegis.host_state.list" => {
                let list = service.list(tenant).await.map_err(err)?;
                Ok(json!({ "host_states": list.iter().map(view).collect::<Vec<_>>() }))
            }
            "aegis.host_state.archive" => {
                let name = text_arg(args, "name")?;
                service.archive(tenant, name).await.map_err(err)?;
                Ok(json!({ "archived": name }))
            }
            "aegis.host_state.plan" | "aegis.host_state.apply" => {
                let principal = HostStatePrincipal::Attested {
                    principal: actor.clone(),
                    via: "seal_session",
                    reference: execution_id.to_string(),
                };
                let run = service
                    .run(
                        tenant,
                        text_arg(args, "name")?,
                        tool_name == "aegis.host_state.plan",
                        &principal,
                    )
                    .await
                    .map_err(err)?;
                serde_json::to_value(run).map_err(|e| e.to_string())
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
}
