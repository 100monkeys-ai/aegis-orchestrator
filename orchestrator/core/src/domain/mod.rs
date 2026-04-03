// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Domain Layer
//!
//! Pure business logic for AEGIS. This module contains all aggregates, value objects,
//! domain services, and repository *traits* (interfaces). It has **no I/O dependencies**
//! — no database calls, no HTTP, no filesystem access. Infrastructure implementations
//! live in [`crate::infrastructure`].
//!
//! ## Module Map
//!
//! | Module | Bounded Context | Description |
//! |---|---|---|
//! | [`agent`] | BC-1 Agent Lifecycle | `Agent` aggregate, `AgentManifest`, `AgentId` |
//! | [`execution`] | BC-2 Execution | `Execution` aggregate, `Iteration`, 100monkeys loop types |
//! | [`supervisor`] | BC-2 Execution | `Supervisor` domain service driving the iteration loop (ADR-005) |
//! | [`runtime`] | BC-2 Execution | `AgentRuntime` trait, `RuntimeConfig`, `InstanceId` |
//! | [`runtime_registry`] | BC-2 Execution | `StandardRuntimeRegistry` — certified language+version → image mapping (ADR-043) |
//! | [`policy`] | BC-4 Security Policy | `SecurityPolicy`, `NetworkPolicy`, `FilesystemPolicy`, `ResourceLimits` |
//! | [`security_context`] | BC-4/BC-12 SEAL | `SecurityContext` aggregate, `Capability` value object |
//! | [`seal_session`] | BC-12 SEAL | `SealSession` aggregate, `EnvelopeVerifier` trait |
//! | [`seal_session_repository`] | BC-12 SEAL | `SealSessionRepository` trait |
//! | [`workflow`] | BC-3 Workflow | `Workflow` FSM aggregate, `WorkflowState`, `Blackboard` (ADR-015) |
//! | [`workflow_registry`] | BC-8 Stimulus-Response | `WorkflowRegistry` aggregate root — routing table + RouterAgent ref (ADR-021) |
//! | [`stimulus`] | BC-8 Stimulus-Response | `Stimulus`, `StimulusId`, `StimulusSource`, `RoutingDecision` value objects (ADR-021) |
//! | [`volume`] | BC-7 Storage Gateway | `Volume` aggregate, `StorageClass`, `VolumeMount` (ADR-032) |
//! | [`storage`] | BC-7 Storage Gateway | `StorageProvider` trait, `OpenMode`, ACL for SeaweedFS |
//! | [`fsal`] | BC-7 Storage Gateway | `AegisFSAL` transport-agnostic security boundary (ADR-036) |
//! | [`path_sanitizer`] | BC-7 Storage Gateway | Path canonicalization and traversal-rejection |
//! | [`mcp`] | BC-12 SEAL / Tool Routing | `ToolServer`, `MCPError`, MCP integration types (ADR-033) |
//! | [`secrets`] | BC-11 Secrets & Identity | `SensitiveString`, `SecretPath`, `AccessContext`, `DomainDynamicSecret` (ADR-034) |
//! | [`shared_kernel`] | Shared Kernel | Cross-context identity types — DDD Shared Kernel pattern |
//! | [`discovery`] | BC-1/BC-3 Agent & Workflow Discovery | `DiscoveryQuery`, `DiscoveryResult`, `DiscoveryResponse` value objects (ADR-075) |
//! | [`env_guard`] | Cross-cutting | Environment variable isolation guard for execution contexts |
//! | [`events`] | Cross-cutting | All domain events — single catalog used by the event bus (ADR-030) |
//! | [`repository`] | Cross-cutting | Repository traits for all aggregate roots |
//! | [`validation`] | BC-2 Execution | `ValidationConfig`, gradient validation types (ADR-017) |
//! | [`llm`] | Cross-cutting | `LLMProvider` trait, LLM request/response value objects |
//! | [`node_config`] | Infrastructure config | `NodeConfigManifest` parsed from `aegis-config.yaml` |
//! | [`cluster`] | BC-7 Infrastructure & Hosting | `NodeCluster` aggregate, `NodePeer`, `NodeRouter` (ADR-059) |
//! | [`iam`] | BC-13 IAM & Identity Federation | `IdentityRealm`, `UserIdentity`, `IdentityProvider` trait (ADR-041) |

pub mod agent;
pub mod api_key;
pub mod api_scope;
pub mod cluster;
pub mod discovery;
pub mod dispatch;
pub mod env_guard;
pub mod events;
pub mod execution;
pub mod fsal;
pub mod iam;
pub mod llm;
pub mod mcp;
pub mod node_config;
pub mod path_sanitizer;
pub mod policy;
pub mod rate_limit;
pub mod repository;
pub mod runtime;
pub mod runtime_registry;
pub mod seal_session;
pub mod seal_session_repository;
pub mod secrets;
pub mod security_context;
pub mod shared_kernel;
pub mod stimulus;
pub mod storage;
pub mod supervisor;
pub mod tenancy;
pub mod tenant;
pub mod validation;
pub mod volume;
pub mod workflow;
pub mod workflow_registry;
