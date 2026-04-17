// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! HTTP router: assembles all routes from handler modules.

use std::sync::Arc;

use axum::routing::{delete, get, post, put};
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
    create_api_key_handler, list_api_keys_handler, revoke_api_key_handler, validate_api_key_handler,
};
use crate::daemon::handlers::approvals::{
    approve_request_handler, get_pending_approval_handler, list_pending_approvals_handler,
    reject_request_handler,
};
use crate::daemon::handlers::billing::{
    create_checkout_handler, create_portal_handler, get_subscription_handler,
    list_invoices_handler, list_prices_handler, update_seats_handler,
};
use crate::daemon::handlers::canvas::{
    create_session_handler as canvas_create_session_handler,
    get_session_handler as canvas_get_session_handler,
    list_sessions_handler as canvas_list_sessions_handler,
    stream_session_events_handler as canvas_stream_events_handler,
    terminate_session_handler as canvas_terminate_session_handler,
};
use crate::daemon::handlers::cluster::{cluster_nodes_handler, cluster_status_handler};
use crate::daemon::handlers::colony::{
    get_saml_config, get_subscription, invite_member, list_members, remove_member, set_saml_config,
    update_role,
};
use crate::daemon::handlers::cortex::{
    get_cortex_metrics_handler, get_cortex_skills_handler, list_cortex_patterns_handler,
};
use crate::daemon::handlers::credentials::{
    add_grant_handler, delete_secret_handler, device_poll_handler, get_credential_handler,
    get_secret_handler, list_credentials_handler, list_grants_handler, list_secrets_handler,
    oauth_callback_handler, oauth_initiate_handler, revoke_credential_handler,
    revoke_grant_handler, rotate_credential_handler, store_api_key_handler, write_secret_handler,
};
use crate::daemon::handlers::dispatch::{dispatch_gateway_handler, temporal_events_handler};
use crate::daemon::handlers::executions::{
    cancel_execution_handler, delete_execution_handler, get_execution_file_handler,
    get_execution_handler, list_executions_handler, stream_events_handler,
};
use crate::daemon::handlers::git_repo::{
    commit_git_repo, create_git_repo, delete_git_repo, diff_git_repo, get_git_repo, list_git_repos,
    push_git_repo, refresh_git_repo, webhook_git_repo,
};
use crate::daemon::handlers::health::{health_handler, readiness_handler};
use crate::daemon::handlers::observability::{
    dashboard_summary_handler, get_stimulus_handler, list_security_incidents_handler,
    list_stimuli_handler, list_storage_violations_handler,
};
use crate::daemon::handlers::script::{
    create_script, delete_script, get_script, list_scripts, update_script,
};
use crate::daemon::handlers::seal::{
    attest_seal_handler, invoke_seal_handler, list_seal_tools_handler,
};
use crate::daemon::handlers::stimulus::{ingest_stimulus_handler, webhook_handler};
use crate::daemon::handlers::swarms::{get_swarm_handler, list_swarms_handler};
use crate::daemon::handlers::tenant_provisioning::keycloak_event_handler;
use crate::daemon::handlers::volumes;
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
            "/v1/executions/{execution_id}/files/{*path}",
            get(get_execution_file_handler),
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
        .route(
            "/v1/stimuli",
            get(list_stimuli_handler).post(ingest_stimulus_handler),
        )
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
        // Validate route MUST come before /{id} to avoid matching "validate" as a UUID
        .route("/v1/api-keys/validate", post(validate_api_key_handler))
        .route("/v1/api-keys/{id}", delete(revoke_api_key_handler))
        // Keycloak webhook for tenant provisioning (ADR-097)
        .route("/v1/webhooks/keycloak", post(keycloak_event_handler))
        // BC-8 Webhook stimulus ingestion (ADR-021)
        .route("/v1/webhooks/{source}", post(webhook_handler))
        // BC-11 Credential management (ADR-078) — static paths BEFORE parameterized
        .route("/v1/credentials", get(list_credentials_handler))
        .route("/v1/credentials/api-keys", post(store_api_key_handler))
        .route(
            "/v1/credentials/oauth/initiate",
            post(oauth_initiate_handler),
        )
        .route(
            "/v1/credentials/oauth/callback",
            get(oauth_callback_handler),
        )
        .route(
            "/v1/credentials/oauth/device/poll",
            post(device_poll_handler),
        )
        .route(
            "/v1/credentials/{id}",
            get(get_credential_handler).delete(revoke_credential_handler),
        )
        .route(
            "/v1/credentials/{id}/rotate",
            post(rotate_credential_handler),
        )
        .route(
            "/v1/credentials/{id}/grants",
            get(list_grants_handler).post(add_grant_handler),
        )
        .route(
            "/v1/credentials/{id}/grants/{grant_id}",
            delete(revoke_grant_handler),
        )
        // BC-11 Secrets admin (ADR-034)
        .route("/v1/secrets", get(list_secrets_handler))
        .route(
            "/v1/secrets/{path}",
            get(get_secret_handler)
                .put(write_secret_handler)
                .delete(delete_secret_handler),
        )
        // User volume management (Gap 079)
        // Note: /v1/volumes/quota MUST be registered before /v1/volumes/{id} to avoid
        // axum routing ambiguity — "quota" would otherwise be matched as an id segment.
        .route(
            "/v1/volumes",
            post(volumes::create_volume).get(volumes::list_volumes),
        )
        .route("/v1/volumes/quota", get(volumes::get_quota))
        .route(
            "/v1/volumes/{id}",
            get(volumes::get_volume)
                .patch(volumes::rename_volume)
                .delete(volumes::delete_volume),
        )
        .route(
            "/v1/volumes/{id}/files",
            get(volumes::list_files).delete(volumes::delete_path),
        )
        .route(
            "/v1/volumes/{id}/files/download",
            get(volumes::download_file),
        )
        .route("/v1/volumes/{id}/files/upload", post(volumes::upload_file))
        .route("/v1/volumes/{id}/files/mkdir", post(volumes::mkdir))
        .route("/v1/volumes/{id}/files/move", post(volumes::move_path))
        // BC-7 Git Repository Bindings (ADR-081 Waves A2 / A3)
        .route("/v1/storage/git", post(create_git_repo).get(list_git_repos))
        .route(
            "/v1/storage/git/{id}",
            get(get_git_repo).delete(delete_git_repo),
        )
        .route("/v1/storage/git/{id}/refresh", post(refresh_git_repo))
        // BC-7 Canvas git-write (ADR-106 Wave B2) — commit / push / diff
        .route("/v1/storage/git/{id}/commit", post(commit_git_repo))
        .route("/v1/storage/git/{id}/push", post(push_git_repo))
        .route("/v1/storage/git/{id}/diff", get(diff_git_repo))
        // BC-7 Git webhook (ADR-081 Wave A3) — HMAC-authenticated, exempt
        // from Keycloak JWT via EXEMPT_PATH_PREFIXES ("/v1/webhooks").
        .route("/v1/webhooks/git/{secret}", post(webhook_git_repo))
        // BC-7 Script persistence (ADR-110 §D7) — saved TypeScript
        // programs for Live Mode / Code Mode client-side execution.
        .route("/v1/scripts", post(create_script).get(list_scripts))
        .route(
            "/v1/scripts/{id}",
            get(get_script).put(update_script).delete(delete_script),
        )
        // BC-7 Vibe-Code Canvas sessions (ADR-106, Wave C2). SSE must be
        // registered before the `{id}` catch-all so axum does not match the
        // literal `events` segment as a session id.
        .route(
            "/v1/canvas/sessions",
            post(canvas_create_session_handler).get(canvas_list_sessions_handler),
        )
        .route(
            "/v1/canvas/sessions/{id}/events",
            get(canvas_stream_events_handler),
        )
        .route(
            "/v1/canvas/sessions/{id}",
            get(canvas_get_session_handler).delete(canvas_terminate_session_handler),
        )
        // Colony management (BC-12 / ADR-097): member, SAML IdP, subscription endpoints
        .route("/v1/colony/members", get(list_members).post(invite_member))
        .route("/v1/colony/members/{user_id}", delete(remove_member))
        .route("/v1/colony/roles", put(update_role))
        .route("/v1/colony/saml", get(get_saml_config).put(set_saml_config))
        .route("/v1/colony/subscription", get(get_subscription))
        // Stripe billing integration (BC-12)
        .route("/v1/billing/prices", get(list_prices_handler))
        .route("/v1/billing/checkout", post(create_checkout_handler))
        .route("/v1/billing/portal", post(create_portal_handler))
        .route("/v1/billing/seats", post(update_seats_handler))
        .route("/v1/billing/subscription", get(get_subscription_handler))
        .route("/v1/billing/invoices", get(list_invoices_handler))
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
