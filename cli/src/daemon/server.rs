//! Daemon HTTP server implementation

use anyhow::{Context, Result};
use axum::{
    extract::{State, Path},
    routing::{get, post},
    Json, Router,
    http::StatusCode,
    response::{IntoResponse, sse::{Event, Sse}},
};
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{info};
use uuid::Uuid;

use aegis_core::{
    application::{
        execution::StandardExecutionService, execution::ExecutionService,
        lifecycle::StandardAgentLifecycleService, agent::AgentLifecycleService,
    },
    domain::{
        node_config::NodeConfig,
        agent::AgentId,
        execution::ExecutionId,
        execution::ExecutionInput,
        supervisor::Supervisor,
        judge::BasicJudge,
    },
    infrastructure::{
        event_bus::EventBus,
        llm::registry::ProviderRegistry,
        repositories::{InMemoryAgentRepository, InMemoryExecutionRepository},
        runtime::DockerRuntime,
    },
};

use super::{remove_pid_file, write_pid_file};

pub async fn start_daemon(config_path: Option<PathBuf>, port: u16) -> Result<()> {
    // Daemonize on Unix
    // NOTE: We skip internal daemonization because calling fork() (via daemonize)
    // inside a Tokio runtime (#[tokio::main]) breaks the reactor.
    // The CLI 'daemon start' command already spawns this process as a detached background child.
    /*
    #[cfg(unix)]
    {
        daemonize_process()?;
    }
    */

    // Write PID file
    let pid = std::process::id();
    write_pid_file(pid)?;

    // Ensure PID file cleanup on exit
    let _guard = PidFileGuard;

    info!("AEGIS daemon starting (PID: {})", pid);

    // Load configuration
    println!("Loading configuration...");
    let config = NodeConfig::load_or_default(config_path)
        .context("Failed to load configuration")?;

    config
        .validate()
        .context("Configuration validation failed")?;

    println!("Configuration loaded. Initializing services...");

    // Initialize services
    let agent_repo = Arc::new(InMemoryAgentRepository::new());
    let execution_repo = Arc::new(InMemoryExecutionRepository::new());
    let event_bus = Arc::new(EventBus::new(100));
    
    println!("Initializing LLM registry...");
    let llm_registry = Arc::new(
        ProviderRegistry::from_config(&config)
            .context("Failed to initialize LLM providers")?,
    );

    println!("Initializing Docker runtime...");
    // Force a timeout on docker connection if possible, or just log before/after
    let runtime = Arc::new(
        DockerRuntime::new()
            .context("Failed to initialize Docker runtime")?
    );
    println!("Docker runtime initialized.");

    let judge = Arc::new(BasicJudge);
    let supervisor = Arc::new(Supervisor::new(runtime.clone(), judge));

    let agent_service = Arc::new(StandardAgentLifecycleService::new(agent_repo.clone()));
    let execution_service = Arc::new(StandardExecutionService::new(
        runtime,
        agent_service.clone(),
        supervisor,
        execution_repo.clone(),
    ));

    let app_state = AppState {
        agent_service,
        execution_service,
        event_bus,
        _llm_registry: llm_registry,
        start_time: std::time::Instant::now(),
    };

    println!("Building router...");
    // Build HTTP router
    let app = Router::new()
        .route("/health", get(health_handler))

        .route("/api/agents/:agent_id/execute", post(execute_agent_handler))
        .route("/api/executions/:execution_id", get(get_execution_handler))
        .route(
            "/api/executions/:execution_id/cancel",
            post(cancel_execution_handler),
        )
        .route("/api/executions/:execution_id/events", get(stream_events_handler))
        .route("/api/executions", get(list_executions_handler))
        .route("/api/executions/:execution_id", axum::routing::delete(delete_execution_handler))
        .route("/api/agents", post(deploy_agent_handler).get(list_agents_handler))
        .route("/api/agents/:id", get(get_agent_handler).delete(delete_agent_handler))
        .with_state(Arc::new(app_state));

    // Start HTTP server
    let addr = format!("127.0.0.1:{}", port);
    println!("Binding to {}...", addr);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {}", addr))?;

    info!("Daemon listening on {}", addr);
    println!("Daemon listening on {}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed")?;

    info!("Daemon shutting down");

    Ok(())
}

#[cfg(unix)]
fn daemonize_process() -> Result<()> {
    // Kept for reference but unused in current flow
    /*
    use daemonize::Daemonize;

    let stdout = std::fs::File::create("/tmp/aegis.out").unwrap();
    let stderr = std::fs::File::create("/tmp/aegis.err").unwrap();

    let daemon = Daemonize::new()
        .working_directory("/tmp")
        .umask(0o027)
        .stdout(stdout)
        .stderr(stderr)
        .privileged_action(|| {
            info!("Daemonizing process");
        });

    daemon
        .start()
        .context("Failed to daemonize process")?;
    */
    Ok(())
}

struct PidFileGuard;

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = remove_pid_file();
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C signal");
        },
        _ = terminate => {
            info!("Received SIGTERM signal");
        },
    }
}

// Application state
#[derive(Clone)]
struct AppState {
    agent_service: Arc<StandardAgentLifecycleService>,
    execution_service: Arc<StandardExecutionService>,
    event_bus: Arc<EventBus>,
    _llm_registry: Arc<ProviderRegistry>,
    start_time: std::time::Instant,
}

// HTTP handlers
async fn health_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "uptime_seconds": state.start_time.elapsed().as_secs(),
    }))
}

async fn deploy_agent_handler(
    State(state): State<Arc<AppState>>,
    Json(manifest): Json<aegis_sdk::manifest::AgentManifest>,
) -> impl IntoResponse {
    let core_manifest: aegis_core::domain::agent::AgentManifest = serde_json::from_value(serde_json::to_value(manifest).unwrap()).unwrap();
    match state.agent_service.deploy_agent(core_manifest).await {
        Ok(id) => (StatusCode::OK, Json(serde_json::json!({"agent_id": id.0}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))),
    }
}

#[derive(serde::Deserialize)]
struct ExecuteRequest {
    input: serde_json::Value,
}

async fn execute_agent_handler(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<Uuid>,
    Json(request): Json<ExecuteRequest>,
) -> impl IntoResponse {
    let input = ExecutionInput {
        intent: Some(request.input.to_string()), // Simplified assumption
        payload: request.input,
    };
    
    match state.execution_service.start_execution(AgentId(agent_id), input).await {
        Ok(id) => (StatusCode::OK, Json(serde_json::json!({"execution_id": id.0}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))),
    }
}

async fn get_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    match state.execution_service.get_execution(ExecutionId(execution_id)).await {
        Ok(exec) => Json(serde_json::json!({
            "id": exec.id.0,
            "agent_id": exec.agent_id.0,
            "status": format!("{:?}", exec.status),
            // "started_at": exec.started_at,
            // "ended_at": exec.ended_at
        })),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn cancel_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    match state.execution_service.cancel_execution(ExecutionId(execution_id)).await {
        Ok(_) => Json(serde_json::json!({"success": true})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn stream_events_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let _follow = params.get("follow").map(|v| v != "false").unwrap_or(true);
    
    // Subscribe to events using the filtered subscriber
    let mut receiver = state.event_bus.subscribe_execution(aegis_core::domain::execution::ExecutionId(execution_id));
    
    let stream = async_stream::stream! {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    // Convert domain event to SSE event
                    let json = match event {
                        aegis_core::domain::events::ExecutionEvent::ExecutionStarted { .. } => {
                            serde_json::json!({
                                "event_type": "ExecutionStarted",
                                "timestamp": chrono::Utc::now().to_rfc3339(),
                                "data": {}
                            })
                        },
                        aegis_core::domain::events::ExecutionEvent::IterationStarted { iteration_number, action, .. } => {
                            serde_json::json!({
                                "event_type": "IterationStarted",
                                "iteration_number": iteration_number,
                                "action": action, 
                                "timestamp": chrono::Utc::now().to_rfc3339(),
                                "data": { "action": action }
                            })
                        },
                         aegis_core::domain::events::ExecutionEvent::IterationCompleted { iteration_number, output, .. } => {
                            serde_json::json!({
                                "event_type": "IterationCompleted",
                                "iteration_number": iteration_number,
                                "timestamp": chrono::Utc::now().to_rfc3339(),
                                "data": { "output": output }
                            })
                        },
                        aegis_core::domain::events::ExecutionEvent::ExecutionCompleted { final_output, .. } => {
                            serde_json::json!({
                                "event_type": "ExecutionCompleted",
                                "total_iterations": 0, 
                                "timestamp": chrono::Utc::now().to_rfc3339(),
                                "data": { "result": final_output }
                            })
                        },
                         aegis_core::domain::events::ExecutionEvent::ExecutionFailed { reason, .. } => {
                            serde_json::json!({
                                "event_type": "ExecutionFailed",
                                "reason": reason,
                                "timestamp": chrono::Utc::now().to_rfc3339(),
                                "data": { "error": reason }
                            })
                        },
                        _ => {
                             serde_json::json!({
                                "event_type": "Unknown",
                                "timestamp": chrono::Utc::now().to_rfc3339(),
                                "data": {}
                            })
                        }
                    };
                    
                    yield Ok::<_, anyhow::Error>(Event::default().data(json.to_string()));
                }
                Err(_) => {
                    // Stream closed or lagged
                    break;
                }
            }
        }
    };

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

async fn delete_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    match state.execution_service.delete_execution(ExecutionId(execution_id)).await {
        Ok(_) => Json(serde_json::json!({"success": true})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

#[derive(serde::Deserialize)]
struct ListExecutionsQuery {
    agent_id: Option<Uuid>,
    limit: Option<usize>,
}

async fn list_executions_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<ListExecutionsQuery>,
) -> Json<serde_json::Value> {
    let agent_id = query.agent_id.map(AgentId);
    let limit = query.limit.unwrap_or(20);

    match state.execution_service.list_executions(agent_id, limit).await {
        Ok(executions) => {
            let json_executions: Vec<serde_json::Value> = executions.into_iter().map(|exec| {
                serde_json::json!({
                    "id": exec.id.0,
                    "agent_id": exec.agent_id.0,
                    "status": format!("{:?}", exec.status),
                    "started_at": exec.started_at,
                    "ended_at": exec.ended_at
                })
            }).collect();
            Json(serde_json::json!(json_executions))
        },
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn list_agents_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    match state.agent_service.list_agents().await {
        Ok(agents) => {
             let json_agents: Vec<serde_json::Value> = agents.into_iter().map(|agent| {
                serde_json::json!({
                    "id": agent.id.0,
                    "name": agent.manifest.agent.name,
                    "version": agent.manifest.agent.version.clone().unwrap_or_else(|| "0.0.1".to_string()),
                    "description": agent.manifest.agent.description.clone().unwrap_or_default(),
                    "status": format!("{:?}", agent.status)
                })
            }).collect();
            Json(serde_json::json!(json_agents))
        },
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn delete_agent_handler(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    match state.agent_service.delete_agent(AgentId(agent_id)).await {
        Ok(_) => Json(serde_json::json!({"success": true})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn get_agent_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Json<serde_json::Value> {
    match state.agent_service.get_agent(AgentId(id)).await {
        Ok(agent) => {
             Json(serde_json::to_value(agent.manifest).unwrap_or_else(|e| serde_json::json!({"error": e.to_string()})))
        },
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}
