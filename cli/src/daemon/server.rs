//! Daemon HTTP server implementation

use anyhow::{Context, Result};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{error, info};

use aegis_core::{
    application::{
        execution_service::StandardExecutionService, lifecycle::AgentLifecycleService,
    },
    domain::node_config::NodeConfig,
    infrastructure::{
        event_bus::EventBus,
        llm::registry::ProviderRegistry,
        repositories::{InMemoryAgentRepository, InMemoryExecutionRepository},
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
    let config = NodeConfig::discover(config_path.as_deref())
        .await
        .context("Failed to load configuration")?;

    config
        .validate()
        .await
        .context("Configuration validation failed")?;

    info!("Configuration loaded: node_id={}", config.node.node_id);

    // Initialize services
    let agent_repo = Arc::new(InMemoryAgentRepository::new());
    let execution_repo = Arc::new(InMemoryExecutionRepository::new());
    let event_bus = Arc::new(EventBus::new());
    let llm_registry = Arc::new(
        ProviderRegistry::from_config(&config)
            .await
            .context("Failed to initialize LLM providers")?,
    );

    let agent_service = Arc::new(AgentLifecycleService::new(agent_repo.clone()));
    let execution_service = Arc::new(StandardExecutionService::new(
        execution_repo.clone(),
        agent_repo.clone(),
        event_bus.clone(),
    ));

    let app_state = AppState {
        agent_service,
        execution_service,
        event_bus,
        llm_registry,
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
        .route("/api/executions/:execution_id/events", get(stream_events_handler))
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
    agent_service: Arc<AgentLifecycleService<InMemoryAgentRepository>>,
    execution_service: Arc<StandardExecutionService>,
    event_bus: Arc<EventBus>,
    llm_registry: Arc<ProviderRegistry>,
    start_time: std::time::Instant,
}

// HTTP handlers (stubs - need full implementation)
async fn health_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "uptime_seconds": state.start_time.elapsed().as_secs(),
    }))
}

async fn deploy_agent_handler(
    State(_state): State<Arc<AppState>>,
    Json(_manifest): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    // TODO: Implement
    Json(serde_json::json!({"agent_id": uuid::Uuid::new_v4()}))
}

async fn execute_agent_handler(
    State(_state): State<Arc<AppState>>,
    Json(_request): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    // TODO: Implement
    Json(serde_json::json!({"execution_id": uuid::Uuid::new_v4()}))
}

async fn get_execution_handler(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // TODO: Implement
    Json(serde_json::json!({"status": "running"}))
}

async fn cancel_execution_handler(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // TODO: Implement
    Json(serde_json::json!({"success": true}))
}

async fn list_executions_handler(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // TODO: Implement
    Json(serde_json::json!([]))
}

async fn stream_events_handler(State(_state): State<Arc<AppState>>) -> String {
    // TODO: Implement SSE stream
    "".to_string()
}
