use super::attachment_args::parse_attachments;
use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_agent_create_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
        calling_agent: Option<&str>,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.agent.create requires 'manifest_yaml' string".to_string(),
                )
            })?;
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;

        let manifest = match AgentManifestParser::parse_yaml(&manifest_yaml) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.create",
                    "validated": false,
                    "deployed": false,
                    "errors": [format!("Agent manifest parse/validation failed: {}", e)]
                })));
            }
        };

        // AEGIS ADR-005 O7d: the generator's floor.
        if let Some(sentence) = generator_floor(
            calling_agent,
            &manifest,
            &self.remote_servers_named(&manifest),
        ) {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.create",
                "validated": false,
                "deployed": false,
                "name": manifest.metadata.name,
                "version": manifest.metadata.version,
                "errors": [sentence]
            })));
        }

        // ADR-087, with AEGIS ADR-132 S7a and S7c: every declared tool is in
        // the tool catalog or is a tool of one of the node's remote servers.
        if let Some(sentence) = self.unregistered_tools_sentence(&manifest).await {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.create",
                "validated": false,
                "deployed": false,
                "errors": [sentence]
            })));
        }

        // AEGIS ADR-005 O7c: the program runs on its sample input before the
        // agent is deployed.
        let program_check = match self.check_program(&manifest).await {
            Ok(check) => check,
            Err(refusal) => {
                return Ok(ToolInvocationResult::Direct(refusal.answer(
                    "aegis.agent.create",
                    "deployed",
                    &manifest,
                )));
            }
        };

        match self
            .agent_lifecycle
            .deploy_agent_for_tenant(
                &tenant_id,
                manifest.clone(),
                force,
                crate::domain::agent::AgentScope::Tenant,
                None,
            )
            .await
        {
            Ok(agent_id) => {
                let persisted_path = self
                    .persist_generated_manifest(
                        "agents",
                        &manifest.metadata.name,
                        &manifest.metadata.version,
                        &manifest_yaml,
                    )
                    .map_err(|e| {
                        SealSessionError::InternalError(format!(
                            "Agent deployed but failed to persist manifest: {e}"
                        ))
                    })?;
                let mut answer = serde_json::json!({
                    "tool": "aegis.agent.create",
                    "validated": true,
                    "deployed": true,
                    "agent_id": agent_id.0.to_string(),
                    "name": manifest.metadata.name,
                    "version": manifest.metadata.version,
                    "force": force,
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path
                });
                if let Some(check) = program_check {
                    answer["program_check"] = check;
                }
                Ok(ToolInvocationResult::Direct(answer))
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("is already deployed") {
                    Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.create",
                        "status": "conflict",
                        "error": format!(
                            "Agent '{}' version '{}' is already deployed. Increment the version (e.g. to '{}.1') in the manifest and retry.",
                            manifest.metadata.name,
                            manifest.metadata.version,
                            manifest.metadata.version,
                        ),
                        "hint": "increment_version"
                    })))
                } else {
                    Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.create",
                        "validated": true,
                        "deployed": false,
                        "name": manifest.metadata.name,
                        "version": manifest.metadata.version,
                        "errors": [format!("Agent deployment failed: {}", e)]
                    })))
                }
            }
        }
    }

    pub(super) async fn invoke_aegis_agent_list_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let agents = self
            .agent_lifecycle
            .list_agents_visible_for_tenant(&tenant_id)
            .await
            .map_err(|e| SealSessionError::InternalError(format!("Failed to list agents: {e}")))?;

        let entries: Vec<serde_json::Value> = agents
            .iter()
            .map(|a| {
                let mut entry = serde_json::json!({
                    "id": a.id.0.to_string(),
                    "name": a.name,
                    "version": a.manifest.metadata.version,
                    "status": format!("{:?}", a.status).to_lowercase(),
                    "description": a.manifest.metadata.description,
                    "labels": a.manifest.metadata.labels,
                    "intent": "Top-level instruction that steers this agent's behavior. Pass directly as 'intent' when calling aegis.task.execute.",
                    "input_schema": a.manifest.spec.input_schema,
                });
                // AEGIS ADR-005 O6: an agent O4 or O5 refuses says so, so a
                // caller never dispatches it.
                if let Some(refusal) = crate::domain::tool_requirement::refusal_of(a) {
                    entry["refused"] = Value::String(refusal.to_string());
                }
                entry
            })
            .collect();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.agent.list",
            "count": entries.len(),
            "agents": entries
        })))
    }

    pub(super) async fn invoke_aegis_agent_delete_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let agent_id_str = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.agent.delete requires 'agent_id' string".to_string(),
                )
            })?;

        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let agent_id = crate::domain::agent::AgentId(
            uuid::Uuid::parse_str(&agent_id_str)
                .map_err(|e| SealSessionError::InvalidArguments(format!("Invalid UUID: {e}")))?,
        );

        match self
            .agent_lifecycle
            .delete_agent_for_tenant(&tenant_id, agent_id)
            .await
        {
            Ok(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.delete",
                "deleted": true,
                "agent_id": agent_id_str
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.delete",
                "deleted": false,
                "error": format!("Failed to delete agent: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_agent_generate_tool(
        &self,
        args: &mut Value,
        _security_context: &crate::domain::security_context::SecurityContext,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let raw_input = args.get("input").cloned().unwrap_or(serde_json::json!({}));

        // AEGIS ADR-131 U6: the goal this generation is started for, checked
        // before anything starts.
        let goal_id = match self
            .goal_for_start("aegis.agent.generate", args, caller_identity, _scope)
            .await?
        {
            Ok(goal_id) => goal_id,
            Err(refused) => return Ok(refused),
        };

        // ADR-113: parse attachments from the SEAL JSON-RPC tool call args so
        // they reach `ExecutionInput.attachments` and get merged into
        // `input.attachments` by `prepare_execution_input` downstream.
        let attachments = parse_attachments(args)?;

        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;

        // Normalize: if the caller passed a plain string, wrap it as { "input": <str> }.
        // Then inject tenant_id using the same pattern as invoke_aegis_task_execute_tool.
        let mut payload = if raw_input.is_object() {
            raw_input
        } else {
            serde_json::json!({ "input": raw_input })
        };
        // Zaru ADR-0055 D14: the dispatch's binding choices ride in the
        // input's reserved key `contexts`.
        super::context_args::carry_contexts(args, &mut payload)?;
        if let Some(map) = payload.as_object_mut() {
            map.entry("tenant_id")
                .or_insert_with(|| serde_json::Value::String(tenant_id.to_string()));
        }

        let agent_id = match self
            .agent_lifecycle
            .lookup_agent_visible_for_tenant(&tenant_id, "agent-creator-agent")
            .await
        {
            Ok(Some(id)) => id,
            _ => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.generate",
                    "error": "Generator agent 'agent-creator-agent' not found"
                })));
            }
        };

        match self
            .execution_service
            .start_execution(
                agent_id,
                crate::domain::execution::ExecutionInput {
                    intent: None,
                    input: payload,
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments,
                },
                "aegis-system-agent-runtime".to_string(),
                caller_identity,
            )
            .await
        {
            Ok(exec_id) => {
                if let Some(closed) = self
                    .bind_to_goal(
                        goal_id,
                        &exec_id.to_string(),
                        crate::domain::goal::BoundKind::Agent,
                    )
                    .await
                {
                    return Ok(ToolInvocationResult::Direct(closed));
                }
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.generate",
                    "execution_id": exec_id.to_string(),
                    "status": "started"
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.generate",
                "error": format!("Failed to start generation: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_agent_update_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
        calling_agent: Option<&str>,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.agent.update requires 'manifest_yaml' string".to_string(),
                )
            })?;

        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        let manifest = match AgentManifestParser::parse_yaml(manifest_yaml) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.update",
                    "validated": false,
                    "updated": false,
                    "error": format!("Manifest parsing failed: {}", e)
                })));
            }
        };

        if let Err(e) = manifest.validate() {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.update",
                "validated": false,
                "updated": false,
                "name": manifest.metadata.name,
                "version": manifest.metadata.version,
                "error": format!("Schema validation failed: {}", e)
            })));
        }

        // AEGIS ADR-005 O7d: the generator's floor.
        if let Some(sentence) = generator_floor(
            calling_agent,
            &manifest,
            &self.remote_servers_named(&manifest),
        ) {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.update",
                "validated": false,
                "updated": false,
                "name": manifest.metadata.name,
                "version": manifest.metadata.version,
                "errors": [sentence]
            })));
        }

        let existing_agent = match self
            .agent_lifecycle
            .lookup_agent_for_tenant(&tenant_id, &manifest.metadata.name)
            .await
        {
            Ok(Some(id)) => self
                .agent_lifecycle
                .get_agent_for_tenant(&tenant_id, id)
                .await
                .ok(),
            _ => None,
        };

        let agent_id = match &existing_agent {
            Some(a) => a.id,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.update",
                    "validated": true,
                    "updated": false,
                    "name": manifest.metadata.name,
                    "error": "Agent not found"
                })));
            }
        };

        // Version check if force=false
        if !force {
            if let Some(existing) = existing_agent {
                if existing.manifest.metadata.version == manifest.metadata.version {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.update",
                        "validated": true,
                        "updated": false,
                        "name": manifest.metadata.name,
                        "version": manifest.metadata.version,
                        "error": format!("Agent '{}' with version '{}' already exists. Create a new version or use 'force' to overwrite.", manifest.metadata.name, manifest.metadata.version)
                    })));
                }
            }
        }

        // ADR-087, with AEGIS ADR-132 S7a and S7c: every declared tool is in
        // the tool catalog or is a tool of one of the node's remote servers.
        if let Some(sentence) = self.unregistered_tools_sentence(&manifest).await {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.update",
                "validated": false,
                "updated": false,
                "errors": [sentence]
            })));
        }

        // AEGIS ADR-005 O7c: the program runs on its sample input before the
        // agent is updated.
        let program_check = match self.check_program(&manifest).await {
            Ok(check) => check,
            Err(refusal) => {
                return Ok(ToolInvocationResult::Direct(refusal.answer(
                    "aegis.agent.update",
                    "updated",
                    &manifest,
                )));
            }
        };

        match self
            .agent_lifecycle
            .update_agent_for_tenant(&tenant_id, agent_id, manifest.clone())
            .await
        {
            Ok(_) => {
                let persisted_path = self
                    .persist_generated_manifest(
                        "agents",
                        &manifest.metadata.name,
                        &manifest.metadata.version,
                        manifest_yaml,
                    )
                    .map_err(|e| {
                        SealSessionError::InternalError(format!(
                            "Agent updated but failed to persist manifest: {e}"
                        ))
                    })?;
                let mut answer = serde_json::json!({
                    "tool": "aegis.agent.update",
                    "validated": true,
                    "updated": true,
                    "name": manifest.metadata.name,
                    "version": manifest.metadata.version,
                    "agent_id": agent_id.0.to_string(),
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path,
                });
                if let Some(check) = program_check {
                    answer["program_check"] = check;
                }
                Ok(ToolInvocationResult::Direct(answer))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.update",
                "validated": true,
                "updated": false,
                "name": manifest.metadata.name,
                "version": manifest.metadata.version,
                "errors": [format!("Agent update failed: {}", e)]
            }))),
        }
    }

    pub(super) async fn invoke_aegis_agent_export_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
                "aegis.agent.export requires 'name' string".to_string(),
            )
        })?;

        let agent_id = match self
            .agent_lifecycle
            .lookup_agent_for_tenant(&tenant_id, name)
            .await
        {
            Ok(Some(id)) => id,
            _ => {
                if let Ok(uuid) = uuid::Uuid::parse_str(name) {
                    crate::domain::agent::AgentId(uuid)
                } else {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.export",
                        "error": "Agent not found or invalid UUID"
                    })));
                }
            }
        };

        match self
            .agent_lifecycle
            .get_agent_for_tenant(&tenant_id, agent_id)
            .await
        {
            Ok(agent) => {
                let yaml = serde_yaml::to_string(&agent.manifest).unwrap_or_default();
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.export",
                    "name": agent.name,
                    "manifest_yaml": yaml
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.export",
                "error": format!("Failed to get agent: {}", e)
            }))),
        }
    }

    /// Handler for `aegis.agent.logs` — retrieves agent-level activity log snapshot.
    pub(super) async fn invoke_aegis_agent_logs_tool(
        &self,
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: bind/verify the requested tenant against the authenticated
        // scope before any agent activity is read. Without this gate any
        // caller could read another tenant's agent activity log by guessing
        // the agent UUID.
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

        let agent_id_str = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.agent.logs requires 'agent_id' string".to_string(),
                )
            })?;

        let agent_uuid = uuid::Uuid::parse_str(agent_id_str).map_err(|e| {
            SealSessionError::InvalidArguments(format!(
                "aegis.agent.logs: invalid agent_id UUID: {e}"
            ))
        })?;

        let limit: usize = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(200))
            .unwrap_or(50);
        let offset: usize = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);

        // Pre-flight tenant validation: confirm the agent is visible to the
        // caller's tenant before fetching its activity. `get_agent_for_tenant`
        // returns an error when the agent does not belong to `tenant_id` and
        // is not globally visible.
        if let Err(e) = self
            .agent_lifecycle
            .get_agent_for_tenant(&tenant_id, crate::domain::agent::AgentId(agent_uuid))
            .await
        {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.logs",
                "error": format!("Agent '{agent_id_str}' not found: {e}")
            })));
        }

        let port = match &self.agent_activity {
            Some(p) => p,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.logs",
                    "error": "Agent activity port not configured"
                })));
            }
        };

        match port
            .agent_logs_snapshot(&tenant_id, agent_uuid, limit, offset)
            .await
        {
            Ok(events) => {
                let total = events.len();
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.logs",
                    "agent_id": agent_id_str,
                    "events": events,
                    "total": total,
                    "limit": limit,
                    "offset": offset,
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.logs",
                "error": format!("Failed to fetch agent logs: {e}")
            }))),
        }
    }
}

impl ToolInvocationService {
    /// The node's remote servers whose tools `manifest` names in
    /// `spec.tools`, each once, in the order first named (AEGIS ADR-132
    /// S7b), by the rule a call is routed by.
    fn remote_servers_named(&self, manifest: &crate::domain::agent::AgentManifest) -> Vec<String> {
        let mut servers: Vec<String> = Vec::new();
        for tool in &manifest.spec.tools {
            if let Some((server, _)) = self.remote_tool_of(tool) {
                if !servers.iter().any(|named| named == server) {
                    servers.push(server.to_string());
                }
            }
        }
        servers
    }

    /// The refusal for a manifest declaring a tool that is neither in the
    /// tool catalog nor a tool of one of the node's remote servers (ADR-087;
    /// AEGIS ADR-132 S7a, S7c), naming the remote servers and how their
    /// tools are declared; `None` when every tool is known or the node has
    /// no catalog. The catalog is read whole, never one page of it (S7f).
    async fn unregistered_tools_sentence(
        &self,
        manifest: &crate::domain::agent::AgentManifest,
    ) -> Option<String> {
        let catalog = self.tool_catalog.as_ref()?;
        let declared_tools = &manifest.spec.tools;
        if declared_tools.is_empty() {
            return None;
        }
        let registered = catalog.registered_names().await;
        let unknown: Vec<&str> = declared_tools
            .iter()
            .filter(|tool| !registered.contains(tool.as_str()))
            .filter(|tool| self.remote_tool_of(tool).is_none())
            .map(String::as_str)
            .collect();
        if unknown.is_empty() {
            return None;
        }
        Some(unregistered_tools_sentence(
            &unknown,
            &self.remote_tool_servers,
        ))
    }
}

/// The refusal for tools neither registered nor of a remote server.
pub(crate) fn unregistered_tools_sentence(unknown: &[&str], remote_servers: &[String]) -> String {
    format!(
        "Agent manifest references tools not registered on this platform: [{}]. A remote \
         server's tool is named '<server>.<tool>' and is declared with a spec.contexts entry \
         {{service: <server>}}; this node's remote servers: [{}]. Call aegis.tools.list to see \
         available tools.",
        unknown.join(", "),
        remote_servers.join(", ")
    )
}

/// The context rule's sentence for a remote server whose tools an agent
/// names without declaring its context.
pub(crate) fn undeclared_context_sentence(name: &str, server: &str) -> String {
    format!("agent '{name}' uses {server}.* tools but declares no {server} context")
}

/// The built-in agent that generates agents (AEGIS ADR-005 O7d).
pub(crate) const AGENT_GENERATOR_NAME: &str = "agent-creator-agent";

/// The bound on one run of an agent's program on its sample input (O7c).
pub(crate) const PROGRAM_CHECK_TIMEOUT_SECS: u64 = 120;

/// The most stdout, and stderr, a program check answers (O7c).
const PROGRAM_CHECK_SHOWN_CHARS: usize = 4096;

/// O7's sentence for an agent that has no program.
pub(crate) fn no_program_sentence(name: &str) -> String {
    format!(
        "agent '{name}' has no program: its instruction asks the model to write or compute the \
         solution on each run; the agent must carry its program and run it"
    )
}

/// The output declaration's sentence for a file an agent writes and does not
/// declare.
pub(crate) fn undeclared_output_sentence(name: &str, path: &str) -> String {
    format!("agent '{name}' writes {path} but does not declare it in spec.execution.outputs")
}

/// AEGIS ADR-005 O7d and O8: a manifest the generator creates or updates is
/// refused when it declares `cmd.run` and carries no program (O7's
/// sentence), and for each `/workspace` file its instruction or prompt
/// template names that `spec.execution.outputs` does not list; and (AEGIS
/// ADR-132 S7b) for each remote server in `remote_servers_named` (the
/// node's remote servers whose tools it names) that `spec.contexts` does
/// not declare. Every sentence is reported, joined with "; ".
pub(crate) fn generator_floor(
    calling_agent: Option<&str>,
    manifest: &crate::domain::agent::AgentManifest,
    remote_servers_named: &[String],
) -> Option<String> {
    if calling_agent != Some(AGENT_GENERATOR_NAME) {
        return None;
    }
    let name = &manifest.metadata.name;
    let mut sentences: Vec<String> = Vec::new();
    let runs_commands = manifest.spec.tools.iter().any(|tool| tool == "cmd.run");
    if runs_commands && manifest.spec.program.is_none() {
        sentences.push(no_program_sentence(name));
    }
    let declared: Vec<&str> = manifest
        .spec
        .execution
        .as_ref()
        .map(|execution| {
            execution
                .outputs
                .iter()
                .map(|output| output.path.as_str())
                .collect()
        })
        .unwrap_or_default();
    let mut written: Vec<&str> = Vec::new();
    if let Some(task) = &manifest.spec.task {
        for text in [&task.instruction, &task.prompt_template]
            .into_iter()
            .flatten()
        {
            for file in crate::domain::tool_requirement::workspace_files_named(text) {
                if !written.contains(&file) {
                    written.push(file);
                }
            }
        }
    }
    for file in written {
        if !declared.contains(&file) {
            sentences.push(undeclared_output_sentence(name, file));
        }
    }
    for server in remote_servers_named {
        let declares = manifest
            .spec
            .contexts
            .iter()
            .any(|context| context.service.trim() == server);
        if !declares {
            sentences.push(undeclared_context_sentence(name, server));
        }
    }
    if sentences.is_empty() {
        None
    } else {
        Some(sentences.join("; "))
    }
}

fn head_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
}

/// Why a program check refused an agent, with what the check saw (O7c).
pub(crate) struct ProgramRefusal {
    sentence: String,
    check: Option<Value>,
}

impl ProgramRefusal {
    fn answer(
        &self,
        tool: &str,
        done_key: &str,
        manifest: &crate::domain::agent::AgentManifest,
    ) -> Value {
        let mut answer = serde_json::json!({
            "tool": tool,
            "validated": true,
            "name": manifest.metadata.name,
            "version": manifest.metadata.version,
            "errors": [self.sentence],
        });
        answer[done_key] = Value::Bool(false);
        if let Some(check) = &self.check {
            answer["program_check"] = check.clone();
        }
        answer
    }
}

impl ToolInvocationService {
    /// The tool call an execution of the agent must make before a text
    /// answer completes it (AEGIS ADR-135 D5, ADR-005 O7e).
    pub(crate) async fn agent_tool_call_requirement(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> anyhow::Result<crate::domain::agent::ToolCallRequirement> {
        let agent = self
            .agent_lifecycle
            .get_agent_visible(tenant_id, agent_id)
            .await?;
        Ok(crate::domain::agent::ToolCallRequirement::of(
            &agent.manifest,
        ))
    }

    /// The calling agent's name, when the call comes from an execution
    /// (AEGIS ADR-005 O7d).
    pub(super) async fn calling_agent_name(
        &self,
        scope: &crate::domain::iam::TenantScope,
        agent_id: AgentId,
    ) -> Option<String> {
        self.agent_lifecycle
            .get_agent_visible(&scope.authenticated_tenant, agent_id)
            .await
            .ok()
            .map(|agent| agent.manifest.metadata.name)
    }

    /// AEGIS ADR-005 O7c: run the program `manifest` carries on its sample
    /// input, in the agent's own image with no network, and answer what it
    /// did; refuse when it exits non-zero, prints nothing or does not finish.
    /// `Ok(None)`: no program, or no sample input to run it on.
    pub(crate) async fn check_program(
        &self,
        manifest: &crate::domain::agent::AgentManifest,
    ) -> Result<Option<Value>, ProgramRefusal> {
        use crate::domain::runtime::{
            program_container_files, ContainerResources, ContainerStepConfig, ContainerStepError,
        };
        let Some(program) = &manifest.spec.program else {
            return Ok(None);
        };
        let name = &manifest.metadata.name;
        let cannot = |why: String| ProgramRefusal {
            sentence: format!("agent '{name}' program cannot be checked: {why}"),
            check: None,
        };
        let Some(runner) = &self.program_runner else {
            return Err(cannot("this node has no program runner".to_string()));
        };
        let Some(sample) = &program.sample_input else {
            return Ok(None);
        };
        let runtime = &manifest.spec.runtime;
        let image = match &runtime.image {
            Some(image) => image.clone(),
            None => {
                let language = runtime.language.as_deref().unwrap_or("");
                let version = runtime.version.as_deref().unwrap_or("");
                let Some(registry) = &self.runtime_registry else {
                    return Err(cannot(format!(
                        "this node cannot resolve the image for {language} {version}"
                    )));
                };
                registry
                    .resolve(language, version)
                    .map_err(|e| cannot(format!("{e}")))?
            }
        };
        let state_name = crate::domain::workflow::StateName::new("PROGRAM_CHECK")
            .map_err(|e| cannot(e.to_string()))?;
        let config = ContainerStepConfig {
            name: format!("program-check-{name}"),
            image,
            image_pull_policy: runtime.image_pull_policy,
            entrypoint: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
            command: vec![program.run.clone()],
            stdin: None,
            env: std::collections::HashMap::new(),
            workdir: Some(crate::domain::agent::PROGRAM_DIR.to_string()),
            volumes: Vec::new(),
            resources: Some(ContainerResources {
                cpu: None,
                memory: None,
                timeout: Some(std::time::Duration::from_secs(PROGRAM_CHECK_TIMEOUT_SECS)),
            }),
            registry_credentials: None,
            execution_id: crate::domain::execution::ExecutionId::new(),
            state_name,
            read_only_root_filesystem: false,
            run_as_user: Some("1000:1000".to_string()),
            network_mode: Some("none".to_string()),
            workflow_execution_id: None,
            files: program_container_files(&program.files, Some(&sample.to_string())),
        };
        let result = match runner.run_step(config).await {
            Ok(result) => result,
            Err(ContainerStepError::TimeoutExpired { timeout_secs }) => {
                return Err(ProgramRefusal {
                    sentence: format!(
                        "agent '{name}' program did not finish on its sample input within {timeout_secs} s"
                    ),
                    check: None,
                });
            }
            Err(error) => return Err(cannot(error.to_string())),
        };
        let check = serde_json::json!({
            "exit_code": result.exit_code,
            "stdout": head_chars(&result.stdout, PROGRAM_CHECK_SHOWN_CHARS),
            "stderr": tail_chars(&result.stderr, PROGRAM_CHECK_SHOWN_CHARS),
            "duration_ms": result.duration_ms,
        });
        if result.exit_code != 0 {
            return Err(ProgramRefusal {
                sentence: format!(
                    "agent '{name}' program failed on its sample input: it exited {}; {}",
                    result.exit_code,
                    tail_chars(result.stderr.trim(), PROGRAM_CHECK_SHOWN_CHARS)
                ),
                check: Some(check),
            });
        }
        if result.stdout.trim().is_empty() {
            return Err(ProgramRefusal {
                sentence: format!("agent '{name}' program printed nothing on its sample input"),
                check: Some(check),
            });
        }
        Ok(Some(check))
    }
}

/// AEGIS ADR-005 O6 at the tool routes: an agent O4 or O5 refuses is refused
/// by `aegis.task.execute` before anything starts, carries `refused` on
/// `aegis.agent.list` and `aegis.agent.search`, and a workflow whose step was
/// refused answers the reason on `aegis.workflow.wait`. Driven through a real
/// `ToolInvocationService` over a real `StandardExecutionService` and the
/// in-memory stores.
#[cfg(test)]
mod refused_agent_routes {
    use super::*;
    use crate::application::discovery_service::DiscoveryService;
    use crate::application::execution::StandardExecutionService;
    use crate::application::volume_manager::VolumeService;
    use crate::domain::agent::{Agent, AgentManifest, VolumeSpec};
    use crate::domain::discovery::{
        DiscoveryQuery, DiscoveryResourceKind, DiscoveryResponse, DiscoveryResult, SearchMode,
    };
    use crate::domain::events::ExecutionEvent;
    use crate::domain::execution::{ExecutionId, ExecutionStatus};
    use crate::domain::iam::{IdentityKind, TenantScope, ZaruTier};
    use crate::domain::repository::{
        AgentRepository, ExecutionRepository, RepositoryError, WorkflowExecutionRepository,
    };
    use crate::domain::runtime::{
        AgentRuntime, InstanceId, InstanceStatus, RuntimeConfig, RuntimeError, TaskInput,
        TaskOutput,
    };
    use crate::domain::security_context::SecurityContext;
    use crate::domain::supervisor::Supervisor;
    use crate::domain::volume::{
        AccessMode, StorageClass, Volume, VolumeId, VolumeMount, VolumeOwnership,
    };
    use crate::domain::workflow::{WorkflowExecution, WorkflowExecutionEventRecord};
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
        InMemoryWorkflowExecutionRepository,
    };
    use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
    use crate::infrastructure::storage::LocalHostStorageProvider;
    use crate::infrastructure::workflow_parser::WorkflowParser;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashMap;

    const REFUSAL: &str =
        "Agent 'unit-conversion-agent' is refused (tool-requirement/no-volume): it \
        declares fs.write and no read-write volume, so those tools have nothing to write to in a \
        run of its own. Declare a read-write volume with mount_path /workspace; inside a workflow \
        it yields to the workflow's workspace.";

    fn manifest(name: &str, tools: &str, volumes: &str) -> AgentManifest {
        AgentManifestParser::parse_yaml(&format!(
            r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: {name}
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: |
      You convert between units. Write the conversion as Python to
      /workspace/convert.py, run it, and return the printed result.
{tools}
{volumes}
"#
        ))
        .expect("the test manifest parses")
    }

    const EARLIER_TOOLS: &str = "  tools:\n    - fs.write\n    - cmd.run\n    - fs.read";
    const WORKSPACE: &str =
        "  volumes:\n    - name: workspace\n      storage_class: ephemeral\n      \
        type: seaweedfs\n      mount_path: /workspace\n      access_mode: read-write\n      \
        size_limit: 1Gi\n      ttl_hours: 1";

    /// `unit-conversion-agent` before its update to 1.0.1, stored as an
    /// agent deployed before O4 and O5 landed would be.
    fn refused_agent() -> Agent {
        Agent::new(manifest("unit-conversion-agent", EARLIER_TOOLS, ""))
    }

    /// The same agent once it declares its volume, as 1.0.1 does.
    fn passing_agent() -> Agent {
        Agent::new(manifest(
            "unit-converter-with-volume",
            EARLIER_TOOLS,
            WORKSPACE,
        ))
    }

    fn tenant() -> TenantId {
        TenantId::consumer()
    }

    fn tenant_scope() -> TenantScope {
        TenantScope::new(
            tenant(),
            IdentityKind::ServiceAccount {
                client_id: "aegis-test".to_string(),
            },
        )
    }

    fn security_context() -> SecurityContext {
        SecurityContext {
            name: "aegis-system-agent-runtime".to_string(),
            description: String::new(),
            capabilities: vec![],
            deny_list: vec![],
            metadata: crate::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        }
    }

    struct NoPublisher;

    #[async_trait]
    impl crate::domain::fsal::EventPublisher for NoPublisher {
        async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
    }

    /// A runtime that records every spawn and runs nothing.
    #[derive(Default)]
    struct NoContainers {
        spawned: std::sync::Mutex<usize>,
    }

    #[async_trait]
    impl AgentRuntime for NoContainers {
        async fn spawn(&self, _config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
            *self.spawned.lock().unwrap() += 1;
            Err(RuntimeError::SpawnFailed(
                "no containers in this test".to_string(),
            ))
        }
        async fn execute(
            &self,
            _id: &InstanceId,
            _input: TaskInput,
        ) -> Result<TaskOutput, RuntimeError> {
            Err(RuntimeError::ExecutionFailed(
                "no containers in this test".to_string(),
            ))
        }
        async fn terminate(&self, _id: &InstanceId) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn status(&self, _id: &InstanceId) -> Result<InstanceStatus, RuntimeError> {
            Err(RuntimeError::InstanceNotFound(
                "no containers in this test".to_string(),
            ))
        }
    }

    /// No volume is ever created: a refused start never reaches one.
    struct NoVolumes;

    #[async_trait]
    impl VolumeService for NoVolumes {
        async fn create_volume(
            &self,
            _name: String,
            _tenant_id: TenantId,
            _storage_class: StorageClass,
            _size_limit_mb: u64,
            _ownership: VolumeOwnership,
        ) -> Result<VolumeId> {
            anyhow::bail!("no volumes in this test")
        }
        async fn get_volume(&self, id: VolumeId) -> Result<Volume> {
            anyhow::bail!("volume {id} not found")
        }
        async fn list_volumes_by_tenant(&self, _tenant_id: TenantId) -> Result<Vec<Volume>> {
            Ok(vec![])
        }
        async fn list_volumes_by_ownership(
            &self,
            _ownership: &VolumeOwnership,
        ) -> Result<Vec<Volume>> {
            Ok(vec![])
        }
        async fn attach_volume(
            &self,
            _volume_id: VolumeId,
            _instance_id: InstanceId,
            _mount_point: PathBuf,
            _access_mode: AccessMode,
        ) -> Result<VolumeMount> {
            anyhow::bail!("no volumes in this test")
        }
        async fn detach_volume(
            &self,
            _volume_id: VolumeId,
            _instance_id: InstanceId,
        ) -> Result<()> {
            anyhow::bail!("no volumes in this test")
        }
        async fn delete_volume(&self, _volume_id: VolumeId) -> Result<()> {
            anyhow::bail!("no volumes in this test")
        }
        async fn get_volume_usage(&self, _volume_id: VolumeId) -> Result<u64> {
            anyhow::bail!("no volumes in this test")
        }
        async fn cleanup_expired_volumes(&self) -> Result<usize> {
            Ok(0)
        }
        async fn create_volumes_for_execution(
            &self,
            _execution_id: ExecutionId,
            _tenant_id: TenantId,
            _volume_specs: &[VolumeSpec],
            _storage_mode: &str,
        ) -> Result<Vec<Volume>> {
            anyhow::bail!("no volumes in this test")
        }
        async fn persist_external_volume(
            &self,
            _volume_id: VolumeId,
            _name: String,
            _tenant_id: TenantId,
            _remote_path: String,
            _size_limit_bytes: u64,
            _ownership: VolumeOwnership,
        ) -> Result<()> {
            Ok(())
        }
    }

    /// The index answers both agents, as the discovery service would.
    struct BothIndexed(Vec<Agent>);

    #[async_trait]
    impl DiscoveryService for BothIndexed {
        async fn search_agents(
            &self,
            _tenant_id: &TenantId,
            _tier: &ZaruTier,
            _query: DiscoveryQuery,
        ) -> Result<DiscoveryResponse> {
            Ok(DiscoveryResponse {
                results: self
                    .0
                    .iter()
                    .map(|agent| DiscoveryResult {
                        resource_id: agent.id.0.to_string(),
                        kind: DiscoveryResourceKind::Agent,
                        name: agent.name.clone(),
                        version: "1.0.0".to_string(),
                        description: String::new(),
                        labels: HashMap::new(),
                        similarity_score: 0.9,
                        relevance_score: 0.9,
                        tenant_id: tenant().as_str().to_string(),
                        updated_at: chrono::Utc::now(),
                        is_platform_template: false,
                        input_schema: None,
                    })
                    .collect(),
                total_indexed: self.0.len() as u64,
                query_time_ms: 1,
                search_mode: SearchMode::Semantic,
            })
        }
        async fn search_workflows(
            &self,
            _tenant_id: &TenantId,
            _tier: &ZaruTier,
            _query: DiscoveryQuery,
        ) -> Result<DiscoveryResponse> {
            anyhow::bail!("not used in this test")
        }
        async fn find_similar_agents(
            &self,
            _tenant_id: &TenantId,
            _description: &str,
            _threshold: f64,
        ) -> Result<Vec<DiscoveryResult>> {
            Ok(vec![])
        }
        async fn find_similar_workflows(
            &self,
            _tenant_id: &TenantId,
            _description: &str,
            _threshold: f64,
        ) -> Result<Vec<DiscoveryResult>> {
            Ok(vec![])
        }
    }

    struct Harness {
        service: ToolInvocationService,
        runtime: Arc<NoContainers>,
        executions: Arc<dyn ExecutionRepository>,
        event_bus: Arc<EventBus>,
        refused: Agent,
        passing: Agent,
    }

    async fn harness() -> Harness {
        let refused = refused_agent();
        let passing = passing_agent();
        let agents = Arc::new(InMemoryAgentRepository::new());
        for agent in [&refused, &passing] {
            agents.save_for_tenant(&tenant(), agent).await.unwrap();
        }
        let executions: Arc<dyn ExecutionRepository> = Arc::new(InMemoryExecutionRepository::new());
        let runtime = Arc::new(NoContainers::default());
        let event_bus = Arc::new(EventBus::new(256));
        let execution_service = Arc::new(StandardExecutionService::new(
            agents.clone(),
            Arc::new(NoVolumes),
            Arc::new(Supervisor::new(runtime.clone())),
            executions.clone(),
            event_bus.clone(),
            Arc::new(crate::domain::node_config::NodeConfigManifest::default()),
        ));
        let storage_root =
            std::env::temp_dir().join(format!("aegis-refused-tests-{}", uuid::Uuid::new_v4()));
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
            Arc::new(InMemoryVolumeRepository::new()),
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoPublisher),
        ));
        let service = ToolInvocationService::new(
            Arc::new(InMemorySealSessionRepository::new()),
            Arc::new(
                crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
            ),
            Arc::new(SealMiddleware::new()),
            Arc::new(ToolRouter::new(vec![])),
            fsal,
            NfsVolumeRegistry::new(),
            agents,
            execution_service,
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
            event_bus.clone(),
            None,
        )
        .with_discovery_service(Arc::new(BothIndexed(vec![
            refused.clone(),
            passing.clone(),
        ])));
        Harness {
            service,
            runtime,
            executions,
            event_bus,
            refused,
            passing,
        }
    }

    fn direct(result: ToolInvocationResult) -> Value {
        match result {
            ToolInvocationResult::Direct(payload) => payload,
            ToolInvocationResult::DispatchRequired(_) => panic!("expected a direct answer"),
        }
    }

    /// `aegis.task.execute` for `unit-conversion-agent`'s earlier shape answers
    /// `error` as the refusal with no wrapper, and nothing is saved, started
    /// or spawned.
    #[tokio::test]
    async fn task_execute_of_a_refused_agent_answers_the_refusal_and_nothing_starts() {
        let h = harness().await;
        let mut events = h.event_bus.subscribe();
        let mut args = json!({
            "agent_id": "unit-conversion-agent",
            "intent": "43 inches in centimeters",
            "input": { "value": 43, "source_unit": "inch", "target_unit": "cm" },
        });
        let answer = direct(
            h.service
                .invoke_aegis_task_execute_tool(
                    &mut args,
                    &security_context(),
                    None,
                    &tenant_scope(),
                )
                .await
                .expect("aegis.task.execute answers"),
        );

        let mut complaints = Vec::new();
        let expected = format!("Execution refused: {REFUSAL}");
        if answer.get("error").and_then(Value::as_str) != Some(expected.as_str()) {
            complaints.push(format!("aegis.task.execute answered {answer}"));
        }
        if answer.get("execution_id").is_some() {
            complaints.push(format!("an execution id was answered: {answer}"));
        }
        let stored = h
            .executions
            .find_by_agent_for_tenant(&tenant(), h.refused.id, 100)
            .await
            .unwrap();
        if !stored.is_empty() {
            complaints.push(format!("{} execution(s) saved", stored.len()));
        }
        while let Ok(event) = events.try_recv() {
            if let DomainEvent::Execution(ExecutionEvent::ExecutionStarted {
                execution_id, ..
            }) = event
            {
                complaints.push(format!("ExecutionStarted for {execution_id}"));
            }
        }
        let spawned = *h.runtime.spawned.lock().unwrap();
        if spawned > 0 {
            complaints.push(format!("{spawned} container(s) spawned"));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// Every answer naming the two agents, by name.
    fn by_name<'a>(entries: &'a [Value], name: &str) -> Option<&'a Value> {
        entries
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
    }

    fn refused_key_complaints(tool: &str, entries: &[Value], h: &Harness) -> Vec<String> {
        let mut complaints = Vec::new();
        match by_name(entries, &h.refused.name) {
            Some(entry) => {
                if entry.get("refused").and_then(Value::as_str) != Some(REFUSAL) {
                    complaints.push(format!("{tool}: the refused agent answered {entry}"));
                }
            }
            None => complaints.push(format!("{tool}: the refused agent is not listed")),
        }
        match by_name(entries, &h.passing.name) {
            Some(entry) => {
                if entry.get("refused").is_some() {
                    complaints.push(format!(
                        "{tool}: the passing agent carries refused: {entry}"
                    ));
                }
            }
            None => complaints.push(format!("{tool}: the passing agent is not listed")),
        }
        complaints
    }

    /// `aegis.agent.list` carries `refused` with the sentence on the refused agent
    /// and no `refused` key on the passing one.
    #[tokio::test]
    async fn agent_list_carries_refused_on_a_refused_agent_only() {
        let h = harness().await;
        let answer = direct(
            h.service
                .invoke_aegis_agent_list_tool(&mut json!({}), &tenant_scope())
                .await
                .expect("aegis.agent.list answers"),
        );
        let entries = answer["agents"].as_array().cloned().unwrap_or_default();
        let complaints = refused_key_complaints("aegis.agent.list", &entries, &h);
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// `aegis.agent.search` carries `refused` with the sentence on the refused agent
    /// and no `refused` key on the passing one.
    #[tokio::test]
    async fn agent_search_carries_refused_on_a_refused_agent_only() {
        let h = harness().await;
        let answer = direct(
            h.service
                .invoke_aegis_agent_search_tool(
                    &mut json!({ "query": "convert units" }),
                    &security_context(),
                    &tenant_scope(),
                )
                .await
                .expect("aegis.agent.search answers"),
        );
        let entries = answer["results"].as_array().cloned().unwrap_or_default();
        let complaints = refused_key_complaints("aegis.agent.search", &entries, &h);
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// The in-memory workflow store with the events the listener persists.
    struct WithEvents {
        inner: InMemoryWorkflowExecutionRepository,
        events: Vec<WorkflowExecutionEventRecord>,
    }

    #[async_trait]
    impl WorkflowExecutionRepository for WithEvents {
        async fn save_for_tenant(
            &self,
            tenant_id: &TenantId,
            execution: &WorkflowExecution,
        ) -> Result<(), RepositoryError> {
            self.inner.save_for_tenant(tenant_id, execution).await
        }
        async fn find_by_id_for_tenant(
            &self,
            tenant_id: &TenantId,
            id: ExecutionId,
        ) -> Result<Option<WorkflowExecution>, RepositoryError> {
            self.inner.find_by_id_for_tenant(tenant_id, id).await
        }
        async fn find_active_for_tenant(
            &self,
            tenant_id: &TenantId,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            self.inner.find_active_for_tenant(tenant_id).await
        }
        async fn find_by_workflow_for_tenant(
            &self,
            tenant_id: &TenantId,
            workflow_id: crate::domain::workflow::WorkflowId,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            self.inner
                .find_by_workflow_for_tenant(tenant_id, workflow_id, limit, offset)
                .await
        }
        async fn list_paginated_for_tenant(
            &self,
            tenant_id: &TenantId,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            self.inner
                .list_paginated_for_tenant(tenant_id, limit, offset)
                .await
        }
        async fn list_paginated_all(
            &self,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            self.inner.list_paginated_all(limit, offset).await
        }
        async fn count_by_workflow_for_tenant(
            &self,
            tenant_id: &TenantId,
            workflow_id: crate::domain::workflow::WorkflowId,
        ) -> Result<i64, RepositoryError> {
            self.inner
                .count_by_workflow_for_tenant(tenant_id, workflow_id)
                .await
        }
        async fn update_temporal_linkage_for_tenant(
            &self,
            tenant_id: &TenantId,
            execution_id: ExecutionId,
            temporal_workflow_id: &str,
            temporal_run_id: &str,
        ) -> Result<(), RepositoryError> {
            self.inner
                .update_temporal_linkage_for_tenant(
                    tenant_id,
                    execution_id,
                    temporal_workflow_id,
                    temporal_run_id,
                )
                .await
        }
        async fn append_event(
            &self,
            _execution_id: ExecutionId,
            _sequence_number: i64,
            _event_type: String,
            _payload: Value,
            _iteration_number: Option<u8>,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn find_events_by_execution(
            &self,
            _id: ExecutionId,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<WorkflowExecutionEventRecord>, RepositoryError> {
            Ok(self
                .events
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .collect())
        }
        async fn find_tenant_id_by_execution(
            &self,
            id: ExecutionId,
        ) -> Result<Option<TenantId>, RepositoryError> {
            self.inner.find_tenant_id_by_execution(id).await
        }
    }

    fn event(sequence: i64, event_type: &str, payload: Value) -> WorkflowExecutionEventRecord {
        WorkflowExecutionEventRecord {
            sequence,
            event_type: event_type.to_string(),
            state_name: None,
            iteration_number: Some(1),
            payload,
            recorded_at: chrono::Utc::now(),
        }
    }

    /// A workflow whose Agent state's start was refused: the worker's events
    /// as `temporal_event_listener` stores them (the serialised payload, whose
    /// `error` is the worker's reason), and the execution failed. Its
    /// `aegis.workflow.wait` answer carries that reason as `error`.
    #[tokio::test]
    async fn workflow_wait_of_a_workflow_whose_step_was_refused_answers_the_reason() {
        let h = harness().await;
        let workflow = WorkflowParser::parse_yaml(
            r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: convert-units
  version: "1.0.0"
spec:
  initial_state: CONVERT
  states:
    CONVERT:
      kind: Agent
      agent: unit-conversion-agent
      input: "{{input}}"
      transitions: []
"#,
        )
        .expect("the test workflow parses");
        let execution_id = ExecutionId::new();
        let mut execution = WorkflowExecution::new(&workflow, execution_id, json!({}));
        execution.status = ExecutionStatus::Failed;
        let reason = format!(
            "Agent execution failed: Failed to start execution: Execution refused: {REFUSAL}"
        );
        let repo = WithEvents {
            inner: InMemoryWorkflowExecutionRepository::new(),
            events: vec![
                event(
                    1,
                    "WorkflowIterationFailed",
                    json!({ "event_type": "WorkflowIterationFailed", "error": reason }),
                ),
                event(
                    2,
                    "WorkflowExecutionFailed",
                    json!({ "event_type": "WorkflowExecutionFailed", "error": reason }),
                ),
            ],
        };
        repo.save_for_tenant(&tenant(), &execution).await.unwrap();
        let service = h.service.with_workflow_execution_repo(Arc::new(repo));

        let answer = direct(
            service
                .invoke_aegis_workflow_wait_tool(
                    &mut json!({ "execution_id": execution_id.0.to_string(), "timeout_seconds": 1 }),
                    &tenant_scope(),
                )
                .await
                .expect("aegis.workflow.wait answers"),
        );
        assert_eq!(
            answer.get("error").and_then(Value::as_str),
            Some(reason.as_str()),
            "aegis.workflow.wait answered {answer}"
        );
    }

    // -----------------------------------------------------------------------
    // AEGIS ADR-005 O7c and O7d: the program check and the generator's floor
    // -----------------------------------------------------------------------

    /// `vrp-solver-agent` 1.0.1 as `aegis.agent.export` answered it on
    /// 2026-10-06: it declares `cmd.run` and carries no program.
    const VRP_SOLVER: &str = include_str!("../../../tests/fixtures/vrp-solver-agent-1.0.1.yaml");

    const O7_SENTENCE: &str = "agent 'vrp-solver-agent' has no program: its instruction asks the \
        model to write or compute the solution on each run; the agent must carry its program and \
        run it";

    /// The output declaration's sentence for the fixture's itinerary.
    const O8_SENTENCE: &str = "agent 'vrp-solver-agent' writes /workspace/itinerary.md but does \
        not declare it in spec.execution.outputs";

    /// `vrp-solver-agent` 1.0.1 meets both floors: every clause is reported,
    /// joined with "; ".
    fn vrp_refusal() -> String {
        format!("{O7_SENTENCE}; {O8_SENTENCE}")
    }

    /// A program runner that answers as it is told and keeps what it was given.
    struct ScriptedRunner {
        answer: std::sync::Mutex<
            Option<
                Result<
                    crate::domain::runtime::ContainerStepResult,
                    crate::domain::runtime::ContainerStepError,
                >,
            >,
        >,
        seen: std::sync::Mutex<Vec<crate::domain::runtime::ContainerStepConfig>>,
    }

    impl ScriptedRunner {
        fn exits(exit_code: i32, stdout: &str, stderr: &str) -> Arc<Self> {
            Arc::new(Self {
                answer: std::sync::Mutex::new(Some(Ok(
                    crate::domain::runtime::ContainerStepResult {
                        exit_code,
                        stdout: stdout.to_string(),
                        stderr: stderr.to_string(),
                        duration_ms: 12,
                    },
                ))),
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn times_out() -> Arc<Self> {
            Arc::new(Self {
                answer: std::sync::Mutex::new(Some(Err(
                    crate::domain::runtime::ContainerStepError::TimeoutExpired {
                        timeout_secs: PROGRAM_CHECK_TIMEOUT_SECS,
                    },
                ))),
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl crate::domain::runtime::ContainerStepRunner for ScriptedRunner {
        async fn run_step(
            &self,
            config: crate::domain::runtime::ContainerStepConfig,
        ) -> Result<
            crate::domain::runtime::ContainerStepResult,
            crate::domain::runtime::ContainerStepError,
        > {
            self.seen.lock().unwrap().push(config);
            self.answer
                .lock()
                .unwrap()
                .take()
                .expect("the program ran once")
        }
    }

    /// A manifest named `name` carrying a program, run on its sample input.
    fn program_yaml(name: &str, version: &str) -> String {
        format!(
            r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: {name}
  version: "{version}"
spec:
  runtime:
    image: ghcr.io/example/python:3.11
  task:
    instruction: Call cmd.run with "python /opt/aegis/program/solve.py" and present its output.
  program:
    files:
      - path: solve.py
        content: |
          import json
          data = json.load(open("/opt/aegis/program/input.json"))
          print(json.dumps({{"total": sum(data["amounts"])}}))
    run: python /opt/aegis/program/solve.py
    sample_input: {{"amounts": [3, 4.5]}}
  tools:
    - cmd.run
"#
        )
    }

    async fn deployed(h: &Harness, name: &str) -> bool {
        h.service
            .agent_lifecycle
            .lookup_agent_for_tenant(&tenant(), name)
            .await
            .unwrap()
            .is_some()
    }

    async fn create(service: &ToolInvocationService, yaml: &str, caller: Option<&str>) -> Value {
        direct(
            service
                .invoke_aegis_agent_create_tool(
                    &mut json!({ "manifest_yaml": yaml }),
                    &tenant_scope(),
                    caller,
                )
                .await
                .expect("aegis.agent.create answers"),
        )
    }

    async fn update(service: &ToolInvocationService, yaml: &str, caller: Option<&str>) -> Value {
        direct(
            service
                .invoke_aegis_agent_update_tool(
                    &mut json!({ "manifest_yaml": yaml }),
                    &tenant_scope(),
                    caller,
                )
                .await
                .expect("aegis.agent.update answers"),
        )
    }

    fn errors(answer: &Value) -> Vec<String> {
        answer
            .get("errors")
            .and_then(Value::as_array)
            .map(|errors| {
                errors
                    .iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// T1, O7d: the generator's `aegis.agent.create` and `aegis.agent.update`
    /// of `vrp-solver-agent` 1.0.1 are refused with O7's sentence (joined to
    /// the output declaration's, which it also meets) and nothing
    /// is deployed; the same manifest from another caller meets no floor.
    #[tokio::test]
    async fn the_generator_cannot_deploy_vrp_solver_agent_without_a_program() {
        let h = harness().await;
        let mut complaints = Vec::new();

        let answer = create(&h.service, VRP_SOLVER, Some(AGENT_GENERATOR_NAME)).await;
        if errors(&answer) != vec![vrp_refusal()] {
            complaints.push(format!("the generator's create answered {answer}"));
        }
        if answer.get("deployed") != Some(&Value::Bool(false)) {
            complaints.push(format!(
                "the generator's create did not say deployed false: {answer}"
            ));
        }
        if deployed(&h, "vrp-solver-agent").await {
            complaints.push("vrp-solver-agent was deployed by the generator".to_string());
        }

        // Deployed by a person, then updated by the generator: refused.
        let by_person = create(&h.service, VRP_SOLVER, None).await;
        if by_person.get("deployed") != Some(&Value::Bool(true)) {
            complaints.push(format!("a person's create was refused: {by_person}"));
        }
        let raised = VRP_SOLVER.replace("version: 1.0.1", "version: 1.0.2");
        let answer = update(&h.service, &raised, Some(AGENT_GENERATOR_NAME)).await;
        if errors(&answer) != vec![vrp_refusal()] {
            complaints.push(format!("the generator's update answered {answer}"));
        }
        if answer.get("updated") != Some(&Value::Bool(false)) {
            complaints.push(format!(
                "the generator's update did not say updated false: {answer}"
            ));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T5, O7c: a program that exits 1, prints nothing or does not finish is
    /// refused with its sentence and the agent is not deployed; a program
    /// that prints its result is deployed with `program_check` in the answer.
    #[tokio::test]
    async fn create_runs_the_program_on_its_sample_input_before_deploying() {
        let mut complaints = Vec::new();
        let cases: Vec<(&str, Arc<ScriptedRunner>, Option<String>)> = vec![
            (
                "exits-one-agent",
                ScriptedRunner::exits(1, "", "Traceback\nKeyError: 'amounts'\n"),
                Some(
                    "agent 'exits-one-agent' program failed on its sample input: it exited 1; \
                     Traceback\nKeyError: 'amounts'"
                        .to_string(),
                ),
            ),
            (
                "silent-agent",
                ScriptedRunner::exits(0, "  \n", ""),
                Some("agent 'silent-agent' program printed nothing on its sample input".to_string()),
            ),
            (
                "endless-agent",
                ScriptedRunner::times_out(),
                Some(format!(
                    "agent 'endless-agent' program did not finish on its sample input within {PROGRAM_CHECK_TIMEOUT_SECS} s"
                )),
            ),
            ("sum-agent", ScriptedRunner::exits(0, "{\"total\": 7.5}\n", ""), None),
        ];
        for (name, runner, refusal) in cases {
            let h = harness().await;
            let service = h.service.with_program_runner(runner.clone());
            let answer = create(&service, &program_yaml(name, "1.0.0"), None).await;
            let is_deployed = service
                .agent_lifecycle
                .lookup_agent_for_tenant(&tenant(), name)
                .await
                .unwrap()
                .is_some();
            match refusal {
                Some(sentence) => {
                    if errors(&answer) != vec![sentence.clone()] {
                        complaints.push(format!(
                            "{name}: expected \"{sentence}\", answered {answer}"
                        ));
                    }
                    if is_deployed || answer.get("deployed") != Some(&Value::Bool(false)) {
                        complaints.push(format!("{name}: deployed after a failed check: {answer}"));
                    }
                }
                None => {
                    if !is_deployed || answer.get("deployed") != Some(&Value::Bool(true)) {
                        complaints.push(format!("{name}: not deployed: {answer}"));
                    }
                    let expected = json!({
                        "exit_code": 0,
                        "stdout": "{\"total\": 7.5}\n",
                        "stderr": "",
                        "duration_ms": 12,
                    });
                    if answer.get("program_check") != Some(&expected) {
                        complaints.push(format!(
                            "{name}: program_check is not in the answer: {answer}"
                        ));
                    }
                }
            }
            let seen = runner.seen.lock().unwrap();
            match seen.first() {
                None => complaints.push(format!("{name}: the program never ran")),
                Some(config) => {
                    let paths: Vec<(&str, u32)> = config
                        .files
                        .iter()
                        .map(|f| (f.path.as_str(), f.mode))
                        .collect();
                    if paths
                        != vec![
                            ("/opt/aegis/program/solve.py", 0o644),
                            ("/opt/aegis/program/input.json", 0o644),
                        ]
                    {
                        complaints.push(format!("{name}: files placed {paths:?}"));
                    }
                    if config.files[1].content != br#"{"amounts":[3,4.5]}"#.to_vec() {
                        complaints.push(format!(
                            "{name}: the sample input placed was {}",
                            String::from_utf8_lossy(&config.files[1].content)
                        ));
                    }
                    if config.command != vec!["python /opt/aegis/program/solve.py".to_string()]
                        || config.entrypoint != Some(vec!["/bin/sh".to_string(), "-c".to_string()])
                        || config.image != "ghcr.io/example/python:3.11"
                        || config.network_mode.as_deref() != Some("none")
                    {
                        complaints.push(format!(
                            "{name}: ran {:?} {:?} in {} on network {:?}",
                            config.entrypoint, config.command, config.image, config.network_mode
                        ));
                    }
                }
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// O7c: an update runs the check too; a passing check carries
    /// `program_check` in the update's answer.
    #[tokio::test]
    async fn update_runs_the_program_on_its_sample_input_before_updating() {
        let h = harness().await;
        let first = ScriptedRunner::exits(0, "7.5\n", "");
        let service = h.service.with_program_runner(first);
        let created = create(&service, &program_yaml("sum-agent", "1.0.0"), None).await;
        assert_eq!(
            created.get("deployed"),
            Some(&Value::Bool(true)),
            "{created}"
        );
        let failing = ScriptedRunner::exits(2, "", "SyntaxError\n");
        let service = service.with_program_runner(failing);
        let answer = update(&service, &program_yaml("sum-agent", "1.0.1"), None).await;
        assert_eq!(
            errors(&answer),
            vec![
                "agent 'sum-agent' program failed on its sample input: it exited 2; SyntaxError"
                    .to_string()
            ],
            "{answer}"
        );
        assert_eq!(answer.get("updated"), Some(&Value::Bool(false)), "{answer}");
        let passing = ScriptedRunner::exits(0, "7.5\n", "");
        let service = service.with_program_runner(passing);
        let answer = update(&service, &program_yaml("sum-agent", "1.0.1"), None).await;
        assert_eq!(answer.get("updated"), Some(&Value::Bool(true)), "{answer}");
        assert_eq!(
            answer.pointer("/program_check/stdout"),
            Some(&json!("7.5\n")),
            "{answer}"
        );
    }

    /// C2: a node with no program runner refuses a manifest carrying a
    /// program, never deploying it unchecked.
    #[tokio::test]
    async fn a_node_with_no_program_runner_refuses_a_program_unchecked() {
        let h = harness().await;
        let answer = create(&h.service, &program_yaml("sum-agent", "1.0.0"), None).await;
        assert_eq!(
            errors(&answer),
            vec![
                "agent 'sum-agent' program cannot be checked: this node has no program runner"
                    .to_string()
            ],
            "{answer}"
        );
        assert!(
            !deployed(&h, "sum-agent").await,
            "deployed unchecked: {answer}"
        );
    }

    // -----------------------------------------------------------------------
    // The generator's floor: an agent that writes a file declares it
    // -----------------------------------------------------------------------

    /// A manifest carrying a program whose instruction (or prompt template)
    /// writes `/workspace/itinerary.md`, declaring `outputs`.
    fn writing_yaml(name: &str, version: &str, in_template: bool, outputs: &[&str]) -> String {
        let mut yaml = program_yaml(name, version);
        let write = "Write the itinerary to /workspace/itinerary.md.";
        if in_template {
            yaml = yaml.replace(
                "  program:\n",
                &format!("    prompt_template: \"{{{{input}}}} {write}\"\n  program:\n"),
            );
        } else {
            yaml = yaml.replace(
                "present its output.\n",
                &format!("present its output. {write}\n"),
            );
        }
        if !outputs.is_empty() {
            let listed: String = outputs
                .iter()
                .map(|path| format!("      - path: {path}\n"))
                .collect();
            yaml.push_str(&format!("  execution:\n    outputs:\n{listed}"));
        }
        yaml
    }

    fn o8_sentence(name: &str) -> String {
        format!(
            "agent '{name}' writes /workspace/itinerary.md but does not declare it in \
             spec.execution.outputs"
        )
    }

    /// T3: the generator's `aegis.agent.create` of an agent whose instruction,
    /// or prompt template, writes a `/workspace` file its outputs do not list
    /// is refused with the sentence, and nothing is deployed.
    #[tokio::test]
    async fn the_generator_refuses_an_agent_that_writes_an_undeclared_file() {
        let h = harness().await;
        let mut complaints = Vec::new();
        for (name, in_template) in [("route-agent", false), ("route-template-agent", true)] {
            let yaml = writing_yaml(name, "1.0.0", in_template, &[]);
            let answer = create(&h.service, &yaml, Some(AGENT_GENERATOR_NAME)).await;
            println!("{name}: the generator's create answered {answer}");
            if errors(&answer) != vec![o8_sentence(name)] {
                complaints.push(format!(
                    "{name} writing an undeclared file was not refused with the sentence: {answer}"
                ));
            }
            if deployed(&h, name).await {
                complaints.push(format!("{name} was deployed by the generator"));
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T4: the floor passes an agent whose outputs list the file it writes,
    /// and does not judge an agent another caller deploys.
    #[test]
    fn a_declared_file_and_another_callers_agent_meet_no_floor() {
        let mut complaints = Vec::new();
        let declared = AgentManifestParser::parse_yaml(&writing_yaml(
            "route-agent",
            "1.0.0",
            false,
            &["/workspace/itinerary.md"],
        ))
        .expect("the declared manifest parses");
        let floor = generator_floor(Some(AGENT_GENERATOR_NAME), &declared, &[]);
        println!("declared manifest: the floor answered {floor:?}");
        if floor.is_some() {
            complaints.push(format!(
                "an agent declaring the file it writes was refused: {floor:?}"
            ));
        }
        let undeclared =
            AgentManifestParser::parse_yaml(&writing_yaml("route-agent", "1.0.0", false, &[]))
                .expect("the undeclared manifest parses");
        if let Some(sentence) = generator_floor(None, &undeclared, &[]) {
            complaints.push(format!("another caller's agent met the floor: {sentence}"));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T5: `vrp-solver-agent` 1.0.1 meets both floors, and both sentences
    /// are reported, joined with "; ".
    #[test]
    fn the_vrp_fixture_is_refused_with_both_sentences() {
        let manifest = AgentManifestParser::parse_yaml(VRP_SOLVER).expect("the fixture parses");
        let floor = generator_floor(Some(AGENT_GENERATOR_NAME), &manifest, &[]);
        println!("vrp-solver-agent 1.0.1: the floor answered {floor:?}");
        assert_eq!(
            floor,
            Some(vrp_refusal()),
            "the fixture was not refused with both sentences"
        );
    }

    /// T6: the generator's `aegis.agent.update` of an agent that writes an
    /// undeclared file is refused with the sentence; with the file declared
    /// the floor lets it through.
    #[tokio::test]
    async fn the_generators_update_refuses_an_agent_that_writes_an_undeclared_file() {
        let h = harness().await;
        let mut complaints = Vec::new();
        let service =
            h.service
                .with_program_runner(ScriptedRunner::exits(0, "{\"total\": 7.5}", ""));
        let by_person = create(&service, &program_yaml("route-agent", "1.0.0"), None).await;
        println!("a person's create answered {by_person}");
        if by_person.get("deployed") != Some(&Value::Bool(true)) {
            complaints.push(format!("a person's create was refused: {by_person}"));
        }
        // A runner that answers again, so the update reaches the floor or
        // passes it on the floor's word alone.
        let service = service.with_program_runner(ScriptedRunner::exits(0, "{\"total\": 7.5}", ""));
        let yaml = writing_yaml("route-agent", "1.0.1", false, &[]);
        let answer = update(&service, &yaml, Some(AGENT_GENERATOR_NAME)).await;
        println!("the generator's update answered {answer}");
        if errors(&answer) != vec![o8_sentence("route-agent")] {
            complaints.push(format!(
                "the generator's update of an undeclared file was not refused with the sentence: {answer}"
            ));
        }
        if answer.get("updated") != Some(&Value::Bool(false)) {
            complaints.push(format!("the update did not say updated false: {answer}"));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    // AEGIS ADR-132 S7a to S7f: an agent declares a remote server's tools by
    // context, and the creator is taught so.
    mod remote_tools {
        use super::*;
        use crate::application::tool_catalog::StandardToolCatalog;

        const NOTES_READ: &str = "nuclear-notes.pages.read";

        /// The creator's own template, as the daemon deploys it.
        const CREATOR_TEMPLATE: &str =
            include_str!("../../../../../cli/templates/agents/agent-creator-agent.yaml");

        /// A catalog holding the built-in tools, as the daemon fills it.
        async fn builtin_catalog() -> Arc<StandardToolCatalog> {
            let catalog = Arc::new(StandardToolCatalog::new());
            let tools = ToolRouter::new(vec![])
                .list_tools()
                .await
                .expect("the built-in tools list");
            catalog.refresh_from(tools).await;
            catalog
        }

        /// The harness's service with a catalog and the node's remote servers.
        async fn service_with(servers: &[&str]) -> ToolInvocationService {
            let h = harness().await;
            h.service
                .with_tool_catalog(builtin_catalog().await)
                .with_remote_tool_servers(servers.iter().map(|s| s.to_string()).collect())
        }

        async fn is_deployed(service: &ToolInvocationService, name: &str) -> bool {
            service
                .agent_lifecycle
                .lookup_agent_for_tenant(&tenant(), name)
                .await
                .unwrap()
                .is_some()
        }

        /// An agent naming `tools`, with a `nuclear-notes` context or none.
        fn notes_yaml(name: &str, version: &str, tools: &[&str], context: bool) -> String {
            let listed: String = tools.iter().map(|t| format!("    - {t}\n")).collect();
            let contexts = if context {
                "  contexts:\n    - service: nuclear-notes\n"
            } else {
                ""
            };
            format!(
                "apiVersion: 100monkeys.ai/v1\nkind: Agent\nmetadata:\n  name: {name}\n  \
                 version: \"{version}\"\nspec:\n  runtime:\n    language: python\n    \
                 version: \"3.11\"\n  task:\n    instruction: Read the pages the request \
                 names and summarise them.\n{contexts}  tools:\n{listed}"
            )
        }

        fn s7b_sentence(name: &str) -> String {
            format!(
                "agent '{name}' uses nuclear-notes.* tools but declares no nuclear-notes context"
            )
        }

        /// R1: the creator's create and update accept a remote server's tool
        /// declared with its context when the node names the server.
        #[tokio::test]
        async fn create_and_update_accept_a_remote_tool_declared_with_its_context() {
            let service = service_with(&["nuclear-notes"]).await;
            let mut complaints = Vec::new();
            let name = "notes-summarizer-agent";
            let answer = create(
                &service,
                &notes_yaml(name, "1.0.0", &[NOTES_READ], true),
                Some(AGENT_GENERATOR_NAME),
            )
            .await;
            println!("the creator's create answered {answer}");
            if answer.get("deployed") != Some(&Value::Bool(true)) {
                complaints.push(format!(
                    "a remote tool declared with its context was refused at create: {answer}"
                ));
            }
            let answer = update(
                &service,
                &notes_yaml(
                    name,
                    "1.0.1",
                    &[NOTES_READ, "nuclear-notes.pages.list"],
                    true,
                ),
                Some(AGENT_GENERATOR_NAME),
            )
            .await;
            println!("the creator's update answered {answer}");
            if answer.get("updated") != Some(&Value::Bool(true)) {
                complaints.push(format!(
                    "a remote tool declared with its context was refused at update: {answer}"
                ));
            }
            assert!(complaints.is_empty(), "{}", complaints.join("\n"));
        }

        /// R2: a tool of a server the node does not name is still refused as
        /// not registered, and nothing is deployed.
        #[tokio::test]
        async fn a_tool_of_a_server_the_node_does_not_name_is_refused() {
            let service = service_with(&[]).await;
            let name = "notes-summarizer-agent";
            let answer = create(
                &service,
                &notes_yaml(name, "1.0.0", &[NOTES_READ], true),
                None,
            )
            .await;
            println!("create on a node with no remote servers answered {answer}");
            let refused = errors(&answer).iter().any(|e| {
                e.starts_with("Agent manifest references tools not registered on this platform: [nuclear-notes.pages.read]")
            });
            assert!(
                refused && !is_deployed(&service, name).await,
                "a tool of a server the node does not name was not refused as not registered: {answer}"
            );
        }

        /// R3: the creator's manifest naming the tool with no context is
        /// refused with the context sentence, joined with "; " to the floor's
        /// other sentences.
        #[tokio::test]
        async fn the_creators_manifest_with_no_context_is_refused_with_the_sentence() {
            let service = service_with(&["nuclear-notes"]).await;
            let mut complaints = Vec::new();
            let name = "notes-summarizer-agent";
            let answer = create(
                &service,
                &notes_yaml(name, "1.0.0", &[NOTES_READ], false),
                Some(AGENT_GENERATOR_NAME),
            )
            .await;
            println!("the creator's create with no context answered {answer}");
            if errors(&answer) != vec![s7b_sentence(name)] {
                complaints.push(format!(
                    "the creator's manifest with no context was not refused with the sentence: {answer}"
                ));
            }
            let joined = writing_yaml("route-agent", "1.0.0", false, &[]).replace(
                "    - cmd.run\n",
                "    - cmd.run\n    - nuclear-notes.pages.read\n",
            );
            let answer = create(&service, &joined, Some(AGENT_GENERATOR_NAME)).await;
            println!("the creator's create meeting two floors answered {answer}");
            let expected = format!(
                "{}; {}",
                o8_sentence("route-agent"),
                s7b_sentence("route-agent")
            );
            if errors(&answer) != vec![expected.clone()] {
                complaints.push(format!(
                    "the two refusals were not joined with \"; \": expected [{expected}], got {answer}"
                ));
            }
            if is_deployed(&service, name).await || is_deployed(&service, "route-agent").await {
                complaints.push("a refused manifest was deployed".to_string());
            }
            assert!(complaints.is_empty(), "{}", complaints.join("\n"));
        }

        /// R4: another caller's manifest naming the tool with no context is
        /// accepted: the binding-grant route stands for it.
        #[tokio::test]
        async fn another_callers_manifest_with_no_context_is_accepted() {
            let service = service_with(&["nuclear-notes"]).await;
            let name = "remote-tools-proof-b";
            let answer = create(
                &service,
                &notes_yaml(name, "1.0.0", &[NOTES_READ], false),
                None,
            )
            .await;
            println!("a person's create with no context answered {answer}");
            assert!(
                answer.get("deployed") == Some(&Value::Bool(true)) && is_deployed(&service, name).await,
                "another caller's manifest naming a remote tool with no context was refused: {answer}"
            );
        }

        /// R5: the catalog's refusal names the node's remote servers and the
        /// context form.
        #[tokio::test]
        async fn the_refusal_names_the_remote_servers_and_the_context() {
            let service = service_with(&["nuclear-notes"]).await;
            let answer = create(
                &service,
                &notes_yaml(
                    "notes-summarizer-agent",
                    "1.0.0",
                    &["notes.pages.read"],
                    true,
                ),
                None,
            )
            .await;
            println!("create naming an unknown tool answered {answer}");
            let refusal = errors(&answer).join(" ");
            assert!(
                refusal.contains("[notes.pages.read]")
                    && refusal.contains("this node's remote servers: [nuclear-notes]")
                    && refusal.contains("spec.contexts"),
                "the refusal does not name the node's remote servers and the context: {refusal}"
            );
        }

        /// R6: `aegis.tools.list` answers each remote server with its tools'
        /// form and its context.
        #[tokio::test]
        async fn tools_list_answers_the_remote_servers() {
            let service = service_with(&["nuclear-notes"]).await;
            let context = SecurityContext {
                name: "test".to_string(),
                description: String::new(),
                capabilities: vec![crate::domain::security_context::Capability {
                    tool_pattern: "*".to_string(),
                    path_allowlist: None,
                    command_allowlist: None,
                    subcommand_allowlist: None,
                    domain_allowlist: None,
                    max_response_size: None,
                    rate_limit: None,
                    max_concurrent: None,
                }],
                deny_list: vec![],
                metadata: crate::domain::security_context::SecurityContextMetadata {
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                    version: 1,
                },
            };
            let answer = direct(
                service
                    .invoke_aegis_tools_list(&json!({}), &context)
                    .await
                    .expect("aegis.tools.list answers"),
            );
            let servers = answer.get("remote_servers").cloned();
            println!("aegis.tools.list answered remote_servers {servers:?}");
            assert_eq!(
                servers,
                Some(json!([{
                    "server": "nuclear-notes",
                    "tools": "nuclear-notes.*",
                    "context": {"service": "nuclear-notes"}
                }])),
                "aegis.tools.list does not answer the node's remote servers"
            );
        }

        /// R7: the search and list tools declare their inputs, so a model's
        /// query reaches the handler.
        #[tokio::test]
        async fn the_search_and_list_tools_declare_their_inputs() {
            let tools = ToolRouter::new(vec![]).list_tools().await.unwrap();
            let schema = |name: &str| {
                tools
                    .iter()
                    .find(|t| t.name == name)
                    .map(|t| t.input_schema.clone())
                    .unwrap_or(Value::Null)
            };
            let mut complaints = Vec::new();
            let search = schema("aegis.tools.search");
            for key in [
                "keyword",
                "name_pattern",
                "source",
                "category",
                "tags",
                "fleet_capable",
            ] {
                if search.pointer(&format!("/properties/{key}")).is_none() {
                    complaints.push(format!(
                        "aegis.tools.search does not declare {key}: {search}"
                    ));
                }
            }
            let list = schema("aegis.tools.list");
            for key in ["offset", "limit", "source", "category", "fleet_capable"] {
                if list.pointer(&format!("/properties/{key}")).is_none() {
                    complaints.push(format!("aegis.tools.list does not declare {key}: {list}"));
                }
            }
            assert!(complaints.is_empty(), "{}", complaints.join("\n"));
        }

        /// R8: the creator's step 3 teaches the remote servers' tools, their
        /// context, and that the search does not show them.
        #[test]
        fn the_creator_template_teaches_the_remote_servers() {
            let manifest: serde_yaml::Value =
                serde_yaml::from_str(CREATOR_TEMPLATE).expect("the creator template parses");
            let instruction = manifest["spec"]["task"]["instruction"]
                .as_str()
                .expect("the creator has an instruction");
            let flat = instruction.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut complaints = Vec::new();
            for sentence in [
                "A remote server's tools are named `<server>.<tool>`",
                "declare a `spec.contexts` entry for the server: `contexts: [{service: <server>}]`",
                "They do not appear in the `aegis.tools.search` results",
                "its refusal names this node's remote servers",
            ] {
                if !flat.contains(sentence) {
                    complaints.push(format!("step 3 does not say: {sentence}"));
                }
            }
            assert!(complaints.is_empty(), "{}", complaints.join("\n"));
        }

        /// R9: a tool past the 200th catalog entry is not refused as
        /// unregistered.
        #[tokio::test]
        async fn a_tool_past_the_200th_catalog_entry_passes_the_check() {
            let h = harness().await;
            let catalog = Arc::new(StandardToolCatalog::new());
            let mut tools = ToolRouter::new(vec![]).list_tools().await.unwrap();
            for i in 0..250 {
                tools.push(crate::infrastructure::tool_router::ToolMetadata {
                    name: format!("filler.tool_{i:03}"),
                    description: "A filler tool.".to_string(),
                    input_schema: json!({"type": "object"}),
                    ..Default::default()
                });
            }
            let last = tools.last().expect("a tool").name.clone();
            catalog.refresh_from(tools).await;
            let service = h.service.with_tool_catalog(catalog);
            let answer = create(
                &service,
                &notes_yaml("filler-agent", "1.0.0", &[last.as_str()], false),
                None,
            )
            .await;
            println!("create naming {last} answered {answer}");
            assert_eq!(
                answer.get("deployed"),
                Some(&Value::Bool(true)),
                "a tool past the 200th catalog entry was refused as unregistered: {answer}"
            );
        }
    }
}
