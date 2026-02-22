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
//! | [`attestation_service`] | BC-12 SMCP | Orchestrates SMCP attestation flow (ADR-035) |
//! | [`tool_invocation_service`] | BC-12 SMCP | Mediates all MCP tool calls through the orchestrator proxy (ADR-033) |
//! | [`validation_service`] | BC-2 Execution | Gradient validation application service (ADR-017) |
//! | [`register_workflow`] | BC-3 Workflow | `RegisterWorkflowUseCase` — parse + persist workflow manifests |
//! | [`start_workflow_execution`] | BC-3 Workflow | `StartWorkflowExecutionUseCase` — submit workflow to Temporal |
//! | [`complete_workflow_execution`] | BC-3 Workflow | `CompleteWorkflowExecutionUseCase` — handle Temporal completion |
//! | [`temporal_mapper`] | BC-3 Workflow | Maps AEGIS workflow types to/from Temporal gRPC proto types |
//! | [`temporal_workflow_executor`] | BC-3 Workflow | Spawns Temporal workflow activities |
//! | [`volume_manager`] | BC-7 Storage Gateway | `VolumeService` trait, volume lifecycle management |
//! | [`nfs_gateway`] | BC-7 Storage Gateway | `NfsGatewayService` — manages the user-space NFS server lifecycle (ADR-036) |
//! | [`storage_event_persister`] | BC-7 Storage Gateway | Subscribes to `StorageEvent`s and persists them for audit trail |
//! | [`repository_factory`] | Cross-cutting | Builds concrete repository implementations from config |
//!
//! ## Removed Modules
//!
//! `workflow_engine` was removed when Temporal integration replaced the in-process FSM
//! executor. Workflow execution now routes through [`temporal_workflow_executor`].

pub mod lifecycle;
pub mod agent;
pub mod attestation_service;
pub mod tool_invocation_service;
pub mod execution;
pub mod validation_service;

pub mod policy;
// pub mod workflow_engine; Removed during Temporal integration
pub mod temporal_mapper;
pub mod register_workflow;
pub mod start_workflow_execution;
pub mod complete_workflow_execution;
pub mod temporal_workflow_executor;
pub mod volume_manager;
pub mod nfs_gateway;
pub mod storage_event_persister;
pub mod repository_factory;

// Re-export use cases for convenience
pub use register_workflow::{RegisterWorkflowUseCase, StandardRegisterWorkflowUseCase, RegisteredWorkflow};
pub use start_workflow_execution::{StartWorkflowExecutionUseCase, StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest, StartedWorkflowExecution};
pub use complete_workflow_execution::{CompleteWorkflowExecutionUseCase, StandardCompleteWorkflowExecutionUseCase, CompleteWorkflowExecutionRequest, CompletedWorkflowExecution};
