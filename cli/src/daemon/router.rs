// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! HTTP router: assembles all routes from handler modules.

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post, put};
use axum::{middleware, Router};

use aegis_orchestrator_core::presentation::webhook_guard::MAX_WEBHOOK_BODY_BYTES;

use aegis_orchestrator_core::domain::iam::IdentityProvider;

use crate::daemon::api_key_identity::lookup_from_repo;
use crate::daemon::handlers::admin::{
    admin_rate_limit_router, get_user_rate_limit_usage_handler, AdminRateLimitState,
    RateLimitOverrideStore,
};
use crate::daemon::handlers::agents::{
    delete_agent_handler, deploy_agent_handler, execute_agent_handler, get_agent_handler,
    list_agent_versions_handler, list_agents_handler, lookup_agent_handler,
    stream_agent_events_handler, update_agent_handler, update_agent_scope_handler,
};
use crate::daemon::handlers::api_keys::{
    create_api_key_handler, list_api_keys_handler, revoke_api_key_handler, validate_api_key_handler,
};
use crate::daemon::handlers::approvals::{approvals_router, ApprovalsState};
use crate::daemon::handlers::billing::{
    change_tier_handler, create_checkout_handler, create_portal_handler, get_subscription_handler,
    list_invoices_handler, list_prices_handler, preview_tier_change_handler, update_seats_handler,
};
use crate::daemon::handlers::canvas::{
    create_session_handler as canvas_create_session_handler,
    get_session_handler as canvas_get_session_handler,
    list_sessions_handler as canvas_list_sessions_handler,
    stream_session_events_handler as canvas_stream_events_handler,
    terminate_session_handler as canvas_terminate_session_handler,
    update_session_handler as canvas_update_session_handler,
};
use crate::daemon::handlers::cluster::{cluster_nodes_handler, cluster_status_handler};
use crate::daemon::handlers::colony::{
    accept_invitation, cancel_invitation, create_invitation, create_team, delete_team,
    get_saml_config, get_subscription, list_invitations, list_members, list_teams, remove_member,
    set_saml_config, update_role,
};
use crate::daemon::handlers::consumer::ensure_provisioned_handler;
use crate::daemon::handlers::cortex::{
    get_cortex_metrics_handler, get_cortex_skills_handler, list_cortex_patterns_handler,
};
use crate::daemon::handlers::credentials::{
    credentials_by_id_router, credentials_calendars_router, credentials_mailboxes_router,
    credentials_oauth_providers_router, delete_secret_handler, device_poll_handler,
    get_secret_handler, list_credentials_handler, list_secrets_handler, oauth_callback_handler,
    oauth_initiate_handler, store_api_key_handler, write_secret_handler, CredentialsByIdState,
    CredentialsCalendarsState, CredentialsMailboxesState, CredentialsOAuthProvidersState,
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
use crate::daemon::handlers::llm::{llm_aliases_router, with_api_key_lookup, LlmAliasesState};
use crate::daemon::handlers::observability::{
    dashboard_summary_handler, get_stimulus_handler, list_security_incidents_handler,
    list_stimuli_handler, list_storage_violations_handler,
};
use crate::daemon::handlers::operator_escalations::{
    operator_escalations_router, OperatorEscalationsState,
};
use crate::daemon::handlers::schedules::{schedules_router, SchedulesState};
use crate::daemon::handlers::script::{
    create_script, delete_script, get_script, list_scripts, update_script,
};
use crate::daemon::handlers::seal::{
    attest_seal_handler, context_tools_seal_handler, invoke_seal_handler, list_seal_tools_handler,
};
use crate::daemon::handlers::stimulus::{ingest_stimulus_handler, webhook_handler};
use crate::daemon::handlers::swarms::{get_swarm_handler, list_swarms_handler};
use crate::daemon::handlers::tenant_provisioning::keycloak_event_handler;
use crate::daemon::handlers::tool_approvals::{tool_approvals_router, ToolApprovalsState};
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
        .route("/v1/seal/attest", post(attest_seal_handler))
        .route("/v1/seal/invoke", post(invoke_seal_handler))
        .route("/v1/seal/tools", get(list_seal_tools_handler))
        .route("/v1/seal/context-tools", post(context_tools_seal_handler))
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
        // BC-8 Webhook stimulus ingestion (ADR-021).
        // Audit 002 §4.21: cap pre-auth body reads at
        // MAX_WEBHOOK_BODY_BYTES to prevent memory DoS.
        .route(
            "/v1/webhooks/{source}",
            post(webhook_handler).layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES)),
        )
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
        .route("/v1/volumes/{id}/files/stat", get(volumes::stat_file))
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
        // Audit 002 §4.13: secret is supplied via the
        // `X-Aegis-Webhook-Secret` header, never in the URL path.
        // Audit 002 §4.21: cap webhook body at MAX_WEBHOOK_BODY_BYTES.
        .route(
            "/v1/webhooks/git",
            post(webhook_git_repo).layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES)),
        )
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
            get(canvas_get_session_handler)
                .patch(canvas_update_session_handler)
                .delete(canvas_terminate_session_handler),
        )
        // Colony management (BC-12 / ADR-111): team CRUD, membership, invitations
        .route("/v1/colony/teams", get(list_teams).post(create_team))
        .route("/v1/colony/teams/{team_id}", delete(delete_team))
        .route("/v1/colony/members", get(list_members))
        .route("/v1/colony/members/{user_id}", delete(remove_member))
        .route("/v1/colony/roles", put(update_role))
        .route(
            "/v1/colony/invitations",
            get(list_invitations).post(create_invitation),
        )
        .route(
            "/v1/colony/invitations/{invitation_id}",
            delete(cancel_invitation),
        )
        .route(
            "/v1/colony/invitations/{token}/accept",
            post(accept_invitation),
        )
        .route("/v1/colony/saml", get(get_saml_config).put(set_saml_config))
        .route("/v1/colony/subscription", get(get_subscription))
        // Stripe billing integration (BC-12)
        .route("/v1/billing/prices", get(list_prices_handler))
        .route("/v1/billing/checkout", post(create_checkout_handler))
        .route("/v1/billing/portal", post(create_portal_handler))
        .route("/v1/billing/seats", post(update_seats_handler))
        .route(
            "/v1/billing/preview-tier-change",
            post(preview_tier_change_handler),
        )
        .route("/v1/billing/change-tier", post(change_tier_handler))
        .route("/v1/billing/subscription", get(get_subscription_handler))
        .route("/v1/billing/invoices", get(list_invoices_handler))
        // Consumer self-service (ADR-097) — login-time tenant provisioning self-heal
        .route(
            "/v1/consumer/ensure-provisioned",
            post(ensure_provisioned_handler),
        )
        .with_state(app_state.clone());

    // Admin rate-limit override management (ADR-072, ADR-073 §9). Mounted as
    // its own sub-router over the narrow `RateLimitOverrideStore` port so the
    // operator-only gate in `handlers::admin` is exercised through the real
    // middleware stack by its regression tests.
    let router = router.merge(admin_rate_limit_router(AdminRateLimitState {
        store: app_state
            .rate_limit_override_repo
            .clone()
            .map(|repo| repo as Arc<dyn RateLimitOverrideStore>),
    }));

    // Human-approval requests (ADR-097 §approvals, ADR-073 §3e), mounted over
    // their own narrow state so their gate is driven through the real
    // middleware stack by the handler tests.
    let router = router.merge(approvals_router(ApprovalsState {
        human_input_service: app_state.human_input_service.clone(),
    }));

    // Tool approvals (AEGIS ADR-126 D4): a user answers a gated tool call
    // and manages "always allow". Over their own narrow state: the gate's
    // service and the tool invocation service, which runs an approved call.
    let router = router.merge(tool_approvals_router(ToolApprovalsState {
        service: app_state.tool_invocation_service.tool_approvals(),
        runner: app_state.tool_invocation_service.clone(),
    }));

    // Schedules (AEGIS ADR-139 N10) and the worker's fire route (N6), over
    // their own narrow state: the schedule service, and the agents and
    // workflows a defaults read looks its target up in (N12).
    let router = router.merge(schedules_router(SchedulesState {
        service: app_state.schedule_service.clone(),
        agents: app_state.agent_service.clone(),
        workflows: app_state.workflow_repo.clone(),
    }));

    // Credential bindings by id (ADR-078; security audit 003 F-1), over
    // their own narrow state so the service-side reach rule is driven
    // through the real middleware stack by the handler tests.
    let router = router.merge(credentials_by_id_router(CredentialsByIdState {
        credential_service: app_state.credential_service.clone(),
    }));

    // Mailbox connections by SMTP and IMAP (AEGIS ADR-125 D1), over their
    // own narrow state and beneath the same authentication layers as
    // `/v1/credentials/api-keys`.
    let router = router.merge(credentials_mailboxes_router(CredentialsMailboxesState {
        credential_service: app_state.credential_service.clone(),
    }));

    // Calendar accounts by CalDAV password (AEGIS ADR-138 K10), over their
    // own narrow state and beneath the same authentication layers as
    // `/v1/credentials/mailboxes`.
    let router = router.merge(credentials_calendars_router(CredentialsCalendarsState {
        credential_service: app_state.credential_service.clone(),
    }));

    // The OAuth providers the registry serves, by name and display name
    // (AEGIS ADR-125, Update of 2026-10-04, clause 2), beneath the same
    // authentication layers as `GET /v1/credentials`.
    let router = router.merge(credentials_oauth_providers_router(
        CredentialsOAuthProvidersState {
            credential_service: app_state.credential_service.clone(),
        },
    ));

    // Model alias lookup (AEGIS ADR-124 D3): the model an alias resolves to,
    // read from the daemon's one provider registry, the instance the inner
    // loop routes every model call through (`AppState::llm_registry`).
    // Exempt from the JWT-only IAM layer, which still attaches a valid JWT's
    // identity on this path; the handler also accepts an `aegis_*` API key
    // by the lookup `/v1/seal/attest` uses (Zaru ADR-0049 D4) and refuses
    // everything else with 401.
    let router = router.merge(with_api_key_lookup(
        llm_aliases_router(LlmAliasesState {
            registry: app_state.llm_registry.clone(),
        }),
        lookup_from_repo(
            app_state.api_key_repo.as_ref(),
            app_state.operator_escalations.as_ref(),
        ),
    ));

    // Operator escalation (AEGIS ADR-129): the operator web interface mints
    // a code, an API key redeems it, and either ends it. Exempt from the
    // JWT-only IAM layer, which still attaches a valid JWT's identity and
    // claims on the web interface's routes; the handlers admit exactly D13's
    // token there and an `aegis_*` key on the redemption routes.
    let router = router.merge(operator_escalations_router(OperatorEscalationsState {
        service: app_state.operator_escalations.clone(),
        api_keys: lookup_from_repo(
            app_state.api_key_repo.as_ref(),
            app_state.operator_escalations.as_ref(),
        ),
    }));

    // ADR-117 §F: mount `/v1/edge/*` whenever the edge bundle was constructed
    // (i.e. a Postgres pool is available). Pure-worker deployments without a
    // pool skip this mount and never serve the operator surface.
    let router = if let Some(edge_state) = app_state.edge_api.clone() {
        router.merge(aegis_orchestrator_core::api::rest::edge::router(edge_state))
    } else {
        router
    };

    // Tenant-context middleware (ADR-056, ADR-111 §Tenant-Context Header
    // Extension) — inserts the resolved TenantId into request extensions and
    // enforces consumer team-switch authorization via MembershipRepository.
    let tenant_state =
        aegis_orchestrator_core::presentation::tenant_middleware::TenantMiddlewareState {
            team_repo: app_state.team_repo.clone(),
            membership_repo: app_state.membership_repo.clone(),
            event_bus: app_state.event_bus.clone(),
        };
    apply_request_auth_layers(router, tenant_state, iam_service)
}

/// Wrap `router` in the request authentication stack every daemon route
/// sits behind: `tenant_context_middleware` inside, `iam_auth_middleware`
/// outside (axum applies the last `.layer()` outermost, so the IAM layer
/// runs first and inserts the `UserIdentity` the tenant layer reads).
///
/// When `iam_service` is `None` — a node whose config has no `spec.iam`
/// block, which `server.rs` refuses for production-labelled nodes — no
/// authentication layer is mounted and handlers see no `UserIdentity`.
/// Handlers gating on identity must therefore treat a missing identity as
/// a refusal.
pub(crate) fn apply_request_auth_layers(
    router: Router,
    tenant_state: aegis_orchestrator_core::presentation::tenant_middleware::TenantMiddlewareState,
    iam_service: Option<Arc<dyn IdentityProvider>>,
) -> Router {
    let router = router.layer(middleware::from_fn_with_state(
        tenant_state,
        aegis_orchestrator_core::presentation::tenant_middleware::tenant_context_middleware,
    ));

    if let Some(iam_service) = iam_service {
        router.layer(middleware::from_fn_with_state(
            iam_service,
            aegis_orchestrator_core::presentation::keycloak_auth::iam_auth_middleware,
        ))
    } else {
        router
    }
}
