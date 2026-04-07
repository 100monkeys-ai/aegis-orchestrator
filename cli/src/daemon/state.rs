// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Application state container for the daemon HTTP server.

use std::sync::Arc;

use aegis_orchestrator_core::{
    application::{
        credential_service::CredentialManagementService, execution::StandardExecutionService,
        file_operations_service::FileOperationsService, lifecycle::StandardAgentLifecycleService,
        register_workflow::StandardRegisterWorkflowUseCase,
        start_workflow_execution::StandardStartWorkflowExecutionUseCase, stimulus::StimulusService,
        user_volume_service::UserVolumeService, CorrelatedActivityStreamService,
    },
    domain::{
        cluster::NodeClusterRepository,
        iam::{IdentityProvider, RealmRepository},
        node_config::NodeConfigManifest,
        repository::{StorageEventRepository, WorkflowExecutionRepository, WorkflowRepository},
    },
    infrastructure::{
        event_bus::EventBus, secrets_manager::SecretsManager, temporal_client::TemporalClient,
        TemporalEventListener,
    },
    presentation::webhook_guard::WebhookSecretProvider,
};
use aegis_orchestrator_swarm::infrastructure::StandardSwarmService;

use super::operator_read_models::OperatorReadModelStore;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) agent_service: Arc<StandardAgentLifecycleService>,
    pub(crate) execution_service: Arc<StandardExecutionService>,
    pub(crate) execution_repo:
        Arc<dyn aegis_orchestrator_core::domain::repository::ExecutionRepository>,
    pub(crate) correlated_activity_stream_service: Arc<CorrelatedActivityStreamService>,
    pub(crate) cluster_repo: Option<Arc<dyn NodeClusterRepository>>,
    pub(crate) event_bus: Arc<EventBus>,
    pub(crate) inner_loop_service:
        Arc<aegis_orchestrator_core::application::inner_loop_service::InnerLoopService>,
    pub(crate) human_input_service: Arc<aegis_orchestrator_core::infrastructure::HumanInputService>,
    pub(crate) temporal_event_listener: Arc<TemporalEventListener>,
    pub(crate) register_workflow_use_case: Arc<StandardRegisterWorkflowUseCase>,
    pub(crate) start_workflow_execution_use_case: Arc<StandardStartWorkflowExecutionUseCase>,
    pub(crate) workflow_repo: Arc<dyn WorkflowRepository>,
    pub(crate) workflow_execution_repo: Arc<dyn WorkflowExecutionRepository>,
    pub(crate) workflow_scope_service:
        Arc<aegis_orchestrator_core::application::workflow_scope::WorkflowScopeService>,
    pub(crate) agent_scope_service:
        Arc<aegis_orchestrator_core::application::agent_scope::AgentScopeService>,
    pub(crate) temporal_client_container: Arc<tokio::sync::RwLock<Option<Arc<TemporalClient>>>>,
    pub(crate) storage_event_repo: Arc<dyn StorageEventRepository>,
    pub(crate) tool_invocation_service:
        Arc<aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService>,
    pub(crate) attestation_service:
        Arc<dyn aegis_orchestrator_core::infrastructure::seal::attestation::AttestationService>,
    pub(crate) swarm_service: Arc<StandardSwarmService>,
    pub(crate) operator_read_model: Arc<OperatorReadModelStore>,
    pub(crate) cortex_client:
        Option<Arc<aegis_orchestrator_core::infrastructure::CortexGrpcClient>>,
    pub(crate) rate_limit_override_repo: Option<
        Arc<aegis_orchestrator_core::infrastructure::rate_limit::RateLimitOverrideRepository>,
    >,
    pub(crate) api_key_repo: Option<
        Arc<aegis_orchestrator_core::infrastructure::repositories::PostgresApiKeyRepository>,
    >,
    pub(crate) iam_service: Option<Arc<dyn IdentityProvider>>,
    pub(crate) tenant_provisioning_service: Option<
        Arc<aegis_orchestrator_core::application::tenant_provisioning::TenantProvisioningService>,
    >,
    /// Identity realm repository for dynamic OIDC realm persistence (ADR-041). Optional until wired.
    #[allow(dead_code)]
    pub(crate) realm_repo: Option<Arc<dyn RealmRepository>>,
    /// BC-11 Credential management service (ADR-078). Optional until DB is available.
    pub(crate) credential_service: Option<Arc<dyn CredentialManagementService>>,
    /// Secrets manager for `/v1/secrets/*` admin endpoints (ADR-034).
    pub(crate) secrets_manager: Arc<SecretsManager>,
    /// HMAC secret provider for webhook requests (ADR-021).
    pub(crate) webhook_secret_provider: Arc<dyn WebhookSecretProvider>,
    /// BC-8: Stimulus routing and webhook ingestion service (ADR-021). Optional until wired.
    pub(crate) stimulus_service: Option<Arc<dyn StimulusService>>,
    pub(crate) user_volume_service: Arc<UserVolumeService>,
    pub(crate) file_operations_service: Arc<FileOperationsService>,
    pub(crate) config: NodeConfigManifest,
    pub(crate) start_time: std::time::Instant,
}
