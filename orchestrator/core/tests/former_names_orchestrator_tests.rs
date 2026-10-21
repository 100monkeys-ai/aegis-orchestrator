// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A remote tool server's former names (Carbon Forge ADR-0198, Update of
//! 2026-10-21, R2 and R3 step 1): under
//! `{name: carbon-forge, former_names: [nuclear-notes]}` every reader of a
//! server's name reads `nuclear-notes` as `carbon-forge`, so a deployment
//! can rename the server without breaking a stored row, a caller or an
//! agent manifest; and under today's configuration, which names no former
//! name, nothing changes.
//!
//! The orchestrator's tool invocation service is wired as the daemon wires
//! it (`with_seal_gateway_config`) and dials a loopback SEAL gateway over
//! TLS that records every `InvokeTool`; the credential vault is the real
//! `StandardCredentialManagementService` over in-memory bindings.
//!
//! | Clause | Test |
//! | --- | --- |
//! | A call of `nuclear-notes.pages.read` is routed as `carbon-forge.pages.read`, its contexts key `nuclear-notes` filling `carbon-forge` | `a_former_name_is_routed_as_its_server_and_fills_its_contexts` |
//! | The run-tree server is the entry formerly named `nuclear-notes` | `the_run_tree_server_is_the_entry_formerly_named_nuclear_notes` |
//! | A binding stored with provider `nuclear-notes` is found for `carbon-forge` | `a_binding_stored_under_a_former_name_is_found_for_its_server` |
//! | A former name that is another entry's name is refused at load | `a_former_name_that_is_another_entrys_name_is_refused_at_load` |
//! | Today's configuration reads every name as written | `under_todays_configuration_every_name_reads_as_written` |

mod carbon_forge_support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use serde_json::{json, Value};

use aegis_orchestrator_core::application::credential_service::{
    ContextBinding, CredentialManagementService, GroundingRefusal, OAuthProviderRegistry,
    RemoteServerGrounding, StandardCredentialManagementService, StoreApiKeyCommand, ToolCallActor,
    ToolCredentialSource,
};
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService;
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialProvider,
    CredentialScope, CredentialType, GrantTarget, OAuthPendingState, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::execution::{ContextChoice, ExecutionContexts, ExecutionId};
use aegis_orchestrator_core::domain::execution_token::{ExecutionToken, ExecutionTokens};
use aegis_orchestrator_core::domain::fsal::AegisFSAL;
use aegis_orchestrator_core::domain::node_config::SealGatewayConfig;
use aegis_orchestrator_core::domain::repository::VolumeRepository;
use aegis_orchestrator_core::domain::run_tree::{RunTree, RunTreeError, RunTrees};
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
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;

use carbon_forge_support::{serve_tls, Answer, NoAgents, NoEvents, NoExecutions, StubGateway};

/// The person every call acts for.
const PERSON: &str = "former-names-person";
/// The person's Carbon Forge binding, as Zaru Web chose it.
const BINDING: &str = "6a1d2c3b-0000-4000-8000-0000000000f1";
/// The bearer token that binding resolves to (a fake).
const TOKEN: &str = "former-names-binding-token";
/// The secret of a run's execution token (a fake).
const CHILD_TOKEN: &str = "former-names-child-token";

/// The server's name after the rename, and the name it had.
const RENAMED: &str = "carbon-forge";
const FORMER: &str = "nuclear-notes";

/// `seal_gateway.remote_servers` after the rename (the Update's step 4).
const RENAMED_SERVERS: &str = "remote_servers:\n  - name: carbon-forge\n    grounding_tool: cortex.ground\n    former_names: [nuclear-notes]\n";
/// `seal_gateway.remote_servers` as production names it today.
const TODAYS_SERVERS: &str =
    "remote_servers:\n  - name: nuclear-notes\n    grounding_tool: cortex.ground\n";

fn binding_id() -> CredentialBindingId {
    CredentialBindingId(uuid::Uuid::parse_str(BINDING).unwrap())
}

/// `seal_gateway` dialling `url` (its CA at `ca`) with `servers`, read as
/// the daemon reads its configuration.
fn gateway_config(url: &str, ca: &std::path::Path, servers: &str) -> SealGatewayConfig {
    serde_yaml::from_str(&format!(
        "url: {url}\nca_cert_path: {}\n{servers}",
        ca.display()
    ))
    .unwrap_or_else(|e| panic!("seal_gateway with {servers:?} did not load: {e}"))
}

// ---------------------------------------------------------------------------
// The person's binding, as a credential source answers for one server name
// ---------------------------------------------------------------------------

/// The person's one binding, known only under `server`: what the vault
/// answers once it reads the binding for that server. It keeps a run's
/// execution token as the secret store does.
struct OneBindingFor {
    server: &'static str,
    children: Mutex<HashMap<uuid::Uuid, ExecutionToken>>,
}

impl OneBindingFor {
    fn new(server: &'static str) -> Arc<Self> {
        Arc::new(Self {
            server,
            children: Mutex::default(),
        })
    }
}

#[async_trait]
impl ToolCredentialSource for OneBindingFor {
    async fn tool_server_credential(
        &self,
        actor: &ToolCallActor<'_>,
        server: &str,
    ) -> anyhow::Result<Option<SensitiveString>> {
        let chosen = matches!(actor.context, ContextChoice::Binding(id) if id == binding_id());
        Ok((server == self.server && actor.user_id == PERSON && chosen)
            .then(|| SensitiveString::new(TOKEN.to_string())))
    }

    async fn context_bindings(
        &self,
        _: &TenantId,
        user_id: &str,
        server: &str,
    ) -> anyhow::Result<Vec<ContextBinding>> {
        Ok(if server == self.server && user_id == PERSON {
            vec![ContextBinding {
                id: binding_id(),
                name: "Work instance".to_string(),
                reach: None,
            }]
        } else {
            Vec::new()
        })
    }

    async fn store_execution_token(
        &self,
        _: &TenantId,
        _: &str,
        execution_id: uuid::Uuid,
        token: &ExecutionToken,
    ) -> anyhow::Result<()> {
        self.children
            .lock()
            .unwrap()
            .insert(execution_id, token.clone());
        Ok(())
    }

    async fn execution_token(
        &self,
        _: &TenantId,
        _: &str,
        execution_id: uuid::Uuid,
    ) -> anyhow::Result<Option<ExecutionToken>> {
        Ok(self.children.lock().unwrap().get(&execution_id).cloned())
    }
}

// ---------------------------------------------------------------------------
// The node: the tool invocation service as the daemon wires it
// ---------------------------------------------------------------------------

/// A call as `POST /v1/seal/invoke` receives it.
struct Envelope {
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

/// The person's plan context, admitting `pattern` and `aegis.*`.
fn plan_context(pattern: &str) -> SecurityContext {
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
        capabilities: vec![capability(pattern), capability("aegis.*")],
        deny_list: vec![],
        metadata: SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

struct Node {
    service: ToolInvocationService,
    calls: Arc<
        Mutex<Vec<aegis_orchestrator_core::infrastructure::seal_gateway_proto::InvokeToolRequest>>,
    >,
    session_token: String,
    tenant: TenantId,
    _stop: tokio::sync::oneshot::Sender<()>,
    _dirs: tempfile::TempDir,
}

impl Node {
    /// Each `InvokeTool` the gateway received: its server, its tool and the
    /// credential it carried.
    fn invoked(&self) -> Vec<(String, String, String)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| {
                (
                    call.server.clone(),
                    call.tool.clone(),
                    call.credential
                        .as_ref()
                        .map(|c| c.value.clone())
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Call `tool` as the person's own client, with `meta` as the payload's
    /// `params._meta`.
    async fn call(&self, tool: &str, meta: Value) -> Result<Value, SealSessionError> {
        self.service
            .invoke_tool_with_meta(
                &Envelope {
                    token: self.session_token.clone().into(),
                    tool: tool.to_string(),
                    arguments: json!({ "path": "notes/welcome" }),
                },
                Some(&meta),
            )
            .await
    }
}

/// A node whose `seal_gateway.remote_servers` is `servers`, whose person's
/// plan admits `pattern`, dialling a gateway that answers as `stub` and
/// resolving the person's binding through `credentials`.
async fn node(
    servers: &str,
    pattern: &str,
    stub: StubGateway,
    credentials: Arc<dyn ToolCredentialSource>,
) -> Node {
    let dirs = tempfile::tempdir().unwrap();
    let calls = stub.calls.clone();
    let (url, ca, stop) = serve_tls(stub, dirs.path()).await;
    let config = gateway_config(&url, &ca, servers);

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
    let volume_service =
        aegis_orchestrator_core::application::volume_manager::StandardVolumeService::new(
            fsal.volume_repository().clone(),
            fsal.storage_provider().clone(),
            event_bus.clone(),
            "http://localhost:8888".to_string(),
            "local_host",
        )
        .expect("the volume service");
    let user_volumes = Arc::new(
        aegis_orchestrator_core::application::user_volume_service::UserVolumeService::new(
            fsal.volume_repository().clone(),
            Arc::new(volume_service),
            event_bus.clone(),
            aegis_orchestrator_core::domain::volume::StorageTierLimits::default(),
        ),
    );

    let tenant = TenantId::for_consumer_user(PERSON).unwrap();
    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(plan_context(pattern)).await.unwrap();
    let session_token = format!("token-{}", uuid::Uuid::new_v4());
    sessions
        .save(
            SealSession::new(
                AgentId::new(),
                ExecutionId::new(),
                vec![],
                session_token.clone(),
                plan_context(pattern),
                tenant.clone(),
            )
            .with_principal_metadata(
                Some(PERSON.to_string()),
                Some(PERSON.to_string()),
                None,
                Some("free".to_string()),
            ),
        )
        .await
        .unwrap();

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
        event_bus,
        Some(url),
    )
    .with_user_volume_service(user_volumes)
    .with_seal_gateway_config(Some(&config), Some(credentials))
    .expect("the daemon's seal_gateway wiring accepts the configuration");
    Node {
        service,
        calls,
        session_token,
        tenant,
        _stop: stop,
        _dirs: dirs,
    }
}

/// A gateway answering `pages.read`, and the run's grounding, mint and
/// tree as Carbon Forge answers them.
fn carbon_forge(run: uuid::Uuid) -> StubGateway {
    let mut stub = carbon_forge_support::nuclear_notes(run);
    stub.answers.insert(
        "pages.read".to_string(),
        Answer::Result(json!({ "content": [{ "type": "text", "text": "# Welcome" }] })),
    );
    stub.answers.insert(
        "tokens.mint_execution".to_string(),
        Answer::Result(json!({
            "token_id": "7d1e0c4a-0000-4000-8000-0000000000f2",
            "secret": CHILD_TOKEN,
            "execution_id": run,
        })),
    );
    // The tree's revision is gone: its refusal is Carbon Forge's own
    // sentence, answered after the call reached the server.
    stub.answers.insert(
        "trees.materialize".to_string(),
        Answer::RemoteError("revision 4242 of code/former-names was not found".to_string()),
    );
    stub
}

// ---------------------------------------------------------------------------
// The renamed server
// ---------------------------------------------------------------------------

/// R3 step 1: a person's client calling `nuclear-notes.pages.read` with
/// `_meta.contexts` `{"nuclear-notes": <binding>}` is admitted by a plan
/// that names only `carbon-forge.*`, and the gateway receives
/// `carbon-forge` `pages.read` with the binding the former key chose.
#[tokio::test]
async fn a_former_name_is_routed_as_its_server_and_fills_its_contexts() {
    let node = node(
        RENAMED_SERVERS,
        "carbon-forge.*",
        carbon_forge(uuid::Uuid::new_v4()),
        OneBindingFor::new(RENAMED),
    )
    .await;
    let answer = node
        .call(
            "nuclear-notes.pages.read",
            json!({ "contexts": { FORMER: BINDING } }),
        )
        .await;
    println!("nuclear-notes.pages.read answered {answer:?}");
    let invoked = node.invoked();
    println!("the gateway received {invoked:?}");
    assert!(
        answer.is_ok(),
        "R3 step 1: the call of nuclear-notes.pages.read under {{name: carbon-forge, \
         former_names: [nuclear-notes]}} was refused: {answer:?}"
    );
    assert_eq!(
        invoked,
        vec![(
            RENAMED.to_string(),
            "pages.read".to_string(),
            TOKEN.to_string()
        )],
        "R3 step 1: nuclear-notes.pages.read was not routed as carbon-forge.pages.read with \
         the binding its contexts key nuclear-notes chose"
    );
}

/// R3 step 1: the run-tree server is the configured entry whose former
/// names include `nuclear-notes`: a run whose `contexts` chose the person's
/// binding under `nuclear-notes` has its execution token minted on
/// `carbon-forge` with that binding, and its tree's call carries the
/// execution token to `carbon-forge` (`gateway.rs`' execution-token path).
#[tokio::test]
async fn the_run_tree_server_is_the_entry_formerly_named_nuclear_notes() {
    let run = uuid::Uuid::new_v4();
    let credentials = OneBindingFor::new(RENAMED);
    let node = node(
        RENAMED_SERVERS,
        "carbon-forge.*",
        carbon_forge(run),
        credentials.clone(),
    )
    .await;
    let contexts = ExecutionContexts::from_value(Some(&json!({ FORMER: BINDING })));
    let tree = RunTree {
        workspace: "former-names".to_string(),
        path: "code/former-names".to_string(),
        revision: 4242,
        volume_id: None,
        blobs: Vec::new(),
    };
    let minted = node
        .service
        .mint_for_run(
            &node.tenant,
            Some(PERSON),
            run,
            &contexts,
            &tree,
            &["trees.materialize", "blobs.get"],
        )
        .await;
    println!("the mint answered {minted:?}");
    let materialized = node
        .service
        .materialize_for_run(&node.tenant, Some(PERSON), run, &contexts, vec![tree])
        .await;
    println!("the materialization answered {materialized:?}");
    let invoked = node.invoked();
    println!("the gateway received {invoked:?}");
    let expected = vec![
        (
            RENAMED.to_string(),
            "cortex.ground".to_string(),
            TOKEN.to_string(),
        ),
        (
            RENAMED.to_string(),
            "tokens.mint_execution".to_string(),
            TOKEN.to_string(),
        ),
        (
            RENAMED.to_string(),
            "trees.materialize".to_string(),
            CHILD_TOKEN.to_string(),
        ),
    ];
    assert!(
        minted.is_ok() && credentials.children.lock().unwrap().contains_key(&run),
        "R3 step 1: the run's execution token was not minted on the entry formerly named \
         nuclear-notes: {minted:?}, the gateway received {invoked:?}"
    );
    assert!(
        matches!(&materialized, Err(RunTreeError::Refused(sentence)) if sentence.contains("revision 4242")),
        "R3 step 1: the tree's call did not reach the entry formerly named nuclear-notes with \
         the run's execution token: {materialized:?}"
    );
    assert_eq!(
        invoked, expected,
        "R3 step 1: the run-tree server is not the entry formerly named nuclear-notes"
    );
}

// ---------------------------------------------------------------------------
// The vault: a binding stored under the former name
// ---------------------------------------------------------------------------

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
        Ok(())
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

/// A grounding that answers every token with one instance, recording the
/// (server, tool) it was asked.
#[derive(Default)]
struct Grounding {
    asked: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl RemoteServerGrounding for Grounding {
    async fn ground_token(
        &self,
        _: &TenantId,
        _: &str,
        server: &str,
        tool: &str,
        _: &SensitiveString,
    ) -> Result<Value, GroundingRefusal> {
        self.asked
            .lock()
            .unwrap()
            .push((server.to_string(), tool.to_string()));
        Ok(json!({
            "instance": {"id": "root-id", "slug": "main"},
            "you": {
                "userId": "u-1",
                "currentWorkspace": null,
                "instances": [{"id": "i-1", "slug": "main", "name": "main", "role": "member"}],
            },
        }))
    }
}

/// The vault over `bindings` and `store`, its remote servers read from
/// `servers` as the daemon reads them.
fn vault(
    bindings: Arc<Bindings>,
    store: Arc<TestSecretStore>,
    grounding: &Arc<Grounding>,
    servers: &str,
) -> StandardCredentialManagementService {
    let event_bus = Arc::new(EventBus::new(64));
    let service = StandardCredentialManagementService::new(
        bindings,
        Arc::new(SecretsManager::from_store(store, event_bus.clone())),
        event_bus,
        Arc::new(OAuthProviderRegistry::new()),
    );
    let config = gateway_config(
        "https://aegis-seal-gateway:50055",
        std::path::Path::new("/nonexistent/ca.crt"),
        servers,
    );
    let weak: Weak<Grounding> = Arc::downgrade(grounding);
    let weak: Weak<dyn RemoteServerGrounding> = weak;
    assert!(service.set_remote_grounding(weak, config.remote_server_entries().unwrap()));
    service
}

/// Store the person's token for `provider` through `service`.
async fn store_for(
    service: &StandardCredentialManagementService,
    tenant: &TenantId,
    provider: &str,
) -> anyhow::Result<CredentialBindingId> {
    service
        .store_api_key(StoreApiKeyCommand {
            owner_user_id: PERSON.to_string(),
            tenant_id: tenant.clone(),
            provider: CredentialProvider::new(provider),
            label: "Work instance".to_string(),
            scope: CredentialScope::Personal,
            api_key_value: SensitiveString::new(TOKEN),
            credential_type: CredentialType::Secret,
        })
        .await
}

/// R3 step 1: a binding stored with provider `nuclear-notes` before the
/// rename is found for `carbon-forge` after it: listed as the person's
/// context, resolved when a run chose it and when a client chose nothing;
/// and a token stored for `nuclear-notes` after the rename is stored for
/// `carbon-forge`, grounded there with `cortex.ground`.
#[tokio::test]
async fn a_binding_stored_under_a_former_name_is_found_for_its_server() {
    let bindings = Arc::new(Bindings::default());
    let store = Arc::new(TestSecretStore::new());
    let grounding = Arc::new(Grounding::default());
    let tenant = TenantId::for_consumer_user(PERSON).unwrap();
    // Before the rename: today's configuration stores the row.
    let before = vault(bindings.clone(), store.clone(), &grounding, TODAYS_SERVERS);
    let stored = store_for(&before, &tenant, FORMER)
        .await
        .expect("today's store");

    // After the rename: the same rows, the renamed configuration.
    let after = vault(bindings.clone(), store.clone(), &grounding, RENAMED_SERVERS);
    let mut complaints = Vec::new();
    let listed = after
        .context_bindings(&tenant, PERSON, RENAMED)
        .await
        .unwrap();
    println!("the person's carbon-forge contexts: {listed:?}");
    if listed.iter().map(|b| b.id).collect::<Vec<_>>() != vec![stored] {
        complaints.push(format!(
            "the binding stored with provider nuclear-notes is not among the person's \
             carbon-forge contexts: {listed:?}"
        ));
    }
    let actor = ToolCallActor {
        tenant_id: &tenant,
        user_id: PERSON,
        agent_id: AgentId::new(),
        workflow_id: None,
        context: ContextChoice::Binding(stored),
    };
    let chosen = after.tool_server_credential(&actor, RENAMED).await.unwrap();
    if chosen.as_ref().map(|s| s.expose().to_string()) != Some(TOKEN.to_string()) {
        complaints.push(format!(
            "the chosen binding stored with provider nuclear-notes did not resolve for \
             carbon-forge: {}",
            chosen.is_some()
        ));
    }
    let own = after
        .own_server_credential(&tenant, PERSON, RENAMED)
        .await
        .unwrap();
    if own.as_ref().map(|s| s.expose().to_string()) != Some(TOKEN.to_string()) {
        complaints.push(format!(
            "the person's own binding stored with provider nuclear-notes did not resolve for \
             carbon-forge: {}",
            own.is_some()
        ));
    }

    // A token stored for the former name after the rename.
    let renamed = store_for(&after, &tenant, FORMER)
        .await
        .expect("the renamed store");
    let provider = bindings
        .find_by_id(&renamed)
        .await
        .unwrap()
        .map(|b| b.provider.as_str().to_string());
    println!("a token stored for nuclear-notes after the rename has provider {provider:?}");
    if provider.as_deref() != Some(RENAMED) {
        complaints.push(format!(
            "a token stored for nuclear-notes after the rename was not stored for carbon-forge: \
             {provider:?}"
        ));
    }
    let asked = grounding.asked.lock().unwrap().clone();
    println!("the grounding was asked {asked:?}");
    if asked.last() != Some(&(RENAMED.to_string(), "cortex.ground".to_string())) {
        complaints.push(format!(
            "the token stored for nuclear-notes after the rename was not grounded on \
             carbon-forge with cortex.ground: {asked:?}"
        ));
    }
    assert!(
        complaints.is_empty(),
        "R3 step 1: {}",
        complaints.join("; ")
    );
}

// ---------------------------------------------------------------------------
// The configuration
// ---------------------------------------------------------------------------

/// R2: a former name that is also another entry's name, or another
/// entry's former name, would make one name mean two servers; the
/// configuration is refused as it loads, the sentence naming both entries.
#[test]
fn a_former_name_that_is_another_entrys_name_is_refused_at_load() {
    let mut complaints = Vec::new();
    for (servers, sentence) in [
        (
            "remote_servers:\n  - github\n  - name: carbon-forge\n    former_names: [github]\n",
            "seal_gateway.remote_servers: 'carbon-forge' names the former name 'github', which \
             is the name of the entry 'github'",
        ),
        (
            "remote_servers:\n  - name: carbon-forge\n    former_names: [nuclear-notes]\n  - name: notes-2\n    former_names: [nuclear-notes]\n",
            "seal_gateway.remote_servers: 'carbon-forge' names the former name 'nuclear-notes', \
             which the entry 'notes-2' also names as a former name",
        ),
    ] {
        let loaded = serde_yaml::from_str::<SealGatewayConfig>(&format!(
            "url: https://aegis-seal-gateway:50055\n{servers}"
        ));
        let refusal = match &loaded {
            Ok(config) => format!("loaded as {:?}", config.remote_servers.entries()),
            Err(e) => e.to_string(),
        };
        println!("{servers:?}: {refusal}");
        if loaded.is_ok() || !refusal.contains(sentence) {
            complaints.push(format!(
                "R2: {servers:?} was not refused at load with \"{sentence}\": {refusal}"
            ));
        }
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// Under today's configuration, which names no former name, every name
/// reads as written: `nuclear-notes.pages.read` is routed to
/// `nuclear-notes`, `carbon-forge.pages.read` names no server of the node,
/// the configuration is written back as it was read, and the former names
/// are none.
#[tokio::test]
async fn under_todays_configuration_every_name_reads_as_written() {
    let mut complaints = Vec::new();
    let config = gateway_config(
        "https://aegis-seal-gateway:50055",
        std::path::Path::new("/nonexistent/ca.crt"),
        TODAYS_SERVERS,
    );
    let written = serde_json::to_value(&config.remote_servers).unwrap();
    if written != json!([{ "name": FORMER, "grounding_tool": "cortex.ground" }]) {
        complaints.push(format!(
            "today's remote_servers were written back as {written}"
        ));
    }
    if !config.former_names().is_empty() {
        complaints.push(format!(
            "today's configuration names former names: {:?}",
            config.former_names()
        ));
    }

    let node = node(
        TODAYS_SERVERS,
        "nuclear-notes.*",
        carbon_forge(uuid::Uuid::new_v4()),
        OneBindingFor::new(FORMER),
    )
    .await;
    let today = node
        .call(
            "nuclear-notes.pages.read",
            json!({ "contexts": { FORMER: BINDING } }),
        )
        .await;
    println!("today, nuclear-notes.pages.read answered {today:?}");
    let renamed = node
        .call(
            "carbon-forge.pages.read",
            json!({ "contexts": { RENAMED: BINDING } }),
        )
        .await;
    println!("today, carbon-forge.pages.read answered {renamed:?}");
    let invoked = node.invoked();
    println!("today, the gateway received {invoked:?}");
    if today.is_err() {
        complaints.push(format!(
            "today, nuclear-notes.pages.read was refused: {today:?}"
        ));
    }
    if renamed.is_ok() {
        complaints.push(format!(
            "today, carbon-forge.pages.read was answered though the node names no such \
             server: {renamed:?}"
        ));
    }
    if invoked
        != vec![(
            FORMER.to_string(),
            "pages.read".to_string(),
            TOKEN.to_string(),
        )]
    {
        complaints.push(format!(
            "today, the gateway did not receive exactly nuclear-notes pages.read: {invoked:?}"
        ));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
