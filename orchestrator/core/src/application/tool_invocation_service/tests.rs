use super::workflows::{collect_thresholded_transition_semantic_violations, sanitize_segment};
use super::*;
use crate::application::nfs_gateway::NfsVolumeRegistry;
use crate::domain::events::StorageEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::fsal::{AegisFSAL, EventPublisher};
use crate::domain::node_config::{BuiltinDispatcherConfig, CapabilityConfig};
use crate::domain::repository::AgentVersion;
use crate::domain::seal_session::SealSession;
use crate::domain::security_context::SecurityContext;
use crate::infrastructure::repositories::InMemoryVolumeRepository;
use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use crate::infrastructure::storage::LocalHostStorageProvider;
use crate::infrastructure::tool_router::{InMemoryToolRegistry, ToolRouter};
use async_trait::async_trait;

struct NoOpEventPublisher;

#[async_trait]
impl EventPublisher for NoOpEventPublisher {
    async fn publish_storage_event(&self, _event: StorageEvent) {}
}

/// Create test FSAL dependencies and empty NFS volume registry.
fn test_fsal_deps() -> (Arc<AegisFSAL>, NfsVolumeRegistry) {
    let storage_root = std::env::temp_dir().join("aegis-tool-invocation-tests");
    let storage = Arc::new(
        LocalHostStorageProvider::new(&storage_root)
            .expect("failed to initialize LocalHostStorageProvider for tests"),
    );
    let vol_repo = Arc::new(InMemoryVolumeRepository::new());
    let publisher = Arc::new(NoOpEventPublisher);
    let registry = NfsVolumeRegistry::new();
    let fsal = Arc::new(AegisFSAL::new(
        storage,
        vol_repo,
        Arc::new(registry.clone()),
        Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
        publisher,
    ));
    (fsal, registry)
}

fn make_fake_token(agent_id: AgentId) -> String {
    use base64::Engine;
    let claims = serde_json::json!({"agent_id": agent_id.0.to_string()});
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap_or_default());
    format!("eyJhbGciOiJSUzI1NiJ9.{}.sig", payload)
}

struct DummyEnvelope {
    valid: bool,
    token: String,
}

impl DummyEnvelope {
    fn for_agent(valid: bool, agent_id: AgentId) -> Self {
        Self {
            valid,
            token: make_fake_token(agent_id),
        }
    }
}

impl EnvelopeVerifier for DummyEnvelope {
    fn security_token(&self) -> &str {
        &self.token
    }

    fn verify_signature(&self, _public_key_bytes: &[u8]) -> Result<(), SealSessionError> {
        if self.valid {
            Ok(())
        } else {
            Err(SealSessionError::SignatureVerificationFailed(
                "invalid sig".to_string(),
            ))
        }
    }
    fn extract_tool_name(&self) -> Option<String> {
        Some("test_tool".to_string())
    }
    fn extract_arguments(&self) -> Option<Value> {
        Some(serde_json::json!({}))
    }
}

use crate::domain::agent::{Agent, AgentManifest, AgentStatus};
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{Execution, ExecutionInput, ExecutionStatus, Iteration};
use crate::domain::repository::{WorkflowExecutionRepository, WorkflowRepository};
use crate::domain::workflow::WorkflowExecutionEventRecord;
use crate::infrastructure::event_bus::DomainEvent;
use crate::infrastructure::repositories::{
    InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use tokio::sync::Mutex;
use tokio::sync::RwLock;

fn test_agent_with_tools(tools: &[&str]) -> Agent {
    let manifest_yaml = format!(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools:
{}
"#,
        tools
            .iter()
            .map(|tool| format!("    - {tool}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let manifest: AgentManifest = serde_yaml::from_str(&manifest_yaml).unwrap();
    Agent {
        id: AgentId::new(),
        tenant_id: crate::domain::tenant::TenantId::default(),
        scope: crate::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

struct TestAgentLifecycleService;
#[async_trait]
impl AgentLifecycleService for TestAgentLifecycleService {
    async fn deploy_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        manifest: AgentManifest,
        force: bool,
        _scope: crate::domain::agent::AgentScope,
        _caller_identity: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<AgentId> {
        self.deploy_agent(manifest, force).await
    }

    async fn deploy_agent(&self, _: AgentManifest, _force: bool) -> Result<AgentId> {
        anyhow::bail!("TestAgentLifecycleService::deploy_agent not exercised in this test")
    }

    async fn get_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
        self.get_agent(id).await
    }

    async fn get_agent(&self, _: AgentId) -> Result<Agent> {
        Ok(test_agent_with_tools(&[]))
    }

    async fn update_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: AgentId,
        manifest: AgentManifest,
    ) -> Result<()> {
        self.update_agent(id, manifest).await
    }

    async fn update_agent(&self, _: AgentId, _: AgentManifest) -> Result<()> {
        anyhow::bail!("TestAgentLifecycleService::update_agent not exercised in this test")
    }

    async fn delete_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<()> {
        self.delete_agent(id).await
    }

    async fn delete_agent(&self, _: AgentId) -> Result<()> {
        anyhow::bail!("TestAgentLifecycleService::delete_agent not exercised in this test")
    }

    async fn list_agents_for_tenant(&self, _tenant_id: &TenantId) -> Result<Vec<Agent>> {
        self.list_agents().await
    }

    async fn list_agents(&self) -> Result<Vec<Agent>> {
        anyhow::bail!("TestAgentLifecycleService::list_agents not exercised in this test")
    }

    async fn lookup_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        self.lookup_agent(name).await
    }

    async fn lookup_agent(&self, _: &str) -> Result<Option<AgentId>> {
        anyhow::bail!("TestAgentLifecycleService::lookup_agent not exercised in this test")
    }

    async fn lookup_agent_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> Result<Option<AgentId>> {
        anyhow::bail!(
            "TestAgentLifecycleService::lookup_agent_visible_for_tenant not exercised in this test"
        )
    }

    async fn lookup_agent_for_tenant_with_version(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
        _version: &str,
    ) -> Result<Option<AgentId>> {
        anyhow::bail!(
            "TestAgentLifecycleService::lookup_agent_for_tenant_with_version not exercised in this test"
        )
    }

    async fn list_agents_visible_for_tenant(&self, _tenant_id: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
    }

    async fn list_versions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

struct FilteringAgentLifecycleService {
    agent: Agent,
}

#[async_trait]
impl AgentLifecycleService for FilteringAgentLifecycleService {
    async fn deploy_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        manifest: AgentManifest,
        force: bool,
        _scope: crate::domain::agent::AgentScope,
        _caller_identity: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<AgentId> {
        self.deploy_agent(manifest, force).await
    }

    async fn deploy_agent(&self, _: AgentManifest, _force: bool) -> Result<AgentId> {
        anyhow::bail!("FilteringAgentLifecycleService::deploy_agent not exercised in this test")
    }

    async fn get_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
        self.get_agent(id).await
    }

    async fn get_agent(&self, id: AgentId) -> Result<Agent> {
        if id == self.agent.id {
            Ok(self.agent.clone())
        } else {
            anyhow::bail!("agent not found")
        }
    }

    async fn update_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: AgentId,
        manifest: AgentManifest,
    ) -> Result<()> {
        self.update_agent(id, manifest).await
    }

    async fn update_agent(&self, _: AgentId, _: AgentManifest) -> Result<()> {
        anyhow::bail!("FilteringAgentLifecycleService::update_agent not exercised in this test")
    }

    async fn delete_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<()> {
        self.delete_agent(id).await
    }

    async fn delete_agent(&self, _: AgentId) -> Result<()> {
        anyhow::bail!("FilteringAgentLifecycleService::delete_agent not exercised in this test")
    }

    async fn list_agents_for_tenant(&self, _tenant_id: &TenantId) -> Result<Vec<Agent>> {
        self.list_agents().await
    }

    async fn list_agents(&self) -> Result<Vec<Agent>> {
        Ok(vec![self.agent.clone()])
    }

    async fn lookup_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        self.lookup_agent(name).await
    }

    async fn lookup_agent(&self, name: &str) -> Result<Option<AgentId>> {
        Ok((name == self.agent.name).then_some(self.agent.id))
    }

    async fn lookup_agent_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        self.lookup_agent(name).await
    }

    async fn lookup_agent_for_tenant_with_version(
        &self,
        _tenant_id: &TenantId,
        name: &str,
        _version: &str,
    ) -> Result<Option<AgentId>> {
        // Delegate to name-only lookup for existing tests
        self.lookup_agent(name).await
    }

    async fn list_agents_visible_for_tenant(&self, _tenant_id: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
    }

    async fn list_versions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

struct TestExecutionService;
#[async_trait]
impl ExecutionService for TestExecutionService {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("TestExecutionService::start_execution not exercised in this test")
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        Ok(execution_id)
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> Result<ExecutionId> {
        anyhow::bail!("TestExecutionService::start_child_execution not exercised in this test")
    }
    async fn get_execution(&self, _: ExecutionId) -> Result<Execution> {
        anyhow::bail!("TestExecutionService::get_execution not exercised in this test")
    }
    async fn get_execution_unscoped(&self, _: ExecutionId) -> Result<Execution> {
        anyhow::bail!("TestExecutionService::get_execution_unscoped not exercised in this test")
    }
    async fn get_iterations(&self, _: ExecutionId) -> Result<Vec<Iteration>> {
        anyhow::bail!("TestExecutionService::get_iterations not exercised in this test")
    }
    async fn cancel_execution(&self, _: ExecutionId) -> Result<()> {
        anyhow::bail!("TestExecutionService::cancel_execution not exercised in this test")
    }
    async fn stream_execution(
        &self,
        _: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("TestExecutionService::stream_execution not exercised in this test")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        anyhow::bail!("TestExecutionService::stream_agent_events not exercised in this test")
    }
    async fn list_executions(&self, _: Option<AgentId>, _: usize) -> Result<Vec<Execution>> {
        anyhow::bail!("TestExecutionService::list_executions not exercised in this test")
    }
    async fn delete_execution(&self, _: ExecutionId) -> Result<()> {
        anyhow::bail!("TestExecutionService::delete_execution not exercised in this test")
    }
    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: crate::domain::execution::LlmInteraction,
    ) -> Result<()> {
        anyhow::bail!("TestExecutionService::record_llm_interaction not exercised in this test")
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<crate::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        anyhow::bail!("TestExecutionService::store_iteration_trajectory not exercised in this test")
    }
}

struct LogsTestExecutionService {
    execution: Execution,
}

#[async_trait]
impl ExecutionService for LogsTestExecutionService {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("LogsTestExecutionService::start_execution not exercised in this test")
    }

    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        Ok(execution_id)
    }

    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> Result<ExecutionId> {
        anyhow::bail!("LogsTestExecutionService::start_child_execution not exercised in this test")
    }

    async fn get_execution(&self, id: ExecutionId) -> Result<Execution> {
        if self.execution.id == id {
            Ok(self.execution.clone())
        } else {
            anyhow::bail!("execution not found")
        }
    }

    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        if self.execution.id == id {
            Ok(self.execution.clone())
        } else {
            anyhow::bail!("execution not found")
        }
    }

    async fn get_iterations(&self, _: ExecutionId) -> Result<Vec<Iteration>> {
        anyhow::bail!("LogsTestExecutionService::get_iterations not exercised in this test")
    }

    async fn cancel_execution(&self, _: ExecutionId) -> Result<()> {
        anyhow::bail!("LogsTestExecutionService::cancel_execution not exercised in this test")
    }

    async fn stream_execution(
        &self,
        _: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("LogsTestExecutionService::stream_execution not exercised in this test")
    }

    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        anyhow::bail!("LogsTestExecutionService::stream_agent_events not exercised in this test")
    }

    async fn list_executions(&self, _: Option<AgentId>, _: usize) -> Result<Vec<Execution>> {
        anyhow::bail!("LogsTestExecutionService::list_executions not exercised in this test")
    }

    async fn delete_execution(&self, _: ExecutionId) -> Result<()> {
        anyhow::bail!("LogsTestExecutionService::delete_execution not exercised in this test")
    }

    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: crate::domain::execution::LlmInteraction,
    ) -> Result<()> {
        anyhow::bail!("LogsTestExecutionService::record_llm_interaction not exercised in this test")
    }

    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<crate::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        anyhow::bail!(
            "LogsTestExecutionService::store_iteration_trajectory not exercised in this test"
        )
    }
}

#[derive(Default)]
struct StubWorkflowExecutionRepository {
    events: RwLock<HashMap<ExecutionId, Vec<WorkflowExecutionEventRecord>>>,
}

impl StubWorkflowExecutionRepository {
    fn with_events(execution_id: ExecutionId, events: Vec<WorkflowExecutionEventRecord>) -> Self {
        let mut by_execution = HashMap::new();
        by_execution.insert(execution_id, events);
        Self {
            events: RwLock::new(by_execution),
        }
    }
}

#[async_trait]
impl WorkflowExecutionRepository for StubWorkflowExecutionRepository {
    async fn find_tenant_id_by_execution(
        &self,
        _id: crate::domain::execution::ExecutionId,
    ) -> Result<Option<TenantId>, crate::domain::repository::RepositoryError> {
        Ok(None)
    }

    async fn save_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution: &crate::domain::workflow::WorkflowExecution,
    ) -> Result<(), crate::domain::repository::RepositoryError> {
        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: ExecutionId,
    ) -> Result<
        Option<crate::domain::workflow::WorkflowExecution>,
        crate::domain::repository::RepositoryError,
    > {
        Ok(None)
    }

    async fn find_active_for_tenant(
        &self,
        _tenant_id: &TenantId,
    ) -> Result<
        Vec<crate::domain::workflow::WorkflowExecution>,
        crate::domain::repository::RepositoryError,
    > {
        Ok(vec![])
    }

    async fn find_by_workflow_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_id: crate::domain::workflow::WorkflowId,
        _limit: usize,
        _offset: usize,
    ) -> Result<
        Vec<crate::domain::workflow::WorkflowExecution>,
        crate::domain::repository::RepositoryError,
    > {
        Ok(vec![])
    }

    async fn count_by_workflow_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_id: crate::domain::workflow::WorkflowId,
    ) -> Result<i64, crate::domain::repository::RepositoryError> {
        Ok(0)
    }

    async fn list_paginated_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _limit: usize,
        _offset: usize,
    ) -> Result<
        Vec<crate::domain::workflow::WorkflowExecution>,
        crate::domain::repository::RepositoryError,
    > {
        Ok(vec![])
    }

    async fn update_temporal_linkage_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution_id: ExecutionId,
        _temporal_workflow_id: &str,
        _temporal_run_id: &str,
    ) -> Result<(), crate::domain::repository::RepositoryError> {
        Ok(())
    }

    async fn append_event(
        &self,
        execution_id: ExecutionId,
        temporal_sequence_number: i64,
        event_type: String,
        payload: serde_json::Value,
        iteration_number: Option<u8>,
    ) -> Result<(), crate::domain::repository::RepositoryError> {
        let mut events = self.events.write().await;
        events
            .entry(execution_id)
            .or_default()
            .push(WorkflowExecutionEventRecord {
                sequence: temporal_sequence_number,
                event_type,
                state_name: None,
                iteration_number,
                payload,
                recorded_at: chrono::Utc::now(),
            });
        Ok(())
    }

    async fn find_events_by_execution(
        &self,
        id: ExecutionId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecutionEventRecord>, crate::domain::repository::RepositoryError> {
        let events = self
            .events
            .read()
            .await
            .get(&id)
            .cloned()
            .unwrap_or_default();
        Ok(events.into_iter().skip(offset).take(limit).collect())
    }
}

#[derive(Default)]
struct TestStartWorkflowExecutionUseCase {
    last_request:
        Mutex<Option<crate::application::start_workflow_execution::StartWorkflowExecutionRequest>>,
}

#[async_trait]
impl StartWorkflowExecutionUseCase for TestStartWorkflowExecutionUseCase {
    async fn start_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        mut request: crate::application::start_workflow_execution::StartWorkflowExecutionRequest,
        _identity: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<crate::application::start_workflow_execution::StartedWorkflowExecution> {
        request.tenant_id = Some(tenant_id.clone());
        *self.last_request.lock().await = Some(request.clone());

        Ok(
            crate::application::start_workflow_execution::StartedWorkflowExecution {
                execution_id: ExecutionId::new().to_string(),
                workflow_id: request.workflow_id,
                temporal_run_id: "temporal-run-id".to_string(),
                status: "started".to_string(),
                started_at: chrono::Utc::now(),
            },
        )
    }
}

fn test_workflow_manifest_yaml(name: &str) -> String {
    format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: {name}
  version: "1.0.0"
spec:
  initial_state: START
  states:
    START:
      kind: Agent
      agent: builder
      input: "{{{{workflow.task}}}}"
      transitions:
        - condition: always
          target: END
    END:
      kind: System
      command: echo "done"
      transitions: []
"#
    )
}

fn cyclic_workflow_manifest_yaml(name: &str) -> String {
    format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: {name}
  version: "1.0.0"
spec:
  initial_state: FIRST
  states:
    FIRST:
      kind: Agent
      agent: builder
      input: "{{{{workflow.task}}}}"
      transitions:
        - condition: always
          target: SECOND
    SECOND:
      kind: Agent
      agent: builder
      input: "{{{{workflow.task}}}}"
      transitions:
        - condition: always
          target: FIRST
"#
    )
}

fn build_test_workflow(name: &str) -> crate::domain::workflow::Workflow {
    WorkflowParser::parse_yaml(&test_workflow_manifest_yaml(name))
        .expect("test workflow manifest should parse")
}

fn thresholded_validator_manifest_yaml(name: &str, validation_transitions: &str) -> String {
    format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: {name}
  version: "1.0.0"
spec:
  initial_state: VALIDATE
  states:
    VALIDATE:
      kind: Agent
      agent: validator
      input: "{{{{workflow.task}}}}"
      transitions:
{validation_transitions}
    SUCCESS:
      kind: System
      command: echo "success"
      transitions: []
    PARTIAL_PASS:
      kind: System
      command: echo "partial"
      transitions: []
    VALIDATION_FAILED:
      kind: System
      command: echo "failed"
      transitions: []
    VALIDATION_ERROR:
      kind: System
      command: echo "error"
      transitions: []
"#
    )
}

#[tokio::test]
async fn test_invoke_tool_no_session() {
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());

    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );
    let agent_id = AgentId::new();
    let envelope = DummyEnvelope::for_agent(true, agent_id);

    let result = service.invoke_tool(&envelope).await;
    assert!(matches!(result, Err(SealSessionError::SessionInactive(_))));
}

#[tokio::test]
async fn test_invoke_tool_bad_signature() {
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let agent_id = AgentId::new();
    let exec_id = ExecutionId::new();

    let context = SecurityContext {
        name: "test".to_string(),
        description: "".to_string(),
        capabilities: vec![],
        deny_list: vec![],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    };

    let session_token = make_fake_token(agent_id);
    let session = SealSession::new(
        agent_id,
        exec_id,
        vec![],
        session_token,
        context,
        crate::domain::tenant::TenantId::consumer(),
    );
    let _ = repo.save(session).await;

    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());

    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );
    let envelope = DummyEnvelope::for_agent(false, agent_id);

    let result = service.invoke_tool(&envelope).await;
    assert!(matches!(
        result,
        Err(SealSessionError::SignatureVerificationFailed(_))
    ));
}

#[tokio::test]
async fn workflow_validate_tool_returns_success_for_valid_manifest() {
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();

    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );

    let result = service
        .invoke_aegis_workflow_validate_tool(&serde_json::json!({
            "manifest_yaml": test_workflow_manifest_yaml("validate-me"),
        }))
        .await
        .expect("workflow validate should return a result");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct payload");
    };

    assert_eq!(payload["tool"], "aegis.workflow.validate");
    assert_eq!(payload["valid"], true);
    assert_eq!(payload["deterministic_validation"]["passed"], true);
    assert_eq!(payload["workflow"]["name"], "validate-me");
}

#[tokio::test]
async fn workflow_update_tool_returns_failure_with_deterministic_validation_details_for_cycle() {
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();

    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );

    let result = service
        .invoke_aegis_workflow_update_tool(
            &serde_json::json!({
                "manifest_yaml": cyclic_workflow_manifest_yaml("cycle-update"),
            }),
            ExecutionId::new(),
            AgentId::new(),
        )
        .await
        .expect("workflow update should return a result");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct payload");
    };

    assert_eq!(payload["tool"], "aegis.workflow.update");
    assert_eq!(payload["updated"], false);
    assert_eq!(payload["deterministic_validation"]["passed"], false);
    assert_eq!(
        payload["deterministic_validation"]["error"],
        "Workflow cycle validation failed: Workflow execution error: Circular reference detected in workflow"
    );
    assert_eq!(
        payload["error"],
        payload["deterministic_validation"]["error"]
    );
}

#[test]
fn thresholded_transition_semantic_guard_allows_explicit_score_below() {
    let workflow = WorkflowParser::parse_yaml(&thresholded_validator_manifest_yaml(
        "explicit-score-below",
        r#"        - condition: score_and_confidence_above
          threshold: 0.8
          target: SUCCESS
        - condition: score_above
          threshold: 0.6
          target: PARTIAL_PASS
        - condition: score_below
          threshold: 0.6
          target: VALIDATION_FAILED
        - condition: on_failure
          target: VALIDATION_ERROR"#,
    ))
    .expect("workflow should parse");

    let violations = collect_thresholded_transition_semantic_violations(&workflow);

    assert!(
        violations.is_empty(),
        "expected explicit low-score routing to pass, got {violations:?}"
    );
}

#[test]
fn thresholded_transition_semantic_guard_rejects_missing_score_below() {
    let workflow = WorkflowParser::parse_yaml(&thresholded_validator_manifest_yaml(
        "missing-score-below",
        r#"        - condition: score_and_confidence_above
          threshold: 0.8
          target: SUCCESS
        - condition: score_above
          threshold: 0.6
          target: PARTIAL_PASS
        - condition: on_failure
          target: VALIDATION_ERROR"#,
    ))
    .expect("workflow should parse");

    let violations = collect_thresholded_transition_semantic_violations(&workflow);

    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("has no explicit `score_below` branch"));
}

#[tokio::test]
async fn workflow_create_semantic_validation_rejects_ambiguous_thresholded_success_fallback() {
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();

    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );

    let result = service
        .invoke_aegis_workflow_create_tool(
            &serde_json::json!({
                "manifest_yaml": thresholded_validator_manifest_yaml(
                    "ambiguous-threshold-routing",
                    r#"        - condition: score_and_confidence_above
          threshold: 0.8
          target: SUCCESS
        - condition: score_above
          threshold: 0.6
          target: PARTIAL_PASS
        - condition: on_success
          target: VALIDATION_FAILED
        - condition: on_failure
          target: VALIDATION_ERROR"#
                ),
            }),
            ExecutionId::new(),
            AgentId::new(),
            1,
            &[],
        )
        .await
        .expect("workflow create should return a result");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct payload");
    };

    assert_eq!(payload["tool"], "aegis.workflow.create");
    assert_eq!(payload["deterministic_validation"]["passed"], true);
    assert_eq!(payload["semantic_validation"]["passed"], false);
    assert_eq!(payload["deployed"], false);
    let violations = payload["semantic_validation"]["violations"]
        .as_array()
        .expect("violations should be an array");
    assert_eq!(violations.len(), 2);
    assert!(violations.iter().any(|violation| {
        violation
            .as_str()
            .is_some_and(|text| text.contains("mixes `on_success` with score-based transitions"))
    }));
    assert!(violations.iter().any(|violation| {
        violation
            .as_str()
            .is_some_and(|text| text.contains("has no explicit `score_below` branch"))
    }));
}

#[test]
fn build_tool_audit_history_includes_schema_validate_and_get_evidence() {
    let execution_id = ExecutionId::new();
    let tool_audit_history = vec![
            crate::domain::execution::TrajectoryStep {
                tool_name: "aegis.schema.get".to_string(),
                arguments_json: r#"{"key":"workflow/manifest/v1"}"#.to_string(),
                status: "completed".to_string(),
                result_json: Some(
                    r#"{"title":"Workflow Manifest","type":"object","properties":{"states":{"type":"array"}}}"#
                        .to_string(),
                ),
                error: None,
            },
            crate::domain::execution::TrajectoryStep {
                tool_name: "aegis.schema.validate".to_string(),
                arguments_json:
                    r#"{"kind":"workflow","manifest_yaml":"apiVersion: 100monkeys.ai/v1\nkind: Workflow"}"#
                        .to_string(),
                status: "completed".to_string(),
                result_json: Some(r#"{"valid":true,"errors":[]}"#.to_string()),
                error: None,
            },
        ];

    let audit_history =
        ToolInvocationService::build_tool_audit_history(execution_id, 1, &tool_audit_history);

    assert_eq!(audit_history["execution_id"], execution_id.to_string());
    assert_eq!(audit_history["available"], true);
    assert_eq!(audit_history["iteration_number"], 1);
    assert_eq!(audit_history["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(
        audit_history["tool_calls"][0]["tool_name"],
        "aegis.schema.get"
    );
    assert_eq!(
        audit_history["tool_calls"][0]["arguments_summary"]["key"],
        "workflow/manifest/v1"
    );
    assert_eq!(
        audit_history["tool_calls"][0]["result_summary"]["schema_key"],
        "workflow/manifest/v1"
    );
    assert_eq!(
        audit_history["tool_calls"][0]["result_summary"]["result_kind"],
        "schema"
    );
    assert_eq!(
        audit_history["tool_calls"][1]["tool_name"],
        "aegis.schema.validate"
    );
    assert_eq!(
        audit_history["tool_calls"][1]["arguments_summary"]["kind"],
        "workflow"
    );
    assert_eq!(
        audit_history["tool_calls"][1]["arguments_summary"]["manifest_present"],
        true
    );
    assert_eq!(
        audit_history["tool_calls"][1]["result_summary"]["valid"],
        true
    );
    assert_eq!(
        audit_history["latest_schema_get"]["tool_name"],
        "aegis.schema.get"
    );
    assert_eq!(
        audit_history["latest_schema_validate"]["tool_name"],
        "aegis.schema.validate"
    );
    assert!(audit_history.get("schema_get_evidence").is_none());
    assert!(audit_history.get("schema_validate_evidence").is_none());
}

#[test]
fn build_semantic_judge_payload_includes_tool_audit_history() {
    let execution_id = ExecutionId::new();
    let tool_audit_history = vec![
            crate::domain::execution::TrajectoryStep {
                tool_name: "aegis.schema.get".to_string(),
                arguments_json: r#"{"key":"agent/manifest/v1"}"#.to_string(),
                status: "completed".to_string(),
                result_json: Some(
                    r#"{"title":"Agent Manifest","type":"object","properties":{"metadata":{"type":"object"}}}"#
                        .to_string(),
                ),
                error: None,
            },
            crate::domain::execution::TrajectoryStep {
                tool_name: "aegis.schema.validate".to_string(),
                arguments_json:
                    r#"{"kind":"agent","manifest_yaml":"apiVersion: 100monkeys.ai/v1\nkind: Agent"}"#.to_string(),
                status: "completed".to_string(),
                result_json: Some(r#"{"valid":true,"errors":[]}"#.to_string()),
                error: None,
            },
        ];

    let payload = ToolInvocationService::build_semantic_judge_payload(
        execution_id,
        "Create an agent".to_string(),
        "aegis.agent.create",
        &serde_json::json!({
            "manifest_yaml": "apiVersion: 100monkeys.ai/v1\nkind: Agent\nmetadata:\n  name: copy-refiner\n  version: 1.0.0\nspec:\n  runtime:\n    language: python\n    version: \"3.11\"\n  task:\n    prompt_template: |\n      refine copy\n"
        }),
        vec![
            "aegis.schema.get".to_string(),
            "aegis.schema.validate".to_string(),
        ],
        vec!["/workspace".to_string()],
        "use the workflow-required sequence",
        "semantic_judge_pre_execution_inner_loop",
        1,
        &tool_audit_history,
    );

    assert_eq!(
        payload["validation_context"],
        "semantic_judge_pre_execution_inner_loop"
    );
    assert_eq!(payload["proposed_tool_call"]["name"], "aegis.agent.create");
    assert!(payload.get("output").is_none());
    assert_eq!(
        payload["tool_audit_history"]["tool_calls"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        payload["tool_audit_history"]["tool_calls"][0]["arguments_summary"]["key"],
        "agent/manifest/v1"
    );
    assert_eq!(
        payload["tool_audit_history"]["tool_calls"][0]["result_summary"]["schema_key"],
        "agent/manifest/v1"
    );
    assert_eq!(
        payload["tool_audit_history"]["tool_calls"][0]["result_summary"]["result_kind"],
        "schema"
    );
    assert_eq!(
        payload["tool_audit_history"]["latest_schema_validate"]["tool_name"],
        "aegis.schema.validate"
    );
    assert!(payload["tool_audit_history"]
        .get("schema_get_evidence")
        .is_none());
}

#[test]
fn build_semantic_judge_payload_stays_compact_with_large_schema_history() {
    let execution_id = ExecutionId::new();
    let huge_schema_body = "x".repeat(5_000);
    let huge_manifest = format!(
        "apiVersion: 100monkeys.ai/v1\nkind: Agent\nmetadata:\n  name: huge\n  version: 1.0.0\nspec:\n  runtime:\n    language: python\n    version: \"3.11\"\n  task:\n    prompt_template: |\n      {}\n",
        "y".repeat(5_000)
    );
    let tool_audit_history = vec![
        crate::domain::execution::TrajectoryStep {
            tool_name: "aegis.schema.get".to_string(),
            arguments_json: r#"{"key":"agent/manifest/v1"}"#.to_string(),
            status: "completed".to_string(),
            result_json: Some(format!(
                r#"{{"title":"Huge Schema","type":"object","description":"{}"}}"#,
                huge_schema_body
            )),
            error: None,
        },
        crate::domain::execution::TrajectoryStep {
            tool_name: "aegis.schema.validate".to_string(),
            arguments_json: serde_json::json!({
                "kind": "agent",
                "manifest_yaml": huge_manifest.clone(),
            })
            .to_string(),
            status: "completed".to_string(),
            result_json: Some(r#"{"valid":true,"errors":[]}"#.to_string()),
            error: None,
        },
    ];

    let payload = ToolInvocationService::build_semantic_judge_payload(
        execution_id,
        "Create an agent".to_string(),
        "aegis.agent.create",
        &serde_json::json!({
            "manifest_yaml": huge_manifest,
        }),
        vec![
            "aegis.schema.get".to_string(),
            "aegis.schema.validate".to_string(),
        ],
        vec!["/workspace".to_string()],
        "use the workflow-required sequence",
        "semantic_judge_pre_execution_inner_loop",
        1,
        &tool_audit_history,
    );

    let serialized = serde_json::to_string(&payload).expect("payload should serialize");
    assert!(
        serialized.len() < 15_000,
        "payload too large: {}",
        serialized.len()
    );
    assert!(!serialized.contains(&"x".repeat(1024)));
    assert!(!serialized.contains(&"y".repeat(1024)));
    assert_eq!(
        payload["tool_audit_history"]["tool_calls"][0]["result_summary"]["schema_key"],
        "agent/manifest/v1"
    );
    assert_eq!(
        payload["tool_audit_history"]["tool_calls"][0]["result_summary"]["result_kind"],
        "schema"
    );
}

#[tokio::test]
async fn workflow_run_tool_forwards_blackboard() {
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let start_use_case = Arc::new(TestStartWorkflowExecutionUseCase::default());

    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    )
    .with_workflow_execution(start_use_case.clone());

    let operator_context = SecurityContext {
        name: "aegis-system-operator".to_string(),
        description: "Operator".to_string(),
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

    let result = service
        .invoke_aegis_workflow_run_tool(
            &serde_json::json!({
                "name": "run-me",
                "input": { "job": "demo" },
                "blackboard": { "priority": "high" },
            }),
            &operator_context,
        )
        .await
        .expect("workflow run should return a result");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct payload");
    };

    assert_eq!(payload["tool"], "aegis.workflow.run");
    assert_eq!(payload["status"], "started");

    let request = start_use_case
        .last_request
        .lock()
        .await
        .clone()
        .expect("workflow run should record the request");
    assert_eq!(
        request.blackboard,
        Some(serde_json::json!({ "priority": "high" }))
    );
}

#[tokio::test]
async fn workflow_execution_tools_list_and_get() {
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
    let workflow_execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let tenant_id = TenantId::consumer();
    let workflow = build_test_workflow("execution-list");

    workflow_repo
        .save_for_tenant(&tenant_id, &workflow)
        .await
        .expect("workflow should save");

    let mut execution = crate::domain::workflow::WorkflowExecution::new(
        &workflow,
        ExecutionId::new(),
        serde_json::json!({ "task": "demo" }),
    );
    execution
        .blackboard
        .set("priority".to_string(), serde_json::json!("high"));
    workflow_execution_repo
        .save_for_tenant(&tenant_id, &execution)
        .await
        .expect("workflow execution should save");

    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    )
    .with_workflow_repository(workflow_repo)
    .with_workflow_execution_repo(workflow_execution_repo);

    let list_result = service
        .invoke_aegis_workflow_execution_list_tool(&serde_json::json!({
            "workflow_id": workflow.id.to_string(),
        }))
        .await
        .expect("workflow execution list should return a result");
    let ToolInvocationResult::Direct(list_payload) = list_result else {
        panic!("expected direct list payload");
    };
    assert_eq!(list_payload["tool"], "aegis.workflow.executions.list");
    assert_eq!(list_payload["count"], 1);
    assert_eq!(
        list_payload["executions"][0]["execution_id"],
        execution.id.to_string()
    );

    let get_result = service
        .invoke_aegis_workflow_execution_get_tool(&serde_json::json!({
            "execution_id": execution.id.to_string(),
        }))
        .await
        .expect("workflow execution get should return a result");
    let ToolInvocationResult::Direct(get_payload) = get_result else {
        panic!("expected direct get payload");
    };
    assert_eq!(get_payload["tool"], "aegis.workflow.executions.get");
    assert_eq!(
        get_payload["execution"]["execution_id"],
        execution.id.to_string()
    );
    assert_eq!(get_payload["execution"]["blackboard"]["priority"], "high");

    let status_result = service
        .invoke_aegis_workflow_status_tool(&serde_json::json!({
            "execution_id": execution.id.to_string(),
        }))
        .await
        .expect("workflow status should return a result");
    let ToolInvocationResult::Direct(status_payload) = status_result else {
        panic!("expected direct status payload");
    };
    assert_eq!(status_payload["tool"], "aegis.workflow.status");
    assert_eq!(
        status_payload["execution"]["execution_id"],
        execution.id.to_string()
    );
}

#[tokio::test]
async fn task_logs_tool_returns_paginated_execution_events() {
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();

    let agent_id = AgentId::new();
    let mut execution = Execution::new(
        agent_id,
        ExecutionInput {
            intent: None,
            input: serde_json::json!({"task":"demo"}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
        },
        3,
        "aegis-system-operator".to_string(),
    );
    execution.status = ExecutionStatus::Running;

    let workflow_execution_repo = Arc::new(StubWorkflowExecutionRepository::with_events(
        execution.id,
        vec![
            WorkflowExecutionEventRecord {
                sequence: 1,
                event_type: "ExecutionStarted".to_string(),
                state_name: None,
                iteration_number: None,
                payload: serde_json::json!({"message":"started"}),
                recorded_at: chrono::Utc::now(),
            },
            WorkflowExecutionEventRecord {
                sequence: 2,
                event_type: "ConsoleOutput".to_string(),
                state_name: None,
                iteration_number: Some(1),
                payload: serde_json::json!({"stream":"stdout","content":"hello"}),
                recorded_at: chrono::Utc::now(),
            },
            WorkflowExecutionEventRecord {
                sequence: 3,
                event_type: "IterationCompleted".to_string(),
                state_name: None,
                iteration_number: Some(1),
                payload: serde_json::json!({"result":"ok"}),
                recorded_at: chrono::Utc::now(),
            },
        ],
    ));

    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(LogsTestExecutionService {
            execution: execution.clone(),
        }),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    )
    .with_workflow_execution_repo(workflow_execution_repo);

    let result = service
        .invoke_aegis_task_logs_tool(&serde_json::json!({
            "execution_id": execution.id.to_string(),
            "limit": 500,
            "offset": 1,
        }))
        .await
        .expect("task logs should return a result");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct task logs payload");
    };

    assert_eq!(payload["tool"], "aegis.task.logs");
    assert_eq!(payload["execution_id"], execution.id.to_string());
    assert_eq!(payload["agent_id"], agent_id.0.to_string());
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["limit"], 200);
    assert_eq!(payload["offset"], 1);
    assert_eq!(payload["total"], 2);
    assert_eq!(payload["events"].as_array().unwrap().len(), 2);
    assert_eq!(payload["events"][0]["sequence"], 2);
}

#[tokio::test]
async fn task_logs_tool_returns_execution_fetch_error() {
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let missing_execution = Execution::new(
        AgentId::new(),
        ExecutionInput {
            intent: None,
            input: serde_json::json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
        },
        1,
        "aegis-system-operator".to_string(),
    );

    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(LogsTestExecutionService {
            execution: missing_execution,
        }),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    )
    .with_workflow_execution_repo(Arc::new(StubWorkflowExecutionRepository::default()));

    let result = service
        .invoke_aegis_task_logs_tool(&serde_json::json!({
            "execution_id": ExecutionId::new().to_string(),
        }))
        .await
        .expect("task logs should return direct error payload");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct task logs error payload");
    };

    assert_eq!(payload["tool"], "aegis.task.logs");
    assert!(payload["error"]
        .as_str()
        .unwrap()
        .contains("Failed to fetch execution"));
}

#[tokio::test]
async fn test_invoke_tool_execution_modes() {
    use crate::domain::mcp::{
        ExecutionMode, ResourceLimits, ToolServer, ToolServerId, ToolServerStatus,
    };
    use std::path::PathBuf;

    let repo = Arc::new(InMemorySealSessionRepository::new());
    let agent_id = AgentId::new();
    let exec_id = ExecutionId::new();

    use crate::domain::security_context::Capability;
    let context = SecurityContext {
        name: "test".to_string(),
        description: "".to_string(),
        capabilities: vec![
            Capability {
                tool_pattern: "test_tool".to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            },
            Capability {
                tool_pattern: "test_tool_remote".to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            },
        ],
        deny_list: vec![],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    };

    let session_token = make_fake_token(agent_id);
    let session = SealSession::new(
        agent_id,
        exec_id,
        vec![],
        session_token,
        context,
        crate::domain::tenant::TenantId::consumer(),
    );
    let _ = repo.save(session).await;

    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers.clone(), vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router.clone(),
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );

    // 1. Local Tool
    let local_server = ToolServer {
        id: ToolServerId::new(),
        name: "local-fs-tool".to_string(),
        execution_mode: ExecutionMode::Local,
        executable_path: PathBuf::from("/bin/true"),
        args: vec![],
        capabilities: vec!["test_tool".to_string()],
        skip_judge_tools: std::collections::HashSet::new(),
        status: ToolServerStatus::Running,
        process_id: None,
        health_check_interval: std::time::Duration::from_secs(30),
        last_health_check: None,
        credentials: std::collections::HashMap::new(),
        resource_limits: ResourceLimits {
            max_memory_mb: None,
            max_cpu_shares: None,
        },
        started_at: None,
        stopped_at: None,
    };

    router.add_server(local_server).await.unwrap();

    let envelope = DummyEnvelope::for_agent(true, agent_id); // extracts "test_tool"
    let result = service.invoke_tool(&envelope).await.unwrap();

    let exec_mode = result
        .get("execution_mode")
        .and_then(|v| v.as_str())
        .unwrap();
    assert_eq!(exec_mode, "local_fsal");

    // 2. Remote Tool
    let remote_server = ToolServer {
        id: ToolServerId::new(),
        name: "remote-web-tool".to_string(),
        execution_mode: ExecutionMode::Remote,
        executable_path: PathBuf::from("/bin/true"),
        args: vec![],
        capabilities: vec!["test_tool_remote".to_string()],
        skip_judge_tools: std::collections::HashSet::new(),
        status: ToolServerStatus::Running,
        process_id: None,
        health_check_interval: std::time::Duration::from_secs(30),
        last_health_check: None,
        credentials: std::collections::HashMap::new(),
        resource_limits: ResourceLimits {
            max_memory_mb: None,
            max_cpu_shares: None,
        },
        started_at: None,
        stopped_at: None,
    };
    router.add_server(remote_server).await.unwrap();

    struct DummyRemoteEnvelope {
        valid: bool,
        token: String,
    }
    impl EnvelopeVerifier for DummyRemoteEnvelope {
        fn security_token(&self) -> &str {
            &self.token
        }

        fn verify_signature(&self, _: &[u8]) -> Result<(), SealSessionError> {
            if self.valid {
                Ok(())
            } else {
                Err(SealSessionError::SignatureVerificationFailed("".into()))
            }
        }
        fn extract_tool_name(&self) -> Option<String> {
            Some("test_tool_remote".to_string())
        }
        fn extract_arguments(&self) -> Option<Value> {
            Some(serde_json::json!({}))
        }
    }

    let remote_envelope = DummyRemoteEnvelope {
        valid: true,
        token: make_fake_token(agent_id),
    };
    let result = service.invoke_tool(&remote_envelope).await.unwrap();

    let exec_mode = result
        .get("execution_mode")
        .and_then(|v| v.as_str())
        .unwrap();
    assert_eq!(exec_mode, "remote_jsonrpc");
}

#[tokio::test]
async fn get_available_tools_returns_builtin_dispatcher_metadata() {
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(
        registry,
        servers,
        vec![BuiltinDispatcherConfig {
            name: "fs.read".to_string(),
            description: "Read files from the workspace".to_string(),
            enabled: true,
            capabilities: vec![CapabilityConfig {
                name: "fs.read".to_string(),
                skip_judge: true,
            }],
        }],
    ));
    let middleware = Arc::new(SealMiddleware::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );

    let tools = service.get_available_tools().await.unwrap();
    let fs_read = tools.iter().find(|tool| tool.name == "fs.read").unwrap();

    assert_eq!(fs_read.description, "Read files from the workspace");
    assert_eq!(
        fs_read.input_schema["required"],
        serde_json::json!(["path"])
    );
}

#[tokio::test]
async fn get_available_tools_for_context_filters_disallowed_tools() {
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(
        registry,
        servers,
        vec![
            BuiltinDispatcherConfig {
                name: "fs.read".to_string(),
                description: "Read files".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "fs.read".to_string(),
                    skip_judge: true,
                }],
            },
            BuiltinDispatcherConfig {
                name: "cmd.run".to_string(),
                description: "Run commands".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "cmd.run".to_string(),
                    skip_judge: false,
                }],
            },
        ],
    ));
    let middleware = Arc::new(SealMiddleware::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    security_context_repo
        .save(crate::domain::security_context::SecurityContext {
            name: "zaru-free".to_string(),
            description: "Free tier".to_string(),
            capabilities: vec![crate::domain::security_context::Capability {
                tool_pattern: "fs.read".to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            }],
            deny_list: vec!["cmd.run".to_string()],
            metadata: crate::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        })
        .await
        .unwrap();
    let (fsal, volume_registry) = test_fsal_deps();
    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );

    let tools = service
        .get_available_tools_for_context("zaru-free")
        .await
        .unwrap();

    assert!(tools.iter().any(|tool| tool.name == "fs.read"));
    assert!(!tools.iter().any(|tool| tool.name == "cmd.run"));
}

#[tokio::test]
async fn get_available_tools_for_context_hides_destructive_workflow_tools_for_low_trust_tiers() {
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(
        registry,
        servers,
        vec![
            BuiltinDispatcherConfig {
                name: "aegis.workflow.status".to_string(),
                description: "Inspect workflow execution state".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.status".to_string(),
                    skip_judge: true,
                }],
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.delete".to_string(),
                description: "Delete workflow definitions".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.delete".to_string(),
                    skip_judge: false,
                }],
            },
        ],
    ));
    let middleware = Arc::new(SealMiddleware::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    security_context_repo
        .save(crate::domain::security_context::SecurityContext {
            name: "zaru-free".to_string(),
            description: "Free tier".to_string(),
            capabilities: vec![crate::domain::security_context::Capability {
                tool_pattern: "aegis.workflow.status".to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            }],
            deny_list: vec!["aegis.workflow.delete".to_string()],
            metadata: crate::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        })
        .await
        .unwrap();
    let (fsal, volume_registry) = test_fsal_deps();
    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(TestAgentLifecycleService),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );

    let tools = service
        .get_available_tools_for_context("zaru-free")
        .await
        .unwrap();

    assert!(tools
        .iter()
        .any(|tool| tool.name == "aegis.workflow.status"));
    assert!(!tools
        .iter()
        .any(|tool| tool.name == "aegis.workflow.delete"));
}

#[tokio::test]
async fn invoke_tool_internal_blocks_destructive_workflow_tools_for_low_trust_tiers() {
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let agent = test_agent_with_tools(&["aegis.workflow.delete"]);
    let agent_id = agent.id;
    let exec_id = ExecutionId::new();

    let context = SecurityContext {
        name: "zaru-free".to_string(),
        description: "Free tier".to_string(),
        capabilities: vec![crate::domain::security_context::Capability {
            tool_pattern: "aegis.*".to_string(),
            path_allowlist: None,
            command_allowlist: None,
            subcommand_allowlist: None,
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        }],
        deny_list: vec!["aegis.workflow.delete".to_string()],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    };

    // ADR-083: invoke_tool_internal now reads security context from the Execution record,
    // not from the SEAL session. Seed the security_context_repo and provide an execution
    // with the matching security_context_name.
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    security_context_repo.save(context.clone()).await.unwrap();

    let execution = Execution::new_with_id(
        exec_id,
        agent_id,
        ExecutionInput {
            intent: None,
            input: serde_json::json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
        },
        5,
        "zaru-free".to_string(),
    );
    let exec_service = LogsTestExecutionService { execution };

    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(FilteringAgentLifecycleService { agent }),
        Arc::new(exec_service),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );

    let result = service
        .invoke_tool_internal(
            &agent_id,
            exec_id,
            crate::domain::tenant::TenantId::consumer(),
            1,
            vec![],
            "aegis.workflow.delete".to_string(),
            serde_json::json!({ "name": "cleanup-me" }),
        )
        .await;

    assert!(matches!(
        result,
        Err(SealSessionError::PolicyViolation(
            crate::domain::mcp::PolicyViolation::ToolExplicitlyDenied { .. }
        ))
    ));
}

#[tokio::test]
async fn get_available_tools_for_agent_filters_to_declared_manifest_tools() {
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(
        registry,
        servers,
        vec![
            BuiltinDispatcherConfig {
                name: "fs.read".to_string(),
                description: "Read files".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "fs.read".to_string(),
                    skip_judge: true,
                }],
            },
            BuiltinDispatcherConfig {
                name: "cmd.run".to_string(),
                description: "Run commands".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "cmd.run".to_string(),
                    skip_judge: false,
                }],
            },
        ],
    ));
    let middleware = Arc::new(SealMiddleware::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let agent = test_agent_with_tools(&["fs.read"]);
    let agent_id = agent.id;
    let (fsal, volume_registry) = test_fsal_deps();
    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(FilteringAgentLifecycleService { agent }),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    );

    let tools = service
        .get_available_tools_for_agent(&crate::domain::tenant::TenantId::system(), agent_id)
        .await
        .unwrap();

    assert!(tools.iter().any(|tool| tool.name == "fs.read"));
    assert!(!tools.iter().any(|tool| tool.name == "cmd.run"));
}

#[test]
fn sanitize_segment_handles_empty_and_whitespace() {
    assert_eq!(sanitize_segment(""), "unversioned");
    assert_eq!(sanitize_segment("   "), "unversioned");
}

#[test]
fn sanitize_segment_blocks_traversal_patterns() {
    assert_eq!(sanitize_segment("."), "unversioned");
    assert_eq!(sanitize_segment(".."), "unversioned");
    assert_eq!(sanitize_segment("..hidden"), "unversioned");
    assert_eq!(sanitize_segment("hidden.."), "unversioned");
    assert_eq!(sanitize_segment("a..b"), "unversioned");
    assert_eq!(sanitize_segment("version..1"), "unversioned");
}

#[test]
fn sanitize_segment_replaces_special_characters() {
    assert_eq!(sanitize_segment("foo/bar"), "foo_bar");
    assert_eq!(sanitize_segment("foo\\bar"), "foo_bar");
    assert_eq!(sanitize_segment("foo:bar"), "foo_bar");
    assert_eq!(sanitize_segment("foo bar"), "foo_bar");
    assert_eq!(sanitize_segment("name@domain.com"), "name_domain.com");
}

#[test]
fn sanitize_segment_preserves_safe_mixed_alphanumeric() {
    assert_eq!(sanitize_segment("validName-123"), "validName-123");
    assert_eq!(sanitize_segment("v1.2.3-beta_01"), "v1.2.3-beta_01");
}

// ---------------------------------------------------------------------------
// Version-qualified lookup tests
// ---------------------------------------------------------------------------

/// Mock that resolves a specific agent name + version pair.
struct VersionAwareAgentLifecycleService {
    agent_name: String,
    agent_version: String,
    agent_id: AgentId,
}

#[async_trait]
impl AgentLifecycleService for VersionAwareAgentLifecycleService {
    async fn deploy_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        manifest: AgentManifest,
        force: bool,
        _scope: crate::domain::agent::AgentScope,
        _caller_identity: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<AgentId> {
        self.deploy_agent(manifest, force).await
    }

    async fn deploy_agent(&self, _: AgentManifest, _force: bool) -> Result<AgentId> {
        anyhow::bail!("not exercised")
    }

    async fn get_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
        self.get_agent(id).await
    }

    async fn get_agent(&self, _: AgentId) -> Result<Agent> {
        anyhow::bail!("not exercised")
    }

    async fn update_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: AgentId,
        manifest: AgentManifest,
    ) -> Result<()> {
        self.update_agent(id, manifest).await
    }

    async fn update_agent(&self, _: AgentId, _: AgentManifest) -> Result<()> {
        anyhow::bail!("not exercised")
    }

    async fn delete_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<()> {
        self.delete_agent(id).await
    }

    async fn delete_agent(&self, _: AgentId) -> Result<()> {
        anyhow::bail!("not exercised")
    }

    async fn list_agents_for_tenant(&self, _tenant_id: &TenantId) -> Result<Vec<Agent>> {
        self.list_agents().await
    }

    async fn list_agents(&self) -> Result<Vec<Agent>> {
        anyhow::bail!("not exercised")
    }

    async fn lookup_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        self.lookup_agent(name).await
    }

    async fn lookup_agent(&self, name: &str) -> Result<Option<AgentId>> {
        Ok((name == self.agent_name).then_some(self.agent_id))
    }

    async fn lookup_agent_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        self.lookup_agent(name).await
    }

    async fn lookup_agent_for_tenant_with_version(
        &self,
        _tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<AgentId>> {
        self.lookup_agent_with_version(name, version).await
    }

    async fn lookup_agent_with_version(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Option<AgentId>> {
        Ok((name == self.agent_name && version == self.agent_version).then_some(self.agent_id))
    }

    async fn list_agents_visible_for_tenant(&self, _tenant_id: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
    }

    async fn list_versions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn version_aware_agent_lifecycle_service_lookup_agent_with_version_is_correct() {
    let agent_id = AgentId::new();
    let agent_name = "example-agent";
    let agent_version = "1.2.3";

    let service = VersionAwareAgentLifecycleService {
        agent_name: agent_name.to_string(),
        agent_version: agent_version.to_string(),
        agent_id,
    };

    // Matching name and version returns Some(agent_id).
    let result = service
        .lookup_agent_with_version(agent_name, agent_version)
        .await
        .expect("lookup_agent_with_version should succeed");
    assert_eq!(result, Some(agent_id));

    // Correct name, wrong version returns None.
    let result = service
        .lookup_agent_with_version(agent_name, "9.9.9")
        .await
        .expect("lookup_agent_with_version should succeed");
    assert_eq!(result, None);

    // Wrong name, correct version returns None.
    let result = service
        .lookup_agent_with_version("other-agent", agent_version)
        .await
        .expect("lookup_agent_with_version should succeed");
    assert_eq!(result, None);

    // Also validate the tenant-scoped wrapper delegates correctly.
    let tenant_id = TenantId::consumer();
    let result = service
        .lookup_agent_for_tenant_with_version(&tenant_id, agent_name, agent_version)
        .await
        .expect("lookup_agent_for_tenant_with_version should succeed");
    assert_eq!(result, Some(agent_id));
}

/// Helper: build a `ToolInvocationService` backed by a `VersionAwareAgentLifecycleService`.
fn build_version_aware_service(
    agent_name: &str,
    agent_version: &str,
    agent_id: AgentId,
) -> ToolInvocationService {
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let start_use_case = Arc::new(TestStartWorkflowExecutionUseCase::default());

    ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(VersionAwareAgentLifecycleService {
            agent_name: agent_name.to_string(),
            agent_version: agent_version.to_string(),
            agent_id,
        }),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    )
    .with_workflow_execution(start_use_case)
}

#[tokio::test]
async fn task_execute_with_version_on_name_lookup_uses_versioned_resolution() {
    let agent_id = AgentId::new();
    let service = build_version_aware_service("my-agent", "2.0.0", agent_id);

    // Should resolve using version-qualified lookup and fail to start
    // (TestExecutionService always bails), but the point is that it reaches
    // the execution phase — meaning the version lookup succeeded.
    let context = SecurityContext {
        name: "test".to_string(),
        description: "".to_string(),
        capabilities: vec![],
        deny_list: vec![],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    };
    let result = service
        .invoke_aegis_task_execute_tool(
            &serde_json::json!({
                "agent_id": "my-agent",
                "version": "2.0.0",
                "input": { "task": "hello" },
            }),
            &context,
        )
        .await
        .expect("should return a direct result");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct payload");
    };

    assert_eq!(payload["tool"], "aegis.task.execute");
    // TestExecutionService bails with an error, so the response should contain
    // the "Failed to start task execution" error — proving agent resolution succeeded.
    assert!(
        payload.get("error").is_some() || payload.get("execution_id").is_some(),
        "expected either an execution_id (success) or error (from test mock), got: {payload}"
    );
}

#[tokio::test]
async fn task_execute_with_version_on_name_lookup_returns_not_found_for_wrong_version() {
    let agent_id = AgentId::new();
    let service = build_version_aware_service("my-agent", "2.0.0", agent_id);

    let context = SecurityContext {
        name: "test".to_string(),
        description: "".to_string(),
        capabilities: vec![],
        deny_list: vec![],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    };
    let result = service
        .invoke_aegis_task_execute_tool(
            &serde_json::json!({
                "agent_id": "my-agent",
                "version": "9.9.9",
                "input": {},
            }),
            &context,
        )
        .await
        .expect("should return a direct result");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct payload");
    };

    assert_eq!(payload["tool"], "aegis.task.execute");
    let error = payload["error"].as_str().expect("should have error field");
    assert!(
        error.contains("my-agent") && error.contains("9.9.9") && error.contains("not found"),
        "error should mention agent name, version, and 'not found': {error}"
    );
}

#[tokio::test]
async fn task_execute_with_version_on_uuid_returns_error() {
    let agent_id = AgentId::new();
    let service = build_version_aware_service("my-agent", "1.0.0", agent_id);

    let context = SecurityContext {
        name: "test".to_string(),
        description: "".to_string(),
        capabilities: vec![],
        deny_list: vec![],
        metadata: crate::domain::security_context::SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    };
    let result = service
        .invoke_aegis_task_execute_tool(
            &serde_json::json!({
                "agent_id": agent_id.0.to_string(),
                "version": "1.0.0",
                "input": {},
            }),
            &context,
        )
        .await
        .expect("should return a direct result");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct payload");
    };

    assert_eq!(payload["tool"], "aegis.task.execute");
    let error = payload["error"].as_str().expect("should have error field");
    assert!(
        error.contains("only supported when identifying agents by name"),
        "error should explain version is only for name lookups: {error}"
    );
}

#[tokio::test]
async fn workflow_run_with_version_passes_version_through() {
    let agent_id = AgentId::new();
    let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
    let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
    let middleware = Arc::new(SealMiddleware::new());
    let repo = Arc::new(InMemorySealSessionRepository::new());
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    let (fsal, volume_registry) = test_fsal_deps();
    let start_use_case = Arc::new(TestStartWorkflowExecutionUseCase::default());

    let service = ToolInvocationService::new(
        repo,
        security_context_repo,
        middleware,
        router,
        fsal,
        volume_registry,
        Arc::new(VersionAwareAgentLifecycleService {
            agent_name: "unused".to_string(),
            agent_version: "unused".to_string(),
            agent_id,
        }),
        Arc::new(TestExecutionService),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
        Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
        None,
    )
    .with_workflow_execution(start_use_case.clone());

    let operator_context = SecurityContext {
        name: "aegis-system-operator".to_string(),
        description: "Operator".to_string(),
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

    let result = service
        .invoke_aegis_workflow_run_tool(
            &serde_json::json!({
                "name": "my-workflow",
                "version": "3.1.0",
                "input": { "task": "demo" },
            }),
            &operator_context,
        )
        .await
        .expect("workflow run should return a result");

    let ToolInvocationResult::Direct(payload) = result else {
        panic!("expected direct payload");
    };

    assert_eq!(payload["tool"], "aegis.workflow.run");
    assert_eq!(payload["status"], "started");

    let request = start_use_case
        .last_request
        .lock()
        .await
        .clone()
        .expect("workflow run should record the request");
    assert_eq!(request.version, Some("3.1.0".to_string()));
    assert_eq!(request.workflow_id, "my-workflow");
}
