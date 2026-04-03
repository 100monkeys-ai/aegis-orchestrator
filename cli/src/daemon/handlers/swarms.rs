// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Swarm handlers and view types.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use uuid::Uuid;

use aegis_orchestrator_swarm::application::SwarmService;

use crate::daemon::handlers::{LimitQuery, bounded_limit};
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
    Query(query): Query<LimitQuery>,
) -> Json<serde_json::Value> {
    let swarms = state.swarm_service.list_swarms().await;
    let limit = bounded_limit(query.limit, swarms.len().max(1), 500);
    let mut items = Vec::new();
    for swarm in swarms.into_iter().take(limit) {
        let messages = state.swarm_service.messages_for_swarm(swarm.id).await;
        let locks = state.swarm_service.locks_for_swarm(swarm.id).await;
        items.push(SwarmView {
            swarm_id: swarm.id.0.to_string(),
            parent_execution_id: swarm.parent_execution_id.0.to_string(),
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

    Json(serde_json::json!({ "items": items }))
}

pub(crate) async fn get_swarm_handler(
    State(state): State<Arc<AppState>>,
    Path(swarm_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    let swarm_id = aegis_orchestrator_swarm::domain::SwarmId(swarm_id);
    match state.swarm_service.get_swarm(swarm_id).await {
        Ok(Some(swarm)) => {
            let messages = state.swarm_service.messages_for_swarm(swarm_id).await;
            let locks = state.swarm_service.locks_for_swarm(swarm_id).await;
            let view = SwarmView {
                swarm_id: swarm.id.0.to_string(),
                parent_execution_id: swarm.parent_execution_id.0.to_string(),
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
            Json(serde_json::json!({
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
            }))
        }
        Ok(None) | Err(_) => Json(serde_json::json!({"error": "swarm not found"})),
    }
}
