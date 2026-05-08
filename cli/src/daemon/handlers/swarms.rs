// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Swarm handlers and view types.
//!
//! All swarm reads are tenant-scoped (audit 002, finding 4.34). Handlers
//! resolve the caller's tenant via [`resolved_tenant`] and pass it into
//! every `SwarmService` / inherent method that takes a `SwarmId`.

use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::Extension;
use uuid::Uuid;

use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;
use aegis_orchestrator_swarm::application::SwarmService;

use crate::daemon::handlers::{bounded_limit, is_operator, resolved_tenant, LimitQuery};
use crate::daemon::state::AppState;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SwarmMessageView {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) payload_bytes: usize,
    pub(crate) sent_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SwarmLockView {
    pub(crate) resource_id: String,
    pub(crate) held_by: String,
    pub(crate) acquired_at: chrono::DateTime<chrono::Utc>,
    pub(crate) expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SwarmView {
    pub(crate) swarm_id: String,
    pub(crate) parent_execution_id: String,
    pub(crate) tenant_id: String,
    pub(crate) member_ids: Vec<String>,
    pub(crate) member_count: usize,
    pub(crate) status: String,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) dissolved_at: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) lock_count: usize,
    pub(crate) recent_message_count: usize,
}

pub(crate) async fn list_swarms_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Query(query): Query<LimitQuery>,
    request: Request,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("swarm:list")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let caller_is_operator = is_operator(identity_ref);
    let tenant_id = resolved_tenant(&request, identity_ref);
    // Operator cross-tenant aggregation (ADR-097): each swarm carries its
    // own `tenant_id`; messages/locks lookups for the operator path use
    // the swarm's own tenant rather than the caller's resolved tenant.
    let swarms = if caller_is_operator {
        state.swarm_service.list_swarms_unscoped().await
    } else {
        state.swarm_service.list_swarms(&tenant_id).await
    };
    let limit = bounded_limit(query.limit, swarms.len().max(1), 500);
    let mut items = Vec::new();
    for swarm in swarms.into_iter().take(limit) {
        let (messages, locks) = if caller_is_operator {
            (
                state
                    .swarm_service
                    .messages_for_swarm_unscoped(swarm.id)
                    .await,
                state.swarm_service.locks_for_swarm_unscoped(swarm.id).await,
            )
        } else {
            (
                state
                    .swarm_service
                    .messages_for_swarm(&tenant_id, swarm.id)
                    .await,
                state
                    .swarm_service
                    .locks_for_swarm(&tenant_id, swarm.id)
                    .await,
            )
        };
        items.push(SwarmView {
            swarm_id: swarm.id.0.to_string(),
            parent_execution_id: swarm.parent_execution_id.0.to_string(),
            tenant_id: swarm.tenant_id.as_str().to_string(),
            member_ids: swarm
                .member_ids()
                .into_iter()
                .map(|id| id.0.to_string())
                .collect(),
            member_count: swarm.member_ids().len(),
            status: format!("{:?}", swarm.status).to_lowercase(),
            created_at: swarm.created_at,
            dissolved_at: swarm.dissolved_at,
            lock_count: locks.len(),
            recent_message_count: messages.len(),
        });
    }

    Ok(axum::Json(serde_json::json!({ "items": items })))
}

pub(crate) async fn get_swarm_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(swarm_id): Path<Uuid>,
    request: Request,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("swarm:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let caller_is_operator = is_operator(identity_ref);
    let tenant_id = resolved_tenant(&request, identity_ref);
    let swarm_id = aegis_orchestrator_swarm::domain::SwarmId(swarm_id);
    let swarm_lookup: Result<Option<_>, anyhow::Error> = if caller_is_operator {
        Ok(state.swarm_service.get_swarm_unscoped(swarm_id).await)
    } else {
        state.swarm_service.get_swarm(&tenant_id, swarm_id).await
    };
    match swarm_lookup {
        Ok(Some(swarm)) => {
            let (messages, locks) = if caller_is_operator {
                (
                    state
                        .swarm_service
                        .messages_for_swarm_unscoped(swarm_id)
                        .await,
                    state.swarm_service.locks_for_swarm_unscoped(swarm_id).await,
                )
            } else {
                (
                    state
                        .swarm_service
                        .messages_for_swarm(&tenant_id, swarm_id)
                        .await,
                    state
                        .swarm_service
                        .locks_for_swarm(&tenant_id, swarm_id)
                        .await,
                )
            };
            let view = SwarmView {
                swarm_id: swarm.id.0.to_string(),
                parent_execution_id: swarm.parent_execution_id.0.to_string(),
                tenant_id: swarm.tenant_id.as_str().to_string(),
                member_ids: swarm
                    .member_ids()
                    .into_iter()
                    .map(|id| id.0.to_string())
                    .collect(),
                member_count: swarm.member_ids().len(),
                status: format!("{:?}", swarm.status).to_lowercase(),
                created_at: swarm.created_at,
                dissolved_at: swarm.dissolved_at,
                lock_count: locks.len(),
                recent_message_count: messages.len(),
            };
            Ok(axum::Json(serde_json::json!({
                "swarm": view,
                "locks": locks.into_iter().map(|lock| SwarmLockView {
                    resource_id: lock.resource_id,
                    held_by: lock.held_by.0.to_string(),
                    acquired_at: lock.acquired_at,
                    expires_at: lock.expires_at,
                }).collect::<Vec<_>>(),
                "recent_messages": messages.into_iter().map(|message| SwarmMessageView {
                    from: message.from.0.to_string(),
                    to: message.to.0.to_string(),
                    payload_bytes: message.payload.len(),
                    sent_at: message.sent_at,
                }).collect::<Vec<_>>(),
            })))
        }
        Ok(None) | Err(_) => Ok(axum::Json(serde_json::json!({"error": "swarm not found"}))),
    }
}
