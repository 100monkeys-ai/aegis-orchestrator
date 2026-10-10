// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An alias names its private gateway, and an agent's manifest label
//! `data: private` chooses it:
//!
//! - a model entry may carry `private_gateway`; a call of a private agent on
//!   that alias is sent with `cf-aig-gateway-id` set to it, and a standard
//!   agent's call keeps the provider's `cf-aig-gateway-id`;
//! - a private agent on an alias that names no `private_gateway` is refused
//!   with an error naming the alias, and no request is sent;
//! - the class is read on the orchestrator's side from the manifest of the
//!   execution record's agent, never from the container's `Generate`: the
//!   alias and the agent id the container sends cannot cross classes;
//! - the fallback alias is resolved in the caller's class;
//! - a judge, a child execution of a private execution, sends through the
//!   private gateway;
//! - configuration validation refuses an empty `private_gateway`.
//!
//! Every request goes to a local stand-in server (mockito) and is asserted on
//! its `cf-aig-gateway-id` header.

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::inner_loop_service::InnerLoopService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService;
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus};
use aegis_orchestrator_core::domain::dispatch::{AgentMessage, OrchestratorMessage};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::fsal::AegisFSAL;
use aegis_orchestrator_core::domain::llm::{ChatResponse, GenerationOptions, LLMError};
use aegis_orchestrator_core::domain::node_config::{
    LLMProviderConfig, LLMSelection, NodeConfigManifest,
};
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::security_context::{
    SecurityContext, SecurityContextMetadata, SecurityContextRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::llm::registry::DataClass;
use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use mockito::{Matcher, Mock, ServerGuard};
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

const GATEWAY_HEADER: &str = "cf-aig-gateway-id";
const PRODUCTION: &str = "inference-production";
const PRIVATE: &str = "inference-private";
const CONTEXT: &str = "private-gateway-test-context";

const ANSWER_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"an answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;
const WORKERS_AI_408_BODY: &str = r#"{"errors":[{"message":"AiError: AiError: Request timeout (df1159f0-0e44-40d5-b6ba-66bfec32483f)","code":3046}],"success":false,"result":{},"messages":[]}"#;

// ---------------------------------------------------------------------------
// The configuration and the stand-in
// ---------------------------------------------------------------------------

/// One `workers-ai` provider on `url` whose headers send
/// `cf-aig-gateway-id: inference-production`, with three aliases:
///
/// - `smart` (model `m-smart`): `private_gateway: inference-private`, and
///   `smart_extra` as further lines of its entry;
/// - `coder` (model `m-coder`): `coder_extra` as further lines;
/// - `plain` (model `m-plain`): no `private_gateway`.
fn providers(url: &str, smart_extra: &str, coder_extra: &str) -> String {
    format!(
        r#"- name: workers-ai
  type: openai-compatible
  endpoint: "{url}"
  api_key: "test-key"
  headers:
    cf-aig-gateway-id: "{PRODUCTION}"
  models:
    - alias: "smart"
      model: "m-smart"
      capabilities: ["chat"]
      context_window: 128000
      max_output_tokens: 1024
      private_gateway: "{PRIVATE}"
      {smart_extra}
    - alias: "coder"
      model: "m-coder"
      capabilities: ["chat"]
      context_window: 128000
      max_output_tokens: 1024
      {coder_extra}
    - alias: "plain"
      model: "m-plain"
      capabilities: ["chat"]
      context_window: 128000
      max_output_tokens: 1024
"#
    )
}

fn manifest_of(providers_yaml: &str) -> NodeConfigManifest {
    let providers: Vec<LLMProviderConfig> =
        serde_yaml::from_str(providers_yaml).expect("the providers block parses");
    let selection: LLMSelection =
        serde_yaml::from_str("max_retries: 2\nretry_delay_ms: 1\nllm_overall_timeout_secs: 60\n")
            .expect("the llm_selection block parses");
    let mut manifest = NodeConfigManifest::default();
    manifest.spec.node.id = "550e8400-e29b-41d4-a716-446655440000".to_string();
    manifest.spec.llm_providers = providers;
    manifest.spec.llm_selection = selection;
    manifest
}

fn registry_of(providers_yaml: &str) -> ProviderRegistry {
    let manifest = manifest_of(providers_yaml);
    manifest
        .validate()
        .expect("the configuration under test is valid");
    ProviderRegistry::from_config(&manifest).expect("the registry builds")
}

/// A stand-in answer to a request at `model` whose `cf-aig-gateway-id` is
/// `gateway`, expected `times` times.
async fn answers(server: &mut ServerGuard, model: &str, gateway: &str, times: usize) -> Mock {
    server
        .mock("POST", "/chat/completions")
        .match_header(GATEWAY_HEADER, gateway)
        .match_body(Matcher::PartialJson(json!({ "model": model })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(ANSWER_BODY)
        .expect(times)
        .create_async()
        .await
}

/// A stand-in that answers a request at `model` whose `cf-aig-gateway-id`
/// is `gateway` with Workers AI's time limit (408, code 3046).
async fn times_out(server: &mut ServerGuard, model: &str, gateway: &str, times: usize) -> Mock {
    server
        .mock("POST", "/chat/completions")
        .match_header(GATEWAY_HEADER, gateway)
        .match_body(Matcher::PartialJson(json!({ "model": model })))
        .with_status(408)
        .with_body(WORKERS_AI_408_BODY)
        .expect(times)
        .create_async()
        .await
}

/// Every request at `model`, whatever its headers, expected `times` times.
async fn any_request_at(server: &mut ServerGuard, model: &str, times: usize) -> Mock {
    server
        .mock("POST", Matcher::Any)
        .match_body(Matcher::PartialJson(json!({ "model": model })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(ANSWER_BODY)
        .expect(times)
        .create_async()
        .await
}

fn final_text(res: Result<ChatResponse, LLMError>) -> String {
    match res {
        Ok(ChatResponse::FinalText(r)) => r.text,
        other => panic!("expected the stand-in's answer, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The registry: one adapter per alias and class
// ---------------------------------------------------------------------------

/// A private call on `smart` carries `cf-aig-gateway-id: inference-private`;
/// a standard call on the same alias carries `inference-production`.
#[tokio::test]
async fn registry_private_call_carries_the_private_gateway_and_standard_the_production_one() {
    let mut server = mockito::Server::new_async().await;
    let private = answers(&mut server, "m-smart", PRIVATE, 1).await;
    let production = answers(&mut server, "m-smart", PRODUCTION, 1).await;
    let registry = registry_of(&providers(&server.url(), "", ""));

    let p = registry
        .generate_chat(
            "smart",
            DataClass::Private,
            &[],
            &[],
            &GenerationOptions::default(),
        )
        .await;
    let s = registry
        .generate_chat(
            "smart",
            DataClass::Standard,
            &[],
            &[],
            &GenerationOptions::default(),
        )
        .await;

    private.assert_async().await;
    production.assert_async().await;
    assert_eq!(final_text(p), "an answer");
    assert_eq!(final_text(s), "an answer");
}

/// The single-prompt entry point takes the class the same way.
#[tokio::test]
async fn registry_private_generate_carries_the_private_gateway() {
    let mut server = mockito::Server::new_async().await;
    let private = answers(&mut server, "m-smart", PRIVATE, 1).await;
    let production = answers(&mut server, "m-smart", PRODUCTION, 0).await;
    let registry = registry_of(&providers(&server.url(), "", ""));

    let res = registry
        .generate(
            "smart",
            DataClass::Private,
            "hello",
            &GenerationOptions::default(),
        )
        .await;

    private.assert_async().await;
    production.assert_async().await;
    assert!(res.is_ok(), "{res:?}");
}

/// A private call on an alias that names no `private_gateway` is refused
/// with an error naming the alias, and the stand-in receives no request.
#[tokio::test]
async fn registry_private_call_on_an_alias_without_private_gateway_is_refused_and_sends_nothing() {
    let mut server = mockito::Server::new_async().await;
    let none = any_request_at(&mut server, "m-plain", 0).await;
    let registry = registry_of(&providers(&server.url(), "", ""));

    let res = registry
        .generate_chat(
            "plain",
            DataClass::Private,
            &[],
            &[],
            &GenerationOptions::default(),
        )
        .await;
    let within = registry
        .generate_chat_within(
            "plain",
            DataClass::Private,
            &[],
            &[],
            &GenerationOptions::default(),
            std::time::Duration::from_secs(30),
            None,
        )
        .await;
    let single = registry
        .generate(
            "plain",
            DataClass::Private,
            "hello",
            &GenerationOptions::default(),
        )
        .await;

    none.assert_async().await;
    for err in [
        res.expect_err("a private call on `plain` must be refused")
            .to_string(),
        within
            .expect_err("a private call on `plain` must be refused")
            .to_string(),
        single
            .expect_err("a private call on `plain` must be refused")
            .to_string(),
    ] {
        assert!(
            err.contains("'plain'") && err.contains("private_gateway"),
            "the refusal must name the alias and the missing private_gateway: {err}"
        );
    }
}

/// After the provider's time limit on `smart`, a private call's fallback to
/// `coder` (which names `inference-private` too) carries the private
/// gateway, and a standard call's carries the production one.
#[tokio::test]
async fn registry_fallback_alias_keeps_the_class() {
    let mut server = mockito::Server::new_async().await;
    let smart_private = times_out(&mut server, "m-smart", PRIVATE, 1).await;
    let smart_production = times_out(&mut server, "m-smart", PRODUCTION, 1).await;
    let coder_private = answers(&mut server, "m-coder", PRIVATE, 1).await;
    let coder_production = answers(&mut server, "m-coder", PRODUCTION, 1).await;
    let registry = registry_of(&providers(
        &server.url(),
        r#"fallback_alias: "coder""#,
        &format!(r#"private_gateway: "{PRIVATE}""#),
    ));

    let p = registry
        .generate_chat(
            "smart",
            DataClass::Private,
            &[],
            &[],
            &GenerationOptions::default(),
        )
        .await;
    let s = registry
        .generate_chat(
            "smart",
            DataClass::Standard,
            &[],
            &[],
            &GenerationOptions::default(),
        )
        .await;

    smart_private.assert_async().await;
    smart_production.assert_async().await;
    coder_private.assert_async().await;
    coder_production.assert_async().await;
    assert_eq!(final_text(p), "an answer");
    assert_eq!(final_text(s), "an answer");
}

/// After the provider's time limit on `smart`, a private call whose fallback
/// alias `coder` names no `private_gateway` is refused naming `coder`, and
/// `coder` receives no request through any gateway.
#[tokio::test]
async fn registry_private_fallback_to_an_alias_without_private_gateway_is_refused_and_sends_nothing(
) {
    let mut server = mockito::Server::new_async().await;
    let smart_private = times_out(&mut server, "m-smart", PRIVATE, 1).await;
    let coder_none = any_request_at(&mut server, "m-coder", 0).await;
    let registry = registry_of(&providers(&server.url(), r#"fallback_alias: "coder""#, ""));

    let res = registry
        .generate_chat(
            "smart",
            DataClass::Private,
            &[],
            &[],
            &GenerationOptions::default(),
        )
        .await;

    smart_private.assert_async().await;
    coder_none.assert_async().await;
    let err = res
        .expect_err("a private fallback to `coder` must be refused")
        .to_string();
    assert!(
        err.contains("'coder'") && err.contains("private_gateway"),
        "the refusal must name the fallback alias and the missing private_gateway: {err}"
    );
}

// ---------------------------------------------------------------------------
// Configuration validation
// ---------------------------------------------------------------------------

#[test]
fn validation_refuses_an_empty_private_gateway() {
    for value in [r#""""#, r#""   ""#] {
        let manifest = manifest_of(&providers(
            "https://inference.example/v1",
            "",
            &format!("private_gateway: {value}"),
        ));
        let err = manifest
            .validate()
            .expect_err("an empty private_gateway must be refused")
            .to_string();
        assert!(
            err.contains("'coder'") && err.contains("private_gateway"),
            "the refusal must name the alias and the field: {err}"
        );
    }
    manifest_of(&providers("https://inference.example/v1", "", ""))
        .validate()
        .expect("a private_gateway with a value validates");
}

#[test]
fn validation_refuses_a_private_gateway_on_a_provider_that_sends_no_headers() {
    let manifest = manifest_of(&format!(
        r#"- name: claude
  type: anthropic
  endpoint: "https://api.anthropic.example/v1"
  api_key: "test-key"
  models:
    - alias: "smart"
      model: "m-smart"
      capabilities: ["chat"]
      context_window: 128000
      private_gateway: "{PRIVATE}"
"#
    ));
    let err = manifest
        .validate()
        .expect_err("a private_gateway the adapter cannot send must be refused")
        .to_string();
    assert!(
        err.contains("'smart'") && err.contains("private_gateway"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// The inner loop: the class from the manifest, on the orchestrator's side
// ---------------------------------------------------------------------------

fn agent(name: &str, labels: &[(&str, &str)]) -> Agent {
    let mut manifest: AgentManifest = serde_yaml::from_str(&format!(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: {name}
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
"#
    ))
    .unwrap();
    for (k, v) in labels {
        manifest
            .metadata
            .labels
            .insert((*k).to_string(), (*v).to_string());
    }
    Agent {
        id: AgentId::new(),
        tenant_id: TenantId::default(),
        scope: aegis_orchestrator_core::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn input() -> ExecutionInput {
    ExecutionInput {
        intent: None,
        input: json!({}),
        workspace_volume_id: None,
        workspace_volume_mount_path: None,
        workspace_remote_path: None,
        workflow_execution_id: None,
        attachments: Vec::new(),
    }
}

fn execution_of(agent: &Agent) -> Execution {
    let mut e = Execution::new_with_id(
        ExecutionId::new(),
        agent.id,
        input(),
        5,
        CONTEXT.to_string(),
    );
    e.tenant_id = TenantId::default();
    e
}

fn child_execution_of(agent: &Agent, parent: &Execution) -> Execution {
    let mut e = Execution::new_child(agent.id, input(), 5, parent).expect("a child execution");
    e.security_context_name = CONTEXT.to_string();
    e
}

struct World {
    inner_loop: InnerLoopService,
}

/// The inner loop as the daemon builds it, over `agents` and `executions`,
/// with the registry of `providers_yaml`.
async fn world(agents: Vec<Agent>, executions: Vec<Execution>, providers_yaml: &str) -> World {
    let agents = Arc::new(Agents(agents.into_iter().map(|a| (a.id, a)).collect()));
    let executions = Arc::new(Executions(
        executions.into_iter().map(|e| (e.id, e)).collect(),
    ));
    let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
    security_context_repo
        .save(SecurityContext {
            name: CONTEXT.to_string(),
            description: "private gateway test".to_string(),
            capabilities: vec![],
            deny_list: vec![],
            metadata: SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        })
        .await
        .unwrap();
    let storage_root = std::env::temp_dir().join(format!(
        "aegis-private-gateway-tests-{}",
        uuid::Uuid::new_v4()
    ));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let event_bus = Arc::new(EventBus::new(1024));
    let tools = Arc::new(ToolInvocationService::new(
        Arc::new(InMemorySealSessionRepository::new()),
        security_context_repo,
        Arc::new(SealMiddleware::new()),
        Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
        fsal,
        NfsVolumeRegistry::new(),
        agents,
        executions.clone(),
        Arc::new(
            aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(
            ),
        ),
        event_bus,
        None,
    ));
    World {
        inner_loop: InnerLoopService::new(tools, executions, Arc::new(registry_of(providers_yaml))),
    }
}

impl World {
    /// The container's `Generate` for `execution`, naming `agent_id` and
    /// `alias` as the container sends them.
    async fn generate(
        &self,
        execution: &Execution,
        agent_id: AgentId,
        alias: &str,
    ) -> Result<OrchestratorMessage> {
        self.inner_loop
            .handle_agent_message(AgentMessage::Generate {
                agent_id: agent_id.0.to_string(),
                execution_id: execution.id.0.to_string(),
                iteration_number: 1,
                prompt: "the task".to_string(),
                messages: Vec::new(),
                model_alias: alias.to_string(),
            })
            .await
    }
}

fn assert_final(res: Result<OrchestratorMessage>) {
    match res {
        Ok(OrchestratorMessage::Final { content, .. }) => assert_eq!(content, "an answer"),
        other => panic!("expected the stand-in's final answer, got {other:?}"),
    }
}

/// An agent labelled `data: private` sends through `inference-private`; an
/// agent without the label through `inference-production`.
#[tokio::test]
async fn inner_loop_private_agent_sends_through_the_private_gateway_and_standard_through_production(
) {
    let mut server = mockito::Server::new_async().await;
    let private = answers(&mut server, "m-smart", PRIVATE, 1).await;
    let production = answers(&mut server, "m-smart", PRODUCTION, 1).await;
    let private_agent = agent("mail-reader", &[("data", "private")]);
    let standard_agent = agent("copywriter", &[("role", "writer")]);
    let private_run = execution_of(&private_agent);
    let standard_run = execution_of(&standard_agent);
    let w = world(
        vec![private_agent.clone(), standard_agent.clone()],
        vec![private_run.clone(), standard_run.clone()],
        &providers(&server.url(), "", ""),
    )
    .await;

    assert_final(w.generate(&private_run, private_agent.id, "smart").await);
    assert_final(w.generate(&standard_run, standard_agent.id, "smart").await);

    private.assert_async().await;
    production.assert_async().await;
}

/// A private agent whose alias names no `private_gateway` is refused with an
/// error naming the alias, and the stand-in receives no request.
#[tokio::test]
async fn inner_loop_private_agent_on_an_alias_without_private_gateway_is_refused_and_sends_nothing()
{
    let mut server = mockito::Server::new_async().await;
    let none = any_request_at(&mut server, "m-plain", 0).await;
    let private_agent = agent("mail-reader", &[("data", "private")]);
    let run = execution_of(&private_agent);
    let w = world(
        vec![private_agent.clone()],
        vec![run.clone()],
        &providers(&server.url(), "", ""),
    )
    .await;

    let res = w.generate(&run, private_agent.id, "plain").await;

    none.assert_async().await;
    let err = format!(
        "{:#}",
        res.expect_err("a private agent on `plain` must be refused")
    );
    assert!(
        err.contains("'plain'") && err.contains("private_gateway"),
        "the refusal must name the alias and the missing private_gateway: {err}"
    );
}

/// What the container sends cannot cross classes: every alias a standard
/// agent's container names goes through the provider's gateway, and a
/// `Generate` naming a standard agent's id for a private agent's execution
/// still sends through the private gateway (and the reverse goes through
/// production), because the class is read from the execution record's agent.
#[tokio::test]
async fn inner_loop_the_container_sent_alias_and_agent_id_cannot_cross_classes() {
    let mut server = mockito::Server::new_async().await;
    // The standard agent names `smart` (which has a private gateway),
    // `coder` and `plain`: three requests, each through production.
    let smart_production = answers(&mut server, "m-smart", PRODUCTION, 2).await;
    let coder_production = answers(&mut server, "m-coder", PRODUCTION, 1).await;
    let plain_production = answers(&mut server, "m-plain", PRODUCTION, 1).await;
    // The private agent's execution, whose container names the standard
    // agent's id: through the private gateway.
    let smart_private = answers(&mut server, "m-smart", PRIVATE, 1).await;
    let private_agent = agent("mail-reader", &[("data", "private")]);
    let standard_agent = agent("copywriter", &[]);
    let private_run = execution_of(&private_agent);
    let standard_run = execution_of(&standard_agent);
    let w = world(
        vec![private_agent.clone(), standard_agent.clone()],
        vec![private_run.clone(), standard_run.clone()],
        &providers(&server.url(), "", ""),
    )
    .await;

    for alias in ["smart", "coder", "plain"] {
        assert_final(w.generate(&standard_run, standard_agent.id, alias).await);
    }
    // The container of the private execution names the standard agent.
    assert_final(w.generate(&private_run, standard_agent.id, "smart").await);
    // The container of the standard execution names the private agent.
    assert_final(w.generate(&standard_run, private_agent.id, "smart").await);

    smart_production.assert_async().await;
    coder_production.assert_async().await;
    plain_production.assert_async().await;
    smart_private.assert_async().await;
}

/// A judge, started as a child execution of a private agent's execution,
/// sends through the private gateway although its own manifest carries no
/// label: it reads the output it grades.
#[tokio::test]
async fn inner_loop_a_judge_child_of_a_private_execution_sends_through_the_private_gateway() {
    let mut server = mockito::Server::new_async().await;
    let private = answers(&mut server, "m-smart", PRIVATE, 1).await;
    let production = answers(&mut server, "m-smart", PRODUCTION, 1).await;
    let private_agent = agent("mail-reader", &[("data", "private")]);
    let standard_agent = agent("copywriter", &[]);
    let judge = agent("output-judge", &[("role", "judge")]);
    let private_run = execution_of(&private_agent);
    let standard_run = execution_of(&standard_agent);
    let judge_of_private = child_execution_of(&judge, &private_run);
    let judge_of_standard = child_execution_of(&judge, &standard_run);
    let w = world(
        vec![private_agent, standard_agent, judge.clone()],
        vec![
            private_run,
            standard_run,
            judge_of_private.clone(),
            judge_of_standard.clone(),
        ],
        &providers(&server.url(), "", ""),
    )
    .await;

    assert_final(w.generate(&judge_of_private, judge.id, "smart").await);
    assert_final(w.generate(&judge_of_standard, judge.id, "smart").await);

    private.assert_async().await;
    production.assert_async().await;
}

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

struct Executions(HashMap<ExecutionId, Execution>);

#[async_trait]
impl ExecutionService for Executions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
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
        _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
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

/// The agents, by id.
struct Agents(HashMap<AgentId, Agent>);

#[async_trait]
impl AgentLifecycleService for Agents {
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
    async fn get_agent_for_tenant(&self, _: &TenantId, id: AgentId) -> Result<Agent> {
        self.0
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("agent not found"))
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
        Ok(self.0.values().cloned().collect())
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        anyhow::bail!("not exercised")
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        anyhow::bail!("not exercised")
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
        Ok(self.0.values().cloned().collect())
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
impl aegis_orchestrator_core::domain::fsal::EventPublisher for NoOpPublisher {
    async fn publish_storage_event(
        &self,
        _event: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}
