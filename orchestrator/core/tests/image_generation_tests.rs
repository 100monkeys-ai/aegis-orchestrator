// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An image generation model is an alias of the node's provider table with
//! the capability `image_generation` (AEGIS ADR-143 I1, on ADR-046 D1).
//!
//! Each test parses a provider block as an operator writes it in
//! `aegis-config.yaml`, builds the real `ProviderRegistry` from it, and, where
//! it calls the provider, calls a local stand-in of the OpenAI image
//! generation endpoint on a loopback port. No request leaves the loopback
//! interface.

use aegis_orchestrator_core::domain::node_config::{LLMProviderConfig, NodeConfigManifest};
use aegis_orchestrator_core::domain::vision::{ImageOptions, VisionError};
use aegis_orchestrator_core::infrastructure::agent_manifest_parser::AgentManifestParser;
use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use std::sync::{Arc, Mutex};

/// The first bytes of a PNG file, then a few more: what a provider's image
/// decodes to in these tests.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRfake-image";

/// The stand-in's record of every image request: its headers and its body.
type Seen = Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>;

/// What the stand-in answers to `/v1/images/generations`.
#[derive(Clone)]
enum Answer {
    /// `data[].b64_json`, one entry per requested image.
    Base64,
    /// `data[].url`, pointing at the stand-in's own `/files/<n>.png`.
    Url,
    /// The status and the body, as given.
    Error(u16, &'static str),
}

/// Serve `/v1/images/generations` and `/files/{n}` on a loopback port.
/// Returns the provider endpoint (`http://127.0.0.1:<port>/v1`) and the
/// record.
async fn stand_in(answer: Answer) -> (String, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("listener address");
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = Router::new()
        .route(
            "/v1/images/generations",
            post(
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let record = record.clone();
                    let answer = answer.clone();
                    async move {
                        let n = body["n"].as_u64().unwrap_or(1);
                        record.lock().unwrap().push((headers, body));
                        let data: Vec<serde_json::Value> = (0..n)
                        .map(|i| match answer {
                            Answer::Url => {
                                serde_json::json!({"url": format!("http://{addr}/files/{i}.png")})
                            }
                            _ => serde_json::json!({
                                "b64_json": base64::engine::general_purpose::STANDARD.encode(PNG)
                            }),
                        })
                        .collect();
                        match answer {
                            Answer::Error(status, text) => (
                                StatusCode::from_u16(status).unwrap(),
                                Json(serde_json::json!({"error": {"message": text}})),
                            ),
                            _ => (
                                StatusCode::OK,
                                Json(serde_json::json!({"created": 1, "data": data})),
                            ),
                        }
                    }
                },
            ),
        )
        .route("/files/{name}", get(|| async { PNG.to_vec() }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve stand-in");
    });
    (format!("http://{addr}/v1"), seen)
}

/// A node with one provider of `provider_type` at `endpoint`: the language
/// model `default` and the image generation model `artist`.
fn node(provider_type: &str, endpoint: &str) -> NodeConfigManifest {
    let providers: Vec<LLMProviderConfig> = serde_yaml::from_str(&format!(
        r#"- name: images
  type: {provider_type}
  endpoint: "{endpoint}"
  api_key: "test-key"
  models:
    - alias: "default"
      model: "m-chat"
      capabilities: ["chat"]
      context_window: 8192
    - alias: "artist"
      model: "m-image"
      capabilities: ["image_generation"]
      context_window: 0
"#
    ))
    .expect("the providers block parses");
    let mut manifest = NodeConfigManifest::default();
    manifest.spec.llm_providers = providers;
    manifest
}

fn registry(manifest: &NodeConfigManifest) -> ProviderRegistry {
    ProviderRegistry::from_config(manifest).expect("registry builds")
}

#[test]
fn an_image_generation_alias_binds_an_image_provider_and_no_language_model() {
    for provider_type in ["openai", "openai-compatible"] {
        let registry = registry(&node(provider_type, "http://127.0.0.1:9/v1"));
        assert_eq!(
            registry.image_generation_aliases(),
            vec!["artist".to_string()],
            "{provider_type}: the node's image generation aliases"
        );
        assert!(
            registry.image_provider("artist").is_some(),
            "{provider_type}: 'artist' has no image provider"
        );
        assert!(
            !registry.has_alias("artist"),
            "{provider_type}: 'artist' is bound as a language model"
        );
        assert!(
            registry.has_alias("default") && registry.image_provider("default").is_none(),
            "{provider_type}: 'default' is not a language model only"
        );
    }
}

#[test]
fn a_node_with_no_image_generation_alias_has_none() {
    let mut manifest = node("openai-compatible", "http://127.0.0.1:9/v1");
    manifest.spec.llm_providers[0].models.pop();
    let registry = registry(&manifest);
    assert!(registry.image_generation_aliases().is_empty());
    assert!(registry.image_provider("artist").is_none());
}

#[tokio::test]
async fn the_adapter_posts_to_images_generations_and_decodes_b64_json() {
    let (endpoint, seen) = stand_in(Answer::Base64).await;
    let registry = registry(&node("openai-compatible", &endpoint));
    let provider = registry
        .image_provider("artist")
        .expect("an image provider");

    let answer = provider
        .generate_image(
            "a lighthouse at dusk",
            &ImageOptions {
                size: Some("512x512".to_string()),
                count: 2,
            },
        )
        .await
        .expect("the provider answers");

    assert_eq!(answer.images.len(), 2);
    for image in &answer.images {
        assert_eq!(image.bytes, PNG);
        assert_eq!(image.content_type, "image/png");
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one request");
    let (headers, body) = &seen[0];
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some("Bearer test-key")
    );
    assert_eq!(body["model"], "m-image");
    assert_eq!(body["prompt"], "a lighthouse at dusk");
    assert_eq!(body["n"], 2);
    assert_eq!(body["size"], "512x512");
}

#[tokio::test]
async fn an_answer_by_url_is_fetched_into_bytes() {
    let (endpoint, _) = stand_in(Answer::Url).await;
    let registry = registry(&node("openai", &endpoint));
    let provider = registry
        .image_provider("artist")
        .expect("an image provider");

    let answer = provider
        .generate_image("a red bicycle", &ImageOptions::default())
        .await
        .expect("the provider answers");

    assert_eq!(answer.images.len(), 1);
    assert_eq!(answer.images[0].bytes, PNG);
    assert_eq!(answer.images[0].content_type, "image/png");
}

#[tokio::test]
async fn a_provider_400_is_invalid_input_with_its_message() {
    let (endpoint, _) = stand_in(Answer::Error(400, "size is not supported")).await;
    let registry = registry(&node("openai-compatible", &endpoint));
    let provider = registry
        .image_provider("artist")
        .expect("an image provider");

    let error = provider
        .generate_image("a red bicycle", &ImageOptions::default())
        .await
        .expect_err("the provider refuses");

    match error {
        VisionError::InvalidInput(message) => assert!(
            message.contains("size is not supported"),
            "the provider's message is lost: {message}"
        ),
        other => panic!("not InvalidInput: {other:?}"),
    }
}

fn agent_yaml(model: &str) -> String {
    format!(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: painter
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    model: "{model}"
  task:
    instruction: "Paint something"
"#
    )
}

#[test]
fn an_agent_whose_model_generates_images_is_refused_at_parse() {
    let image_aliases =
        registry(&node("openai-compatible", "http://127.0.0.1:9/v1")).image_generation_aliases();

    let refusal = AgentManifestParser::parse_yaml_for_node(&agent_yaml("artist"), &image_aliases)
        .expect_err("the manifest is refused");
    assert!(
        refusal
            .to_string()
            .contains("'artist' generates images; an agent's model must be a language model"),
        "the refusal reads: {refusal}"
    );

    let manifest = AgentManifestParser::parse_yaml_for_node(&agent_yaml("default"), &image_aliases)
        .expect("a language model's manifest parses");
    assert_eq!(manifest.spec.runtime.model, "default");
}

// ===========================================================================
// `aegis.image.generate` (AEGIS ADR-143 I2)
// ===========================================================================

mod tool {
    use super::PNG;
    use aegis_orchestrator_core::application::agent::AgentLifecycleService;
    use aegis_orchestrator_core::application::execution::ExecutionService;
    use aegis_orchestrator_core::application::file_operations_service::FileOperationsService;
    use aegis_orchestrator_core::application::nfs_gateway::{
        NfsVolumeRegistry, VolumeRegistration,
    };
    use aegis_orchestrator_core::application::tool_invocation_service::{
        ToolInvocationResult, ToolInvocationService,
    };
    use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
    use aegis_orchestrator_core::application::volume_manager::VolumeService;
    use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus};
    use aegis_orchestrator_core::domain::events::{ExecutionEvent, StorageEvent};
    use aegis_orchestrator_core::domain::execution::{
        Execution, ExecutionId, ExecutionInput, Iteration,
    };
    use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher, FsalAccessPolicy};
    use aegis_orchestrator_core::domain::iam::ZaruTier;
    use aegis_orchestrator_core::domain::mcp::ToolInputContract;
    use aegis_orchestrator_core::domain::repository::{
        AgentVersion, ExecutionRepository, VolumeRepository,
    };
    use aegis_orchestrator_core::domain::runtime::InstanceId;
    use aegis_orchestrator_core::domain::seal_session::{
        EnvelopeVerifier, SealSession, SealSessionError,
    };
    use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
    use aegis_orchestrator_core::domain::secrets::SensitiveString;
    use aegis_orchestrator_core::domain::security_context::capability::Capability;
    use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
    use aegis_orchestrator_core::domain::security_context::{
        SecurityContext, SecurityContextMetadata,
    };
    use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
    use aegis_orchestrator_core::domain::storage::{
        DirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider,
    };
    use aegis_orchestrator_core::domain::vision::{
        Image, ImageGenerationModels, ImageOptions, ImageResponse, Mask, VisionError,
        VisionProvider,
    };
    use aegis_orchestrator_core::domain::volume::{
        AccessMode, StorageClass, StorageTierLimits, Volume, VolumeBackend, VolumeMount,
        VolumeOwnership,
    };
    use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
    use aegis_orchestrator_core::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
    };
    use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
    use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
    use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
    use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
    use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
    use anyhow::Result;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    const TOOL: &str = "aegis.image.generate";
    const PERSON: &str = "image-person";
    const CONTEXT: &str = "zaru-pro";

    // --- the provider, faked: no network -----------------------------------

    /// The node's image models: `artist` and `sketcher`, each answering
    /// `count` copies of [`PNG`] and recording the calls.
    #[derive(Default)]
    struct FakeImages {
        calls: Mutex<Vec<(String, String, ImageOptions)>>,
        none: bool,
    }

    struct FakeProvider {
        alias: String,
        images: Arc<FakeImages>,
    }

    #[async_trait]
    impl VisionProvider for FakeProvider {
        async fn generate_image(
            &self,
            prompt: &str,
            options: &ImageOptions,
        ) -> Result<ImageResponse, VisionError> {
            self.images.calls.lock().unwrap().push((
                self.alias.clone(),
                prompt.to_string(),
                options.clone(),
            ));
            Ok(ImageResponse {
                images: (0..options.count)
                    .map(|_| Image {
                        bytes: PNG.to_vec(),
                        content_type: "image/png".to_string(),
                    })
                    .collect(),
            })
        }
        async fn edit_image(
            &self,
            _: &Image,
            _: &str,
            _: Option<&Mask>,
        ) -> Result<ImageResponse, VisionError> {
            Err(VisionError::Unsupported("not exercised".to_string()))
        }
    }

    struct Models(Arc<FakeImages>);

    impl ImageGenerationModels for Models {
        fn image_generation_aliases(&self) -> Vec<String> {
            if self.0.none {
                return Vec::new();
            }
            vec!["artist".to_string(), "sketcher".to_string()]
        }
        fn image_provider(&self, alias: &str) -> Option<Arc<dyn VisionProvider>> {
            self.image_generation_aliases()
                .iter()
                .any(|a| a == alias)
                .then(|| {
                    Arc::new(FakeProvider {
                        alias: alias.to_string(),
                        images: self.0.clone(),
                    }) as Arc<dyn VisionProvider>
                })
        }
    }

    // --- the services the dispatch reads ----------------------------------

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
        async fn get_execution_for_tenant(
            &self,
            _: &TenantId,
            id: ExecutionId,
        ) -> Result<Execution> {
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

    struct OneAgent(Agent);

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

    /// The host directory storage, creating a directory that exists as the
    /// production store does (SeaweedFS's `/dir/` creates each ancestor
    /// whether or not it is there), where `LocalHostStorageProvider` refuses
    /// it: the FSAL creates a file's parent before every file, so a second
    /// file in one directory would be refused here and not in production.
    struct Storage(LocalHostStorageProvider);

    #[async_trait]
    impl StorageProvider for Storage {
        async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
            match self.0.create_directory(path).await {
                Err(StorageError::AlreadyExists(_)) => Ok(()),
                other => other,
            }
        }
        async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
            self.0.delete_directory(path).await
        }
        async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
            self.0.set_quota(path, bytes).await
        }
        async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
            self.0.get_usage(path).await
        }
        async fn health_check(&self) -> Result<(), StorageError> {
            self.0.health_check().await
        }
        async fn open_file(&self, path: &str, mode: OpenMode) -> Result<FileHandle, StorageError> {
            self.0.open_file(path, mode).await
        }
        async fn read_at(
            &self,
            handle: &FileHandle,
            offset: u64,
            length: usize,
        ) -> Result<Vec<u8>, StorageError> {
            self.0.read_at(handle, offset, length).await
        }
        async fn write_at(
            &self,
            handle: &FileHandle,
            offset: u64,
            data: &[u8],
        ) -> Result<usize, StorageError> {
            self.0.write_at(handle, offset, data).await
        }
        async fn close_file(&self, handle: &FileHandle) -> Result<(), StorageError> {
            self.0.close_file(handle).await
        }
        async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
            self.0.stat(path).await
        }
        async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
            self.0.readdir(path).await
        }
        async fn create_file(&self, path: &str, mode: u32) -> Result<FileHandle, StorageError> {
            self.0.create_file(path, mode).await
        }
        async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
            self.0.delete_file(path).await
        }
        async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
            self.0.rename(from, to).await
        }
    }

    struct NoStorageEvents;

    #[async_trait]
    impl EventPublisher for NoStorageEvents {
        async fn publish_storage_event(&self, _event: StorageEvent) {}
    }

    /// Volumes as the storage layer makes them: host directories under
    /// `/hosts/<id>` in the scratch root the file services serve.
    struct HostVolumes {
        repo: Arc<InMemoryVolumeRepository>,
    }

    #[async_trait]
    impl VolumeService for HostVolumes {
        async fn create_volume(
            &self,
            name: String,
            tenant_id: TenantId,
            storage_class: StorageClass,
            size_limit_mb: u64,
            ownership: VolumeOwnership,
        ) -> anyhow::Result<VolumeId> {
            let id = VolumeId::new();
            let mut volume = Volume::new(
                name,
                tenant_id,
                storage_class,
                VolumeBackend::HostPath {
                    path: PathBuf::from(format!("/hosts/{id}")),
                },
                size_limit_mb * 1024 * 1024,
                ownership,
            )?;
            volume.id = id;
            volume.mark_available()?;
            self.repo.save(&volume).await?;
            Ok(id)
        }
        async fn get_volume(&self, id: VolumeId) -> anyhow::Result<Volume> {
            self.repo
                .find_by_id(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("not found"))
        }
        async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
            Ok(self.repo.find_by_tenant(tenant_id).await?)
        }
        async fn list_volumes_by_ownership(
            &self,
            ownership: &VolumeOwnership,
        ) -> anyhow::Result<Vec<Volume>> {
            Ok(self.repo.find_by_ownership(ownership).await?)
        }
        async fn attach_volume(
            &self,
            _: VolumeId,
            _: InstanceId,
            _: PathBuf,
            _: AccessMode,
        ) -> anyhow::Result<VolumeMount> {
            unimplemented!()
        }
        async fn detach_volume(&self, _: VolumeId, _: InstanceId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_volume(&self, _: VolumeId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn get_volume_usage(&self, _: VolumeId) -> anyhow::Result<u64> {
            Ok(0)
        }
        async fn cleanup_expired_volumes(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn create_volumes_for_execution(
            &self,
            _: ExecutionId,
            _: TenantId,
            _: &[aegis_orchestrator_core::domain::agent::VolumeSpec],
            _: &str,
        ) -> anyhow::Result<Vec<Volume>> {
            Ok(vec![])
        }
        async fn persist_external_volume(
            &self,
            _: VolumeId,
            _: String,
            _: TenantId,
            _: String,
            _: u64,
            _: VolumeOwnership,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A call as `POST /v1/seal/invoke` receives it.
    struct Envelope {
        token: SensitiveString,
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
            Some(TOOL.to_string())
        }
        fn extract_arguments(&self) -> Option<Value> {
            Some(self.arguments.clone())
        }
        fn replay_nonce(&self) -> String {
            uuid::Uuid::new_v4().to_string()
        }
    }

    fn context() -> SecurityContext {
        SecurityContext {
            name: CONTEXT.to_string(),
            description: "image generation test".to_string(),
            capabilities: vec![Capability {
                tool_pattern: "aegis.*".to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            }],
            deny_list: vec![],
            metadata: SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        }
    }

    fn agent() -> Agent {
        let manifest: AgentManifest = serde_yaml::from_str(
            r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: image-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: ["aegis.image.generate", "aegis.agent.create", "aegis.agent.update"]
"#,
        )
        .unwrap();
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

    struct Fixture {
        service: ToolInvocationService,
        images: Arc<FakeImages>,
        agent_id: AgentId,
        run: ExecutionId,
        run_volume: VolumeId,
        executions: Arc<InMemoryExecutionRepository>,
        files: Arc<FileOperationsService>,
        user_volumes: Arc<UserVolumeService>,
        fsal: Arc<AegisFSAL>,
        token: String,
        _root: tempfile::TempDir,
    }

    /// A tool service with the node's image models (none when `no_models`),
    /// one agent run on its iteration 1 with a workspace volume at
    /// `/workspace`, and a conversation's session of [`PERSON`] with no
    /// execution record.
    async fn fixture(images: FakeImages, with_models: bool) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let volumes = Arc::new(InMemoryVolumeRepository::new());
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(Storage(LocalHostStorageProvider::new(root.path()).unwrap())),
            volumes.clone() as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoStorageEvents),
        ));
        let event_bus = Arc::new(EventBus::new(64));
        let tenant = TenantId::default();

        // The agent's run, on its first iteration.
        let agent = agent();
        let agent_id = agent.id;
        let mut run = Execution::new_with_id(
            ExecutionId::new(),
            agent_id,
            ExecutionInput {
                intent: None,
                input: json!({}),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            5,
            CONTEXT.to_string(),
        );
        run.tenant_id = tenant.clone();
        run.initiating_user_sub = Some(PERSON.to_string());
        run.start();
        run.start_iteration("act".to_string()).unwrap();
        let executions = Arc::new(InMemoryExecutionRepository::new());
        executions.save_for_tenant(&tenant, &run).await.unwrap();

        // Its workspace volume, registered as the run's at /workspace.
        let host = HostVolumes {
            repo: volumes.clone(),
        };
        let run_volume = host
            .create_volume(
                "workspace".to_string(),
                tenant.clone(),
                StorageClass::persistent(),
                64,
                VolumeOwnership::Execution {
                    execution_id: run.id,
                },
            )
            .await
            .unwrap();
        let mut volume = volumes.find_by_id(run_volume).await.unwrap().unwrap();
        volume.mark_attached().unwrap();
        volumes.save(&volume).await.unwrap();
        let registry = NfsVolumeRegistry::new();
        registry.register(VolumeRegistration {
            volume_id: run_volume,
            execution_id: run.id,
            workflow_execution_id: None,
            container_uid: 1000,
            container_gid: 1000,
            policy: FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: vec!["/*".to_string()],
            },
            mount_point: PathBuf::from("/workspace"),
            remote_path: String::new(),
        });

        // The conversation's session: an execution id with no record.
        let sessions = Arc::new(InMemorySealSessionRepository::new());
        let token = format!("token-{}", uuid::Uuid::new_v4());
        let session = SealSession::new(
            AgentId::new(),
            ExecutionId::new(),
            vec![],
            token.clone(),
            context(),
            tenant.clone(),
        )
        .with_principal_metadata(
            Some(PERSON.to_string()),
            Some(PERSON.to_string()),
            None,
            None,
        );
        sessions.save(session).await.unwrap();
        let contexts = Arc::new(InMemorySecurityContextRepository::new());
        contexts.save(context()).await.unwrap();

        let files = Arc::new(FileOperationsService::new(
            fsal.clone(),
            executions.clone() as Arc<dyn ExecutionRepository>,
            Arc::new(InMemoryAgentRepository::new()),
        ));
        let user_volumes = Arc::new(UserVolumeService::new(
            volumes.clone() as Arc<dyn VolumeRepository>,
            Arc::new(HostVolumes {
                repo: volumes.clone(),
            }),
            event_bus.clone(),
            StorageTierLimits::default(),
        ));
        let images = Arc::new(images);
        let mut service = ToolInvocationService::new(
            sessions,
            contexts,
            Arc::new(SealMiddleware::new()),
            Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
            fsal.clone(),
            registry,
            Arc::new(OneAgent(agent)),
            Arc::new(Executions(HashMap::from([(run.id, run.clone())]))),
            Arc::new(
                aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(),
            ),
            event_bus,
            None,
        )
        .with_execution_repository(executions.clone())
        .with_file_operations_service(files.clone())
        .with_user_volume_service(user_volumes.clone());
        if with_models {
            service = service.with_image_generation(Arc::new(Models(images.clone())));
        }
        Fixture {
            service,
            images,
            agent_id,
            run: run.id,
            run_volume,
            executions,
            files,
            user_volumes,
            fsal,
            token,
            _root: root,
        }
    }

    impl Fixture {
        /// The call inside the agent's run, on its iteration 1.
        async fn in_run(&self, args: Value) -> Result<Value, SealSessionError> {
            match self
                .service
                .invoke_tool_internal(
                    &self.agent_id,
                    self.run,
                    TenantId::default(),
                    1,
                    Vec::new(),
                    TOOL.to_string(),
                    args,
                )
                .await?
            {
                ToolInvocationResult::Direct(value) => Ok(value),
                other => panic!("the call was not answered directly: {other:?}"),
            }
        }

        /// A call of `tool` inside the agent's run, on its iteration 1.
        async fn call_in_run(&self, tool: &str, args: Value) -> Result<Value, SealSessionError> {
            match self
                .service
                .invoke_tool_internal(
                    &self.agent_id,
                    self.run,
                    TenantId::default(),
                    1,
                    Vec::new(),
                    tool.to_string(),
                    args,
                )
                .await?
            {
                ToolInvocationResult::Direct(value) => Ok(value),
                other => panic!("the call was not answered directly: {other:?}"),
            }
        }

        /// The call out of band, from the person's conversation.
        async fn in_chat(&self, args: Value) -> Result<Value, SealSessionError> {
            self.service
                .invoke_tool(&Envelope {
                    token: self.token.clone().into(),
                    arguments: args,
                })
                .await
        }
    }

    /// The `files` of an answer, wherever the invoke result carries it.
    fn files_of(answer: &Value) -> Vec<Value> {
        answer
            .get("files")
            .or_else(|| answer.get("result").and_then(|r| r.get("files")))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| panic!("the answer carries no files: {answer}"))
    }

    fn refusal(result: Result<Value, SealSessionError>) -> String {
        match result {
            Ok(answer) => format!("answered {answer}"),
            Err(e) => e.to_string(),
        }
    }

    #[tokio::test]
    async fn aegis_image_generate_is_listed_requiring_prompt_and_ungated() {
        let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
        let tools = router.list_tools().await.unwrap();
        let mut wrong = Vec::new();
        match tools.iter().find(|t| t.name == TOOL) {
            Some(tool) => {
                let schema = &tool.input_schema;
                if schema["required"] != json!(["prompt"]) {
                    wrong.push(format!("required: {}", schema["required"]));
                }
                for (property, kind) in [
                    ("prompt", "string"),
                    ("alias", "string"),
                    ("size", "string"),
                    ("count", "integer"),
                ] {
                    if schema["properties"][property]["type"] != kind {
                        wrong.push(format!("{property} is not a {kind}: {schema}"));
                    }
                }
                if schema["properties"]["count"]["minimum"] != 1
                    || schema["properties"]["count"]["maximum"] != 4
                {
                    wrong.push(format!("count is not 1 to 4: {schema}"));
                }
            }
            None => wrong.push(format!("{TOOL} is not listed")),
        }
        if ToolInputContract::required_fields(TOOL) != ["prompt"] {
            wrong.push(format!(
                "the input contract requires {:?}",
                ToolInputContract::required_fields(TOOL)
            ));
        }
        if router.requires_approval(TOOL) {
            wrong.push(format!("{TOOL} is gated"));
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test]
    async fn in_a_run_each_image_is_a_file_of_its_volume_and_a_produced_file_of_its_iteration() {
        let f = fixture(FakeImages::default(), true).await;
        let answer = f
            .in_run(json!({"prompt": "a lighthouse", "count": 2}))
            .await
            .expect("the run's call answers");
        let files = files_of(&answer);
        let expected: Vec<Value> = (1..=2)
            .map(|n| {
                json!({
                    "volume_id": f.run_volume.to_string(),
                    "path": format!("/workspace/generated/{n}.png"),
                    "content_type": "image/png",
                    "size_bytes": PNG.len(),
                })
            })
            .collect();
        assert_eq!(files, expected, "the answer's files");
        assert!(
            !answer.to_string().contains("http"),
            "the answer names a URL: {answer}"
        );

        // The bytes are in the run's volume.
        for n in 1..=2 {
            let handle = aegis_orchestrator_core::domain::fsal::AegisFileHandle::new(
                f.run,
                f.run_volume,
                "/",
            );
            let policy = FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: vec![],
            };
            let bytes = f
                .fsal
                .read(&handle, &format!("/generated/{n}.png"), &policy, 0, 1 << 20)
                .await
                .expect("the image is in the run's volume");
            assert_eq!(bytes, PNG, "image {n}'s bytes");
        }

        // Each is a produced file of iteration 1.
        let run = f
            .executions
            .find_by_id_for_tenant(&TenantId::default(), f.run)
            .await
            .unwrap()
            .unwrap();
        let produced: Vec<(String, u64, String)> = run.iterations()[0]
            .produced_files
            .iter()
            .map(|p| (p.path.clone(), p.size_bytes, p.content_type.clone()))
            .collect();
        assert_eq!(
            produced,
            (1..=2)
                .map(|n| (
                    format!("/workspace/generated/{n}.png"),
                    PNG.len() as u64,
                    "image/png".to_string()
                ))
                .collect::<Vec<_>>(),
            "the iteration's produced files"
        );

        // The default alias is the node's first; the prompt reaches it.
        let calls = f.images.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "artist");
        assert_eq!(calls[0].1, "a lighthouse");
        assert_eq!(calls[0].2.count, 2);
    }

    #[tokio::test]
    async fn out_of_band_each_image_is_a_file_under_generated_of_the_persons_attachments() {
        let f = fixture(FakeImages::default(), true).await;
        let answer = f
            .in_chat(json!({"prompt": "a red bicycle", "alias": "sketcher", "size": "512x512"}))
            .await
            .expect("the conversation's call answers");
        let files = files_of(&answer);
        assert_eq!(files.len(), 1, "one image: {answer}");
        let attachments = f
            .user_volumes
            .find_or_provision_chat_attachments(&TenantId::default(), PERSON, &ZaruTier::Free)
            .await
            .unwrap();
        let file = &files[0];
        assert_eq!(file["volume_id"], attachments.to_string());
        assert_eq!(file["content_type"], "image/png");
        assert_eq!(file["size_bytes"], PNG.len());
        let path = file["path"].as_str().expect("a path");
        assert!(path.starts_with("generated/"), "the path is {path}");
        assert!(
            !answer.to_string().contains("http"),
            "the answer names a URL: {answer}"
        );
        let read = f
            .files
            .read_file(&attachments, &TenantId::default(), PERSON, path)
            .await
            .expect("the image is in the person's attachments");
        assert_eq!(read.data, PNG);

        let calls = f.images.calls.lock().unwrap();
        assert_eq!(calls[0].0, "sketcher");
        assert_eq!(calls[0].2.size.as_deref(), Some("512x512"));
        assert_eq!(calls[0].2.count, 1);
    }

    #[tokio::test]
    async fn a_node_with_no_image_generation_model_answers_so() {
        for with_models in [false, true] {
            let f = fixture(
                FakeImages {
                    none: true,
                    ..Default::default()
                },
                with_models,
            )
            .await;
            let told = refusal(f.in_run(json!({"prompt": "a cat"})).await);
            assert!(
                told.contains("this node has no image generation model"),
                "with the models attached {with_models}: {told}"
            );
        }
    }

    #[tokio::test]
    async fn a_count_outside_1_to_4_and_an_alias_that_is_no_image_model_are_refused() {
        let f = fixture(FakeImages::default(), true).await;
        for count in [0, 5] {
            let told = refusal(f.in_run(json!({"prompt": "a cat", "count": count})).await);
            assert!(told.contains("'count' is 1 to 4"), "count {count}: {told}");
        }
        let told = refusal(f.in_run(json!({"prompt": "a cat", "alias": "smart"})).await);
        assert!(
            told.contains("'smart' is not an image generation model; the image generation models are artist, sketcher"),
            "{told}"
        );
        assert!(
            f.images.calls.lock().unwrap().is_empty(),
            "a refused call reached the provider"
        );
    }

    /// `aegis.agent.create` and `aegis.agent.update` refuse a manifest whose
    /// model is one of the node's image generation aliases, before anything
    /// is deployed, with I1's sentence.
    #[tokio::test]
    async fn agent_create_and_update_refuse_an_image_alias_as_the_agents_model() {
        let f = fixture(FakeImages::default(), true).await;
        let manifest = r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: painter
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    model: "artist"
  task:
    instruction: "Paint something"
"#;
        let sentence = "'artist' generates images; an agent's model must be a language model";
        let mut wrong = Vec::new();
        for tool in ["aegis.agent.create", "aegis.agent.update"] {
            let told = match f
                .call_in_run(tool, json!({"manifest_yaml": manifest}))
                .await
            {
                Ok(answer) => answer.to_string(),
                Err(e) => e.to_string(),
            };
            if !told.contains(sentence) {
                wrong.push(format!("{tool} answered: {told}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
