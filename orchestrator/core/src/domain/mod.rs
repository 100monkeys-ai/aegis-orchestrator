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
//! | [`policy`] | BC-4 Security Policy | `SecurityPolicy`, `NetworkPolicy`, `FilesystemPolicy`, `ResourceLimits` |
//! | [`security_context`] | BC-4/BC-12 SMCP | `SecurityContext` aggregate, `Capability` value object |
//! | [`smcp_session`] | BC-12 SMCP | `SmcpSession` aggregate, `EnvelopeVerifier` trait |
//! | [`smcp_session_repository`] | BC-12 SMCP | `SmcpSessionRepository` trait |
//! | [`workflow`] | BC-3 Workflow | `Workflow` FSM aggregate, `WorkflowState`, `Blackboard` (ADR-015) |
//! | [`volume`] | BC-7 Storage Gateway | `Volume` aggregate, `StorageClass`, `VolumeMount` (ADR-032) |
//! | [`storage`] | BC-7 Storage Gateway | `StorageProvider` trait, `OpenMode`, ACL for SeaweedFS |
//! | [`fsal`] | BC-7 Storage Gateway | `AegisFSAL` transport-agnostic security boundary (ADR-036) |
//! | [`path_sanitizer`] | BC-7 Storage Gateway | Path canonicalization and traversal-rejection |
//! | [`mcp`] | BC-12 SMCP / Tool Routing | `ToolServer`, `MCPError`, MCP integration types (ADR-033) |
//! | [`events`] | Cross-cutting | All domain events — single catalog used by the event bus (ADR-030) |
//! | [`repository`] | Cross-cutting | Repository traits for all aggregate roots |
//! | [`validation`] | BC-2 Execution | `ValidationConfig`, gradient validation types (ADR-017) |
//! | [`llm`] | Cross-cutting | `LLMProvider` trait, LLM request/response value objects |
//! | [`node_config`] | Infrastructure config | `NodeConfigManifest` parsed from `aegis-config.yaml` |

pub mod agent;
pub mod execution;
pub mod policy;
pub mod events;
pub mod runtime;
pub mod supervisor;
pub mod node_config;
pub mod repository;
pub mod llm;
pub mod workflow;
pub mod validation;
pub mod volume;
pub mod storage;
pub mod path_sanitizer;
pub mod fsal;
pub mod mcp;
pub mod security_context;
pub mod smcp_session;
pub mod smcp_session_repository;
