// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Output Handler Deferred Variant Regression Tests (BC-2 / ADR-103)
//!
//! Regression tests proving that the `Container` and `McpTool` variants of
//! [`OutputHandlerConfig`] return a structured
//! [`OutputHandlerError::NotYetImplemented`] error rather than panicking via
//! `todo!()` when invoked by
//! [`StandardOutputHandlerService::invoke`].
//!
//! ## The bug this test guards
//!
//! Previously the dispatch in `output_handler_service.rs` read:
//!
//! ```ignore
//! OutputHandlerConfig::Container { .. } => {
//!     todo!("Container output handler — ADR-103 phase 2")
//! }
//! OutputHandlerConfig::McpTool { .. } => {
//!     todo!("McpTool output handler — ADR-103 phase 2")
//! }
//! ```
//!
//! When a user-supplied agent YAML declared
//! `output_handler: { type: container, ... }` or `{ type: mcp_tool, ... }`,
//! the worker loop that ran the output handler would panic with `not yet
//! implemented`, crashing the executor rather than surfacing a structured
//! failure on the execution's status record.
//!
//! The fix replaces both `todo!()` calls with
//! `OutputHandlerError::NotYetImplemented`, which maps to gRPC `UNIMPLEMENTED`
//! (HTTP `501 Not Implemented` equivalent) at the daemon boundary and to a
//! structured failure at the execution lifecycle layer.
//!
//! ## What this test asserts
//!
//! 1. Invoking the service with `OutputHandlerConfig::Container` returns
//!    `Err(OutputHandlerError::NotYetImplemented(_))`, does NOT panic.
//! 2. Invoking the service with `OutputHandlerConfig::McpTool` returns
//!    `Err(OutputHandlerError::NotYetImplemented(_))`, does NOT panic.
//! 3. The error message mentions the variant and references ADR-103 phase 2.
//!
//! Before the fix, both tests would unwind the task with `todo!()` and the
//! outer `catch_unwind` assertions would fail.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::output_handler_service::{
    OutputHandlerError, OutputHandlerService, StandardOutputHandlerService,
};
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentScope};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::output_handler::OutputHandlerConfig;
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};

// ----------------------------------------------------------------------------
// Test stubs
//
// Container and McpTool dispatch paths short-circuit in the match arm and
// never touch the execution or agent lifecycle services. Every stub method
// therefore unwinds via `unreachable!()`. If a future refactor accidentally
// makes the deferred variants call into these services, the test will surface
// the regression immediately.
// ----------------------------------------------------------------------------

struct UnusedExecutionService;

#[async_trait]
impl ExecutionService for UnusedExecutionService {
    async fn start_execution(
        &self,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _security_context_name: String,
        _identity: Option<&UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        unreachable!("start_execution must not be called for deferred output handler variants")
    }

    async fn start_execution_with_id(
        &self,
        _execution_id: ExecutionId,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _security_context_name: String,
        _identity: Option<&UserIdentity>,
    ) -> anyhow::Result<ExecutionId> {
        unreachable!(
            "start_execution_with_id must not be called for deferred output handler variants"
        )
    }

    async fn start_child_execution(
        &self,
        _agent_id: AgentId,
        _input: ExecutionInput,
        _parent_execution_id: ExecutionId,
    ) -> anyhow::Result<ExecutionId> {
        unreachable!(
            "start_child_execution must not be called for deferred output handler variants"
        )
    }

    async fn get_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: ExecutionId,
    ) -> anyhow::Result<Execution> {
        unreachable!(
            "get_execution_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn get_execution_unscoped(&self, _id: ExecutionId) -> anyhow::Result<Execution> {
        unreachable!(
            "get_execution_unscoped must not be called for deferred output handler variants"
        )
    }

    async fn get_iterations_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _exec_id: ExecutionId,
    ) -> anyhow::Result<Vec<Iteration>> {
        unreachable!(
            "get_iterations_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn cancel_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: ExecutionId,
    ) -> anyhow::Result<()> {
        unreachable!(
            "cancel_execution_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn stream_execution(
        &self,
        _id: ExecutionId,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>> {
        unreachable!("stream_execution must not be called for deferred output handler variants")
    }

    async fn stream_agent_events(
        &self,
        _id: AgentId,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<DomainEvent>> + Send>>> {
        unreachable!("stream_agent_events must not be called for deferred output handler variants")
    }

    async fn list_executions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: Option<AgentId>,
        _workflow_id: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
        _limit: usize,
    ) -> anyhow::Result<Vec<Execution>> {
        unreachable!(
            "list_executions_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn delete_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: ExecutionId,
    ) -> anyhow::Result<()> {
        unreachable!(
            "delete_execution_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn record_llm_interaction(
        &self,
        _execution_id: ExecutionId,
        _iteration: u8,
        _interaction: aegis_orchestrator_core::domain::execution::LlmInteraction,
    ) -> anyhow::Result<()> {
        unreachable!(
            "record_llm_interaction must not be called for deferred output handler variants"
        )
    }

    async fn store_iteration_trajectory(
        &self,
        _execution_id: ExecutionId,
        _iteration: u8,
        _trajectory: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
    ) -> anyhow::Result<()> {
        unreachable!(
            "store_iteration_trajectory must not be called for deferred output handler variants"
        )
    }
}

struct UnusedAgentLifecycleService;

#[async_trait]
impl AgentLifecycleService for UnusedAgentLifecycleService {
    async fn deploy_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _manifest: AgentManifest,
        _force: bool,
        _scope: AgentScope,
        _caller_identity: Option<&UserIdentity>,
    ) -> anyhow::Result<AgentId> {
        unreachable!(
            "deploy_agent_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn get_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: AgentId,
    ) -> anyhow::Result<Agent> {
        unreachable!("get_agent_for_tenant must not be called for deferred output handler variants")
    }

    async fn update_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: AgentId,
        _manifest: AgentManifest,
    ) -> anyhow::Result<()> {
        unreachable!(
            "update_agent_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn delete_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _id: AgentId,
    ) -> anyhow::Result<()> {
        unreachable!(
            "delete_agent_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn list_agents_for_tenant(&self, _tenant_id: &TenantId) -> anyhow::Result<Vec<Agent>> {
        unreachable!(
            "list_agents_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn list_agents_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
    ) -> anyhow::Result<Vec<Agent>> {
        unreachable!(
            "list_agents_visible_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn lookup_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        unreachable!(
            "lookup_agent_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn lookup_agent_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        unreachable!(
            "lookup_agent_visible_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn list_versions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: AgentId,
    ) -> anyhow::Result<Vec<AgentVersion>> {
        unreachable!(
            "list_versions_for_tenant must not be called for deferred output handler variants"
        )
    }

    async fn lookup_agent_for_tenant_with_version(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
        _version: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        unreachable!(
            "lookup_agent_for_tenant_with_version must not be called for deferred output handler variants"
        )
    }
}

fn build_service() -> StandardOutputHandlerService {
    let execution_service: Arc<dyn ExecutionService> = Arc::new(UnusedExecutionService);
    let agent_lifecycle_service: Arc<dyn AgentLifecycleService> =
        Arc::new(UnusedAgentLifecycleService);
    let event_bus = Arc::new(EventBus::with_default_capacity());
    StandardOutputHandlerService::new(execution_service, agent_lifecycle_service, event_bus)
}

fn container_config() -> OutputHandlerConfig {
    OutputHandlerConfig::Container {
        image: "ghcr.io/example/formatter:latest".to_string(),
        command: vec!["/bin/sh".to_string(), "-c".to_string(), "cat".to_string()],
        env: HashMap::new(),
        resources: None,
        required: false,
    }
}

fn mcp_tool_config() -> OutputHandlerConfig {
    OutputHandlerConfig::McpTool {
        tool_name: "slack_message".to_string(),
        arguments: serde_json::json!({
            "channel": "#alerts",
            "text": "{{output}}",
        }),
        timeout_seconds: None,
        required: false,
    }
}

// ----------------------------------------------------------------------------
// Regression tests
// ----------------------------------------------------------------------------

#[tokio::test]
async fn container_output_handler_returns_not_yet_implemented_instead_of_panicking() {
    let service = build_service();
    let tenant_id = TenantId::consumer();

    // Invoke the Container dispatch path. Before the fix this would `todo!()`
    // and unwind the task. After the fix it returns a structured error.
    let result = service
        .invoke(&container_config(), "final output", None, &tenant_id, None)
        .await;

    match result {
        Err(OutputHandlerError::NotYetImplemented(msg)) => {
            assert!(
                msg.contains("Container"),
                "NotYetImplemented message must name the Container variant, got: {msg}"
            );
            assert!(
                msg.contains("ADR-103 phase 2"),
                "NotYetImplemented message must reference the deferred ADR phase, got: {msg}"
            );
        }
        Err(other) => panic!(
            "expected OutputHandlerError::NotYetImplemented for Container variant, got: {other:?}"
        ),
        Ok(v) => panic!("expected Err(NotYetImplemented) for Container variant, got Ok({v:?})"),
    }
}

#[tokio::test]
async fn mcp_tool_output_handler_returns_not_yet_implemented_instead_of_panicking() {
    let service = build_service();
    let tenant_id = TenantId::consumer();

    let result = service
        .invoke(&mcp_tool_config(), "final output", None, &tenant_id, None)
        .await;

    match result {
        Err(OutputHandlerError::NotYetImplemented(msg)) => {
            assert!(
                msg.contains("McpTool"),
                "NotYetImplemented message must name the McpTool variant, got: {msg}"
            );
            assert!(
                msg.contains("ADR-103 phase 2"),
                "NotYetImplemented message must reference the deferred ADR phase, got: {msg}"
            );
        }
        Err(other) => panic!(
            "expected OutputHandlerError::NotYetImplemented for McpTool variant, got: {other:?}"
        ),
        Ok(v) => panic!("expected Err(NotYetImplemented) for McpTool variant, got Ok({v:?})"),
    }
}

/// Secondary regression: the deferred-variant dispatch path must not panic.
/// Wraps the call in an async-aware panic probe so that any future refactor
/// that reintroduces a `todo!()` / `unimplemented!()` / explicit `panic!` in
/// the match arm is caught even before the `Err` discriminant check.
#[tokio::test]
async fn deferred_variants_never_panic_on_dispatch() {
    // `tokio::spawn` + `JoinError::is_panic` is the async-safe equivalent of
    // `std::panic::catch_unwind` for async code — `catch_unwind` doesn't work
    // directly on non-`UnwindSafe` futures like an async fn that borrows
    // across await points.
    let container_result = tokio::spawn(async {
        let service = build_service();
        let tenant_id = TenantId::consumer();
        service
            .invoke(&container_config(), "final output", None, &tenant_id, None)
            .await
    })
    .await;

    assert!(
        matches!(container_result, Ok(Err(OutputHandlerError::NotYetImplemented(_)))),
        "Container dispatch must complete without panic and return NotYetImplemented, got: {container_result:?}"
    );

    let mcp_result = tokio::spawn(async {
        let service = build_service();
        let tenant_id = TenantId::consumer();
        service
            .invoke(&mcp_tool_config(), "final output", None, &tenant_id, None)
            .await
    })
    .await;

    assert!(
        matches!(mcp_result, Ok(Err(OutputHandlerError::NotYetImplemented(_)))),
        "McpTool dispatch must complete without panic and return NotYetImplemented, got: {mcp_result:?}"
    );
}
