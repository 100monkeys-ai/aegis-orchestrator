// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! HTTP router: assembles all routes from handler modules.

use std::sync::Arc;

use axum::routing::{delete, get, post};
use axum::{middleware, Router};

use aegis_orchestrator_core::domain::iam::IdentityProvider;

use crate::daemon::handlers::admin::{
    delete_rate_limit_override_handler, get_rate_limit_usage_handler,
    get_user_rate_limit_usage_handler, list_rate_limit_overrides_handler,
    upsert_rate_limit_override_handler,
};
use crate::daemon::handlers::agents::{
    delete_agent_handler, deploy_agent_handler, execute_agent_handler, get_agent_handler,
    list_agent_versions_handler, list_agents_handler, lookup_agent_handler,
    stream_agent_events_handler, update_agent_handler, update_agent_scope_handler,
};
use crate::daemon::handlers::api_keys::{
    create_api_key_handler, list_api_keys_handler, revoke_api_key_handler,
};
use crate::daemon::handlers::approvals::{
    approve_request_handler, get_pending_approval_handler, list_pending_approvals_handler,
    reject_request_handler,
};
use crate::daemon::handlers::cluster::{cluster_nodes_handler, cluster_status_handler};
use crate::daemon::handlers::cortex::{
    get_cortex_metrics_handler, get_cortex_skills_handler, list_cortex_patterns_handler,
};
use crate::daemon::handlers::dispatch::{dispatch_gateway_handler, temporal_events_handler};
use crate::daemon::handlers::executions::{
    cancel_execution_handler, delete_execution_handler, get_execution_handler,
    list_executions_handler, stream_events_handler,
};
use crate::daemon::handlers::health::{health_handler, readiness_handler};
use crate::daemon::handlers::observability::{
    dashboard_summary_handler, get_stimulus_handler, list_security_incidents_handler,
    list_stimuli_handler, list_storage_violations_handler,
};
use crate::daemon::handlers::seal::{
    attest_seal_handler, invoke_seal_handler, list_seal_tools_handler,
};
use crate::daemon::handlers::swarms::{get_swarm_handler, list_swarms_handler};
use crate::daemon::handlers::workflow_executions::{
    cancel_workflow_execution_handler, get_workflow_execution_handler, get_workflow_logs_handler,
    list_workflow_executions_handler, remove_workflow_execution_handler,
    signal_workflow_execution_handler, stream_workflow_logs_handler,
};
use crate::daemon::handlers::workflows::{
    delete_workflow_handler, execute_temporal_workflow_handler, get_workflow_handler,
    list_workflow_versions_handler, list_workflows_handler, register_temporal_workflow_handler,
    run_workflow_legacy_handler, update_workflow_scope_handler,
};
use crate::daemon::state::AppState;

/// Assemble the full HTTP router with all routes.
pub(crate) fn create_router(
    app_state: Arc<AppState>,
    iam_service: Option<Arc<dyn IdentityProvider>>,
) -> Router {
    let router = Router::new()
        .route("/health", get(health_handler))
        .route("/health/live", get(health_handler))
        .route("/health/ready", get(readiness_handler))
        .route("/v1/agents/{agent_id}/execute", post(execute_agent_handler))
        .route("/v1/executions/{execution_id}", get(get_execution_handler))
        .route(
            "/v1/executions/{execution_id}/cancel",
            post(cancel_execution_handler),
        )
        .route(
            "/v1/executions/{execution_id}/events",
            get(stream_events_handler),
        )
        .route(
            "/v1/agents/{agent_id}/events",
            get(stream_agent_events_handler),
        )
        .route("/v1/executions", get(list_executions_handler))
        .route(
            "/v1/executions/{execution_id}",
            delete(delete_execution_handler),
        )
        .route(
            "/v1/agents",
            post(deploy_agent_handler).get(list_agents_handler),
        )
        .route("/v1/agents/{id}/versions", get(list_agent_versions_handler))
        .route(
            "/v1/agents/{id}",
            get(get_agent_handler)
                .delete(delete_agent_handler)
                .patch(update_agent_handler),
        )
        .route("/v1/agents/{id}/scope", post(update_agent_scope_handler))
        .route("/v1/agents/lookup/{name}", get(lookup_agent_handler))
        .route("/v1/dispatch-gateway", post(dispatch_gateway_handler))
        .route(
            "/v1/workflows",
            post(register_temporal_workflow_handler).get(list_workflows_handler),
        )
        .route(
            "/v1/workflows/{name}/versions",
            get(list_workflow_versions_handler),
        )
        .route(
            "/v1/workflows/{name}",
            get(get_workflow_handler).delete(delete_workflow_handler),
        )
        .route(
            "/v1/workflows/{name}/scope",
            post(update_workflow_scope_handler),
        )
        .route(
            "/v1/workflows/{name}/run",
            post(run_workflow_legacy_handler),
        )
        // Note: `/v1/workflows/temporal/register` is an explicit alias of POST `/v1/workflows`
        // for Temporal workflow registration and is kept for compatibility/clarity.
        .route(
            "/v1/workflows/temporal/register",
            post(register_temporal_workflow_handler),
        )
        .route(
            "/v1/workflows/temporal/execute",
            post(execute_temporal_workflow_handler),
        )
        .route(
            "/v1/workflows/executions",
            get(list_workflow_executions_handler),
        )
        .route(
            "/v1/workflows/executions/{execution_id}",
            get(get_workflow_execution_handler).delete(remove_workflow_execution_handler),
        )
        .route(
            "/v1/workflows/executions/{execution_id}/logs",
            get(get_workflow_logs_handler),
        )
        .route(
            "/v1/workflows/executions/{execution_id}/logs/stream",
            get(stream_workflow_logs_handler),
        )
        .route(
            "/v1/workflows/executions/{execution_id}/signal",
            post(signal_workflow_execution_handler),
        )
        .route(
            "/v1/workflows/executions/{execution_id}/cancel",
            post(cancel_workflow_execution_handler),
        )
        .route("/v1/temporal-events", post(temporal_events_handler))
        .route("/v1/human-approvals", get(list_pending_approvals_handler))
        .route(
            "/v1/human-approvals/{id}",
            get(get_pending_approval_handler),
        )
        .route(
            "/v1/human-approvals/{id}/approve",
            post(approve_request_handler),
        )
        .route(
            "/v1/human-approvals/{id}/reject",
            post(reject_request_handler),
        )
        .route("/v1/seal/attest", post(attest_seal_handler))
        .route("/v1/seal/invoke", post(invoke_seal_handler))
        .route("/v1/seal/tools", get(list_seal_tools_handler))
        .route("/v1/cluster/status", get(cluster_status_handler))
        .route("/v1/cluster/nodes", get(cluster_nodes_handler))
        .route("/v1/swarms", get(list_swarms_handler))
        .route("/v1/swarms/{swarm_id}", get(get_swarm_handler))
        .route("/v1/stimuli", get(list_stimuli_handler))
        .route("/v1/stimuli/{stimulus_id}", get(get_stimulus_handler))
        .route(
            "/v1/security/incidents",
            get(list_security_incidents_handler),
        )
        .route(
            "/v1/storage/violations",
            get(list_storage_violations_handler),
        )
        .route("/v1/dashboard/summary", get(dashboard_summary_handler))
        .route("/v1/cortex/patterns", get(list_cortex_patterns_handler))
        .route("/v1/cortex/skills", get(get_cortex_skills_handler))
        .route("/v1/cortex/metrics", get(get_cortex_metrics_handler))
        // Admin rate-limit override management (ADR-072)
        .route(
            "/v1/admin/rate-limits/overrides",
            get(list_rate_limit_overrides_handler).post(upsert_rate_limit_override_handler),
        )
        .route(
            "/v1/admin/rate-limits/overrides/{id}",
            delete(delete_rate_limit_override_handler),
        )
        .route(
            "/v1/admin/rate-limits/usage",
            get(get_rate_limit_usage_handler),
        )
        .route(
            "/v1/user/rate-limits/usage",
            get(get_user_rate_limit_usage_handler),
        )
        // API key management (ADR-093)
        .route(
            "/v1/api-keys",
            get(list_api_keys_handler).post(create_api_key_handler),
        )
        .route("/v1/api-keys/{id}", delete(revoke_api_key_handler))
        .with_state(app_state);

    if let Some(iam_service) = iam_service {
        router.layer(middleware::from_fn_with_state(
            iam_service,
            aegis_orchestrator_core::presentation::keycloak_auth::iam_auth_middleware,
        ))
    } else {
        router
    }
}
