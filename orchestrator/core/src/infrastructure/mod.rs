// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Infrastructure Layer (`aegis-core`)
//!
//! Implements the interfaces declared in `crate::domain` and `crate::application`
//! using real I/O: databases, container runtimes, NFS, LLM APIs, and event streaming.
//!
//! ## Module Map
//!
//! | Module | Contents | Key ADR |
//! |--------|----------|---------|
//! | [`repositories`] | `AgentRepository`, `ExecutionRepository`, `VolumeRepository` impls | ADR-025 |
//! | [`runtime`] | Docker runtime adapter implementing `AgentRuntime` trait | ADR-027 |
//! | [`image_manager`] | `DockerImageManager` trait + `StandardDockerImageManager`, `CredentialResolver` | ADR-045 |
//! | [`nfs`] | NFS Server Gateway: `AegisFSAL`, `NfsServer`, `AegisFileHandle` | ADR-036 |
//! | [`smcp`] | SMCP: attestation, envelope, middleware, policy engine, signature | ADR-035 |
//! | [`event_bus`] | In-memory pub/sub `EventBus` + `DomainEvent` unified enum | ADR-030 |
//! | [`llm`] | LLM provider adapters (OpenAI, Anthropic, Ollama) anti-corruption layer | ADR-009 |
//! | [`storage`] | `SeaweedFSAdapter` implementing `StorageProvider` | ADR-032 |
//! | [`security_context`] | `InMemorySecurityContextRepository` | ADR-035 |
//! | [`tool_router`] | `ToolRouter` MCP proxy + `InMemorySmcpSessionRepository` | ADR-033 |
//! | [`db`] | SQLx PostgreSQL connection pool | ADR-025 |
//! | [`workflow_parser`] | YAML → `Workflow` aggregate deserializer | ADR-015/031 |
//! | [`agent_manifest_parser`] | YAML → `AgentManifest` deserializer | — |
//! | [`prompt_template_engine`] | Handlebars template expansion for agent prompts | ADR-031 |
//! | [`context_loader`] | Loads `spec.context` items into agent prompts | — |
//! | [`temporal_client`] | Temporal.io workflow client (deferred) | ADR-022 |
//! | [`human_input_service`] | Suspends execution pending human response | ADR-015 |

//! | [`aegis_runtime_proto`] | Generated `aegis.runtime.v1` types shared by server + cortex client | ADR-042 |
//! | [`cortex_client`] | `CortexGrpcClient` — forwards Cortex RPCs to standalone `aegis-cortex` | ADR-042 |
//! | [`sensor`] | `SensorService` + `StdinSensor` — always-on stimulus listeners (ADR-021) |

pub mod aegis_runtime_proto;
pub mod agent_manifest_parser;
pub mod agentskills_client;
pub mod context_loader;
pub mod cortex_client;
pub mod db;
pub mod event_bus;
pub mod human_input_service;
pub mod image_manager;
pub mod llm;
pub mod nfs;
pub mod prompt_template_engine;
pub mod repositories;
pub mod runtime;
pub mod security_context;
pub mod sensor;
pub mod smcp;
pub mod storage;
pub mod temporal_client;
pub mod temporal_event_listener;
pub mod temporal_proto;
pub mod tool_router;
pub mod workflow_parser;

pub use cortex_client::CortexGrpcClient;
pub use human_input_service::{HumanInputService, HumanInputStatus, PendingRequestInfo};
pub use temporal_event_listener::{
    TemporalEventListener, TemporalEventMapper, TemporalEventPayload,
};
