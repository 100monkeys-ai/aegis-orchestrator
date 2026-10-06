use super::attachment_args::parse_attachments;
use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_agent_create_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
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

        // ADR-087: Validate that all declared tools exist in the tool catalog.
        if let Some(catalog) = &self.tool_catalog {
            let declared_tools = &manifest.spec.tools;
            if !declared_tools.is_empty() {
                let available = catalog
                    .list_tools(
                        &["*".to_string()],
                        crate::application::tool_catalog::ToolListQuery {
                            source: None,
                            category: None,
                            limit: Some(1000),
                            offset: Some(0),
                            fleet_capable: None,
                        },
                    )
                    .await;
                let available_names: std::collections::HashSet<&str> =
                    available.tools.iter().map(|t| t.name.as_str()).collect();
                let unknown: Vec<&str> = declared_tools
                    .iter()
                    .filter(|t| !available_names.contains(t.as_str()))
                    .map(|t| t.as_str())
                    .collect();
                if !unknown.is_empty() {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.create",
                        "validated": false,
                        "deployed": false,
                        "errors": [format!(
                            "Agent manifest references tools not registered on this platform: [{}]. Call aegis.tools.list to see available tools.",
                            unknown.join(", ")
                        )]
                    })));
                }
            }
        }

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
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.create",
                    "validated": true,
                    "deployed": true,
                    "agent_id": agent_id.0.to_string(),
                    "name": manifest.metadata.name,
                    "version": manifest.metadata.version,
                    "force": force,
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path
                })))
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
                self.bind_to_goal(
                    goal_id,
                    &exec_id.to_string(),
                    crate::domain::goal::BoundKind::Agent,
                )
                .await;
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

        // ADR-087: Validate that all declared tools exist in the tool catalog.
        if let Some(catalog) = &self.tool_catalog {
            let declared_tools = &manifest.spec.tools;
            if !declared_tools.is_empty() {
                let available = catalog
                    .list_tools(
                        &["*".to_string()],
                        crate::application::tool_catalog::ToolListQuery {
                            source: None,
                            category: None,
                            limit: Some(1000),
                            offset: Some(0),
                            fleet_capable: None,
                        },
                    )
                    .await;
                let available_names: std::collections::HashSet<&str> =
                    available.tools.iter().map(|t| t.name.as_str()).collect();
                let unknown: Vec<&str> = declared_tools
                    .iter()
                    .filter(|t| !available_names.contains(t.as_str()))
                    .map(|t| t.as_str())
                    .collect();
                if !unknown.is_empty() {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.update",
                        "validated": false,
                        "updated": false,
                        "errors": [format!(
                            "Agent manifest references tools not registered on this platform: [{}]. Call aegis.tools.list to see available tools.",
                            unknown.join(", ")
                        )]
                    })));
                }
            }
        }

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
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.update",
                    "validated": true,
                    "updated": true,
                    "name": manifest.metadata.name,
                    "version": manifest.metadata.version,
                    "agent_id": agent_id.0.to_string(),
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path,
                })))
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

    const REFUSAL: &str = "Agent 'unit-conversion-agent' is refused (AEGIS ADR-005 O5): it \
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
}
