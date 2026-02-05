use axum::{
    routing::{get, post},
    Router, Json, extract::{State, Path},
    response::{Sse, IntoResponse},
};
use std::sync::Arc;
use tokio_stream::StreamExt;
use crate::application::execution::{ExecutionService, ExecutionInput};
use crate::domain::agent::AgentId;
use serde_json::json;
use futures::stream::Stream;
use std::pin::Pin;

pub struct AppState {
    pub execution_service: Arc<dyn ExecutionService>,
}

pub fn app(service: Arc<dyn ExecutionService>) -> Router {
    let state = Arc::new(AppState { execution_service: service });
    
    Router::new()
        .route("/executions", post(start_execution))
        .route("/executions/:id/stream", get(stream_execution))
        .with_state(state)
}

#[derive(serde::Deserialize)]
pub struct StartExecutionRequest {
    pub agent_id: String,
    pub input: String,
}

async fn start_execution(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<StartExecutionRequest>,
) -> impl IntoResponse {
    let agent_id = match uuid::Uuid::parse_str(&payload.agent_id) {
        Ok(id) => AgentId(id),
        Err(_) => return Json(json!({"error": "Invalid agent ID"})),
    };

    let input = ExecutionInput {
        input: payload.input,
    };

    match state.execution_service.start_execution(agent_id, input).await {
        Ok(id) => Json(json!({ "execution_id": id.0.to_string() })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

async fn stream_execution(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let execution_id = match uuid::Uuid::parse_str(&id) {
        Ok(uid) => crate::domain::execution::ExecutionId(uid),
        Err(_) => {
            let stream: Pin<Box<dyn Stream<Item = Result<axum::response::sse::Event, axum::Error>> + Send>> = Box::pin(tokio_stream::wrappers::ReceiverStream::new(tokio::sync::mpsc::channel(1).1).map(Ok::<_, axum::Error>));
            return Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default());
        }
    };

    let stream: Pin<Box<dyn Stream<Item = Result<axum::response::sse::Event, axum::Error>> + Send>> = match state.execution_service.stream_execution(execution_id).await {
        Ok(s) => {
            Box::pin(s.map(|event_res| {
                match event_res {
                    Ok(event) => {
                        Ok(axum::response::sse::Event::default()
                            .data(serde_json::to_string(&event).unwrap_or_default()))
                    },
                    Err(_) => Ok(axum::response::sse::Event::default().data("error")),
                }
            }))
        }
        Err(_) => {
             // Return empty stream on error
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(tokio::sync::mpsc::channel(1).1).map(Ok::<_, axum::Error>))
        }
    };
    
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}
