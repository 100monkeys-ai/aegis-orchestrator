// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The mail read tools in the tool service (AEGIS ADR-125 D4; its Update of
//! 2026-10-07 clauses 2 and 7): the catalogue's entries and contracts, the
//! credential service's answer for a mailbox, and the dispatch handing a
//! mail tool the acting person, the run's choice and whether the run has a
//! record. Sessions against a mail server are tested in
//! `orchestrator/core/tests/mail_session_tests.rs`.

use super::*;
use crate::application::credential_service::{
    StandardCredentialManagementService, ToolCallActor, ToolMailbox, ToolMailboxSource,
};
use crate::application::tools::builtin_mail::{CHOSEN_DIFFERENT, NOT_GRANTED, NO_PERSON};
use crate::domain::agent::{Agent, AgentManifest, AgentStatus};
use crate::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialMetadata,
    CredentialProvider, CredentialScope, CredentialStatus, CredentialType, GrantTarget,
    MailSecurity, MailboxSettings, OAuthPendingState, UserCredentialBinding,
};
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{Execution, ExecutionId, ExecutionInput, Iteration};
use crate::domain::mcp::ToolInputContract;
use crate::domain::repository::AgentVersion;
use crate::domain::seal_session::CallerAnswer;
use crate::domain::secrets::{AccessContext, SecretPath, SensitiveString};
use crate::domain::security_context::SecurityContext;
use crate::infrastructure::event_bus::DomainEvent;
use crate::infrastructure::mail::{
    AdmissionError, AdmittedTarget, BoxedMailStream, MailConnector, MailTarget,
};
use crate::infrastructure::repositories::InMemoryVolumeRepository;
use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use crate::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use crate::infrastructure::storage::LocalHostStorageProvider;
use async_trait::async_trait;
use futures::Stream;
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;

const CONTEXT: &str = "mail-test-context";
const USER: &str = "mail-user";
/// What the stopping connector answers, so a test sees the call got past
/// every check to the mail server.
const REACHED: &str = "Mk3-the-call-reached-the-mail-server";

// ---------------------------------------------------------------------------
// The catalogue's entries and contracts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_mail_tools_are_listed_with_contracts_declaring_mailbox_and_flagged() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let tools = router.list_tools().await.unwrap();
    let mut problems = Vec::new();
    for (name, required, skip_judge) in [
        ("mail.list", vec!["mailbox"], true),
        ("mail.read", vec!["mailbox", "thread_id"], true),
        ("mail.label", vec!["mailbox", "thread_id"], false),
    ] {
        let Some(tool) = tools.iter().find(|t| t.name == name) else {
            problems.push(format!("{name} is not listed"));
            continue;
        };
        let schema = &tool.input_schema;
        if schema["properties"]["mailbox"]["type"] != "string" {
            problems.push(format!(
                "{name}'s schema does not declare mailbox: {schema}"
            ));
        }
        let schema_required: Vec<&str> = schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if schema_required != required {
            problems.push(format!("{name}'s schema requires {schema_required:?}"));
        }
        if ToolInputContract::required_fields(name) != required.as_slice() {
            problems.push(format!(
                "{name}'s input contract requires {:?}",
                ToolInputContract::required_fields(name)
            ));
        }
        if router.is_skip_judge(name).await != skip_judge {
            problems.push(format!("{name}'s judge skip is not {skip_judge}"));
        }
    }
    if let Some(label) = tools.iter().find(|t| t.name == "mail.label") {
        if label.input_schema["properties"]["flagged"]["type"] != "boolean" {
            problems.push(format!(
                "mail.label's schema does not declare flagged: {}",
                label.input_schema
            ));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

// ---------------------------------------------------------------------------
// The credential service's answer for a mailbox
// ---------------------------------------------------------------------------

/// Bindings held in memory, as the Postgres repository holds them.
#[derive(Default)]
struct Bindings(tokio::sync::RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>);

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
        anyhow::bail!("not exercised")
    }
    async fn find_oauth_state(&self, _: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(None)
    }
    async fn delete_oauth_state(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_expired_oauth_states(
        &self,
        _: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<u64> {
        Ok(0)
    }
}

/// The real credential service over in-memory bindings and secrets.
struct Vault {
    service: Arc<StandardCredentialManagementService>,
    bindings: Arc<Bindings>,
    secrets: Arc<SecretsManager>,
}

fn settings() -> MailboxSettings {
    MailboxSettings {
        address: "owner@example.test".to_string(),
        display_name: None,
        imap_host: "imap.example.test".to_string(),
        imap_port: 993,
        imap_security: MailSecurity::Tls,
        smtp_host: "smtp.example.test".to_string(),
        smtp_port: 465,
        smtp_security: MailSecurity::Tls,
        username: "owner@example.test".to_string(),
    }
}

impl Vault {
    fn new() -> Self {
        let bindings = Arc::new(Bindings::default());
        let event_bus = Arc::new(EventBus::new(64));
        let secrets = Arc::new(SecretsManager::from_store(
            Arc::new(TestSecretStore::new()),
            event_bus.clone(),
        ));
        let service = Arc::new(StandardCredentialManagementService::new(
            bindings.clone(),
            secrets.clone(),
            event_bus,
            Arc::new(HashMap::new()),
        ));
        Self {
            service,
            bindings,
            secrets,
        }
    }

    /// Store a binding of `user` in `tenant`, its password `password`.
    #[allow(clippy::too_many_arguments)]
    async fn bind(
        &self,
        tenant: &TenantId,
        user: &str,
        credential_type: CredentialType,
        provider: &str,
        status: CredentialStatus,
        mailbox: Option<MailboxSettings>,
        grants: &[GrantTarget],
    ) -> CredentialBindingId {
        let id = CredentialBindingId::new();
        let path = SecretPath::for_tenant(
            tenant.clone(),
            "kv",
            format!("users/{}/{user}/credentials/{}", tenant.as_str(), id.0),
        );
        self.secrets
            .write_secret(
                &path.effective_mount(),
                &path.path,
                [(
                    "password".to_string(),
                    SensitiveString::new("Mk5-mail-password"),
                )]
                .into_iter()
                .collect(),
                &AccessContext::system("mail-test"),
            )
            .await
            .unwrap();
        let now = chrono::Utc::now();
        let mut binding = UserCredentialBinding {
            id,
            owner_user_id: user.to_string(),
            tenant_id: tenant.clone(),
            credential_type,
            provider: CredentialProvider::new(provider),
            secret_path: path,
            scope: CredentialScope::Personal,
            status,
            metadata: CredentialMetadata {
                label: format!("mailbox of {user}"),
                tags: None,
                service_url: None,
                external_account_id: None,
                oauth_scopes: None,
                mailbox,
                reach: None,
            },
            grants: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        for grant in grants {
            binding.add_grant(grant.clone(), user.to_string());
        }
        self.bindings.save(&binding).await.unwrap();
        id
    }
}

#[tokio::test]
async fn the_credential_service_answers_only_the_persons_own_active_mailbox() {
    let vault = Vault::new();
    let tenant = TenantId::default();
    let agent_id = AgentId::new();
    let own = vault
        .bind(
            &tenant,
            USER,
            CredentialType::Mailbox,
            "imap",
            CredentialStatus::Active,
            Some(settings()),
            &[GrantTarget::Agent { agent_id }],
        )
        .await;
    let own_ungranted = vault
        .bind(
            &tenant,
            USER,
            CredentialType::Mailbox,
            "imap",
            CredentialStatus::Active,
            Some(settings()),
            &[],
        )
        .await;
    let others = vault
        .bind(
            &tenant,
            "someone-else",
            CredentialType::Mailbox,
            "imap",
            CredentialStatus::Active,
            Some(settings()),
            &[GrantTarget::AllAgents],
        )
        .await;
    let not_a_mailbox = vault
        .bind(
            &tenant,
            USER,
            CredentialType::Secret,
            "imap",
            CredentialStatus::Active,
            None,
            &[GrantTarget::AllAgents],
        )
        .await;
    let revoked = vault
        .bind(
            &tenant,
            USER,
            CredentialType::Mailbox,
            "imap",
            CredentialStatus::Revoked,
            Some(settings()),
            &[GrantTarget::AllAgents],
        )
        .await;
    let other_tenant = TenantId::from_string("other-tenant").unwrap();
    let elsewhere = vault
        .bind(
            &other_tenant,
            USER,
            CredentialType::Mailbox,
            "imap",
            CredentialStatus::Active,
            Some(settings()),
            &[GrantTarget::AllAgents],
        )
        .await;
    let actor = ToolCallActor {
        tenant_id: &tenant,
        user_id: USER,
        agent_id,
        workflow_id: None,
        context: crate::domain::execution::ContextChoice::NotGiven,
    };
    let answer = |id| {
        let service = vault.service.clone();
        let actor = actor;
        async move { service.tool_mailbox(&actor, &id).await.unwrap() }
    };
    let mailbox = answer(own)
        .await
        .expect("the person's own mailbox is answered");
    assert_eq!(mailbox.settings, settings());
    assert_eq!(mailbox.password.expose(), "Mk5-mail-password");
    assert!(mailbox.granted, "a grant to the calling agent was not seen");
    let ungranted = answer(own_ungranted)
        .await
        .expect("an ungranted own mailbox is answered");
    assert!(!ungranted.granted, "an ungranted mailbox read as granted");
    let mut refused = Vec::new();
    for (what, id) in [
        ("another person's mailbox", others),
        ("a binding that is not a mailbox", not_a_mailbox),
        ("a revoked mailbox", revoked),
        ("a mailbox in another tenant", elsewhere),
        ("a binding that does not exist", CredentialBindingId::new()),
    ] {
        if answer(id).await.is_some() {
            refused.push(what);
        }
    }
    assert!(refused.is_empty(), "answered: {refused:?}");
    let _ = &vault.bindings;
}

// ---------------------------------------------------------------------------
// The dispatch hands a mail tool the acting person and the run's choice
// ---------------------------------------------------------------------------

/// Answers the person's own mailbox `id`, ungranted, and records who asked.
struct RecordingMailbox {
    id: CredentialBindingId,
    asked: StdMutex<Vec<(String, AgentId)>>,
}

#[async_trait]
impl ToolMailboxSource for RecordingMailbox {
    async fn tool_mailbox(
        &self,
        actor: &ToolCallActor<'_>,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<Option<ToolMailbox>> {
        self.asked
            .lock()
            .unwrap()
            .push((actor.user_id.to_string(), actor.agent_id));
        if *binding_id != self.id || actor.user_id != USER {
            return Ok(None);
        }
        Ok(Some(ToolMailbox {
            binding_id: self.id,
            settings: settings(),
            password: SensitiveString::new("Mk5-mail-password"),
            granted: false,
        }))
    }
}

/// Refuses every endpoint with [`REACHED`]: a call that gets here passed
/// every check before the mail server.
struct StopAtTheServer;

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

fn agent() -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: mail-test-agent
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
        tenant_id: TenantId::default(),
        scope: crate::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn security_context() -> SecurityContext {
    SecurityContext {
        name: CONTEXT.to_string(),
        description: "mail test".to_string(),
        capabilities: vec![crate::domain::security_context::Capability {
            tool_pattern: "mail.*".to_string(),
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
    }
}

/// A tool service with `mailboxes` (or none) and one execution per entry of
/// `runs`: its person and its `contexts` input.
async fn service_with(
    mailboxes: Option<Arc<dyn ToolMailboxSource>>,
    runs: &[(Option<&str>, Option<Value>)],
) -> (ToolInvocationService, AgentId, Vec<ExecutionId>) {
    let agent = agent();
    let agent_id = agent.id;
    let tenant = TenantId::default();
    let executions: Vec<Execution> = runs
        .iter()
        .map(|(user, contexts)| {
            let mut e = Execution::new_with_id(
                ExecutionId::new(),
                agent_id,
                ExecutionInput {
                    intent: None,
                    input: match contexts {
                        Some(contexts) => json!({ "contexts": contexts }),
                        None => json!({}),
                    },
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: Vec::new(),
                },
                5,
                CONTEXT.to_string(),
            );
            e.tenant_id = tenant.clone();
            e.initiating_user_sub = user.map(str::to_string);
            e
        })
        .collect();
    let ids = executions.iter().map(|e| e.id).collect();
    let security_context_repo =
        Arc::new(crate::infrastructure::security_context::InMemorySecurityContextRepository::new());
    security_context_repo
        .save(security_context())
        .await
        .unwrap();
    let storage_root =
        std::env::temp_dir().join(format!("aegis-mail-tests-{}", uuid::Uuid::new_v4()));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let service = ToolInvocationService::new(
        Arc::new(InMemorySealSessionRepository::new()),
        security_context_repo,
        Arc::new(SealMiddleware::new()),
        Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(agent)),
        Arc::new(Executions(
            executions.into_iter().map(|e| (e.id, e)).collect(),
        )),
        Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
        Arc::new(EventBus::new(1024)),
        None,
    );
    let service = match mailboxes {
        Some(source) => service.with_mail_tools_over(source, Arc::new(StopAtTheServer)),
        None => service,
    };
    (service, agent_id, ids)
}

async fn list_on(
    service: &ToolInvocationService,
    agent_id: AgentId,
    execution: ExecutionId,
    mailbox: CredentialBindingId,
) -> Result<ToolInvocationResult, SealSessionError> {
    service
        .invoke_tool_internal(
            &agent_id,
            execution,
            TenantId::default(),
            0,
            Vec::new(),
            "mail.list".to_string(),
            json!({ "mailbox": mailbox.0.to_string() }),
        )
        .await
}

/// The sentence a refusal tells its caller.
fn told(result: &Result<ToolInvocationResult, SealSessionError>) -> String {
    match result {
        Ok(_) => "the call succeeded".to_string(),
        Err(SealSessionError::Answered {
            answer: CallerAnswer::CredentialBindingRequired { message },
            ..
        }) => message.clone(),
        Err(SealSessionError::InvalidArguments(message)) => message.clone(),
        Err(other) => format!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dispatch_hands_a_mail_tool_the_runs_person_and_its_choice() {
    let id = CredentialBindingId::new();
    let other = CredentialBindingId::new();
    let source = Arc::new(RecordingMailbox {
        id,
        asked: StdMutex::new(Vec::new()),
    });
    let (service, agent_id, runs) = service_with(
        Some(source.clone()),
        &[
            (Some(USER), Some(json!({ "imap": id.0.to_string() }))),
            (Some(USER), Some(json!({ "imap": other.0.to_string() }))),
            (Some(USER), None),
            (None, None),
        ],
    )
    .await;
    let mut wrong = Vec::new();
    let chosen = list_on(&service, agent_id, runs[0], id).await;
    if !told(&chosen).contains(REACHED) {
        wrong.push(format!(
            "the run's own choice was not admitted: {}",
            told(&chosen)
        ));
    }
    let chosen_other = list_on(&service, agent_id, runs[1], id).await;
    if told(&chosen_other) != CHOSEN_DIFFERENT {
        wrong.push(format!(
            "a different choice was not refused: {}",
            told(&chosen_other)
        ));
    }
    let nothing_chosen = list_on(&service, agent_id, runs[2], id).await;
    if told(&nothing_chosen) != NOT_GRANTED {
        wrong.push(format!(
            "an agent's run with nothing chosen and no grant was not refused: {}",
            told(&nothing_chosen)
        ));
    }
    let personless = list_on(&service, agent_id, runs[3], id).await;
    if told(&personless) != NO_PERSON {
        wrong.push(format!(
            "a run with no person was not refused: {}",
            told(&personless)
        ));
    }
    let asked = source.asked.lock().unwrap().clone();
    if asked
        .iter()
        .any(|(user, agent)| user != USER || *agent != agent_id)
    {
        wrong.push(format!("the mailbox was asked for someone else: {asked:?}"));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mail_tool_on_a_node_without_mail_tools_is_not_configured() {
    let (service, agent_id, runs) = service_with(None, &[(Some(USER), None)]).await;
    let result = list_on(&service, agent_id, runs[0], CredentialBindingId::new()).await;
    assert!(
        matches!(
            &result,
            Err(SealSessionError::Answered {
                answer: CallerAnswer::Internal(
                    crate::domain::seal_session::InternalFailure::Unavailable
                ),
                ..
            })
        ),
        "{}",
        told(&result)
    );
}

// ---------------------------------------------------------------------------
// Test doubles the dispatch reads, as the gateway wire tests define them
// ---------------------------------------------------------------------------

/// Serves the executions that call tools.
struct Executions(HashMap<ExecutionId, Execution>);

#[async_trait]
impl ExecutionService for Executions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&crate::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
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
        anyhow::bail!("not exercised")
    }
    async fn get_execution_for_tenant(&self, _: &TenantId, id: ExecutionId) -> Result<Execution> {
        self.get_execution_unscoped(id).await
    }
    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        self.0
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("execution not found"))
    }
    async fn get_iterations_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> Result<Vec<Iteration>> {
        anyhow::bail!("not exercised")
    }
    async fn cancel_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
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
        _: Option<crate::domain::workflow::WorkflowId>,
        _: usize,
    ) -> Result<Vec<Execution>> {
        anyhow::bail!("not exercised")
    }
    async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: crate::domain::execution::LlmInteraction,
    ) -> Result<()> {
        Ok(())
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<crate::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Resolves every agent to one agent with no `tool_validation`, so the
/// inner-loop judge does not run.
struct OneAgent(Agent);

#[async_trait]
impl AgentLifecycleService for OneAgent {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: crate::domain::agent::AgentScope,
        _: Option<&crate::domain::iam::UserIdentity>,
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
        anyhow::bail!("not exercised")
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

struct NoOpPublisher;

#[async_trait]
impl crate::domain::fsal::EventPublisher for NoOpPublisher {
    async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
}
