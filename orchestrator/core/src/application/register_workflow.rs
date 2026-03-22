// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Register Workflow Use Case
//!
//! Application service for registering new workflows with the Temporal engine.
//!
//! # DDD Pattern: Application Service
//!
//! - **Layer:** Application
//! - **Responsibility:** Orchestrate registration of workflow definitions
//! - **Collaborators:**
//!   - Domain: Workflow aggregate (validation)
//!   - Infrastructure: WorkflowParser, WorkflowRepository, TemporalClient, EventBus
//!
//! # Flow
//!
//! 1. Accept YAML manifest as input
//! 2. Parse via WorkflowParser → get Workflow domain aggregate
//! 3. Validate via Workflow invariants (no gaps, all transitions valid)
//! 4. Map via TemporalWorkflowMapper → TemporalWorkflowDefinition (JSON)
//! 5. Register with Temporal via TemporalClient (HTTP POST to TypeScript worker)
//! 6. Persist workflow to WorkflowRepository (PostgreSQL)
//! 7. Publish DomainEvent::WorkflowRegistered
//! 8. Return workflow_id and registration status
//!
//! # Error Handling
//!
//! Returns anyhow::Error with context:
//! - ParseError: Invalid YAML or manifest structure
//! - ValidationError: Workflow invariants violated (missing states, circular references)
//! - RegistrationError: Temporal server unavailable or registration failed
//! - PersistenceError: Database save failed
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for register workflow

use crate::application::agent::AgentLifecycleService;
use crate::application::ports::WorkflowEnginePort;
use crate::application::temporal_mapper::DEFAULT_WORKFLOW_VERSION;
use crate::domain::repository::WorkflowRepository;
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::workflow_parser::WorkflowParser;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tracing::info;

/// Registered workflow response
#[derive(Debug, Clone, serde::Serialize)]
pub struct RegisteredWorkflow {
    pub workflow_id: String,
    pub name: String,
    pub version: String,
    pub status: String,
    pub registered_at: chrono::DateTime<chrono::Utc>,
}

/// Register Workflow Use Case
#[async_trait]
pub trait RegisterWorkflowUseCase: Send + Sync {
    async fn register_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        yaml_manifest: &str,
        force: bool,
    ) -> Result<RegisteredWorkflow>;

    /// Register a new workflow from YAML manifest
    ///
    /// # Arguments
    ///
    /// * `yaml_manifest` - Complete workflow manifest in YAML format
    ///
    /// # Returns
    ///
    /// Registered workflow metadata on success
    ///
    /// # Errors
    ///
    /// - Parse errors: Invalid YAML
    /// - Validation errors: Workflow invariants violated
    /// - Registration errors: Temporal server unavailable
    /// - Persistence errors: Database save failed
    /// - Version error: Workflow same name + version already exists and `force = false`
    async fn register_workflow(
        &self,
        yaml_manifest: &str,
        force: bool,
    ) -> Result<RegisteredWorkflow> {
        self.register_workflow_for_tenant(&TenantId::local_default(), yaml_manifest, force)
            .await
    }
}

/// Standard implementation of RegisterWorkflowUseCase
pub struct StandardRegisterWorkflowUseCase {
    workflow_repository: Arc<dyn WorkflowRepository>,
    workflow_engine: Arc<tokio::sync::RwLock<Option<Arc<dyn WorkflowEnginePort>>>>,
    event_bus: Arc<EventBus>,
    /// Agent lifecycle service used to resolve agent name references to UUIDs at
    /// workflow deploy time, ensuring execution-time failures are surfaced early.
    agent_service: Arc<dyn AgentLifecycleService>,
}

impl StandardRegisterWorkflowUseCase {
    pub fn new(
        workflow_repository: Arc<dyn WorkflowRepository>,
        workflow_engine: Arc<tokio::sync::RwLock<Option<Arc<dyn WorkflowEnginePort>>>>,
        event_bus: Arc<EventBus>,
        agent_service: Arc<dyn AgentLifecycleService>,
    ) -> Self {
        Self {
            workflow_repository,
            workflow_engine,
            event_bus,
            agent_service,
        }
    }
}

#[async_trait]
impl RegisterWorkflowUseCase for StandardRegisterWorkflowUseCase {
    async fn register_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        yaml_manifest: &str,
        force: bool,
    ) -> Result<RegisteredWorkflow> {
        info!("Registering workflow from manifest (force={force})");

        // Step 1: Parse YAML → Workflow domain aggregate
        let mut workflow = WorkflowParser::parse_yaml(yaml_manifest)
            .map_err(|e| anyhow::anyhow!("Failed to parse workflow YAML manifest: {e}"))?;
        let workflow_name = workflow.metadata.name.clone();
        let workflow_version = workflow
            .metadata
            .version
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKFLOW_VERSION.to_string());

        // Step 1a: Ensure every judge reference already exists before we try to
        // map or register the workflow. This fails deterministically instead of
        // surfacing a runtime "judge agent not found" error later.
        let judge_references = workflow.referenced_judge_agents();
        if !judge_references.is_empty() {
            let mut missing_judges = Vec::new();
            for judge_name in judge_references {
                let judge_id = self
                    .agent_service
                    .lookup_agent_for_tenant(tenant_id, &judge_name)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to resolve judge agent '{judge_name}' in workflow '{workflow_name}'"
                        )
                    })?;
                if judge_id.is_none() {
                    missing_judges.push(judge_name);
                }
            }

            if !missing_judges.is_empty() {
                anyhow::bail!(
                    "Workflow '{workflow_name}' references missing judge agents: {}",
                    missing_judges.join(", ")
                );
            }
        }

        // Step 1b: Check for existing version if force=false
        if let Some(existing) = self
            .workflow_repository
            .find_by_name_for_tenant(tenant_id, &workflow_name)
            .await?
        {
            if existing.metadata.version == workflow.metadata.version {
                if !force {
                    anyhow::bail!(
                        "Workflow '{workflow_name}' with version '{workflow_version}' already exists. Create a new version or use 'force' to overwrite."
                    );
                }

                // Force-overwrites keep the original workflow identity stable so
                // downstream persistence and execution references stay aligned.
                workflow.id = existing.id;
            }
        }

        let workflow_id = workflow.id.to_string();

        // Step 2: Map to Temporal definition via anti-corruption layer
        let mut temporal_definition =
            crate::application::temporal_mapper::TemporalWorkflowMapper::to_temporal_definition(
                &workflow, tenant_id,
            )
            .context("Failed to map workflow to Temporal definition")?;

        // Step 2b: Resolve agent name references → UUIDs in all Agent states.
        // Workflow YAML references agents by human-readable name (e.g. `agent: coder`);
        // the Temporal worker and gRPC ExecuteAgent handler require a UUID. Resolving
        // at deploy time gives a clear error message rather than a silent runtime failure.
        for (state_name, state) in temporal_definition.states.iter_mut() {
            if state.kind == "Agent" {
                if let Some(ref name) = state.agent.clone() {
                    if uuid::Uuid::parse_str(name).is_err() {
                        let agent_id = self
                            .agent_service
                            .lookup_agent_for_tenant(tenant_id, name)
                            .await
                            .with_context(|| {
                                format!("Failed to resolve agent '{name}' in state '{state_name}'")
                            })?
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Agent '{name}' referenced in state '{state_name}' not found; \
                                     deploy the agent before registering this workflow"
                                )
                            })?;
                        state.agent = Some(agent_id.0.to_string());
                    }
                }
            }
        }

        // Step 3: Register with Temporal via HTTP to TypeScript worker
        let engine = {
            let lock = self.workflow_engine.read().await;
            lock.clone()
                .ok_or_else(|| anyhow::anyhow!("Workflow engine not connected yet"))?
        };

        engine
            .register_workflow(&temporal_definition)
            .await
            .context("Failed to register workflow with Temporal server")?;

        // Step 4: Persist workflow to repository
        self.workflow_repository
            .save_for_tenant(tenant_id, &workflow)
            .await
            .context("Failed to persist workflow to repository")?;

        // Step 5: Publish domain event
        self.event_bus.publish_workflow_event(
            crate::domain::events::WorkflowEvent::WorkflowRegistered {
                workflow_id: workflow.id,
                name: workflow_name.clone(),
                version: workflow_version.clone(),
                registered_at: chrono::Utc::now(),
            },
        );

        Ok(RegisteredWorkflow {
            workflow_id,
            name: workflow_name,
            version: workflow_version,
            status: "registered".to_string(),
            registered_at: chrono::Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::agent::AgentLifecycleService;
    use crate::application::temporal_mapper::TemporalWorkflowDefinition;
    use crate::domain::agent::{Agent, AgentId, AgentManifest};
    use crate::domain::events::WorkflowEvent;
    use crate::domain::repository::{RepositoryError, WorkflowRepository};
    use crate::domain::workflow::{Workflow, WorkflowId};
    use crate::infrastructure::event_bus::{DomainEvent, EventBusError};
    use crate::infrastructure::repositories::InMemoryWorkflowRepository;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    /// Test fixture: resolves any agent name to a deterministic UUID.
    /// Returns `None` only when `name == "nonexistent-agent"` (for error-path tests).
    struct TestAgentService;

    #[async_trait]
    impl AgentLifecycleService for TestAgentService {
        async fn deploy_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            manifest: AgentManifest,
            force: bool,
        ) -> anyhow::Result<AgentId> {
            self.deploy_agent(manifest, force).await
        }

        async fn deploy_agent(
            &self,
            _manifest: AgentManifest,
            _force: bool,
        ) -> anyhow::Result<AgentId> {
            panic!("test-only fixture: deploy_agent is not exercised in this test")
        }

        async fn get_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            id: AgentId,
        ) -> anyhow::Result<Agent> {
            self.get_agent(id).await
        }

        async fn get_agent(&self, _id: AgentId) -> anyhow::Result<Agent> {
            panic!("test-only fixture: get_agent is not exercised in this test")
        }

        async fn update_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            id: AgentId,
            manifest: AgentManifest,
        ) -> anyhow::Result<()> {
            self.update_agent(id, manifest).await
        }

        async fn update_agent(&self, _id: AgentId, _manifest: AgentManifest) -> anyhow::Result<()> {
            panic!("test-only fixture: update_agent is not exercised in this test")
        }

        async fn delete_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            id: AgentId,
        ) -> anyhow::Result<()> {
            self.delete_agent(id).await
        }

        async fn delete_agent(&self, _id: AgentId) -> anyhow::Result<()> {
            panic!("test-only fixture: delete_agent is not exercised in this test")
        }

        async fn list_agents_for_tenant(
            &self,
            _tenant_id: &TenantId,
        ) -> anyhow::Result<Vec<Agent>> {
            self.list_agents().await
        }

        async fn list_agents(&self) -> anyhow::Result<Vec<Agent>> {
            panic!("test-only fixture: list_agents is not exercised in this test")
        }

        async fn lookup_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            name: &str,
        ) -> anyhow::Result<Option<AgentId>> {
            self.lookup_agent(name).await
        }

        async fn lookup_agent(&self, name: &str) -> anyhow::Result<Option<AgentId>> {
            if name == "nonexistent-agent" {
                return Ok(None);
            }
            Ok(Some(AgentId(
                uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            )))
        }
    }

    fn test_agent_service() -> Arc<dyn AgentLifecycleService> {
        Arc::new(TestAgentService)
    }

    const VALID_WORKFLOW_YAML: &str = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: registration-test-workflow
  version: "1.2.3"
spec:
  initial_state: START
  states:
    START:
      kind: Agent
      agent: coder-v1
      input: "{{workflow.task}}"
      transitions:
        - condition: always
          target: END
    END:
      kind: System
      command: echo "done"
      transitions: []
"#;

    struct RecordingEngine {
        registered: Mutex<Vec<TemporalWorkflowDefinition>>,
    }

    impl RecordingEngine {
        fn new() -> Self {
            Self {
                registered: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<TemporalWorkflowDefinition> {
            self.registered.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WorkflowEnginePort for RecordingEngine {
        async fn register_workflow(&self, definition: &TemporalWorkflowDefinition) -> Result<()> {
            self.registered.lock().unwrap().push(definition.clone());
            Ok(())
        }

        async fn start_workflow(
            &self,
            _workflow_name: &str,
            _execution_id: crate::domain::execution::ExecutionId,
            _input: std::collections::HashMap<String, serde_json::Value>,
            _blackboard: Option<std::collections::HashMap<String, serde_json::Value>>,
        ) -> Result<String> {
            Ok("unused".to_string())
        }
    }

    struct FailingEngine;

    #[async_trait]
    impl WorkflowEnginePort for FailingEngine {
        async fn register_workflow(&self, _definition: &TemporalWorkflowDefinition) -> Result<()> {
            anyhow::bail!("temporal unavailable")
        }

        async fn start_workflow(
            &self,
            _workflow_name: &str,
            _execution_id: crate::domain::execution::ExecutionId,
            _input: std::collections::HashMap<String, serde_json::Value>,
            _blackboard: Option<std::collections::HashMap<String, serde_json::Value>>,
        ) -> Result<String> {
            Ok("unused".to_string())
        }
    }

    struct SaveFailRepo;

    #[async_trait]
    impl WorkflowRepository for SaveFailRepo {
        async fn save_for_tenant(
            &self,
            _tenant_id: &TenantId,
            workflow: &Workflow,
        ) -> Result<(), RepositoryError> {
            self.save(workflow).await
        }

        async fn save(&self, _workflow: &Workflow) -> Result<(), RepositoryError> {
            Err(RepositoryError::Database("write failed".to_string()))
        }

        async fn find_by_id_for_tenant(
            &self,
            _tenant_id: &TenantId,
            id: WorkflowId,
        ) -> Result<Option<Workflow>, RepositoryError> {
            self.find_by_id(id).await
        }

        async fn find_by_id(&self, _id: WorkflowId) -> Result<Option<Workflow>, RepositoryError> {
            Ok(None)
        }

        async fn find_by_name_for_tenant(
            &self,
            _tenant_id: &TenantId,
            name: &str,
        ) -> Result<Option<Workflow>, RepositoryError> {
            self.find_by_name(name).await
        }

        async fn find_by_name(&self, _name: &str) -> Result<Option<Workflow>, RepositoryError> {
            Ok(None)
        }

        async fn list_all_for_tenant(
            &self,
            _tenant_id: &TenantId,
        ) -> Result<Vec<Workflow>, RepositoryError> {
            self.list_all().await
        }

        async fn list_all(&self) -> Result<Vec<Workflow>, RepositoryError> {
            Ok(vec![])
        }

        async fn delete_for_tenant(
            &self,
            _tenant_id: &TenantId,
            id: WorkflowId,
        ) -> Result<(), RepositoryError> {
            self.delete(id).await
        }

        async fn delete(&self, _id: WorkflowId) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn register_workflow_registers_persists_and_publishes_event() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let engine = Arc::new(RecordingEngine::new());
        let event_bus = Arc::new(EventBus::new(32));
        let mut events = event_bus.subscribe();

        let service = StandardRegisterWorkflowUseCase::new(
            repo.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(engine.clone()))),
            event_bus,
            test_agent_service(),
        );

        let response = service
            .register_workflow(VALID_WORKFLOW_YAML, false)
            .await
            .unwrap();
        assert_eq!(response.name, "registration-test-workflow");
        assert_eq!(response.version, "1.2.3");
        assert_eq!(response.status, "registered");

        let calls = engine.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "registration-test-workflow");
        assert_eq!(calls[0].tenant_id, TenantId::local_default().to_string());
        assert_eq!(calls[0].version, "1.2.3");

        let persisted = repo
            .find_by_name("registration-test-workflow")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.metadata.version, Some("1.2.3".to_string()));

        match events.recv().await.unwrap() {
            DomainEvent::Workflow(WorkflowEvent::WorkflowRegistered {
                workflow_id, name, ..
            }) => {
                assert_eq!(name, "registration-test-workflow");
                assert_eq!(workflow_id.to_string(), response.workflow_id);
            }
            other => panic!("unexpected event type: {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_workflow_for_tenant_uses_explicit_tenant_in_temporal_payload() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let engine = Arc::new(RecordingEngine::new());
        let event_bus = Arc::new(EventBus::new(8));
        let tenant_id = TenantId::from_string("tenant-one").unwrap();

        let service = StandardRegisterWorkflowUseCase::new(
            repo,
            Arc::new(tokio::sync::RwLock::new(Some(engine.clone()))),
            event_bus,
            test_agent_service(),
        );

        service
            .register_workflow_for_tenant(&tenant_id, VALID_WORKFLOW_YAML, false)
            .await
            .unwrap();

        let calls = engine.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tenant_id, tenant_id.to_string());
        assert_eq!(calls[0].version, "1.2.3");
        assert_eq!(calls[0].workflow_id.len(), 36);
    }

    #[tokio::test]
    async fn register_workflow_fails_when_engine_missing() {
        let service = StandardRegisterWorkflowUseCase::new(
            Arc::new(InMemoryWorkflowRepository::new()),
            Arc::new(tokio::sync::RwLock::new(None)),
            Arc::new(EventBus::new(8)),
            test_agent_service(),
        );

        let err = service
            .register_workflow(VALID_WORKFLOW_YAML, false)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Workflow engine not connected yet"));
    }

    #[tokio::test]
    async fn register_workflow_fails_when_engine_registration_fails() {
        let service = StandardRegisterWorkflowUseCase::new(
            Arc::new(InMemoryWorkflowRepository::new()),
            Arc::new(tokio::sync::RwLock::new(Some(
                Arc::new(FailingEngine) as Arc<dyn WorkflowEnginePort>
            ))),
            Arc::new(EventBus::new(8)),
            test_agent_service(),
        );

        let err = service
            .register_workflow(VALID_WORKFLOW_YAML, false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Failed to register workflow with Temporal server"));
    }

    #[tokio::test]
    async fn register_workflow_fails_when_persistence_fails() {
        let service = StandardRegisterWorkflowUseCase::new(
            Arc::new(SaveFailRepo),
            Arc::new(tokio::sync::RwLock::new(Some(
                Arc::new(RecordingEngine::new()) as Arc<dyn WorkflowEnginePort>,
            ))),
            Arc::new(EventBus::new(8)),
            test_agent_service(),
        );

        let err = service
            .register_workflow(VALID_WORKFLOW_YAML, false)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to persist workflow to repository"));
    }

    #[tokio::test]
    async fn register_workflow_does_not_persist_when_engine_registration_fails() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let service = StandardRegisterWorkflowUseCase::new(
            repo.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(
                Arc::new(FailingEngine) as Arc<dyn WorkflowEnginePort>
            ))),
            Arc::new(EventBus::new(8)),
            test_agent_service(),
        );

        let _ = service
            .register_workflow(VALID_WORKFLOW_YAML, false)
            .await
            .unwrap_err();
        let stored = repo
            .find_by_name("registration-test-workflow")
            .await
            .unwrap();
        assert!(stored.is_none());
    }

    #[tokio::test]
    async fn register_workflow_does_not_publish_event_when_persistence_fails() {
        let event_bus = Arc::new(EventBus::new(8));
        let mut events = event_bus.subscribe();
        let service = StandardRegisterWorkflowUseCase::new(
            Arc::new(SaveFailRepo),
            Arc::new(tokio::sync::RwLock::new(Some(
                Arc::new(RecordingEngine::new()) as Arc<dyn WorkflowEnginePort>,
            ))),
            event_bus,
            test_agent_service(),
        );

        let _ = service
            .register_workflow(VALID_WORKFLOW_YAML, false)
            .await
            .unwrap_err();
        assert!(matches!(events.try_recv(), Err(EventBusError::Empty)));
    }

    #[tokio::test]
    async fn register_workflow_fails_for_invalid_yaml() {
        let invalid = json!({"not": "yaml"}).to_string();
        let service = StandardRegisterWorkflowUseCase::new(
            Arc::new(InMemoryWorkflowRepository::new()),
            Arc::new(tokio::sync::RwLock::new(None)),
            Arc::new(EventBus::new(8)),
            test_agent_service(),
        );
        let err = service
            .register_workflow(&invalid, false)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to parse workflow YAML manifest"));
    }

    #[tokio::test]
    async fn register_workflow_fails_for_duplicate_version_without_force() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let engine = Arc::new(RecordingEngine::new());
        let event_bus = Arc::new(EventBus::new(8));

        let service = StandardRegisterWorkflowUseCase::new(
            repo.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(
                engine.clone() as Arc<dyn WorkflowEnginePort>
            ))),
            event_bus,
            test_agent_service(),
        );

        // First registration
        service
            .register_workflow(VALID_WORKFLOW_YAML, false)
            .await
            .unwrap();

        // Second registration with same version, no force
        let err = service
            .register_workflow(VALID_WORKFLOW_YAML, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert!(err.to_string().contains("use 'force' to overwrite"));

        // Third registration with same version, WITH force
        let response = service
            .register_workflow(VALID_WORKFLOW_YAML, true)
            .await
            .unwrap();
        assert_eq!(response.status, "registered");
    }

    #[tokio::test]
    async fn register_workflow_force_overwrite_preserves_existing_workflow_id() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let engine = Arc::new(RecordingEngine::new());
        let service = StandardRegisterWorkflowUseCase::new(
            repo.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(
                engine.clone() as Arc<dyn WorkflowEnginePort>
            ))),
            Arc::new(EventBus::new(8)),
            test_agent_service(),
        );

        let first = service
            .register_workflow(VALID_WORKFLOW_YAML, false)
            .await
            .unwrap();
        let second = service
            .register_workflow(VALID_WORKFLOW_YAML, true)
            .await
            .unwrap();

        assert_eq!(second.workflow_id, first.workflow_id);

        let calls = engine.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].workflow_id, first.workflow_id);
        assert_eq!(calls[1].workflow_id, first.workflow_id);

        let workflows = repo.list_all().await.unwrap();
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].id.to_string(), first.workflow_id);
    }

    /// Workflow YAML that references a non-existent agent; should fail during step 2b.
    const WORKFLOW_YAML_UNKNOWN_AGENT: &str = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: test-unknown-agent
  version: "1.0.0"
  description: "Workflow with an unknown agent"
spec:
  initial_state: step1
  states:
    step1:
      kind: Agent
      agent: nonexistent-agent
      input: "test input"
      transitions:
        - condition: always
          target: done
    done:
      kind: System
      command: echo done
      transitions: []
"#;

    #[tokio::test]
    async fn register_workflow_fails_when_agent_not_found() {
        let service = StandardRegisterWorkflowUseCase::new(
            Arc::new(InMemoryWorkflowRepository::new()),
            Arc::new(tokio::sync::RwLock::new(Some(
                Arc::new(RecordingEngine::new()) as Arc<dyn WorkflowEnginePort>,
            ))),
            Arc::new(EventBus::new(8)),
            test_agent_service(),
        );

        let err = service
            .register_workflow(WORKFLOW_YAML_UNKNOWN_AGENT, false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("nonexistent-agent"),
            "error should mention the unknown agent name, got: {err}"
        );
    }

    /// Workflow YAML that references an unknown judge; should fail before Temporal registration.
    const WORKFLOW_YAML_UNKNOWN_JUDGE: &str = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: test-unknown-judge
  version: "1.0.0"
  description: "Workflow with an unknown judge agent"
spec:
  initial_state: step1
  states:
    step1:
      kind: Agent
      agent: builder
      input: "test input"
      judges:
        - agent_id: nonexistent-agent
      transitions:
        - condition: always
          target: done
    done:
      kind: System
      command: echo done
      transitions: []
"#;

    #[tokio::test]
    async fn register_workflow_fails_when_judge_agent_not_found() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let engine = Arc::new(RecordingEngine::new());
        let service = StandardRegisterWorkflowUseCase::new(
            repo,
            Arc::new(tokio::sync::RwLock::new(Some(
                engine.clone() as Arc<dyn WorkflowEnginePort>
            ))),
            Arc::new(EventBus::new(8)),
            test_agent_service(),
        );

        let err = service
            .register_workflow(WORKFLOW_YAML_UNKNOWN_JUDGE, false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing judge agents"),
            "error should mention missing judge agents, got: {msg}"
        );
        assert!(
            msg.contains("nonexistent-agent"),
            "error should mention the unknown judge name, got: {msg}"
        );
        assert!(
            engine.calls().is_empty(),
            "workflow must not reach Temporal registration when judges are missing"
        );
    }

    const WORKFLOW_YAML_NO_VERSION: &str = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: registration-default-version-workflow
spec:
  initial_state: START
  states:
    START:
      kind: System
      command: echo start
      transitions: []
"#;

    #[tokio::test]
    async fn register_workflow_defaults_missing_version_to_one_zero_zero() {
        let repo = Arc::new(InMemoryWorkflowRepository::new());
        let engine = Arc::new(RecordingEngine::new());
        let event_bus = Arc::new(EventBus::new(8));

        let service = StandardRegisterWorkflowUseCase::new(
            repo,
            Arc::new(tokio::sync::RwLock::new(Some(engine.clone()))),
            event_bus,
            test_agent_service(),
        );

        let response = service
            .register_workflow(WORKFLOW_YAML_NO_VERSION, false)
            .await
            .unwrap();

        assert_eq!(response.version, "1.0.0");
        let calls = engine.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].version, "1.0.0");
    }
}
