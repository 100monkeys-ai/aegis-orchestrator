// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Daemon HTTP server implementation

use anyhow::{Context, Result};
use axum::{
    extract::{State, Path},
    routing::{get, post},
    Json, Router,
    http::StatusCode,
    response::{IntoResponse, sse::{Event, Sse}},
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
        workflow_engine::WorkflowEngine, validation_service::ValidationService,
    },
    domain::{
        node_config::NodeConfig,
        agent::AgentId,
        execution::ExecutionId,
        execution::ExecutionInput,
        supervisor::Supervisor,
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

    if config.llm_providers.is_empty() {
        tracing::warn!("Started with NO LLM providers configured. Agent execution will fail!");
        println!("WARNING: No LLM providers configured. Agents will fail to generate text.");
        println!("         Please check your config file or ensure one is discovered.");
    }

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
        DockerRuntime::new(config.runtime.bootstrap_script.clone())
            .context("Failed to initialize Docker runtime")?
    );
    println!("Docker runtime initialized.");

    let supervisor = Arc::new(Supervisor::new(runtime.clone()));

    let agent_service = Arc::new(StandardAgentLifecycleService::new(agent_repo.clone()));
    let execution_service = Arc::new(StandardExecutionService::new(
        agent_service.clone(),
        supervisor,
        execution_repo.clone(),
        event_bus.clone(),
        Arc::new(config.clone()),
    ));

    println!("Initializing workflow engine...");
    let validation_service = Arc::new(ValidationService::new(event_bus.clone(), execution_service.clone()));
    let workflow_engine = Arc::new(WorkflowEngine::new(event_bus.clone(), validation_service, execution_service.clone()));
    println!("Workflow engine initialized.");

    let app_state = AppState {
        agent_service,
        execution_service,
        event_bus,
        _llm_registry: llm_registry,
        workflow_engine,
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
        .route("/api/agents/:agent_id/events", get(stream_agent_events_handler))
        .route("/api/executions", get(list_executions_handler))
        .route("/api/executions/:execution_id", axum::routing::delete(delete_execution_handler))
        .route("/api/agents", post(deploy_agent_handler).get(list_agents_handler))
        .route("/api/agents/:id", get(get_agent_handler).delete(delete_agent_handler))
        .route("/api/agents/lookup/:name", get(lookup_agent_handler))
        .route("/api/llm/generate", post(llm_generate_handler))
        
        // Workflow API routes
        .route("/api/workflows", post(deploy_workflow_handler).get(list_workflows_handler))
        .route("/api/workflows/:name", get(get_workflow_handler).delete(delete_workflow_handler))
        .route("/api/workflows/:name/run", post(run_workflow_handler))
        .route("/api/workflows/executions/:execution_id", get(get_workflow_execution_handler))
        .route("/api/workflows/executions/:execution_id/logs", get(stream_workflow_logs_handler))
        
        .with_state(Arc::new(app_state));

    // Start HTTP server
    let bind_addr = if let Some(network) = &config.network {
        network.bind_address.clone()
    } else {
        "0.0.0.0".to_string()
    };
    
    // Config port takes precedence over CLI default if we consider config the source of truth for the node.
    // However, start_daemon receives `port`. 
    // Let's use the config port if network config is present, otherwise use the passed port.
    let final_port = if let Some(network) = &config.network {
        network.port
    } else {
        port
    };

    let addr = format!("{}:{}", bind_addr, final_port);
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
    workflow_engine: Arc<WorkflowEngine>,
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
    let follow = params.get("follow").map(|v| v != "false").unwrap_or(true);
    let exec_id = aegis_core::domain::execution::ExecutionId(execution_id);
    
    // 1. Subscribe FIRST to catch any events that happen while we fetch history
    let mut receiver = state.event_bus.subscribe_execution(exec_id);
    
    // 2. Fetch history
    let execution_result = state.execution_service.get_execution(exec_id).await;
    
    let stream = async_stream::stream! {
        // 3. Replay history if execution exists
        if let Ok(execution) = execution_result {
            // ExecutionStarted
            let start_event = serde_json::json!({
                "event_type": "ExecutionStarted",
                "timestamp": execution.started_at.to_rfc3339(),
                "data": {}
            });
            yield Ok::<_, anyhow::Error>(Event::default().data(start_event.to_string()));

            // Iterations
            for iter in execution.iterations() { // Iterate over reference to avoid move
                // IterationStarted
                let iter_start = serde_json::json!({
                    "event_type": "IterationStarted",
                    "iteration_number": iter.number,
                    "action": iter.action, 
                    "timestamp": iter.started_at.to_rfc3339(),
                    "data": { "action": iter.action }
                });
                yield Ok::<_, anyhow::Error>(Event::default().data(iter_start.to_string()));

                // Replay LlmInteractions
                for interaction in &iter.llm_interactions {
                    let event = serde_json::json!({
                        "event_type": "LlmInteraction",
                        "timestamp": interaction.timestamp.to_rfc3339(),
                        "data": {
                            "model": interaction.model,
                            "provider": interaction.provider,
                            "prompt": interaction.prompt,
                            "response": interaction.response
                        }
                    });
                    yield Ok::<_, anyhow::Error>(Event::default().data(event.to_string()));
                }

                // Completion/Failure
                if let Some(output) = &iter.output {
                     let iter_end = serde_json::json!({
                        "event_type": "IterationCompleted",
                        "iteration_number": iter.number,
                        "timestamp": iter.ended_at.map(|t| t.to_rfc3339()).unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
                        "data": { "output": output }
                    });
                     yield Ok::<_, anyhow::Error>(Event::default().data(iter_end.to_string()));
                } else if let Some(error) = &iter.error {
                     // Need to map IterationError to string or struct
                     let iter_fail = serde_json::json!({
                        "event_type": "IterationFailed",
                        "iteration_number": iter.number,
                        "timestamp": iter.ended_at.map(|t| t.to_rfc3339()).unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
                        "data": { "error": error.message }
                    });
                     yield Ok::<_, anyhow::Error>(Event::default().data(iter_fail.to_string()));
                }
            }

            // Execution Terminal State
            if let Some(ended_at) = execution.ended_at {
                match execution.status {
                    aegis_core::domain::execution::ExecutionStatus::Completed => {
                         // Need final result? It's usually the last iteration output or not stored directly in Execution struct root except implicitly?
                         // The ExecutionEvent::ExecutionCompleted has `final_output`.
                         // The Execution struct doesn't seem to have `final_output` field in the previous view, just iterations.
                         // We'll infer it from the last iteration for now or empty.
                        let result = execution.iterations().last().and_then(|i| i.output.clone()).unwrap_or_default();
                        
                        let exec_end = serde_json::json!({
                            "event_type": "ExecutionCompleted",
                            "total_iterations": execution.iterations().len(), 
                            "timestamp": ended_at.to_rfc3339(),
                            "data": { "result": result }
                        });
                        yield Ok::<_, anyhow::Error>(Event::default().data(exec_end.to_string()));
                    },
                    aegis_core::domain::execution::ExecutionStatus::Failed => {
                        let reason = execution.error.clone().unwrap_or_else(|| "Execution failed".to_string());
                        let exec_fail = serde_json::json!({
                            "event_type": "ExecutionFailed",
                            "reason": reason,
                            "timestamp": ended_at.to_rfc3339(),
                            "data": { "error": reason }
                        });
                        yield Ok::<_, anyhow::Error>(Event::default().data(exec_fail.to_string()));
                    },
                    aegis_core::domain::execution::ExecutionStatus::Cancelled => {
                         // Add Cancelled event if needed
                    },
                    _ => {}
                }
            }
        }

        // 4. Stream new events if following
        if follow {
            loop {
                // TODO: Deduplication?
                // For now, assume the user accepts potential slight overlap if the execution was active during replay.
                match receiver.recv().await {
                    Ok(event) => {
                         // Convert domain event to JSON (Same logic as before)
                         let json = match event {
                            aegis_core::domain::events::ExecutionEvent::ExecutionStarted { .. } => {
                                // Skip if we already replayed it? 
                                // Simple filter: check timestamp? 
                                // For now, just stream it.
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
                            aegis_core::domain::events::ExecutionEvent::ConsoleOutput { stream, content, .. } => {
                                serde_json::json!({
                                    "event_type": "ConsoleOutput",
                                    "stream": stream,
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                    "data": { "output": content }
                                })
                            },
                            aegis_core::domain::events::ExecutionEvent::LlmInteraction { provider, model, prompt, response, .. } => {
                                serde_json::json!({
                                    "event_type": "LlmInteraction",
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                    "data": {
                                        "provider": provider,
                                        "model": model,
                                        "prompt": prompt,
                                        "response": response
                                    }
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
                        break;
                    }
                }
            }
        }
    };

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

async fn stream_agent_events_handler(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<Uuid>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let follow = params.get("follow").map(|v| v != "false").unwrap_or(false);
    let aid = aegis_core::domain::agent::AgentId(agent_id);
    
    // 1. Subscribe FIRST to catch any events that happen while we fetch history
    let mut receiver = state.event_bus.subscribe_agent(aid);
    
    // 2. Fetch all executions for this agent
    let executions_result = state.execution_service.list_executions(Some(aid), 100).await;
    
    let stream = async_stream::stream! {
        // 3. Replay history for all executions
        if let Ok(mut executions) = executions_result {
            // Sort by started_at to replay in chronological order
            executions.sort_by(|a, b| a.started_at.cmp(&b.started_at));
            
            for execution in executions {
                // ExecutionStarted
                let start_event = serde_json::json!({
                    "event_type": "ExecutionStarted",
                    "execution_id": execution.id.0,
                    "agent_id": execution.agent_id.0,
                    "timestamp": execution.started_at.to_rfc3339(),
                    "data": {}
                });
                yield Ok::<_, anyhow::Error>(Event::default().data(start_event.to_string()));

                // Iterations
                for iter in execution.iterations() {
                    // IterationStarted
                    let iter_start = serde_json::json!({
                        "event_type": "IterationStarted",
                        "execution_id": execution.id.0,
                        "iteration_number": iter.number,
                        "action": iter.action,
                        "timestamp": iter.started_at.to_rfc3339(),
                        "data": { "action": iter.action }
                    });
                    yield Ok::<_, anyhow::Error>(Event::default().data(iter_start.to_string()));

                    // Replay LlmInteractions
                    for interaction in &iter.llm_interactions {
                         let event = serde_json::json!({
                            "event_type": "LlmInteraction",
                            "execution_id": execution.id.0,
                            "agent_id": execution.agent_id.0,
                            "iteration_number": iter.number,
                            "timestamp": interaction.timestamp.to_rfc3339(),
                            "data": {
                                "model": interaction.model,
                                "provider": interaction.provider,
                                "prompt": interaction.prompt,
                                "response": interaction.response
                            }
                         });
                         yield Ok::<_, anyhow::Error>(Event::default().data(event.to_string()));
                    }
                    
                    // Replay validation results as console output
                    if let Some(validation_results) = &iter.validation_results {
                        if let Some(system) = &validation_results.system {
                            // Replay stdout
                            if !system.stdout.is_empty() {
                                let stdout_event = serde_json::json!({
                                    "event_type": "ConsoleOutput",
                                    "execution_id": execution.id.0,
                                    "stream": "stdout",
                                    "timestamp": iter.ended_at.unwrap_or(iter.started_at).to_rfc3339(),
                                    "data": { "output": system.stdout }
                                });
                                yield Ok::<_, anyhow::Error>(Event::default().data(stdout_event.to_string()));
                            }
                            // Replay stderr
                            if !system.stderr.is_empty() {
                                let stderr_event = serde_json::json!({
                                    "event_type": "ConsoleOutput",
                                    "execution_id": execution.id.0,
                                    "stream": "stderr",
                                    "timestamp": iter.ended_at.unwrap_or(iter.started_at).to_rfc3339(),
                                    "data": { "output": system.stderr }
                                });
                                yield Ok::<_, anyhow::Error>(Event::default().data(stderr_event.to_string()));
                            }
                        }
                        
                        // Replay judge evaluation
                        if let Some(semantic) = &validation_results.semantic {
                            let judge_start = serde_json::json!({
                                "event_type": "ConsoleOutput",
                                "execution_id": execution.id.0,
                                "stream": "judge",
                                "timestamp": iter.ended_at.unwrap_or(iter.started_at).to_rfc3339(),
                                "data": { "output": "🧑‍⚖️ Evaluating output..." }
                            });
                            yield Ok::<_, anyhow::Error>(Event::default().data(judge_start.to_string()));
                            
                            let judge_result = if semantic.success {
                                format!("✅ Judge: PASS (confidence: {:.2})", semantic.score)
                            } else {
                                format!("❌ Judge: FAIL (confidence: {:.2})", semantic.score)
                            };
                            let judge_event = serde_json::json!({
                                "event_type": "ConsoleOutput",
                                "execution_id": execution.id.0,
                                "stream": "judge",
                                "timestamp": iter.ended_at.unwrap_or(iter.started_at).to_rfc3339(),
                                "data": { "output": judge_result }
                            });
                            yield Ok::<_, anyhow::Error>(Event::default().data(judge_event.to_string()));
                            
                            if !semantic.reasoning.is_empty() {
                                let feedback_event = serde_json::json!({
                                    "event_type": "ConsoleOutput",
                                    "execution_id": execution.id.0,
                                    "stream": "judge",
                                    "timestamp": iter.ended_at.unwrap_or(iter.started_at).to_rfc3339(),
                                    "data": { "output": format!("   {}", semantic.reasoning) }
                                });
                                yield Ok::<_, anyhow::Error>(Event::default().data(feedback_event.to_string()));
                            }
                        }
                    }

                    // Completion/Failure
                    if let Some(output) = &iter.output {
                        let iter_end = serde_json::json!({
                            "event_type": "IterationCompleted",
                            "execution_id": execution.id.0,
                            "iteration_number": iter.number,
                            "timestamp": iter.ended_at.map(|t| t.to_rfc3339()).unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
                            "data": { "output": output }
                        });
                        yield Ok::<_, anyhow::Error>(Event::default().data(iter_end.to_string()));
                    } else if let Some(error) = &iter.error {
                        let iter_fail = serde_json::json!({
                            "event_type": "IterationFailed",
                            "execution_id": execution.id.0,
                            "iteration_number": iter.number,
                            "timestamp": iter.ended_at.map(|t| t.to_rfc3339()).unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
                            "data": { "error": error.message }
                        });
                        yield Ok::<_, anyhow::Error>(Event::default().data(iter_fail.to_string()));
                    }
                }

                // Execution Terminal State
                if let Some(ended_at) = execution.ended_at {
                    match execution.status {
                        aegis_core::domain::execution::ExecutionStatus::Completed => {
                            let result = execution.iterations().last().and_then(|i| i.output.clone()).unwrap_or_default();
                            let exec_end = serde_json::json!({
                                "event_type": "ExecutionCompleted",
                                "execution_id": execution.id.0,
                                "total_iterations": execution.iterations().len(),
                                "timestamp": ended_at.to_rfc3339(),
                                "data": { "result": result }
                            });
                            yield Ok::<_, anyhow::Error>(Event::default().data(exec_end.to_string()));
                        },
                        aegis_core::domain::execution::ExecutionStatus::Failed => {
                            let reason = execution.error.clone().unwrap_or_else(|| "Execution failed".to_string());
                            let exec_fail = serde_json::json!({
                                "event_type": "ExecutionFailed",
                                "execution_id": execution.id.0,
                                "reason": reason,
                                "timestamp": ended_at.to_rfc3339(),
                                "data": { "error": reason }
                            });
                            yield Ok::<_, anyhow::Error>(Event::default().data(exec_fail.to_string()));
                        },
                        _ => {}
                    }
                }
            }
        }

        // 4. Stream new events if following
        if follow {
            loop {
                match receiver.recv().await {
                    Ok(event) => {
                        use aegis_core::infrastructure::event_bus::DomainEvent;
                        let json = match event {
                            DomainEvent::Execution(exec_event) => {
                                match exec_event {
                                    aegis_core::domain::events::ExecutionEvent::ExecutionStarted { execution_id, agent_id, .. } => {
                                        serde_json::json!({
                                            "event_type": "ExecutionStarted",
                                            "execution_id": execution_id.0,
                                            "agent_id": agent_id.0,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": {}
                                        })
                                    },
                                    aegis_core::domain::events::ExecutionEvent::IterationStarted { execution_id, iteration_number, action, .. } => {
                                        serde_json::json!({
                                            "event_type": "IterationStarted",
                                            "execution_id": execution_id.0,
                                            "iteration_number": iteration_number,
                                            "action": action,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "action": action }
                                        })
                                    },
                                    aegis_core::domain::events::ExecutionEvent::IterationCompleted { execution_id, iteration_number, output, .. } => {
                                        serde_json::json!({
                                            "event_type": "IterationCompleted",
                                            "execution_id": execution_id.0,
                                            "iteration_number": iteration_number,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "output": output }
                                        })
                                    },
                                    aegis_core::domain::events::ExecutionEvent::ExecutionCompleted { execution_id, final_output, total_iterations, .. } => {
                                        serde_json::json!({
                                            "event_type": "ExecutionCompleted",
                                            "execution_id": execution_id.0,
                                            "total_iterations": total_iterations,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "result": final_output }
                                        })
                                    },
                                    aegis_core::domain::events::ExecutionEvent::ExecutionFailed { execution_id, reason, .. } => {
                                        serde_json::json!({
                                            "event_type": "ExecutionFailed",
                                            "execution_id": execution_id.0,
                                            "reason": reason,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "error": reason }
                                        })
                                    },
                                    aegis_core::domain::events::ExecutionEvent::ConsoleOutput { execution_id, stream, content, .. } => {
                                        serde_json::json!({
                                            "event_type": "ConsoleOutput",
                                            "execution_id": execution_id.0,
                                            "stream": stream,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "output": content }
                                        })
                                    },
                                    aegis_core::domain::events::ExecutionEvent::LlmInteraction { execution_id, provider, model, prompt, response, .. } => {
                                        serde_json::json!({
                                            "event_type": "LlmInteraction",
                                            "execution_id": execution_id.0,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": {
                                                "provider": provider,
                                                "model": model,
                                                "prompt": prompt,
                                                "response": response
                                            }
                                        })
                                    },
                                    _ => {
                                        serde_json::json!({
                                            "event_type": "Unknown",
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": {}
                                        })
                                    }
                                }
                            },
                            _ => {
                                serde_json::json!({
                                    "event_type": "Other",
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                    "data": {}
                                })
                            }
                        };
                        
                        yield Ok::<_, anyhow::Error>(Event::default().data(json.to_string()));
                    }
                    Err(_) => {
                        break;
                    }
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

async fn lookup_agent_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.agent_service.lookup_agent(&name).await {
        Ok(Some(id)) => (StatusCode::OK, Json(serde_json::json!({"id": id.0}))),
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "Agent not found"}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))),
    }
}

#[derive(serde::Deserialize)]
struct LlmGenerateRequest {
    execution_id: Option<Uuid>,
    iteration_number: Option<u8>,
    _provider: Option<String>,
    model: Option<String>,
    prompt: String,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
}

async fn llm_generate_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LlmGenerateRequest>,
) -> impl IntoResponse {
    use aegis_core::domain::llm::GenerationOptions;
    
    // Resolve alias or use default
    let alias = req.model.as_deref().unwrap_or("default");
    
    let options = GenerationOptions {
        temperature: req.temperature.or(Some(0.7)),
        max_tokens: req.max_tokens,
        ..Default::default()
    };
    
    let registry = &state._llm_registry;

    // Resolve agent_id for event logging
    let agent_id = if let Some(exec_id) = req.execution_id {
        let execution_id = aegis_core::domain::execution::ExecutionId(exec_id);
        if let Ok(exec) = state.execution_service.get_execution(execution_id).await {
            exec.agent_id
        } else {
            tracing::warn!("Could not find execution {} for LLM event", exec_id);
            aegis_core::domain::agent::AgentId(Uuid::nil())
        }
    } else {
        aegis_core::domain::agent::AgentId(Uuid::nil())
    };
    
    match registry.generate(alias, &req.prompt, &options).await {
        Ok(response) => {
            if agent_id.0 != Uuid::nil() {
                if let Some(exec_id) = req.execution_id {
                    let event = aegis_core::domain::events::ExecutionEvent::LlmInteraction {
                        execution_id: aegis_core::domain::execution::ExecutionId(exec_id),
                        agent_id: agent_id.clone(),
                        iteration_number: req.iteration_number.unwrap_or(0),
                        provider: response.provider.clone(),
                        model: response.model.clone(),
                        input_tokens: Some(response.usage.prompt_tokens),
                        output_tokens: Some(response.usage.completion_tokens),
                        prompt: req.prompt.clone(),
                        response: response.text.clone(),
                        timestamp: chrono::Utc::now(),
                    };
                    state.event_bus.publish_execution_event(event);
                    
                    // Persist interaction
                    let interaction = aegis_core::domain::execution::LlmInteraction {
                        provider: response.provider.clone(),
                        model: response.model.clone(),
                        prompt: req.prompt.clone(),
                        response: response.text.clone(),
                        timestamp: chrono::Utc::now(),
                    };
                    let _ = state.execution_service.record_llm_interaction(
                        aegis_core::domain::execution::ExecutionId(exec_id), 
                        req.iteration_number.unwrap_or(0), 
                        interaction
                    ).await;
                }
            }
            
            (StatusCode::OK, Json(serde_json::json!({
                "content": response.text,
                "usage": {
                    "prompt_tokens": response.usage.prompt_tokens,
                    "completion_tokens": response.usage.completion_tokens,
                    "total_tokens": response.usage.total_tokens
                },
                "provider": response.provider,
                "model": response.model
            })))
        },
        Err(e) => {
            tracing::error!("LLM generation failed: {}", e);

            if agent_id.0 != Uuid::nil() {
                if let Some(exec_id) = req.execution_id {
                    let event = aegis_core::domain::events::ExecutionEvent::LlmInteraction {
                        execution_id: aegis_core::domain::execution::ExecutionId(exec_id),
                        agent_id: agent_id.clone(),
                        iteration_number: req.iteration_number.unwrap_or(0),
                        provider: "unknown".to_string(),
                        model: alias.to_string(),
                        input_tokens: None,
                        output_tokens: None,
                        prompt: req.prompt.clone(),
                        response: format!("ERROR: {}", e),
                        timestamp: chrono::Utc::now(),
                    };
                    state.event_bus.publish_execution_event(event);

                    // Persist interaction
                    let interaction = aegis_core::domain::execution::LlmInteraction {
                        provider: "unknown".to_string(),
                        model: alias.to_string(),
                        prompt: req.prompt.clone(),
                        response: format!("ERROR: {}", e),
                        timestamp: chrono::Utc::now(),
                    };
                    let _ = state.execution_service.record_llm_interaction(
                        aegis_core::domain::execution::ExecutionId(exec_id), 
                        req.iteration_number.unwrap_or(0), 
                        interaction
                    ).await;
                }
            }

            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()})))
        }
    }
}

// ========================================
// Workflow API Handlers
// ========================================

/// POST /api/workflows - Deploy a workflow from YAML
async fn deploy_workflow_handler(
    State(state): State<Arc<AppState>>,
    body: String,
) -> impl IntoResponse {
    use aegis_core::infrastructure::workflow_parser::WorkflowParser;

    // Parse YAML
    match WorkflowParser::parse_yaml(&body) {
        Ok(workflow) => {
            // Register in engine
            match state.workflow_engine.register_workflow(workflow).await {
                Ok(workflow_id) => {
                    (StatusCode::OK, Json(serde_json::json!({
                        "workflow_id": workflow_id,
                        "message": "Workflow deployed successfully"
                    })))
                }
                Err(e) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                        "error": format!("Failed to register workflow: {}", e)
                    })))
                }
            }
        }
        Err(e) => {
            (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "error": format!("Failed to parse workflow YAML: {}", e)
            })))
        }
    }
}

/// GET /api/workflows - List all workflows
async fn list_workflows_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let workflows = state.workflow_engine.list_workflows().await;
    
    let workflow_list: Vec<serde_json::Value> = workflows
        .iter()
        .map(|name| {
            serde_json::json!({
                "name": name,
                "status": "active"
            })
        })
        .collect();

    (StatusCode::OK, Json(workflow_list))
}

/// GET /api/workflows/:name - Get workflow YAML
async fn get_workflow_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    use aegis_core::infrastructure::workflow_parser::WorkflowParser;

    match state.workflow_engine.get_workflow(&name).await {
        Some(workflow) => {
            match WorkflowParser::to_yaml(&workflow) {
                Ok(yaml) => {
                    (StatusCode::OK, yaml)
                }
                Err(e) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to serialize workflow: {}", e))
                }
            }
        }
        None => {
            (StatusCode::NOT_FOUND, format!("Workflow '{}' not found", name))
        }
    }
}

/// DELETE /api/workflows/:name - Delete workflow
async fn delete_workflow_handler(
    State(_state): State<Arc<AppState>>,
    Path(_name): Path<String>,
) -> impl IntoResponse {
    // TODO: Implement workflow deletion once WorkflowEngine has remove method
    (StatusCode::NOT_IMPLEMENTED, Json(serde_json::json!({
        "error": "Workflow deletion not yet implemented"
    })))
}

/// POST /api/workflows/:name/run - Execute a workflow
#[derive(serde::Deserialize)]
struct RunWorkflowRequest {
    input: serde_json::Value,
}

async fn run_workflow_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(request): Json<RunWorkflowRequest>,
) -> impl IntoResponse {
    use aegis_core::application::workflow_engine::WorkflowInput;
    use aegis_core::domain::execution::ExecutionId;

    let execution_id = ExecutionId(Uuid::new_v4());

    let parameters = request.input
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();

    let input = WorkflowInput { parameters };

    match state.workflow_engine.start_execution(&name, execution_id, input).await {
        Ok(()) => {
            (StatusCode::OK, Json(serde_json::json!({
                "execution_id": execution_id.0,
                "status": "running"
            })))
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to start workflow execution: {}", e)
            })))
        }
    }
}

/// GET /api/workflows/executions/:execution_id - Get execution details
async fn get_workflow_execution_handler(
    State(_state): State<Arc<AppState>>,
    Path(_execution_id): Path<Uuid>,
) -> impl IntoResponse {
    // TODO: Implement execution state retrieval once WorkflowEngine exposes active executions
    (StatusCode::NOT_IMPLEMENTED, Json(serde_json::json!({
        "error": "Workflow execution retrieval not yet implemented"
    })))
}

/// GET /api/workflows/executions/:execution_id/logs - Stream workflow logs
async fn stream_workflow_logs_handler(
    State(state): State<Arc<AppState>>,
    Path(_execution_id): Path<Uuid>,
) -> impl IntoResponse {
    // Subscribe to workflow events
    let mut receiver = state.event_bus.subscribe();
    
    // Create async stream from receiver
    let event_stream = async_stream::stream! {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    // Convert domain event to SSE Event
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok::<Event, std::convert::Infallible>(Event::default().data(json));
                    }
                }
                Err(_) => break,
            }
        }
    };

    Sse::new(event_stream)
}
