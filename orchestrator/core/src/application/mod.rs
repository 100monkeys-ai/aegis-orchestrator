// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Application Layer
//!
//! Use-cases and application services that orchestrate domain objects, repositories,
//! and infrastructure. This layer is the boundary between the domain and the outside
//! world: it manages transactions, publishes domain events, and coordinates across
//! bounded contexts.
//!
//! ## Module Map
//!
//! | Module | Bounded Context | Description |
//! |---|---|---|
//! | [`agent`] | BC-1 Agent Lifecycle | `AgentLifecycleService` trait |
//! | [`lifecycle`] | BC-1 Agent Lifecycle | `StandardAgentLifecycleService` implementation |
//! | [`execution`] | BC-2 Execution | `ExecutionService` trait, `StandardExecutionService` impl |
//! | [`policy`] | BC-4 Security Policy | Policy validation use-cases |
//! | [`attestation_service`] | BC-12 SEAL | Orchestrates SEAL attestation flow (ADR-035) |
//! | [`credential_service`] | BC-11 Secrets & Identity | `CredentialManagementService` — user credential binding lifecycle (ADR-078) |
//! | [`tool_catalog`] | BC-14 SEAL Tooling Gateway | `StandardToolCatalog` — enriched tool discovery with source/category/tag classification |
//! | [`tool_invocation_service`] | BC-12 SEAL | Mediates all MCP tool calls through the orchestrator proxy (ADR-033) |
//! | [`validation_service`] | BC-2 Execution | Gradient validation application service (ADR-017) |
//! | [`register_workflow`] | BC-3 Workflow | `RegisterWorkflowUseCase` — parse + persist workflow manifests |
//! | [`start_workflow_execution`] | BC-3 Workflow | `StartWorkflowExecutionUseCase` — submit workflow to Temporal |
//! | [`complete_workflow_execution`] | BC-3 Workflow | `CompleteWorkflowExecutionUseCase` — handle Temporal completion |
//! | [`temporal_mapper`] | BC-3 Workflow | Maps AEGIS workflow types to/from Temporal gRPC proto types |
//! | [`volume_manager`] | BC-7 Storage Gateway | `VolumeService` trait, volume lifecycle management |
//! | [`nfs_gateway`] | BC-7 Storage Gateway | `NfsGatewayService` — manages the user-space NFS server lifecycle (ADR-036) |
//! | [`storage_event_persister`] | BC-7 Storage Gateway | Subscribes to `StorageEvent`s and persists them for audit trail |
//! | [`inner_loop_service`] | BC-2 Execution | Inner loop gateway: LLM ↔ tool call cycle (ADR-038) |
//! | [`repository_factory`] | Cross-cutting | Builds concrete repository implementations from config |
//! | [`stimulus`] | BC-8 Stimulus-Response | `StimulusService` — hybrid routing pipeline, webhook ingestion (ADR-021) |
//!
//! ## Removed Modules
//!
//! `workflow_engine` and `temporal_workflow_executor` were removed when Temporal integration
//! replaced the in-process FSM executor. The TypeScript `aegis_workflow` worker function
//! drives all FSM execution; Rust interacts with it exclusively via gRPC (`TemporalClient`).

pub mod agent;
pub mod agent_scope;
pub mod attestation_service;
pub mod billing_service;
pub mod canvas_service;
pub mod cluster;
pub mod correlated_activity_stream;
pub mod credential_service;
pub mod discovery_service;
pub mod edge;
pub mod effective_tier_service;
pub mod execution;
pub mod lifecycle;
pub mod schema_registry;
pub mod scope_requester;
pub mod tool_catalog;
pub mod tool_invocation_service;
pub mod tools;
pub mod validation_service;

pub mod output_handler_service;
pub mod policy;
pub mod ports;
// pub mod workflow_engine; Removed during Temporal integration
pub mod complete_workflow_execution;
pub mod execution_event_persister;
pub mod file_operations_service;
pub mod git_clone_executor;
pub mod git_repo_service;
pub mod git_ssh_key;
pub mod inner_loop_service;
pub mod nfs_gateway;
pub mod register_workflow;
pub mod repository_factory;
pub mod run_container_step;
pub mod script_service;
pub mod start_workflow_execution;
pub mod stimulus;
pub mod storage_event_persister;
pub mod storage_router;
pub mod team_service;
pub mod temporal_mapper;
pub mod tenant_onboarding;
pub mod tenant_provisioning;
pub mod tenant_quota;
pub mod user_volume_service;
pub mod volume_manager;
pub mod workflow_scope;

// Re-export use cases for convenience
pub use complete_workflow_execution::{
    CompleteWorkflowExecutionRequest, CompleteWorkflowExecutionUseCase, CompletedWorkflowExecution,
    StandardCompleteWorkflowExecutionUseCase,
};
pub use correlated_activity_stream::CorrelatedActivityStreamService;
pub use register_workflow::{
    RegisterWorkflowUseCase, RegisteredWorkflow, StandardRegisterWorkflowUseCase,
};
pub use start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase, StartedWorkflowExecution,
};
pub use stimulus::{
    StandardStimulusService, StimulusError, StimulusIngestResponse, StimulusService,
};
