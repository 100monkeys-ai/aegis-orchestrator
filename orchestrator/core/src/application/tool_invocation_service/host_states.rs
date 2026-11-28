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

/// What a workflow's host action answers (ADR-157 D4): its result, whether
/// it did what it was asked, and a refusal sentence when it did not act.
#[derive(Debug, Clone, Default)]
pub struct HostActionAnswer {
    pub result: Value,
    pub ok: bool,
    pub sentence: Option<String>,
}

impl HostActionAnswer {
    fn refused(sentence: impl Into<String>) -> Self {
        Self {
            result: Value::Null,
            ok: false,
            sentence: Some(sentence.into()),
        }
    }
}

/// A rendered argument: a template renders to a string, so a JSON-shaped
/// value (a target, a tool's args) is read back as JSON.
/// What a workflow's `tool_call` may not name: AEGIS's own tools, which a
/// workflow reaches through their own actions (host states, fleets,
/// repositories) under their own rules.
fn tool_call_target(tool: &str) -> Result<(&str, &str), String> {
    let Some((server, name)) = tool.split_once('.') else {
        return Err(format!(
            "`tool` {tool:?} must be <server>.<tool>, e.g. cloudflare.execute"
        ));
    };
    if server.is_empty() || name.is_empty() {
        return Err(format!("`tool` {tool:?} must be <server>.<tool>"));
    }
    if server == "aegis" || server == "host" || server == "cmd" || server == "fs" {
        return Err(format!(
            "`tool_call` reaches remote servers; {tool} is AEGIS's own: use its action (host_state_apply, edge_fleet_invoke, …)"
        ));
    }
    Ok((server, name))
}

fn arg_json(args: &serde_json::Map<String, Value>, key: &str) -> Option<Value> {
    match args.get(key)? {
        Value::String(s) => serde_json::from_str(s)
            .ok()
            .or_else(|| Some(Value::String(s.clone()))),
        other => Some(other.clone()),
    }
}

fn arg_u64(args: &serde_json::Map<String, Value>, key: &str, default: u64) -> u64 {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(default),
        _ => default,
    }
}

impl ToolInvocationService {
    /// Run one of a workflow's host actions (ADR-157 D4) in its run's
    /// tenant, attested as the run (D12).
    pub async fn run_host_action(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
        run: uuid::Uuid,
        action: &str,
        args_json: &str,
    ) -> HostActionAnswer {
        let args: serde_json::Map<String, Value> = match serde_json::from_str(args_json) {
            Ok(Value::Object(m)) => m,
            _ => return HostActionAnswer::refused("the action's arguments are not a JSON object"),
        };
        if action == "http_check" {
            return http_check(&args).await;
        }
        if action == "tool_call" {
            return self.tool_call(tenant, run, &args).await;
        }
        if self.host_state_service.is_none() {
            return HostActionAnswer::refused(NO_HOST_STATES);
        }
        // The run's person: who the run acts for, attested to the edges (D12)
        // and the one whose approval a changing action waits for.
        let person = match self.workflow_execution_repo.as_ref() {
            Some(executions) => match executions
                .find_by_id_for_tenant(tenant, crate::domain::execution::ExecutionId(run))
                .await
            {
                Ok(Some(execution)) => execution.initiating_user_sub.clone().unwrap_or_default(),
                Ok(None) => return HostActionAnswer::refused(format!("no workflow run {run}")),
                Err(e) => return HostActionAnswer::refused(e.to_string()),
            },
            None => {
                return HostActionAnswer::refused("the workflow execution store is not configured")
            }
        };
        let principal = HostStatePrincipal::Attested {
            principal: if person.is_empty() {
                format!("workflow_run:{run}")
            } else {
                person.clone()
            },
            via: "workflow_run",
            reference: run.to_string(),
        };
        // A changing action waits at the approval gate as the tool it is,
        // so a person's "always allow" for it lets their deploys run alone.
        let gate_tool: Option<&'static str> = match action {
            "host_state_apply" => Some("aegis.host_state.apply"),
            "edge_fleet_invoke" => Some("aegis.edge.fleet.invoke"),
            _ => None,
        };
        let Some(tool) = gate_tool else {
            return self
                .host_action_now(tenant, action, &args, &principal)
                .await;
        };
        let gated = self
            .land_at_gate(
                tenant,
                crate::domain::execution::ExecutionId(run),
                &person,
                tool,
                Value::Object(args.clone()),
                || async {
                    let a = self
                        .host_action_now(tenant, action, &args, &principal)
                        .await;
                    Ok(json!({ "result": a.result, "ok": a.ok, "sentence": a.sentence }))
                },
            )
            .await;
        match gated {
            Ok(super::storage::GatedLanding::Landed(v)) => HostActionAnswer {
                ok: v.get("ok").and_then(Value::as_bool).unwrap_or(false),
                sentence: v
                    .get("sentence")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                result: v.get("result").cloned().unwrap_or(Value::Null),
            },
            Ok(super::storage::GatedLanding::NotApproved(sentence)) => HostActionAnswer::refused(
                sentence.replace("nothing was landed", "nothing was changed"),
            ),
            Err(e) => HostActionAnswer::refused(e.to_string()),
        }
    }

    /// Run a host action at once: the gate, when it applies, has passed.
    async fn host_action_now(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
        action: &str,
        args: &serde_json::Map<String, Value>,
        principal: &HostStatePrincipal,
    ) -> HostActionAnswer {
        let Some(service) = self.host_state_service.as_ref() else {
            return HostActionAnswer::refused(NO_HOST_STATES);
        };
        match action {
            "host_state_plan" | "host_state_apply" => {
                let Some(name) = args.get("name").and_then(Value::as_str) else {
                    return HostActionAnswer::refused("`name` is required");
                };
                if action == "host_state_apply" {
                    if let Some(vars) = arg_json(args, "vars") {
                        let Some(vars) = vars.as_object().and_then(|o| {
                            o.iter()
                                .map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                                .collect::<Option<std::collections::BTreeMap<_, _>>>()
                        }) else {
                            return HostActionAnswer::refused(
                                "`vars` must be a JSON object of strings",
                            );
                        };
                        let actor = match principal {
                            HostStatePrincipal::Attested { principal, .. } => principal.clone(),
                            HostStatePrincipal::Bearer(_) => "rest".to_string(),
                        };
                        if let Err(e) = service.set_vars(tenant, name, vars, &actor).await {
                            return HostActionAnswer::refused(e.to_string());
                        }
                    }
                }
                match service
                    .run(tenant, name, action == "host_state_plan", principal)
                    .await
                {
                    Ok(r) => HostActionAnswer {
                        ok: r.failed_hosts == 0,
                        result: serde_json::to_value(&r).unwrap_or(Value::Null),
                        sentence: None,
                    },
                    Err(e) => HostActionAnswer::refused(e.to_string()),
                }
            }
            "edge_fleet_invoke" => {
                let target: crate::domain::edge::EdgeTarget =
                    match arg_json(args, "target").map(serde_json::from_value) {
                        Some(Ok(t)) => t,
                        _ => return HostActionAnswer::refused(
                            "`target` must be an edge target: a node, a group, a selector or All",
                        ),
                    };
                let Some(tool) = args.get("tool").and_then(Value::as_str) else {
                    return HostActionAnswer::refused("`tool` is required");
                };
                let tool_args = arg_json(args, "args").unwrap_or_else(|| json!({}));
                let context = args
                    .get("security_context")
                    .and_then(Value::as_str)
                    .unwrap_or(crate::domain::host_state::DEFAULT_HOST_CONTEXT);
                let mode = match args.get("mode").and_then(Value::as_str) {
                    Some("parallel") => crate::domain::edge_fleet::FleetMode::Parallel,
                    _ => crate::domain::edge_fleet::FleetMode::Sequential,
                };
                match service
                    .invoke_fleet(tenant, &target, tool, tool_args, context, mode, principal)
                    .await
                {
                    Ok(f) => HostActionAnswer {
                        ok: f.hosts.iter().all(|h| h.ok),
                        result: json!({
                            "fleet_command_id": f.fleet_command_id.0.to_string(),
                            "hosts": f.hosts,
                            "skipped": f.skipped,
                        }),
                        sentence: None,
                    },
                    Err(e) => HostActionAnswer::refused(e.to_string()),
                }
            }
            other => HostActionAnswer::refused(format!("unknown host action {other}")),
        }
    }
}

/// A rollout's health gate: `url` answering `expect_status`, tried
/// `attempts` times `interval_secs` apart. Only https URLs are checked.
async fn http_check(args: &serde_json::Map<String, Value>) -> HostActionAnswer {
    let Some(url) = args.get("url").and_then(Value::as_str) else {
        return HostActionAnswer::refused("`url` is required");
    };
    if !url.starts_with("https://") {
        return HostActionAnswer::refused("`url` must be an https URL");
    }
    let expect = arg_u64(args, "expect_status", 200) as u16;
    let attempts = arg_u64(args, "attempts", 10).clamp(1, 60);
    let interval = std::time::Duration::from_secs(arg_u64(args, "interval_secs", 6).clamp(1, 60));
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return HostActionAnswer::refused(e.to_string()),
    };
    let mut last = None;
    for attempt in 1..=attempts {
        let status = client
            .get(url)
            .send()
            .await
            .map(|r| r.status().as_u16())
            .ok();
        last = status;
        if status == Some(expect) {
            return HostActionAnswer {
                ok: true,
                result: json!({ "url": url, "status": expect, "attempts": attempt }),
                sentence: None,
            };
        }
        if attempt < attempts {
            tokio::time::sleep(interval).await;
        }
    }
    HostActionAnswer {
        ok: false,
        result: json!({ "url": url, "status": last, "attempts": attempts, "expected": expect }),
        sentence: Some(format!(
            "{url} did not answer {expect} after {attempts} attempts"
        )),
    }
}

impl ToolInvocationService {
    /// A workflow's `tool_call` (AEGIS ADR-157 D11): `tool` on its remote
    /// server, with `args`, as the run's person and with the binding the
    /// run's own `contexts` chose for that server, so a server the run chose
    /// nothing for is reached with none of the person's credentials. Carbon
    /// Forge, the run's tree server, is called with the run's execution
    /// token as every run's Carbon Forge call is. A tool the configuration
    /// gates waits at the approval gate as the run's person, or runs at once
    /// on their standing "always allow".
    async fn tool_call(
        &self,
        tenant: &TenantId,
        run: uuid::Uuid,
        args: &serde_json::Map<String, Value>,
    ) -> HostActionAnswer {
        let Some(tool) = args.get("tool").and_then(Value::as_str) else {
            return HostActionAnswer::refused("`tool` is required");
        };
        let (server, name) = match tool_call_target(tool) {
            Ok(target) => target,
            Err(sentence) => return HostActionAnswer::refused(sentence),
        };
        let call_args = match arg_json(args, "args") {
            None => json!({}),
            Some(Value::Object(o)) => Value::Object(o),
            Some(_) => return HostActionAnswer::refused("`args` must be a JSON object"),
        };
        let Some(executions) = self.workflow_execution_repo.as_ref() else {
            return HostActionAnswer::refused("the workflow execution store is not configured");
        };
        let execution = match executions
            .find_by_id_for_tenant(tenant, crate::domain::execution::ExecutionId(run))
            .await
        {
            Ok(Some(execution)) => execution,
            Ok(None) => return HostActionAnswer::refused(format!("no workflow run {run}")),
            Err(e) => return HostActionAnswer::refused(e.to_string()),
        };
        let person = execution.initiating_user_sub.clone().unwrap_or_default();
        let contexts = self.remote_tool_former_names.execution_contexts(
            crate::domain::execution::ExecutionContexts::from_value(
                execution
                    .input
                    .get(crate::domain::execution::CONTEXTS_INPUT_KEY),
            ),
        );
        let acting = Self::run_acting(
            Some(person.as_str()),
            Some(execution.workflow_id.as_uuid()),
            contexts,
        );
        let tree_server = server == self.run_tree_server();
        let call = || async {
            if tree_server {
                self.invoke_remote_tool_for_run(
                    run,
                    tenant,
                    &acting,
                    server,
                    name,
                    call_args.clone(),
                )
                .await
            } else {
                self.invoke_remote_tool_with_binding_for_run(
                    run,
                    tenant,
                    &acting,
                    server,
                    name,
                    call_args.clone(),
                )
                .await
            }
        };
        let gated = self
            .land_at_gate(
                tenant,
                crate::domain::execution::ExecutionId(run),
                &person,
                tool,
                json!({ "tool": tool, "args": call_args }),
                call,
            )
            .await;
        match gated {
            Ok(super::storage::GatedLanding::Landed(result)) => HostActionAnswer {
                ok: true,
                result,
                sentence: None,
            },
            Ok(super::storage::GatedLanding::NotApproved(sentence)) => {
                HostActionAnswer::refused(sentence)
            }
            Err(e) => HostActionAnswer::refused(e.to_string()),
        }
    }
}

#[cfg(test)]
mod host_action_tests {
    use super::*;
    use crate::domain::workflow::ActionName;

    /// ADR-157 D11: tool_call reaches remote servers only; AEGIS's own tools
    /// keep their own actions and rules.
    #[test]
    fn a_tool_call_names_a_remote_servers_tool() {
        assert_eq!(
            super::tool_call_target("cloudflare.execute"),
            Ok(("cloudflare", "execute"))
        );
        assert_eq!(
            super::tool_call_target("github.create_issue"),
            Ok(("github", "create_issue"))
        );
        for refused in [
            "aegis.host_state.apply",
            "host.apply",
            "cmd.run",
            "fs.write",
            "execute",
            ".x",
            "x.",
        ] {
            assert!(super::tool_call_target(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn the_host_actions_parse_and_only_the_changing_ones_are_gated() {
        for (name, gated) in [
            ("host_state_plan", false),
            ("host_state_apply", true),
            ("edge_fleet_invoke", true),
            ("http_check", false),
            ("tool_call", true),
        ] {
            let action = ActionName::from_literal(name).expect(name);
            assert_eq!(action.as_str(), name);
            assert_eq!(action.touches_the_world(), gated, "{name}");
        }
    }

    #[test]
    fn a_rendered_argument_is_read_back_as_json() {
        let args: serde_json::Map<String, Value> = serde_json::from_value(json!({
            "target": "{\"Selector\":{\"tags\":[\"member\"]}}",
            "tool": "host.facts",
            "attempts": "3",
        }))
        .unwrap();
        assert_eq!(
            arg_json(&args, "target").unwrap()["Selector"]["tags"][0],
            "member"
        );
        assert_eq!(arg_json(&args, "tool").unwrap(), json!("host.facts"));
        assert_eq!(arg_u64(&args, "attempts", 10), 3);
        assert_eq!(arg_u64(&args, "interval_secs", 6), 6);
    }

    #[tokio::test]
    async fn an_http_check_takes_only_https() {
        let args: serde_json::Map<String, Value> =
            serde_json::from_value(json!({ "url": "http://example.com" })).unwrap();
        let answer = http_check(&args).await;
        assert!(!answer.ok);
        assert!(answer.sentence.unwrap().contains("https"));
    }
}
