// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The node and the person the arc `mcp-connections-orchestrator`'s tests
//! share (Zaru ADR-0069, Trigger 1): a session attested as the Zaru MCP
//! server attests one, the person's one mailbox, no calendar, no remote
//! server binding, and the approval gate.

#![allow(dead_code)]

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;

use crate::mail_standins::StoredMessage;
use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::credential_service::{
    ContextBinding, ToolCalendar, ToolCalendarSource, ToolCallActor, ToolCredentialSource,
    ToolMailbox, ToolMailboxSource,
};
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_approval_service::ToolApprovalService;
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService;
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest};
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, MailSecurity, MailboxSettings,
};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::repository::{AgentVersion, VolumeRepository};
use aegis_orchestrator_core::domain::seal_session::{
    EnvelopeVerifier, SealSession, SealSessionError,
};
use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::security_context::capability::Capability;
use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
use aegis_orchestrator_core::domain::security_context::{SecurityContext, SecurityContextMetadata};
use aegis_orchestrator_core::domain::shared_kernel::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::mail::{
    AdmissionError, AdmittedTarget, BoxedMailStream, MailAuth, MailConnector, MailTarget,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;

/// The key's user.
pub const PERSON: &str = "mcp-person";
pub const LOGIN: &str = "person@example.test";
pub const PASSWORD: &str = "Mk21-mcp-mailbox-password";
/// The mailbox's context name.
pub const MAILBOX_NAME: &str = "Work inbox";
/// What the stopping connector answers: the call passed every check.
pub const REACHED: &str = "Mk21-the-call-reached-the-mail-server";
/// D5's sentence for a pending call.
pub const WAITS: &str = "This waits for your answer in Zaru Web, under Approvals.";

/// D3's sentence for `what`.
pub fn not_connected(what: &str) -> String {
    format!(
        "You have not connected {what} to Zaru. Connect it on the Connections page in Zaru Web (ask.myzaru.com/vault/connections), then try again."
    )
}

// ---------------------------------------------------------------------------
// The person's connections
// ---------------------------------------------------------------------------

/// The person's one mailbox, at `port` on loopback.
pub struct OneMailbox {
    pub id: CredentialBindingId,
    pub port: u16,
}

#[async_trait]
impl ToolMailboxSource for OneMailbox {
    async fn tool_mailbox(
        &self,
        actor: &ToolCallActor<'_>,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<Option<ToolMailbox>> {
        if *binding_id != self.id || actor.user_id != PERSON {
            return Ok(None);
        }
        Ok(Some(ToolMailbox {
            binding_id: self.id,
            settings: MailboxSettings {
                address: LOGIN.to_string(),
                display_name: None,
                imap_host: "127.0.0.1".to_string(),
                imap_port: self.port,
                imap_security: MailSecurity::Tls,
                smtp_host: "127.0.0.1".to_string(),
                smtp_port: 465,
                smtp_security: MailSecurity::Tls,
                username: LOGIN.to_string(),
            },
            auth: MailAuth::Password(SensitiveString::new(PASSWORD)),
        }))
    }

    async fn mailbox_contexts(
        &self,
        _tenant_id: &TenantId,
        user_id: &str,
    ) -> anyhow::Result<Vec<ContextBinding>> {
        if user_id != PERSON {
            return Ok(Vec::new());
        }
        Ok(vec![ContextBinding {
            id: self.id,
            name: MAILBOX_NAME.to_string(),
            reach: None,
        }])
    }
}

/// No calendar account for anyone.
pub struct NoCalendars;

#[async_trait]
impl ToolCalendarSource for NoCalendars {
    async fn tool_calendar(
        &self,
        _: &ToolCallActor<'_>,
        _: &CredentialBindingId,
    ) -> anyhow::Result<Option<ToolCalendar>> {
        Ok(None)
    }
}

/// No remote server binding for anyone.
pub struct NoServerBindings;

#[async_trait]
impl ToolCredentialSource for NoServerBindings {
    async fn tool_server_credential(
        &self,
        _: &ToolCallActor<'_>,
        _: &str,
    ) -> anyhow::Result<Option<SensitiveString>> {
        Ok(None)
    }
}

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

// ---------------------------------------------------------------------------
// The node: no execution records, no agents
// ---------------------------------------------------------------------------

pub struct NoEvents;

#[async_trait]
impl EventPublisher for NoEvents {
    async fn publish_storage_event(
        &self,
        _: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}

pub struct NoExecutions;

#[async_trait]
impl ExecutionService for NoExecutions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("unused")
    }
    async fn start_execution_with_id(
        &self,
        _: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("unused")
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> Result<ExecutionId> {
        anyhow::bail!("unused")
    }
    async fn get_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<Execution> {
        anyhow::bail!("no execution record")
    }
    async fn get_execution_unscoped(&self, _: ExecutionId) -> Result<Execution> {
        anyhow::bail!("no execution record")
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
        anyhow::bail!("unused")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> Result<
        Pin<
            Box<
                dyn Stream<
                        Item = Result<
                            aegis_orchestrator_core::infrastructure::event_bus::DomainEvent,
                        >,
                    > + Send,
            >,
        >,
    > {
        anyhow::bail!("unused")
    }
    async fn list_executions_for_tenant(
        &self,
        _: &TenantId,
        _: Option<AgentId>,
        _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
        _: usize,
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

pub struct NoAgents;

#[async_trait]
impl AgentLifecycleService for NoAgents {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: aegis_orchestrator_core::domain::agent::AgentScope,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<AgentId> {
        anyhow::bail!("unused")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
        anyhow::bail!("no agent")
    }
    async fn update_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: AgentManifest,
    ) -> Result<()> {
        Ok(())
    }
    async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<()> {
        Ok(())
    }
    async fn list_agents_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(None)
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
        Ok(vec![])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

/// A call as `POST /v1/seal/invoke` receives it.
pub struct Envelope {
    token: SensitiveString,
    tool: String,
    arguments: Value,
}

impl EnvelopeVerifier for Envelope {
    fn security_token(&self) -> &SensitiveString {
        &self.token
    }
    fn verify_signature(&self, _: &[u8]) -> Result<(), SealSessionError> {
        Ok(())
    }
    fn extract_tool_name(&self) -> Option<String> {
        Some(self.tool.clone())
    }
    fn extract_arguments(&self) -> Option<Value> {
        Some(self.arguments.clone())
    }
    fn replay_nonce(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// The person's tier context: the connection families and `aegis.*`.
pub fn tier_context() -> SecurityContext {
    let capability = |pattern: &str| Capability {
        tool_pattern: pattern.to_string(),
        path_allowlist: None,
        command_allowlist: None,
        subcommand_allowlist: None,
        domain_allowlist: None,
        max_response_size: None,
        rate_limit: None,
        max_concurrent: None,
    };
    SecurityContext {
        name: "zaru-free".to_string(),
        description: "the free plan".to_string(),
        capabilities: [
            "mail.*",
            "calendar.*",
            "github.*",
            "nuclear-notes.*",
            "notes-own.*",
            "aegis.*",
        ]
        .iter()
        .map(|p| capability(p))
        .collect(),
        deny_list: vec![],
        metadata: SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

pub struct Fixture {
    pub service: ToolInvocationService,
    pub sessions: Arc<InMemorySealSessionRepository>,
    pub approvals: Arc<InMemoryToolApprovalRepository>,
    pub tenant: TenantId,
    pub token: String,
    pub mailbox: CredentialBindingId,
    pub _dirs: tempfile::TempDir,
}

/// A node with the person's one mailbox at `port` (behind `connector`), no
/// calendar and no remote server binding, the approval gate, and a session
/// attested as the Zaru MCP server attests one.
pub async fn fixture(port: u16, connector: Arc<dyn MailConnector>) -> Fixture {
    let mailbox = CredentialBindingId::new();
    fixture_over(
        Arc::new(OneMailbox { id: mailbox, port }),
        mailbox,
        connector,
    )
    .await
}

/// [`fixture`] with `mailboxes` as the mailbox source.
pub async fn fixture_over(
    mailboxes: Arc<dyn ToolMailboxSource>,
    mailbox: CredentialBindingId,
    connector: Arc<dyn MailConnector>,
) -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let event_bus = Arc::new(EventBus::new(64));
    let volumes = Arc::new(InMemoryVolumeRepository::new());
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(
            aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider::new(
                dirs.path().join("fsal"),
            )
            .unwrap(),
        ),
        volumes as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    ));
    let tenant = TenantId::for_consumer_user(PERSON).unwrap();
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(tier_context()).await.unwrap();
    let token = format!("token-{}", uuid::Uuid::new_v4());
    let session = SealSession::new(
        AgentId::new(),
        ExecutionId::new(),
        vec![],
        token.clone(),
        tier_context(),
        tenant.clone(),
    )
    .with_principal_metadata(
        Some(PERSON.to_string()),
        Some(PERSON.to_string()),
        None,
        Some("free".to_string()),
    );
    sessions.save(session).await.unwrap();
    let session_repo = sessions.clone();

    let approvals = Arc::new(InMemoryToolApprovalRepository::new());
    let service = ToolInvocationService::new(
        sessions,
        contexts,
        Arc::new(SealMiddleware::new()),
        Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(NoAgents),
        Arc::new(NoExecutions),
        Arc::new(
            aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(
            ),
        ),
        event_bus.clone(),
        None,
    )
    .with_mail_tools_over(mailboxes, connector)
    .with_calendar_tools(Arc::new(NoCalendars))
    .with_tool_credentials(Arc::new(NoServerBindings))
    .with_remote_tool_servers(vec![
        "github".to_string(),
        "nuclear-notes".to_string(),
        "notes-own".to_string(),
    ])
    .with_tool_approvals(Arc::new(ToolApprovalService::new(
        approvals.clone(),
        event_bus,
    )));
    Fixture {
        service,
        sessions: session_repo,
        approvals,
        tenant,
        token,
        mailbox,
        _dirs: dirs,
    }
}

impl Fixture {
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<Value, SealSessionError> {
        self.service
            .invoke_tool(&Envelope {
                token: self.token.clone().into(),
                tool: tool.to_string(),
                arguments,
            })
            .await
    }
}

/// The sentence a refusal tells its caller, with its code when it is the
/// binding-required one.
pub fn told(result: &Result<Value, SealSessionError>) -> String {
    match result {
        Ok(value) => format!("answered {value}"),
        Err(e) => {
            let refusal = e.refusal();
            format!("{}: {}", refusal.code, refusal.message)
        }
    }
}

pub fn inbox() -> Vec<StoredMessage> {
    vec![StoredMessage::new(
        1,
        &[],
        "05-Oct-2026 09:00:00 +0000",
        &[
            "Message-ID: <q1@x>",
            "From: Ann <ann@example.test>",
            "To: person@example.test",
            "Subject: Quarterly figures",
            "Date: Mon, 05 Oct 2026 09:00:00 +0000",
        ],
        "The figures are attached.",
    )]
}

/// A source that answers no mailbox for anyone.
pub struct NoMailboxes;

#[async_trait]
impl ToolMailboxSource for NoMailboxes {
    async fn tool_mailbox(
        &self,
        _: &ToolCallActor<'_>,
        _: &CredentialBindingId,
    ) -> anyhow::Result<Option<ToolMailbox>> {
        Ok(None)
    }
}

/// The record pattern of `facing_text_no_record_refs.rs` (AEGIS ADR-134 D3).
pub fn record_pattern() -> regex::Regex {
    regex::Regex::new(
        r"ADR-[0-9]|CD-[0-9]|ADR [0-9]|(?i:decision record)|security audit 0|audit 0[0-9][0-9]|§[0-9]",
    )
    .unwrap()
}
