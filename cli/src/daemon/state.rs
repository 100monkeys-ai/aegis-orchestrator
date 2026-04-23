// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Application state container for the daemon HTTP server.

use std::sync::Arc;

use aegis_orchestrator_core::{
    application::{
        canvas_service::CanvasService, credential_service::CredentialManagementService,
        execution::StandardExecutionService, file_operations_service::FileOperationsService,
        lifecycle::StandardAgentLifecycleService,
        register_workflow::StandardRegisterWorkflowUseCase,
        start_workflow_execution::StandardStartWorkflowExecutionUseCase, stimulus::StimulusService,
        user_volume_service::UserVolumeService, CorrelatedActivityStreamService,
    },
    domain::{
        cluster::NodeClusterRepository,
        iam::{IdentityProvider, RealmRepository},
        node_config::NodeConfigManifest,
        repository::{
            StorageEventRepository, TenantRepository, WorkflowExecutionRepository,
            WorkflowRepository,
        },
    },
    infrastructure::{
        event_bus::EventBus, iam::keycloak_admin_client::KeycloakAdminClient,
        secrets_manager::SecretsManager, temporal_client::TemporalClient, TemporalEventListener,
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
    /// BC-7 Git Repository Binding service (ADR-081). Optional until
    /// the surrounding infrastructure (git2-backed executor, volume
    /// service, OpenBao) is wired in at startup.
    pub(crate) git_repo_service:
        Option<Arc<aegis_orchestrator_core::application::git_repo_service::GitRepoService>>,
    /// BC-7 Vibe-Code Canvas session service (ADR-106 Wave C2). Optional until
    /// the orchestrator is configured with the canvas repository/migration 020.
    pub(crate) canvas_service: Option<Arc<dyn CanvasService>>,
    /// BC-7 Script persistence service (ADR-110 §D7). Optional until a
    /// Postgres pool is available and migration 021 has been applied.
    pub(crate) script_service:
        Option<Arc<aegis_orchestrator_core::application::script_service::ScriptService>>,
    /// Team tenancy service (ADR-111). Optional until a Postgres pool, a
    /// `BillingConfig`, and an `invitation_hmac_key` are all configured.
    #[allow(dead_code)] // handlers land in Phase 2
    pub(crate) team_service:
        Option<Arc<dyn aegis_orchestrator_core::application::team_service::TeamService>>,
    /// Team repository (ADR-111 §Tenant-Context Header Extension). Consumed
    /// by the tenant middleware to resolve `X-Tenant-Id: t-{uuid}` headers
    /// against the team aggregate. Optional until a Postgres pool is
    /// available.
    pub(crate) team_repo: Option<Arc<dyn aegis_orchestrator_core::domain::team::TeamRepository>>,
    /// Membership repository (ADR-111 §Tenant-Context Header Extension).
    /// Consumed by the tenant middleware to gate team-context switching on
    /// Active membership. Optional until a Postgres pool is available.
    pub(crate) membership_repo:
        Option<Arc<dyn aegis_orchestrator_core::domain::team::MembershipRepository>>,
    pub(crate) config: NodeConfigManifest,
    pub(crate) start_time: std::time::Instant,
    /// Keycloak Admin REST client for colony management endpoints.
    pub(crate) keycloak_admin: Option<Arc<KeycloakAdminClient>>,
    /// Tenant repository for subscription queries.
    pub(crate) tenant_repo: Option<Arc<dyn TenantRepository>>,
    /// Billing repository for Stripe subscription state persistence.
    pub(crate) billing_repo:
        Option<Arc<dyn aegis_orchestrator_core::infrastructure::repositories::BillingRepository>>,
    /// Stripe billing configuration from `NodeConfigSpec.billing`.
    pub(crate) billing_config: Option<aegis_orchestrator_core::domain::node_config::BillingConfig>,
    /// Effective personal-tier synchronizer (ADR-111 Phase 3). Owns all writes
    /// to the Keycloak `zaru_tier` attribute, computing `max(personal,
    /// active_colony_tiers)` before each write. Optional until Keycloak admin,
    /// billing repo, team repo, and membership repo are all wired.
    pub(crate) effective_tier_service: Option<
        Arc<dyn aegis_orchestrator_core::application::effective_tier_service::EffectiveTierService>,
    >,
    /// Base URL for the zaru-client service (e.g. `https://myzaru.com`).
    /// Loaded from the `ZARU_URL` environment variable. Used to call internal
    /// endpoints such as `/api/internal/invalidate-sessions` after a tier change.
    pub(crate) zaru_url: Option<String>,
    /// Shared secret for zaru-client internal endpoints.
    /// Loaded from the `ZARU_INTERNAL_SECRET` environment variable.
    pub(crate) zaru_internal_secret: Option<String>,
}
