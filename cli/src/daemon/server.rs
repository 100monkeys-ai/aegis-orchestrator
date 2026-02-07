//! Daemon HTTP server implementation

use anyhow::{Context, Result};
use axum::{
    extract::{State, Path},
    routing::{get, post},
    Json, Router,
};
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
    #[cfg(unix)]
    {
        daemonize_process()?;
    }

    // Write PID file
    let pid = std::process::id();
    write_pid_file(pid)?;

    // Ensure PID file cleanup on exit
    let _guard = PidFileGuard;

    info!("AEGIS daemon starting (PID: {})", pid);

    // Load configuration
    let config = NodeConfig::load_or_default(config_path)
        .context("Failed to load configuration")?;

    config
        .validate()
        .context("Configuration validation failed")?;

    info!("Configuration loaded: node_id={}", config.node.id);

    // Initialize services
    let agent_repo = Arc::new(InMemoryAgentRepository::new());
    let execution_repo = Arc::new(InMemoryExecutionRepository::new());
    let event_bus = Arc::new(EventBus::new(100));
    let llm_registry = Arc::new(
        ProviderRegistry::from_config(&config)
            .context("Failed to initialize LLM providers")?,
    );
     let runtime = Arc::new(
        DockerRuntime::new()
            .context("Failed to initialize Docker runtime")?
    );
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

    // Build HTTP router
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/api/agents", post(deploy_agent_handler))
        .route("/api/agents/:agent_id/execute", post(execute_agent_handler))
        .route("/api/executions/:execution_id", get(get_execution_handler))
        .route(
            "/api/executions/:execution_id/cancel",
            post(cancel_execution_handler),
        )
        // .route("/api/executions/:execution_id/events", get(stream_events_handler)) // TODO: SSE
        .route("/api/executions", get(list_executions_handler))
        .with_state(Arc::new(app_state));

    // Start HTTP server
    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {}", addr))?;

    info!("Daemon listening on {}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed")?;

    info!("Daemon shutting down");

    Ok(())
}

#[cfg(unix)]
fn daemonize_process() -> Result<()> {
    use daemonize::Daemonize;

    let daemon = Daemonize::new()
        .working_directory("/tmp")
        .umask(0o027)
        .privileged_action(|| {
            info!("Daemonizing process");
        });

    daemon
        .start()
        .context("Failed to daemonize process")?;

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
) -> Json<serde_json::Value> {
    let core_manifest: aegis_core::domain::agent::AgentManifest = serde_json::from_value(serde_json::to_value(manifest).unwrap()).unwrap();
    match state.agent_service.deploy_agent(core_manifest).await {
        Ok(id) => Json(serde_json::json!({"agent_id": id.0})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
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
) -> Json<serde_json::Value> {
    let input = ExecutionInput {
        intent: Some(request.input.to_string()), // Simplified assumption
        payload: request.input,
    };
    
    match state.execution_service.start_execution(AgentId(agent_id), input).await {
        Ok(id) => Json(serde_json::json!({"execution_id": id.0})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
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

async fn list_executions_handler(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // TODO: Implement list in execution service
    Json(serde_json::json!([]))
}
