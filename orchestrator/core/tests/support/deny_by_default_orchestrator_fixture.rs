// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The node the arc `deny-by-default-orchestrator`'s tests share (Zaru
//! ADR-0071, Trigger 1): the real credential service over in-memory bindings
//! and secrets, one person whose one mailbox is granted to all their agents
//! and to the calling agent, an agent's runs with execution records, a
//! profile naming the mailbox, and a mail connector that stops every call at
//! the server, so a call that gets there passed every check.

#![allow(dead_code)]

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use serde_json::{json, Map, Value};

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::credential_service::{
    OAuthProviderRegistry, StandardCredentialManagementService,
};
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::profile_service::{
    ProfileAdmission, ProfileError, RunProfiles,
};
use aegis_orchestrator_core::application::tool_invocation_service::{
    ToolInvocationResult, ToolInvocationService,
};
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus};
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialMetadata,
    CredentialProvider, CredentialScope, CredentialStatus, CredentialType, GrantTarget,
    MailSecurity, MailboxSettings, OAuthPendingState, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::profile::{ToolAllowList, ToolPattern};
use aegis_orchestrator_core::domain::repository::{AgentVersion, VolumeRepository};
use aegis_orchestrator_core::domain::seal_session::{SealRefusal, SealSessionError};
use aegis_orchestrator_core::domain::secrets::{AccessContext, SecretPath, SensitiveString};
use aegis_orchestrator_core::domain::security_context::capability::Capability;
use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
use aegis_orchestrator_core::domain::security_context::{SecurityContext, SecurityContextMetadata};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::mail::{
    AdmissionError, AdmittedTarget, BoxedMailStream, MailConnector, MailTarget,
};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;

/// The run's person.
pub const PERSON: &str = "deny-by-default-person";
/// The run's security context, as a workflow's states run under.
pub const RUN_CONTEXT: &str = "aegis-system-agent-runtime";
/// What the stopping connector answers: the call passed every check.
pub const REACHED: &str = "Mk71-the-call-reached-the-mail-server";

/// D1 (e)'s run sentence for a mailbox.
pub const NO_PROFILE_NAMES_MAILBOX: &str = "This run has no profile that names your mailbox. Choose a profile that names it, then start the run again.";

pub fn tenant() -> TenantId {
    TenantId::for_consumer_user(PERSON).expect("a person's tenant")
}

// ---------------------------------------------------------------------------
// The person's bindings
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Bindings(tokio::sync::RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>);

#[async_trait]
impl CredentialBindingRepository for Bindings {
    async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()> {
        self.0.write().await.insert(binding.id, binding.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        Ok(self.0.read().await.get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok(self
            .0
            .read()
            .await
            .values()
            .filter(|b| &b.tenant_id == tenant_id && b.owner_user_id == owner_user_id)
            .cloned()
            .collect())
    }
    async fn find_active_grants_for_target(
        &self,
        _: &TenantId,
        _: &str,
        _: &CredentialProvider,
        _: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>> {
        Ok(Vec::new())
    }
    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
        self.0.write().await.remove(id);
        Ok(())
    }
    async fn save_oauth_state(
        &self,
        _: &str,
        _: &CredentialBindingId,
        _: &str,
        _: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn find_oauth_state(&self, _: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(None)
    }
    async fn delete_oauth_state(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_expired_oauth_states(&self, _: DateTime<Utc>) -> anyhow::Result<u64> {
        Ok(0)
    }
}

pub struct Vault {
    pub service: Arc<StandardCredentialManagementService>,
    pub bindings: Arc<Bindings>,
    secrets: Arc<SecretsManager>,
}

impl Vault {
    pub fn new() -> Self {
        let bindings = Arc::new(Bindings::default());
        let event_bus = Arc::new(EventBus::new(64));
        let secrets = Arc::new(SecretsManager::from_store(
            Arc::new(TestSecretStore::default()),
            event_bus.clone(),
        ));
        let service = Arc::new(StandardCredentialManagementService::new(
            bindings.clone(),
            secrets.clone(),
            event_bus,
            Arc::new(OAuthProviderRegistry::new()),
        ));
        Self {
            service,
            bindings,
            secrets,
        }
    }

    /// The person's mailbox, by password, granted to `grants`.
    pub async fn mailbox(&self, grants: &[GrantTarget]) -> CredentialBindingId {
        let tenant = tenant();
        let id = CredentialBindingId::new();
        let path = SecretPath::for_tenant(
            tenant.clone(),
            "kv",
            format!("users/{}/{PERSON}/credentials/{}", tenant.as_str(), id.0),
        );
        self.secrets
            .write_secret(
                &path.effective_mount(),
                &path.path,
                [(
                    "password".to_string(),
                    SensitiveString::new("Mk71-mail-password"),
                )]
                .into_iter()
                .collect(),
                &AccessContext::system("deny-by-default-test"),
            )
            .await
            .unwrap();
        let now = Utc::now();
        let mut binding = UserCredentialBinding {
            id,
            owner_user_id: PERSON.to_string(),
            tenant_id: tenant,
            credential_type: CredentialType::Mailbox,
            provider: CredentialProvider::new("imap"),
            secret_path: path,
            scope: CredentialScope::Personal,
            status: CredentialStatus::Active,
            metadata: CredentialMetadata {
                label: "Work inbox".to_string(),
                tags: None,
                service_url: None,
                external_account_id: None,
                oauth_scopes: None,
                mailbox: Some(MailboxSettings {
                    address: "person@example.test".to_string(),
                    display_name: None,
                    imap_host: "imap.example.test".to_string(),
                    imap_port: 993,
                    imap_security: MailSecurity::Tls,
                    smtp_host: "smtp.example.test".to_string(),
                    smtp_port: 465,
                    smtp_security: MailSecurity::Tls,
                    username: "person@example.test".to_string(),
                }),
                reach: None,
                calendar: None,
            },
            grants: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        for grant in grants {
            binding.add_grant(grant.clone(), PERSON.to_string());
        }
        self.bindings.save(&binding).await.unwrap();
        id
    }
}

// ---------------------------------------------------------------------------
// The node's stand-ins
// ---------------------------------------------------------------------------

/// Stops every connection with [`REACHED`].
pub struct StopAtTheServer;

#[async_trait]
impl MailConnector for StopAtTheServer {
    async fn admit(&self, _: MailTarget) -> Result<AdmittedTarget, AdmissionError> {
        Err(AdmissionError::NotAllowed {
            field: "imap_host",
            reason: REACHED.to_string(),
        })
    }
    async fn connect(&self, _: &AdmittedTarget) -> std::io::Result<BoxedMailStream> {
        Err(std::io::Error::other("not reached"))
    }
    async fn start_tls(&self, _: BoxedMailStream, _: &str) -> std::io::Result<BoxedMailStream> {
        Err(std::io::Error::other("not reached"))
    }
}

/// The person's profile `id`, naming `bindings` under `imap` and admitting
/// every mail tool.
pub struct OneProfile {
    pub id: uuid::Uuid,
    pub mailbox: CredentialBindingId,
}

#[async_trait]
impl RunProfiles for OneProfile {
    async fn contexts_of(
        &self,
        _: &TenantId,
        person: Option<&str>,
        id: uuid::Uuid,
    ) -> Result<Map<String, Value>, ProfileError> {
        if id != self.id || person != Some(PERSON) {
            return Err(ProfileError::NotFound);
        }
        Ok(json!({ "imap": [self.mailbox.0.to_string()] })
            .as_object()
            .unwrap()
            .clone())
    }

    async fn admission_of(
        &self,
        _: &TenantId,
        person: Option<&str>,
        id: uuid::Uuid,
    ) -> Result<ProfileAdmission, ProfileError> {
        if id != self.id || person != Some(PERSON) {
            return Err(ProfileError::NotFound);
        }
        Ok(ProfileAdmission {
            tools: ToolAllowList(vec![ToolPattern::parse("mail.*").unwrap()]),
            families: vec!["mail".to_string(), "imap".to_string()],
        })
    }
}

/// Serves the runs that call tools, and records the runs started.
#[derive(Default)]
pub struct Runs {
    pub records: Mutex<HashMap<ExecutionId, Execution>>,
    pub started: Mutex<Vec<ExecutionInput>>,
}

impl Runs {
    /// A run of `agent` for the person, its input `input`.
    pub fn record(&self, agent: AgentId, input: Value) -> ExecutionId {
        let mut execution = Execution::new_with_id(
            ExecutionId::new(),
            agent,
            ExecutionInput {
                intent: None,
                input,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            5,
            RUN_CONTEXT.to_string(),
        );
        execution.tenant_id = tenant();
        execution.initiating_user_sub = Some(PERSON.to_string());
        let id = execution.id;
        self.records.lock().unwrap().insert(id, execution);
        id
    }
}

#[async_trait]
impl ExecutionService for Runs {
    async fn start_execution(
        &self,
        _: AgentId,
        input: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        self.started.lock().unwrap().push(input);
        Ok(ExecutionId::new())
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        input: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        self.started.lock().unwrap().push(input);
        Ok(execution_id)
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_for_tenant(&self, _: &TenantId, id: ExecutionId) -> Result<Execution> {
        self.get_execution_unscoped(id).await
    }
    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        self.records
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("execution not found"))
    }
    async fn get_iterations_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> Result<Vec<Iteration>> {
        Ok(vec![])
    }
    async fn cancel_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        Ok(())
    }
    async fn stream_execution(
        &self,
        _: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn list_executions_for_tenant(
        &self,
        _: &TenantId,
        _: Option<AgentId>,
        _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
        _: usize,
    ) -> Result<Vec<Execution>> {
        Ok(vec![])
    }
    async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        Ok(())
    }
    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: aegis_orchestrator_core::domain::execution::LlmInteraction,
    ) -> Result<()> {
        Ok(())
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        Ok(())
    }
}

/// One agent, `mail-triage`, resolved for every name, with no
/// `tool_validation`, so the inner-loop judge does not run.
pub struct OneAgent(pub Agent);

pub fn agent() -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: mail-triage
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: ["mail.list"]
"#,
    )
    .unwrap();
    Agent {
        id: AgentId::new(),
        tenant_id: tenant(),
        scope: aegis_orchestrator_core::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

#[async_trait]
impl AgentLifecycleService for OneAgent {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: aegis_orchestrator_core::domain::agent::AgentScope,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
        Ok(self.0.clone())
    }
    async fn update_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: AgentManifest,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

struct NoEvents;

#[async_trait]
impl EventPublisher for NoEvents {
    async fn publish_storage_event(
        &self,
        _: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}

fn run_context() -> SecurityContext {
    SecurityContext {
        name: RUN_CONTEXT.to_string(),
        description: "a workflow's states".to_string(),
        capabilities: ["mail.*", "calendar.*", "nuclear-notes.*"]
            .iter()
            .map(|pattern| Capability {
                tool_pattern: pattern.to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            })
            .collect(),
        deny_list: vec![],
        metadata: SecurityContextMetadata {
            created_at: Utc::now(),
            updated_at: Utc::now(),
            version: 1,
        },
    }
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

pub struct Fixture {
    pub service: ToolInvocationService,
    pub runs: Arc<Runs>,
    pub agent: Agent,
    pub mailbox: CredentialBindingId,
    pub profile: uuid::Uuid,
    pub _dirs: tempfile::TempDir,
}

/// The node with the person's one mailbox, granted to all their agents and
/// to the calling agent, and their profile naming it.
pub async fn fixture() -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let vault = Vault::new();
    let agent = agent();
    let mailbox = vault
        .mailbox(&[
            GrantTarget::AllAgents,
            GrantTarget::Agent { agent_id: agent.id },
        ])
        .await;
    let profile = uuid::Uuid::new_v4();
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(run_context()).await.unwrap();
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(
            aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider::new(
                dirs.path().join("fsal"),
            )
            .unwrap(),
        ),
        Arc::new(InMemoryVolumeRepository::new()) as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    ));
    let runs = Arc::new(Runs::default());
    let service = ToolInvocationService::new(
        Arc::new(InMemorySealSessionRepository::new()),
        contexts,
        Arc::new(SealMiddleware::new()),
        Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(agent.clone())),
        runs.clone(),
        Arc::new(
            aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(
            ),
        ),
        Arc::new(EventBus::new(64)),
        None,
    )
    .with_mail_tools_over(vault.service.clone(), Arc::new(StopAtTheServer))
    .with_profiles(Arc::new(OneProfile {
        id: profile,
        mailbox,
    }));
    Fixture {
        service,
        runs,
        agent,
        mailbox,
        profile,
        _dirs: dirs,
    }
}

impl Fixture {
    /// A run of the fixture's agent, its input `input`.
    pub fn run(&self, input: Value) -> ExecutionId {
        self.runs.record(self.agent.id, input)
    }

    /// `tool` with `arguments`, called by the run `execution`.
    pub async fn call(
        &self,
        execution: ExecutionId,
        tool: &str,
        arguments: Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        self.service
            .invoke_tool_internal(
                &self.agent.id,
                execution,
                tenant(),
                0,
                Vec::new(),
                tool.to_string(),
                arguments,
            )
            .await
    }
}

/// What a call told its caller: the refusal, or whether it reached the
/// mail server.
pub fn refusal_of(result: &Result<ToolInvocationResult, SealSessionError>) -> Option<SealRefusal> {
    match result {
        Ok(_) => None,
        Err(e) => Some(e.refusal()),
    }
}

/// Whether the call passed every check and reached the mail server.
pub fn reached(result: &Result<ToolInvocationResult, SealSessionError>) -> bool {
    match result {
        Ok(_) => true,
        Err(e) => format!("{e:?} {}", e.refusal().message).contains(REACHED),
    }
}

/// The record pattern of `facing_text_no_record_refs.rs` (AEGIS ADR-134 D3).
pub fn record_pattern() -> regex::Regex {
    regex::Regex::new(
        r"ADR-[0-9]|CD-[0-9]|ADR [0-9]|(?i:decision record)|security audit 0|audit 0[0-9][0-9]|§[0-9]",
    )
    .unwrap()
}
