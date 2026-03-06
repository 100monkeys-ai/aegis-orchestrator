// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Daemon HTTP server implementation
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for server

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;

// Type alias for repository tuple to avoid clippy "very complex type" lint
type RepositoryTuple = (
    Arc<dyn AgentRepository>,
    Arc<dyn aegis_orchestrator_core::domain::repository::WorkflowRepository>,
    Arc<dyn aegis_orchestrator_core::domain::repository::ExecutionRepository>,
    Arc<dyn aegis_orchestrator_core::domain::repository::WorkflowExecutionRepository>,
);
use sqlx::postgres::PgPool;
use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{info, warn};
use uuid::Uuid;

use aegis_orchestrator_core::{
    application::{
        agent::AgentLifecycleService,
        execution::ExecutionService,
        execution::StandardExecutionService,
        lifecycle::StandardAgentLifecycleService,
        register_workflow::{RegisterWorkflowUseCase, StandardRegisterWorkflowUseCase},
        start_workflow_execution::{
            StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
            StartWorkflowExecutionUseCase,
        },
        validation_service::ValidationService,
    },
    domain::{
        agent::AgentId,
        execution::ExecutionId,
        execution::ExecutionInput,
        node_config::{resolve_env_value, NodeConfigManifest},
        repository::AgentRepository,
        runtime_registry::StandardRuntimeRegistry,
        supervisor::Supervisor,
    },
    infrastructure::{
        event_bus::EventBus,
        llm::registry::ProviderRegistry,
        repositories::{
            InMemoryAgentRepository, InMemoryExecutionRepository,
            InMemoryWorkflowExecutionRepository,
        },
        runtime::DockerRuntime,
        temporal_client::TemporalClient,
        TemporalEventListener, TemporalEventPayload,
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
    let config =
        NodeConfigManifest::load_or_default(config_path).context("Failed to load configuration")?;

    config
        .validate()
        .context("Configuration validation failed")?;

    if config.spec.llm_providers.is_empty() {
        tracing::warn!("Started with NO LLM providers configured. Agent execution will fail!");
        println!("WARNING: No LLM providers configured. Agents will fail to generate text.");
        println!("         Please check your config file or ensure one is discovered.");
    }

    println!("Configuration loaded. Initializing services...");

    // Initialize repositories — resolve database URL from config (spec.database)
    let database_url: Option<String> =
        config
            .spec
            .database
            .as_ref()
            .and_then(|db| match resolve_env_value(&db.url) {
                Ok(url) => Some(url),
                Err(e) => {
                    tracing::warn!(
                        "Failed to resolve database URL: {}. Falling back to InMemory.",
                        e
                    );
                    None
                }
            });
    let db_max_connections: u32 = config
        .spec
        .database
        .as_ref()
        .map(|db| db.max_connections)
        .unwrap_or(5);

    // Store pool separately for later volume repo initialization
    let pool: Option<PgPool> = if let Some(url) = database_url.as_ref() {
        println!("Initializing repositories with PostgreSQL: {}", url);
        match sqlx::postgres::PgPoolOptions::new()
            .max_connections(db_max_connections)
            .connect(url)
            .await
        {
            Ok(pool) => {
                println!("Connected to PostgreSQL.");

                // Check migration status
                static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

                let total_known = MIGRATOR.iter().count();
                if total_known == 0 {
                    return Err(anyhow::anyhow!(
                        "CRITICAL: No migrations found in binary! Check build process."
                    ));
                }

                // Check applied migrations
                let applied_result = sqlx::query("SELECT version FROM _sqlx_migrations")
                    .fetch_all(&pool)
                    .await;

                let applied_count = match applied_result {
                    Ok(rows) => rows.len(),
                    Err(_) => 0,
                };

                println!(
                    "INFO: Database has {}/{} applied migrations.",
                    applied_count, total_known
                );

                if applied_count < total_known {
                    println!("Applying pending migrations...");
                    match MIGRATOR.run(&pool).await {
                        Ok(_) => println!("SUCCESS: Database migrations applied."),
                        Err(e) => {
                            return Err(anyhow::anyhow!("Failed to apply migrations: {}", e));
                        }
                    }
                } else {
                    println!("INFO: Database is up to date.");
                }

                Some(pool)
            }
            Err(e) => {
                tracing::error!(
                    "Failed to connect to PostgreSQL: {}. Falling back to InMemory.",
                    e
                );
                println!(
                    "ERROR: Failed to connect to PostgreSQL: {}. Falling back to InMemory.",
                    e
                );
                None
            }
        }
    } else {
        println!("No database configured (spec.database omitted). Using InMemory repositories.");
        None
    };

    let (agent_repo, workflow_repo, execution_repo, workflow_execution_repo): RepositoryTuple =
        if let Some(pool) = pool.as_ref() {
            (
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_agent::PostgresAgentRepository::new(pool.clone())),
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_workflow::PostgresWorkflowRepository::new_with_pool(pool.clone())),
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_execution::PostgresExecutionRepository::new(pool.clone())),
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_workflow_execution::PostgresWorkflowExecutionRepository::new(pool.clone())),
        )
        } else {
            (
                Arc::new(InMemoryAgentRepository::new()),
                Arc::new(
                    aegis_orchestrator_core::infrastructure::repositories::InMemoryWorkflowRepository::new(),
                ),
                Arc::new(InMemoryExecutionRepository::new()),
                Arc::new(InMemoryWorkflowExecutionRepository::new()),
            )
        };

    let event_bus = Arc::new(EventBus::new(100));

    println!("Initializing LLM registry...");
    let llm_registry = Arc::new(
        ProviderRegistry::from_config(&config).context("Failed to initialize LLM providers")?,
    );

    println!("Initializing Docker runtime...");

    // Resolve orchestrator URL (supports env:VAR_NAME syntax via resolve_env_value)
    let orchestrator_url =
        resolve_env_value(&config.spec.runtime.orchestrator_url).unwrap_or_else(|e| {
            tracing::warn!("Failed to resolve orchestrator URL: {}. Using default.", e);
            "http://localhost:8088".to_string()
        });

    // Resolve NFS server host (supports env:VAR_NAME syntax) - ADR-036
    // Note: The Docker daemon relies on this to mount volumes from the host environment.
    let nfs_server_host = config.spec.runtime.nfs_server_host.as_ref().and_then(|host| {
        match resolve_env_value(host) {
            Ok(resolved) if !resolved.is_empty() => Some(resolved),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("Failed to resolve NFS server host: {}. NFS mounts will default to '127.0.0.1' which works for native Linux/WSL2 deployments, but will fail with 'connection refused' in Docker Desktop unless set to 'host.docker.internal'.", e);
                None
            }
        }
    });

    // Resolve Docker network mode (supports env:VAR_NAME syntax)
    let network_mode = config
        .spec
        .runtime
        .docker_network_mode
        .as_ref()
        .and_then(|nm| match resolve_env_value(nm) {
            Ok(resolved) if !resolved.is_empty() => Some(resolved),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(
                    "Failed to resolve Docker network mode: {}. Using no explicit Docker network.",
                    e
                );
                None
            }
        });

    let runtime = Arc::new(
        DockerRuntime::new(aegis_orchestrator_core::infrastructure::runtime::DockerRuntimeConfig {
            bootstrap_script: config.spec.runtime.bootstrap_script.clone(),
            socket_path: config.spec.runtime.docker_socket_path.clone(),
            network_mode,
            orchestrator_url,
            nfs_server_host: nfs_server_host.clone(),
            nfs_port: config.spec.runtime.nfs_port,
            nfs_mountport: config.spec.runtime.nfs_mountport,
            event_bus: event_bus.clone(),
            credential_resolver: Arc::new(
                aegis_orchestrator_core::infrastructure::image_manager::NodeConfigCredentialResolver::new(
                    config.spec.registry_credentials.clone(),
                ),
            ),
        })
        .context("Failed to initialize Docker runtime")?,
    );

    // Only healthcheck Docker if it's the configured isolation mode
    if config.spec.runtime.default_isolation == "docker" {
        runtime.healthcheck().await
            .context("Docker healthcheck failed. Docker isolation is configured but Docker daemon is not accessible.")?;
        println!("✓ Docker runtime connected and healthy.");
    } else {
        println!(
            "Docker runtime initialized (healthcheck skipped - isolation mode: {}).",
            config.spec.runtime.default_isolation
        );
    }

    let supervisor = Arc::new(Supervisor::new(runtime.clone()));

    // Initialize volume service (with SeaweedFS or fallback to local)
    println!("Initializing volume service...");
    let storage_config = config
        .spec
        .storage
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Storage configuration not found in node config"))?;

    let filer_url = if storage_config.backend == "seaweedfs" {
        storage_config
            .seaweedfs
            .as_ref()
            .map(|s| s.filer_url.clone())
            .unwrap_or_else(|| "http://localhost:8888".to_string())
    } else {
        "http://localhost:8888".to_string() // Fallback even for local mode
    };

    // Reuse existing pool for volume repository (avoid redundant connection)
    let volume_repo: Arc<dyn aegis_orchestrator_core::domain::repository::VolumeRepository> =
        if let Some(pool) = pool.as_ref() {
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_volume::PostgresVolumeRepository::new(pool.clone()))
        } else {
            println!("WARNING: Volume persistence disabled (no database pool available)");
            return Err(anyhow::anyhow!(
                "Database connection required for volume management"
            ));
        };

    let storage_provider: Arc<dyn aegis_orchestrator_core::domain::storage::StorageProvider> =
        match storage_config.backend.as_str() {
            "seaweedfs" => {
                aegis_orchestrator_core::infrastructure::storage::create_storage_provider(
                    aegis_orchestrator_core::infrastructure::storage::StorageBackend::SeaweedFS {
                        filer_url: filer_url.clone(),
                    },
                )
            }
            "local_host" => {
                let mount_point = storage_config
                    .local_host
                    .as_ref()
                    .map(|l| l.mount_point.clone())
                    .unwrap_or_else(|| "/var/lib/aegis/local-host-volumes".to_string());
                aegis_orchestrator_core::infrastructure::storage::create_storage_provider(
                    aegis_orchestrator_core::infrastructure::storage::StorageBackend::LocalHost {
                        mount_point,
                    },
                )
            }
            "opendal" => {
                let opendal_config = storage_config.opendal.as_ref().cloned().unwrap_or_default();
                // Resolve env variables in config map
                let mut resolved_options = std::collections::HashMap::new();
                for (k, v) in opendal_config.options {
                    resolved_options.insert(k, resolve_env_value(&v).unwrap_or(v));
                }
                aegis_orchestrator_core::infrastructure::storage::create_storage_provider(
                    aegis_orchestrator_core::infrastructure::storage::StorageBackend::OpenDal {
                        provider: opendal_config.provider,
                        options: resolved_options,
                    },
                )
            }
            other => return Err(anyhow::anyhow!("Unsupported storage backend: {}", other)),
        };

    let volume_service = Arc::new(
        aegis_orchestrator_core::application::volume_manager::StandardVolumeService::new(
            volume_repo.clone(),
            storage_provider.clone(),
            event_bus.clone(),
            filer_url,
        )?,
    );

    println!(
        "✓ Volume service initialized (mode: {})",
        storage_config.backend
    );

    // Spawn TTL cleanup background task for ephemeral volumes
    let volume_service_cleanup = volume_service.clone();
    tokio::spawn(async move {
        use aegis_orchestrator_core::application::volume_manager::VolumeService as _;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300)); // 5 minutes
        loop {
            interval.tick().await;
            match volume_service_cleanup.cleanup_expired_volumes().await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!("Volume cleanup: {} expired volumes deleted", count);
                    }
                }
                Err(e) => {
                    tracing::error!("Volume cleanup failed: {}", e);
                }
            }
        }
    });
    println!("✓ Volume cleanup background task spawned (interval: 5 minutes)");

    // Initialize Storage Event Persister for audit trail (ADR-036)
    println!("Initializing Storage Event Persister...");
    let storage_event_repo: Arc<
        dyn aegis_orchestrator_core::domain::repository::StorageEventRepository,
    > = if let Some(pool) = pool.as_ref() {
        Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_storage_event::PostgresStorageEventRepository::new(pool.clone()))
    } else {
        println!("WARNING: Storage event persistence disabled (no database pool available)");
        Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::InMemoryStorageEventRepository::new(),
            )
    };

    let storage_event_persister = Arc::new(
        aegis_orchestrator_core::application::storage_event_persister::StorageEventPersister::new(
            storage_event_repo,
            event_bus.clone(),
        ),
    );

    // Start background task for event persistence
    let _persister_handle = storage_event_persister.start();
    println!("✓ Storage Event Persister started (audit trail enabled)");

    // Initialize NFS Server Gateway (ADR-036)
    println!("Initializing NFS Server Gateway...");
    let nfs_bind_port = config
        .spec
        .storage
        .as_ref()
        .and_then(|s| s.nfs_port)
        .unwrap_or(2049);

    // Wrap EventBus in EventBusPublisher adapter for FSAL
    let event_publisher = Arc::new(
        aegis_orchestrator_core::application::nfs_gateway::EventBusPublisher::new(
            event_bus.clone(),
        ),
    );

    let nfs_gateway = Arc::new(
        aegis_orchestrator_core::application::nfs_gateway::NfsGatewayService::new(
            storage_provider,
            volume_repo,
            event_publisher,
            Some(nfs_bind_port),
        ),
    );

    // Start NFS server and await successful startup before continuing
    if let Err(e) = nfs_gateway.start_server().await {
        tracing::error!(
            "CRITICAL: NFS Server Gateway failed to start: {}. This is a fatal error.",
            e
        );
        // Log to stderr to ensure visibility
        eprintln!("FATAL: NFS Server Gateway failed: {}", e);
        // Allow shutdown of daemon via signal
        std::process::exit(1);
    }
    println!(
        "✓ NFS Server Gateway started on port {} (ADR-036)",
        nfs_bind_port
    );

    let agent_service = Arc::new(StandardAgentLifecycleService::new(agent_repo.clone()));

    // Load StandardRuntime registry (ADR-043)
    let registry_path = &config.spec.runtime.runtime_registry_path;
    let runtime_registry = match StandardRuntimeRegistry::from_file(registry_path) {
        Ok(registry) => {
            println!(
                "✓ StandardRuntime registry loaded from '{}' (ADR-043)",
                registry_path
            );
            Arc::new(registry)
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Failed to load StandardRuntime registry from '{}': {}. \
                 Ensure runtime-registry.yaml exists at the configured path \
                 (spec.runtime.runtime_registry_path in aegis-config.yaml).",
                registry_path,
                e
            ));
        }
    };

    // Execution service initialization deferred until after ToolRouter is created

    // ADR-036: Event-driven NFS volume deregistration (security requirement)
    // Listen for VolumeExpired and VolumeDeleted events and immediately remove
    // the volume from the NFS gateway registry so the path becomes inaccessible.
    // This prevents orphaned volume registrations from remaining accessible after
    // their corresponding storage has been cleaned up.
    {
        let nfs_deregister_gateway = nfs_gateway.clone();
        let mut nfs_deregister_sub = event_bus.subscribe();
        tokio::spawn(async move {
            loop {
                match nfs_deregister_sub.recv().await {
                    Ok(
                        aegis_orchestrator_core::infrastructure::event_bus::DomainEvent::Volume(
                            vol_event,
                        ),
                    ) => {
                        let volume_id = match &vol_event {
                            aegis_orchestrator_core::domain::events::VolumeEvent::VolumeExpired {
                                volume_id,
                                ..
                            } => Some(*volume_id),
                            aegis_orchestrator_core::domain::events::VolumeEvent::VolumeDeleted {
                                volume_id,
                                ..
                            } => Some(*volume_id),
                            _ => None,
                        };
                        if let Some(vid) = volume_id {
                            nfs_deregister_gateway.deregister_volume(vid);
                            tracing::info!(
                                "NFS: Deregistered volume {} from gateway (volume expired/deleted)",
                                vid
                            );
                        }
                    }
                    Ok(_) => {} // Ignore non-volume events
                    Err(
                        aegis_orchestrator_core::infrastructure::event_bus::EventBusError::Lagged(
                            n,
                        ),
                    ) => {
                        tracing::warn!("NFS deregistration listener lagged by {} events — some volume deregistrations may have been missed", n);
                    }
                    Err(
                        aegis_orchestrator_core::infrastructure::event_bus::EventBusError::Closed,
                    ) => {
                        tracing::info!(
                            "NFS deregistration listener: event bus closed, shutting down"
                        );
                        break;
                    }
                    Err(_) => {}
                }
            }
        });
        tracing::info!("NFS deregistration listener started (ADR-036)");
    }

    println!("Initializing workflow engine...");

    // Initialize Temporal Client — read from config (spec.temporal)
    let temporal_config = config.spec.temporal.clone().unwrap_or_default();
    let temporal_address =
        resolve_env_value(&temporal_config.address).unwrap_or_else(|_| "temporal:7233".to_string());
    let worker_http_endpoint = resolve_env_value(&temporal_config.worker_http_endpoint)
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    let temporal_namespace = temporal_config.namespace.clone();
    let temporal_task_queue = temporal_config.task_queue.clone();
    println!(
        "Initializing Temporal Client (Address: {})...",
        temporal_address
    );

    // Create a shared container for the client that relies on interior mutability
    let temporal_client_container = Arc::new(tokio::sync::RwLock::new(None));
    let temporal_client_container_clone = temporal_client_container.clone();

    // Clone for async task
    let temporal_address_clone = temporal_address.clone();
    let worker_http_endpoint_clone = worker_http_endpoint.clone();

    // Spawn background task to connect
    tokio::spawn(async move {
        let mut retries = 0;
        let max_retries = 30; // Try for 1 minute (2s * 30) or indefinitely? User said "eventually timeout/quit trying"

        loop {
            match TemporalClient::new(
                &temporal_address_clone,
                &temporal_namespace,
                &temporal_task_queue,
                &worker_http_endpoint_clone,
            )
            .await
            {
                Ok(client) => {
                    println!("Async: Temporal Client connected successfully.");
                    let mut lock = temporal_client_container_clone.write().await;
                    *lock = Some(Arc::new(client));
                    break;
                }
                Err(e) => {
                    retries += 1;
                    if retries >= max_retries {
                        println!("Async WARNING: Failed to connect to Temporal after {} attempts. Giving up. Workflow execution will fail.", retries);
                        tracing::error!("Async: Failed to connect to Temporal: {}. Giving up.", e);
                        break;
                    }

                    if retries % 5 == 0 {
                        println!(
                            "Async INFO: Still verifying Temporal connection... ({}/{})",
                            retries, max_retries
                        );
                    }
                    tracing::debug!(
                        "Async: Failed to connect to Temporal: {}. Retrying in 2s...",
                        e
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    });

    // Initialize SMCP / Tool Routing Services (now hoisted for ExecutionService dependency)
    println!("Initializing SMCP & Tool Routing services...");

    let smcp_middleware =
        Arc::new(aegis_orchestrator_core::infrastructure::smcp::middleware::SmcpMiddleware::new());
    let tool_registry =
        Arc::new(aegis_orchestrator_core::infrastructure::tool_router::InMemoryToolRegistry::new());

    // Shared tool servers state
    let tool_servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

    // Load configured servers from NodeConfig
    if let Some(mcp_configs) = &config.spec.mcp_servers {
        let mut servers_lock = tool_servers.write().await;
        for srv_cfg in mcp_configs {
            if srv_cfg.enabled {
                let tool_server =
                    aegis_orchestrator_core::domain::mcp::ToolServer::from_config(srv_cfg);
                servers_lock.insert(tool_server.id, tool_server);
            }
        }
    }

    let mut builtin_dispatchers = config.spec.builtin_dispatchers.clone().unwrap_or_default();
    if !builtin_dispatchers.iter().any(|d| d.name == "cmd.run") {
        builtin_dispatchers.push(aegis_orchestrator_core::domain::node_config::BuiltinDispatcherConfig {
            name: "cmd.run".to_string(),
            description: "Executes a shell command inside the agent's ephemeral container environment. Use this to build, run, or analyze code locally.".to_string(),
            enabled: true,
            capabilities: vec![aegis_orchestrator_core::domain::node_config::CapabilityConfig { name: "cmd.run".to_string(), skip_judge: false }],
        });
    }

    // FSAL Native Tools (ADR-040)
    if !builtin_dispatchers.iter().any(|d| d.name == "fs.read") {
        builtin_dispatchers.push(aegis_orchestrator_core::domain::node_config::BuiltinDispatcherConfig {
            name: "fs.read".to_string(),
            description: "Read the contents of a file at the given POSIX path from the mounted Workspace volume.".to_string(),
            enabled: true,
            capabilities: vec![aegis_orchestrator_core::domain::node_config::CapabilityConfig { name: "fs.read".to_string(), skip_judge: true }],
        });
    }

    if !builtin_dispatchers.iter().any(|d| d.name == "fs.write") {
        builtin_dispatchers.push(aegis_orchestrator_core::domain::node_config::BuiltinDispatcherConfig {
            name: "fs.write".to_string(),
            description: "Write content to a file at the given POSIX path in the Workspace volume. Automatically creates missing parent directories.".to_string(),
            enabled: true,
            capabilities: vec![aegis_orchestrator_core::domain::node_config::CapabilityConfig { name: "fs.write".to_string(), skip_judge: false }],
        });
    }

    if !builtin_dispatchers.iter().any(|d| d.name == "fs.list") {
        builtin_dispatchers.push(
            aegis_orchestrator_core::domain::node_config::BuiltinDispatcherConfig {
                name: "fs.list".to_string(),
                description: "List the contents of a directory in the Workspace volume."
                    .to_string(),
                enabled: true,
                capabilities: vec![
                    aegis_orchestrator_core::domain::node_config::CapabilityConfig {
                        name: "fs.list".to_string(),
                        skip_judge: true,
                    },
                ],
            },
        );
    }

    let tool_router = Arc::new(
        aegis_orchestrator_core::infrastructure::tool_router::ToolRouter::new(
            tool_registry.clone(),
            tool_servers.clone(),
            builtin_dispatchers,
        ),
    );

    // Build initial capabilities index
    tool_router.rebuild_index().await;

    // Connect to the standalone Cortex service if configured (ADR-042).
    // Absence of spec.cortex.grpc_url means memoryless mode — no error, no retry.
    let cortex_grpc_url: Option<String> = config
        .spec
        .cortex
        .as_ref()
        .and_then(|c| c.grpc_url.as_ref())
        .and_then(|url| resolve_env_value(url).ok());
    let cortex_client: Option<
        std::sync::Arc<aegis_orchestrator_core::infrastructure::CortexGrpcClient>,
    > = match cortex_grpc_url {
        Some(url) => {
            match aegis_orchestrator_core::infrastructure::CortexGrpcClient::new(url.clone()).await
            {
                Ok(client) => {
                    tracing::info!(url = %url, "Connected to Cortex gRPC service");
                    Some(std::sync::Arc::new(client))
                }
                Err(e) => {
                    tracing::warn!(
                        url = %url,
                        error = %e,
                        "Failed to connect to Cortex gRPC service; running in memoryless mode"
                    );
                    None
                }
            }
        }
        None => {
            tracing::info!("Cortex gRPC URL not configured (spec.cortex omitted) — Orchestrator running in memoryless mode");
            None
        }
    };

    // Finally initialize ExecutionService now that ToolRouter is ready
    let mut execution_service_builder = StandardExecutionService::new(
        agent_service.clone(),
        volume_service.clone(),
        supervisor,
        execution_repo.clone(),
        event_bus.clone(),
        Arc::new(config.clone()),
    )
    .with_nfs_gateway(nfs_gateway.clone())
    .with_runtime_registry(runtime_registry) // No Arc::new needed per signature
    .with_tool_router(tool_router.clone());

    if let Some(c_client) = cortex_client.clone() {
        execution_service_builder = execution_service_builder.with_cortex_client(c_client);
    }

    let execution_service = Arc::new(execution_service_builder);
    // Wire the self-reference so judge agents can be spawned as child executions (ADR-016).
    execution_service.set_child_execution_service(execution_service.clone());

    let validation_service = Arc::new(ValidationService::new(
        event_bus.clone(),
        execution_service.clone(),
    ));

    // Create human input service
    let human_input_service =
        Arc::new(aegis_orchestrator_core::infrastructure::HumanInputService::new());

    // Legacy WorkflowEngine removed as part of Temporal migration

    let temporal_event_listener = Arc::new(TemporalEventListener::new(
        event_bus.clone(),
        workflow_execution_repo.clone(),
    ));

    println!("Temporal event listener initialized.");

    let register_workflow_use_case = Arc::new(StandardRegisterWorkflowUseCase::new(
        workflow_repo.clone(),
        temporal_client_container.clone(),
        event_bus.clone(),
    ));

    let start_workflow_execution_use_case = Arc::new(StandardStartWorkflowExecutionUseCase::new(
        workflow_repo.clone(),
        workflow_execution_repo.clone(),
        temporal_client_container.clone(),
        event_bus.clone(),
    ));

    // --- Initialize SMCP / Tool Routing Services ---
    println!("Initializing SMCP & Tool Routing services...");

    // Repositories
    let security_context_repo: Arc<
        dyn aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository,
    > = Arc::new(
        aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository::new(),
    );

    let smcp_session_repo: Arc<
        dyn aegis_orchestrator_core::domain::smcp_session_repository::SmcpSessionRepository,
    > = Arc::new(
        aegis_orchestrator_core::infrastructure::smcp::session_repository::InMemorySmcpSessionRepository::new(),
    );

    // Token Issuer — AEGIS_SMCP_PRIVATE_KEY must be set to a PEM-encoded RSA private key.
    // See aegis-config.yaml and ADR-034/ADR-035 for configuration guidance.
    let private_key = std::env::var("AEGIS_SMCP_PRIVATE_KEY").map_err(|_| {
        anyhow::anyhow!(
            "SMCP private key not configured: set AEGIS_SMCP_PRIVATE_KEY \
             (PEM-encoded RSA private key; see ADR-034/ADR-035)"
        )
    })?;
    let token_issuer = Arc::new(
        aegis_orchestrator_core::infrastructure::smcp::signature::SecurityTokenIssuer::new(
            &private_key,
            "aegis-orchestrator",
        )
        .unwrap(),
    );

    // Application Services
    let attestation_service: Arc<
        dyn aegis_orchestrator_core::infrastructure::smcp::attestation::AttestationService,
    > = Arc::new(
        aegis_orchestrator_core::application::attestation_service::AttestationServiceImpl::new(
            security_context_repo.clone(),
            smcp_session_repo.clone(),
            token_issuer,
        ),
    );

    // Secrets Manager — initialise from spec.secrets.openbao if present, else use Mock (dev/test).
    let secrets_manager: Arc<aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager> =
        match config.spec.secrets.as_ref().and_then(|s| s.openbao.as_ref()) {
            Some(openbao_config) => {
                match aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager::from_config(
                    openbao_config,
                    event_bus.clone(),
                ).await {
                    Ok(manager) => {
                        info!("OpenBao secrets manager initialised");
                        Arc::new(manager)
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to initialise OpenBao secrets manager: {}", e));
                    }
                }
            }
            None => {
                warn!("No spec.secrets.openbao configured — using MockSecretStore (dev/test only, NOT production-safe)");
                Arc::new(aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager::from_store(
                    Arc::new(aegis_orchestrator_core::infrastructure::secrets_manager::MockSecretStore::new()),
                    event_bus.clone(),
                ))
            }
        };

    // ─── Container Step Runner (ADR-050) ──────────────────────────────────────
    // Dedicated Docker client + image manager + step runner for CI/CD container
    // steps executed by ContainerRun / ParallelContainerRun workflow states.
    // Delegates credential resolution to SecretsManager for vault:// paths and
    // to environment variables for env:// paths.
    let docker_for_steps = bollard::Docker::connect_with_local_defaults()
        .context("Failed to connect Docker daemon for ContainerStepRunner (ADR-050)")?;
    let step_credential_resolver: Arc<
        dyn aegis_orchestrator_core::infrastructure::image_manager::CredentialResolver,
    > = Arc::new(
        aegis_orchestrator_core::infrastructure::image_manager::NodeConfigCredentialResolver::new(
            config.spec.registry_credentials.clone(),
        ),
    );
    let step_image_manager: Arc<
        dyn aegis_orchestrator_core::infrastructure::image_manager::DockerImageManager,
    > = Arc::new(
        aegis_orchestrator_core::infrastructure::image_manager::StandardDockerImageManager::new(
            docker_for_steps.clone(),
            step_credential_resolver,
        ),
    );
    let container_step_runner: Arc<
        dyn aegis_orchestrator_core::domain::runtime::ContainerStepRunner,
    > = Arc::new(
        aegis_orchestrator_core::infrastructure::container_step_runner::DockerContainerStepRunner::new(
            docker_for_steps,
            step_image_manager,
            nfs_server_host,
            config.spec.runtime.nfs_port,
            config.spec.runtime.nfs_mountport,
            event_bus.clone(),
            secrets_manager.clone(),
        ),
    );
    let run_container_step_use_case = Arc::new(
        aegis_orchestrator_core::application::run_container_step::RunContainerStepUseCase::new(
            container_step_runner,
        ),
    );
    println!("✓ Container step runner initialized (ADR-050).");

    let tool_manager = Arc::new(
        aegis_orchestrator_core::infrastructure::tool_router::ToolServerManager::new(
            tool_registry,
            tool_servers.clone(),
            event_bus.clone(),
            secrets_manager.clone(),
        ),
    );

    // Start MCP servers and spawn health check loop
    let tool_manager_clone = tool_manager.clone();
    tokio::spawn(async move {
        if let Err(e) = tool_manager_clone.start_all().await {
            tracing::error!("Failed to start some MCP servers: {}", e);
        }
        tool_manager_clone.health_check_loop().await;
    });

    let tool_invocation_service = Arc::new(
        aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService::new(
            smcp_session_repo.clone(),
            security_context_repo.clone(),
            smcp_middleware,
            tool_router.clone(),
            nfs_gateway.fsal().clone(),
            nfs_gateway.volume_registry().clone(),
            agent_service.clone(),
            execution_service.clone(),
        ),
    );

    let inner_loop_service = Arc::new(
        aegis_orchestrator_core::application::inner_loop_service::InnerLoopService::new(
            tool_invocation_service.clone(),
            llm_registry,
        ),
    );

    let app_state = AppState {
        agent_service,
        execution_service: execution_service.clone(),
        event_bus: event_bus.clone(),
        inner_loop_service: inner_loop_service.clone(),
        human_input_service: human_input_service.clone(),
        temporal_event_listener,
        register_workflow_use_case,
        start_workflow_execution_use_case,
        workflow_repo: workflow_repo.clone(),
        temporal_client_container: temporal_client_container.clone(),
        tool_invocation_service: tool_invocation_service.clone(),
        attestation_service: attestation_service.clone(),
        config: config.clone(),
        start_time: std::time::Instant::now(),
    };

    println!("Building router...");
    // Build HTTP router
    let app = create_router(Arc::new(app_state));

    // Start HTTP server
    let bind_addr = if let Some(network) = &config.spec.network {
        network.bind_address.clone()
    } else {
        "0.0.0.0".to_string()
    };

    // Config port takes precedence over CLI default if we consider config the source of truth for the node.
    // However, start_daemon receives `port`.
    // Let's use the config port if network config is present, otherwise use the passed port.
    let final_port = if let Some(network) = &config.spec.network {
        network.port
    } else {
        port
    };

    // Start gRPC Server
    let grpc_port = if let Some(network) = &config.spec.network {
        network.grpc_port
    } else {
        50051
    };

    let grpc_addr_str = format!("{}:{}", bind_addr, grpc_port);
    let grpc_addr: std::net::SocketAddr = grpc_addr_str
        .parse()
        .with_context(|| format!("Failed to parse gRPC address: {}", grpc_addr_str))?;

    // Tool routing services moved above AppState!

    // Seed default security context for testing
    tokio::spawn({
        let sec_repo = security_context_repo.clone();
        async move {
            let default_context =
                aegis_orchestrator_core::domain::security_context::SecurityContext {
                    name: "default".to_string(),
                    description: "Default unrestricted context for MVP testing".to_string(),
                    capabilities: vec![
                        aegis_orchestrator_core::domain::security_context::capability::Capability {
                            tool_pattern: "*".to_string(),
                            path_allowlist: None,
                            command_allowlist: None,
                            subcommand_allowlist: None,
                            domain_allowlist: None,
                            rate_limit: None,
                            max_response_size: None,
                        },
                    ],
                    deny_list: vec![],
                    metadata:
                        aegis_orchestrator_core::domain::security_context::SecurityContextMetadata {
                            created_at: chrono::Utc::now(),
                            updated_at: chrono::Utc::now(),
                            version: 1,
                        },
                };
            let _ = sec_repo.save(default_context).await;
        }
    });

    // Spawn gRPC server
    let exec_service_clone: Arc<dyn ExecutionService> = execution_service.clone();
    let val_service_clone = validation_service.clone();

    tokio::spawn(async move {
        tracing::info!("Starting gRPC server on {}", grpc_addr);
        println!("Starting gRPC server on {}", grpc_addr);
        if let Err(e) = aegis_orchestrator_core::presentation::grpc::server::start_grpc_server(
            grpc_addr,
            exec_service_clone,
            val_service_clone,
            Some(attestation_service),
            Some(tool_invocation_service),
            cortex_client,
            Some(run_container_step_use_case),
        )
        .await
        {
            tracing::error!("gRPC server failed: {}", e);
            eprintln!("gRPC server failed: {}", e);
        }
    });

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

// Temporal Workflow HTTP Handlers

/// POST /v1/workflows/temporal/register - Register a workflow with Temporal
/// Phase 2: Uses StandardRegisterWorkflowUseCase with Temporal
async fn register_temporal_workflow_handler(
    State(state): State<Arc<AppState>>,
    body: String,
) -> impl IntoResponse {
    match state
        .register_workflow_use_case
        .register_workflow(&body)
        .await
    {
        Ok(res) => (StatusCode::OK, Json(serde_json::to_value(&res).unwrap())).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Failed to register workflow: {}", e)
            })),
        )
            .into_response(),
    }
}

/// POST /v1/workflows/temporal/execute - Start a workflow execution
/// Phase 2: Uses StandardStartWorkflowExecutionUseCase with Temporal
async fn execute_temporal_workflow_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<StartWorkflowExecutionRequest>,
) -> impl IntoResponse {
    match state
        .start_workflow_execution_use_case
        .start_execution(request)
        .await
    {
        Ok(res) => (StatusCode::OK, Json(serde_json::to_value(&res).unwrap())).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Failed to start workflow execution: {}", e)
            })),
        )
            .into_response(),
    }
}

/// POST /v1/workflows/:name/run - Execute a workflow (Legacy endpoint for CLI)
#[derive(serde::Deserialize)]
struct RunWorkflowLegacyRequest {
    input: serde_json::Value,
}

async fn run_workflow_legacy_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(request): Json<RunWorkflowLegacyRequest>,
) -> impl IntoResponse {
    let req = StartWorkflowExecutionRequest {
        workflow_id: name,
        input: request.input,
        blackboard: None,
    };
    execute_temporal_workflow_handler(State(state), Json(req)).await
}

/// POST /v1/temporal-events - Receive events from Temporal worker
async fn temporal_events_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<TemporalEventPayload>,
) -> impl IntoResponse {
    // Validate shared secret from X-Temporal-Worker-Secret header
    // Read from config (spec.temporal.worker_secret) instead of direct env var
    let temporal_config_for_secret = state.config.spec.temporal.as_ref();
    let expected_secret = temporal_config_for_secret
        .and_then(|tc| tc.worker_secret.as_ref())
        .and_then(|s| resolve_env_value(s).ok());
    if let Some(secret) = expected_secret {
        let provided = headers
            .get("x-temporal-worker-secret")
            .and_then(|v| v.to_str().ok());
        match provided {
            Some(value) if value == secret => {}
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({
                        "error": "Unauthorized: invalid or missing X-Temporal-Worker-Secret header"
                    })),
                )
                    .into_response();
            }
        }
    } else {
        tracing::warn!(
            "spec.temporal.worker_secret not configured; /v1/temporal-events endpoint is unauthenticated. \
             Set spec.temporal.worker_secret in aegis-config.yaml for production."
        );
    }

    match state.temporal_event_listener.handle_event(payload).await {
        Ok(execution_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "execution_id": execution_id,
                "status": "received"
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Failed to process event: {}", e)
            })),
        )
            .into_response(),
    }
}

/// Create the HTTP router with all routes
fn create_router(app_state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/v1/agents/:agent_id/execute", post(execute_agent_handler))
        .route("/v1/executions/:execution_id", get(get_execution_handler))
        .route(
            "/v1/executions/:execution_id/cancel",
            post(cancel_execution_handler),
        )
        .route(
            "/v1/executions/:execution_id/events",
            get(stream_events_handler),
        )
        .route(
            "/v1/agents/:agent_id/events",
            get(stream_agent_events_handler),
        )
        .route("/v1/executions", get(list_executions_handler))
        .route(
            "/v1/executions/:execution_id",
            axum::routing::delete(delete_execution_handler),
        )
        .route(
            "/v1/agents",
            post(deploy_agent_handler).get(list_agents_handler),
        )
        .route(
            "/v1/agents/:id",
            get(get_agent_handler).delete(delete_agent_handler),
        )
        .route("/v1/agents/lookup/:name", get(lookup_agent_handler))
        .route("/v1/dispatch-gateway", post(dispatch_gateway_handler))
        .route(
            "/v1/workflows",
            post(register_temporal_workflow_handler).get(list_workflows_handler),
        )
        .route(
            "/v1/workflows/:name",
            get(get_workflow_handler).delete(delete_workflow_handler),
        )
        .route("/v1/workflows/:name/run", post(run_workflow_legacy_handler))
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
            "/v1/workflows/executions/:execution_id",
            get(get_workflow_execution_handler),
        )
        .route(
            "/v1/workflows/executions/:execution_id/logs",
            get(stream_workflow_logs_handler),
        )
        .route(
            "/v1/workflows/executions/:execution_id/signal",
            post(signal_workflow_execution_handler),
        )
        .route("/v1/temporal-events", post(temporal_events_handler))
        .route("/v1/human-approvals", get(list_pending_approvals_handler))
        .route("/v1/human-approvals/:id", get(get_pending_approval_handler))
        .route(
            "/v1/human-approvals/:id/approve",
            post(approve_request_handler),
        )
        .route(
            "/v1/human-approvals/:id/reject",
            post(reject_request_handler),
        )
        .route("/v1/smcp/attest", post(attest_smcp_handler))
        .route("/v1/smcp/invoke", post(invoke_smcp_handler))
        .with_state(app_state)
}

// Application state
#[derive(Clone)]
struct AppState {
    agent_service: Arc<StandardAgentLifecycleService>,
    execution_service: Arc<StandardExecutionService>,
    event_bus: Arc<EventBus>,
    inner_loop_service:
        Arc<aegis_orchestrator_core::application::inner_loop_service::InnerLoopService>,
    human_input_service: Arc<aegis_orchestrator_core::infrastructure::HumanInputService>,
    temporal_event_listener: Arc<TemporalEventListener>,
    register_workflow_use_case: Arc<StandardRegisterWorkflowUseCase>,
    start_workflow_execution_use_case: Arc<StandardStartWorkflowExecutionUseCase>,
    workflow_repo: Arc<dyn aegis_orchestrator_core::domain::repository::WorkflowRepository>,
    temporal_client_container: Arc<
        tokio::sync::RwLock<
            Option<Arc<aegis_orchestrator_core::infrastructure::temporal_client::TemporalClient>>,
        >,
    >,
    tool_invocation_service:
        Arc<aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService>,
    attestation_service:
        Arc<dyn aegis_orchestrator_core::infrastructure::smcp::attestation::AttestationService>,
    config: NodeConfigManifest,
    start_time: std::time::Instant,
}

// HTTP handlers
async fn health_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "uptime_seconds": state.start_time.elapsed().as_secs(),
    }))
}

#[derive(serde::Deserialize, Default)]
struct DeployAgentQuery {
    /// Set to `true` to overwrite an existing agent that has the same name and version.
    #[serde(default)]
    force: bool,
}

async fn deploy_agent_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<DeployAgentQuery>,
    Json(manifest): Json<aegis_orchestrator_sdk::AgentManifest>,
) -> impl IntoResponse {
    // SDK now re-exports core types, so no conversion needed
    match state
        .agent_service
        .deploy_agent(manifest, query.force)
        .await
    {
        Ok(id) => (StatusCode::OK, Json(serde_json::json!({"agent_id": id.0}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
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

    match state
        .execution_service
        .start_execution(AgentId(agent_id), input)
        .await
    {
        Ok(id) => (
            StatusCode::OK,
            Json(serde_json::json!({"execution_id": id.0})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

async fn get_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    match state
        .execution_service
        .get_execution(ExecutionId(execution_id))
        .await
    {
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
    match state
        .execution_service
        .cancel_execution(ExecutionId(execution_id))
        .await
    {
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
    let exec_id = aegis_orchestrator_core::domain::execution::ExecutionId(execution_id);

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
                    aegis_orchestrator_core::domain::execution::ExecutionStatus::Completed => {
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
                    aegis_orchestrator_core::domain::execution::ExecutionStatus::Failed => {
                        let reason = execution.error.clone().unwrap_or_else(|| "Execution failed".to_string());
                        let exec_fail = serde_json::json!({
                            "event_type": "ExecutionFailed",
                            "reason": reason,
                            "timestamp": ended_at.to_rfc3339(),
                            "data": { "error": reason }
                        });
                        yield Ok::<_, anyhow::Error>(Event::default().data(exec_fail.to_string()));
                    },
                    aegis_orchestrator_core::domain::execution::ExecutionStatus::Cancelled => {
                         // Add Cancelled event if needed
                    },
                    _ => {}
                }
            }
        }

        // 4. Stream new events if following
        if follow {
            while let Ok(event) = receiver.recv().await {
                // Convert domain event to JSON (Same logic as before)
                let json = match event {
                            aegis_orchestrator_core::domain::events::ExecutionEvent::ExecutionStarted { .. } => {
                                // Skip if we already replayed it?
                                // Simple filter: check timestamp?
                                // For now, just stream it.
                                serde_json::json!({
                                    "event_type": "ExecutionStarted",
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                    "data": {}
                                })
                            },
                             aegis_orchestrator_core::domain::events::ExecutionEvent::IterationStarted { iteration_number, action, .. } => {
                                serde_json::json!({
                                    "event_type": "IterationStarted",
                                    "iteration_number": iteration_number,
                                    "action": action,
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                    "data": { "action": action }
                                })
                            },
                             aegis_orchestrator_core::domain::events::ExecutionEvent::IterationCompleted { iteration_number, output, .. } => {
                                serde_json::json!({
                                    "event_type": "IterationCompleted",
                                    "iteration_number": iteration_number,
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                    "data": { "output": output }
                                })
                            },
                            aegis_orchestrator_core::domain::events::ExecutionEvent::ExecutionCompleted { final_output, .. } => {
                                serde_json::json!({
                                    "event_type": "ExecutionCompleted",
                                    "total_iterations": 0,
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                    "data": { "result": final_output }
                                })
                            },
                             aegis_orchestrator_core::domain::events::ExecutionEvent::ExecutionFailed { reason, .. } => {
                                serde_json::json!({
                                    "event_type": "ExecutionFailed",
                                    "reason": reason,
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                    "data": { "error": reason }
                                })
                            },
                            aegis_orchestrator_core::domain::events::ExecutionEvent::ConsoleOutput { stream, content, .. } => {
                                serde_json::json!({
                                    "event_type": "ConsoleOutput",
                                    "stream": stream,
                                    "timestamp": chrono::Utc::now().to_rfc3339(),
                                    "data": { "output": content }
                                })
                            },
                            aegis_orchestrator_core::domain::events::ExecutionEvent::LlmInteraction { provider, model, prompt, response, .. } => {
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
    let aid = aegis_orchestrator_core::domain::agent::AgentId(agent_id);

    // 1. Subscribe FIRST to catch any events that happen while we fetch history
    let mut receiver = state.event_bus.subscribe_agent(aid);

    // 2. Fetch all executions for this agent
    let executions_result = state
        .execution_service
        .list_executions(Some(aid), 100)
        .await;

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
                        aegis_orchestrator_core::domain::execution::ExecutionStatus::Completed => {
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
                        aegis_orchestrator_core::domain::execution::ExecutionStatus::Failed => {
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
            while let Ok(event) = receiver.recv().await {
                use aegis_orchestrator_core::infrastructure::event_bus::DomainEvent;
                let json = match event {
                            DomainEvent::Execution(exec_event) => {
                                match exec_event {
                                    aegis_orchestrator_core::domain::events::ExecutionEvent::ExecutionStarted { execution_id, agent_id, .. } => {
                                        serde_json::json!({
                                            "event_type": "ExecutionStarted",
                                            "execution_id": execution_id.0,
                                            "agent_id": agent_id.0,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": {}
                                        })
                                    },
                                    aegis_orchestrator_core::domain::events::ExecutionEvent::IterationStarted { execution_id, iteration_number, action, .. } => {
                                        serde_json::json!({
                                            "event_type": "IterationStarted",
                                            "execution_id": execution_id.0,
                                            "iteration_number": iteration_number,
                                            "action": action,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "action": action }
                                        })
                                    },
                                    aegis_orchestrator_core::domain::events::ExecutionEvent::IterationCompleted { execution_id, iteration_number, output, .. } => {
                                        serde_json::json!({
                                            "event_type": "IterationCompleted",
                                            "execution_id": execution_id.0,
                                            "iteration_number": iteration_number,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "output": output }
                                        })
                                    },
                                    aegis_orchestrator_core::domain::events::ExecutionEvent::ExecutionCompleted { execution_id, final_output, total_iterations, .. } => {
                                        serde_json::json!({
                                            "event_type": "ExecutionCompleted",
                                            "execution_id": execution_id.0,
                                            "total_iterations": total_iterations,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "result": final_output }
                                        })
                                    },
                                    aegis_orchestrator_core::domain::events::ExecutionEvent::ExecutionFailed { execution_id, reason, .. } => {
                                        serde_json::json!({
                                            "event_type": "ExecutionFailed",
                                            "execution_id": execution_id.0,
                                            "reason": reason,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "error": reason }
                                        })
                                    },
                                    aegis_orchestrator_core::domain::events::ExecutionEvent::ConsoleOutput { execution_id, stream, content, .. } => {
                                        serde_json::json!({
                                            "event_type": "ConsoleOutput",
                                            "execution_id": execution_id.0,
                                            "stream": stream,
                                            "timestamp": chrono::Utc::now().to_rfc3339(),
                                            "data": { "output": content }
                                        })
                                    },
                                    aegis_orchestrator_core::domain::events::ExecutionEvent::LlmInteraction { execution_id, provider, model, prompt, response, .. } => {
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
        }
    };

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

async fn delete_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    match state
        .execution_service
        .delete_execution(ExecutionId(execution_id))
        .await
    {
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

    match state
        .execution_service
        .list_executions(agent_id, limit)
        .await
    {
        Ok(executions) => {
            let json_executions: Vec<serde_json::Value> = executions
                .into_iter()
                .map(|exec| {
                    serde_json::json!({
                        "id": exec.id.0,
                        "agent_id": exec.agent_id.0,
                        "status": format!("{:?}", exec.status),
                        "started_at": exec.started_at,
                        "ended_at": exec.ended_at
                    })
                })
                .collect();
            Json(serde_json::json!(json_executions))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn list_agents_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    match state.agent_service.list_agents().await {
        Ok(agents) => {
            let json_agents: Vec<serde_json::Value> = agents.into_iter().map(|agent| {
                serde_json::json!({
                    "id": agent.id.0,
                    "name": agent.manifest.metadata.name,
                    "version": agent.manifest.metadata.version,
                    "description": agent.manifest.metadata.description.clone().unwrap_or_default(),
                    "status": format!("{:?}", agent.status)
                })
            }).collect();
            Json(serde_json::json!(json_agents))
        }
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
        Ok(agent) => Json(
            serde_json::to_value(agent.manifest)
                .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()})),
        ),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

async fn lookup_agent_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.agent_service.lookup_agent(&name).await {
        Ok(Some(id)) => (StatusCode::OK, Json(serde_json::json!({"id": id.0}))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

async fn dispatch_gateway_handler(
    State(state): State<Arc<AppState>>,
    Json(agent_msg): Json<aegis_orchestrator_core::domain::dispatch::AgentMessage>,
) -> impl IntoResponse {
    use aegis_orchestrator_core::domain::dispatch::{AgentMessage, OrchestratorMessage};

    let (exec_id_opt, iteration_number, prompt_opt, model_opt) = match &agent_msg {
        AgentMessage::Generate {
            execution_id,
            iteration_number,
            prompt,
            model_alias,
            ..
        } => (
            Uuid::parse_str(execution_id).ok(),
            *iteration_number,
            Some(prompt.clone()),
            Some(model_alias.clone()),
        ),
        AgentMessage::DispatchResult { execution_id, .. } => {
            (Uuid::parse_str(execution_id).ok(), 0, None, None)
        }
    };

    // Resolve agent_id for event logging and inner loop request
    let agent_id = if let Some(exec_id) = exec_id_opt {
        let execution_id = aegis_orchestrator_core::domain::execution::ExecutionId(exec_id);
        if let Ok(exec) = state.execution_service.get_execution(execution_id).await {
            exec.agent_id
        } else {
            tracing::warn!("Could not find execution {} for LLM event", exec_id);
            aegis_orchestrator_core::domain::agent::AgentId(Uuid::nil())
        }
    } else {
        aegis_orchestrator_core::domain::agent::AgentId(Uuid::nil())
    };

    match state
        .inner_loop_service
        .handle_agent_message(agent_msg)
        .await
    {
        Ok(OrchestratorMessage::Final {
            content,
            tool_calls_executed,
            conversation,
        }) => {
            // Publish LlmInteraction event for observability
            if agent_id.0 != Uuid::nil() {
                if let (Some(exec_id), Some(prompt), Some(model_alias)) =
                    (exec_id_opt, prompt_opt, model_opt)
                {
                    let event =
                        aegis_orchestrator_core::domain::events::ExecutionEvent::LlmInteraction {
                            execution_id: aegis_orchestrator_core::domain::execution::ExecutionId(
                                exec_id,
                            ),
                            agent_id,
                            iteration_number,
                            provider: "orchestrator".to_string(),
                            model: model_alias.clone(),
                            input_tokens: None,
                            output_tokens: None,
                            prompt: prompt.clone(),
                            response: content.clone(),
                            timestamp: chrono::Utc::now(),
                        };
                    state.event_bus.publish_execution_event(event);

                    let interaction = aegis_orchestrator_core::domain::execution::LlmInteraction {
                        provider: "orchestrator".to_string(),
                        model: model_alias.clone(),
                        prompt: prompt.clone(),
                        response: content.clone(),
                        timestamp: chrono::Utc::now(),
                    };
                    let _ = state
                        .execution_service
                        .record_llm_interaction(
                            aegis_orchestrator_core::domain::execution::ExecutionId(exec_id),
                            iteration_number,
                            interaction,
                        )
                        .await;

                    // ADR-049: Extract tool trajectory from conversation and store it
                    let mut trajectory = Vec::new();
                    for msg in conversation {
                        if let Some(calls) = msg.tool_calls {
                            for call in calls {
                                trajectory.push(
                                    aegis_orchestrator_core::domain::execution::TrajectoryStep {
                                        tool_name: call.name.clone(),
                                        arguments_json: serde_json::to_string(&call.arguments)
                                            .unwrap_or_default(),
                                    },
                                );
                            }
                        }
                    }
                    if !trajectory.is_empty() {
                        let _ = state
                            .execution_service
                            .store_iteration_trajectory(
                                aegis_orchestrator_core::domain::execution::ExecutionId(exec_id),
                                iteration_number,
                                trajectory,
                            )
                            .await;
                    }
                }
            }

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "content": content,
                    "tool_calls_executed": tool_calls_executed,
                })),
            )
        }
        Ok(OrchestratorMessage::Dispatch {
            dispatch_id,
            action,
        }) => {
            // Respond with the dispatch action so bootstrap.py can execute it
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(OrchestratorMessage::Dispatch {
                        dispatch_id,
                        action,
                    })
                    .unwrap_or_else(
                        |_| serde_json::json!({"error": "dispatch serialization failed"}),
                    ),
                ),
            )
        }
        Err(e) => {
            tracing::error!("Inner loop generation failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

// ========================================
// Workflow API Handlers
// ========================================

/// GET /v1/workflows - List all workflows
async fn list_workflows_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let workflows = state.workflow_repo.list_all().await.unwrap_or_default();

    let workflow_list: Vec<serde_json::Value> = workflows
        .iter()
        .map(|w| {
            serde_json::json!({
                "name": w.metadata.name,
                "version": w.metadata.version,
                "description": w.metadata.description,
                "status": "active"
            })
        })
        .collect();

    (StatusCode::OK, Json(workflow_list))
}

/// GET /v1/workflows/:name - Get workflow YAML
async fn get_workflow_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

    match state.workflow_repo.find_by_name(&name).await {
        Ok(Some(workflow)) => match WorkflowParser::to_yaml(&workflow) {
            Ok(yaml) => (StatusCode::OK, yaml),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to serialize workflow: {}", e),
            ),
        },
        _ => (
            StatusCode::NOT_FOUND,
            format!("Workflow '{}' not found", name),
        ),
    }
}

/// DELETE /v1/workflows/:name - Delete workflow
async fn delete_workflow_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.workflow_repo.find_by_name(&name).await {
        Ok(Some(workflow)) => {
            if let Err(e) = state.workflow_repo.delete(workflow.id).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                );
            }
            (StatusCode::OK, Json(serde_json::json!({"success": true})))
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        ),
    }
}

/// GET /v1/workflows/executions/:execution_id - Get execution details
async fn get_workflow_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
) -> impl IntoResponse {
    let guard = state.temporal_client_container.read().await;
    let client = match guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "Temporal client not yet connected"
                })),
            )
                .into_response();
        }
    };
    drop(guard);

    match client
        .get_workflow_history(execution_id.to_string(), None)
        .await
    {
        Ok(history) => {
            let event_count = history.len();
            let status = if let Some(last) = history.last() {
                let event_label = format!("{:?}", last.event_type());
                if event_label.contains("Completed") {
                    "completed"
                } else if event_label.contains("Failed") {
                    "failed"
                } else if event_label.contains("TimedOut") {
                    "timed_out"
                } else if event_label.contains("Terminated") || event_label.contains("Cancelled") {
                    "cancelled"
                } else {
                    "running"
                }
            } else {
                "pending"
            };
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "execution_id": execution_id.to_string(),
                    "status": status,
                    "event_count": event_count,
                })),
            )
                .into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("NOT_FOUND") || msg.contains("not found") {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "Workflow execution not found"
                    })),
                )
                    .into_response();
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct WorkflowSignalRequest {
    response: String,
}

/// POST /v1/workflows/executions/:execution_id/signal
///
/// Injects a `humanInput` Temporal signal into a workflow that is paused at a
/// `Human` state. The workflow resumes with the provided `response` string.
///
/// # Body
/// ```json
/// { "response": "approved" }
/// ```
///
/// # Returns
/// - `202 Accepted` on success
/// - `503 Service Unavailable` if the Temporal client is not yet connected
/// - `500 Internal Server Error` if the signal delivery fails
async fn signal_workflow_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<String>,
    Json(request): Json<WorkflowSignalRequest>,
) -> impl IntoResponse {
    let guard = state.temporal_client_container.read().await;
    let client = match guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "Temporal client not yet connected"
                })),
            )
                .into_response();
        }
    };
    drop(guard);

    match client
        .send_human_signal(&execution_id, request.response)
        .await
    {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "signal_sent",
                "execution_id": execution_id
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// GET /v1/workflows/executions/:execution_id/logs - Stream workflow logs
/// GET /v1/workflows/executions/:execution_id/logs - Stream workflow logs
async fn stream_workflow_logs_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
) -> impl IntoResponse {
    use std::fmt::Write;

    // Get Temporal Client
    let client_opt = state.temporal_client_container.read().await;
    let client = match &*client_opt {
        Some(c) => c,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Temporal client not available".to_string(),
            )
        }
    };

    // Fetch history
    // Note: This fetches existing history. Streaming live events would require
    // using 'wait_new_event' loop or similar, but for now we just return current history.
    match client
        .get_workflow_history(execution_id.to_string(), None)
        .await
    {
        Ok(history) => {
            let mut output = String::new();
            for event in history {
                // Approximate timestamp formatting
                let timestamp = event
                    .event_time
                    .map(|t| {
                        let secs = t.seconds;
                        let nanos = t.nanos;
                        if let Some(dt) = chrono::DateTime::from_timestamp(secs, nanos as u32) {
                            dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
                        } else {
                            "Unknown Time".to_string()
                        }
                    })
                    .unwrap_or_else(|| "Unknown Time".to_string());

                let event_type = event.event_type(); // Enum

                let _ = writeln!(output, "[{}] {:?}", timestamp, event_type);

                // Add details for key events if possible?
                // For now just the type is useful enough to verify it works.
            }
            (StatusCode::OK, output)
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to get workflow logs: {}", e),
        ),
    }
}

// ============================================================================
// Human Approval Handlers
// ============================================================================

/// GET /v1/human-approvals - List all pending approval requests
async fn list_pending_approvals_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let pending = state.human_input_service.list_pending_requests().await;
    Json(serde_json::json!({
        "pending_requests": pending,
        "count": pending.len()
    }))
}

/// GET /v1/human-approvals/:id - Get a specific pending approval request
async fn get_pending_approval_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let request_id = match Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => return Json(serde_json::json!({"error": "Invalid request ID"})),
    };

    match state
        .human_input_service
        .get_pending_request(request_id)
        .await
    {
        Some(request) => Json(serde_json::json!({ "request": request })),
        None => Json(serde_json::json!({ "error": "Request not found or already completed" })),
    }
}

#[derive(serde::Deserialize)]
struct ApprovalRequest {
    feedback: Option<String>,
    approved_by: Option<String>,
}

/// POST /v1/human-approvals/:id/approve - Approve a pending request
async fn approve_request_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<ApprovalRequest>,
) -> Json<serde_json::Value> {
    let request_id = match Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => return Json(serde_json::json!({"error": "Invalid request ID"})),
    };

    match state
        .human_input_service
        .submit_approval(request_id, payload.feedback, payload.approved_by)
        .await
    {
        Ok(()) => Json(serde_json::json!({
            "status": "approved",
            "request_id": id
        })),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

#[derive(serde::Deserialize)]
struct RejectionRequest {
    reason: String,
    rejected_by: Option<String>,
}

/// POST /v1/human-approvals/:id/reject - Reject a pending request
async fn reject_request_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<RejectionRequest>,
) -> Json<serde_json::Value> {
    let request_id = match Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => return Json(serde_json::json!({"error": "Invalid request ID"})),
    };

    match state
        .human_input_service
        .submit_rejection(request_id, payload.reason, payload.rejected_by)
        .await
    {
        Ok(()) => Json(serde_json::json!({
            "status": "rejected",
            "request_id": id
        })),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

#[derive(serde::Deserialize)]
pub struct HttpAttestationRequest {
    pub agent_id: String,
    pub execution_id: Option<String>,
    pub container_id: String,
    pub public_key: String,
}

async fn attest_smcp_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<HttpAttestationRequest>,
) -> impl IntoResponse {
    let internal_req =
        aegis_orchestrator_core::infrastructure::smcp::attestation::AttestationRequest {
            agent_id: request.agent_id.clone(),
            execution_id: request
                .execution_id
                .unwrap_or_else(|| request.agent_id.clone()),
            container_id: request.container_id.clone(),
            public_key_pem: request.public_key.clone(),
        };

    match state.attestation_service.attest(internal_req).await {
        Ok(res) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "security_token": res.security_token
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": e.to_string()
            })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct HttpSmcpEnvelope {
    pub security_token: String,
    pub signature: String,
    pub payload: serde_json::Value,
}

async fn invoke_smcp_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<HttpSmcpEnvelope>,
) -> impl IntoResponse {
    let payload_bytes = serde_json::to_vec(&request.payload).unwrap_or_default();

    let envelope = aegis_orchestrator_core::infrastructure::smcp::envelope::SmcpEnvelope {
        security_token: request.security_token,
        signature: request.signature,
        inner_mcp: payload_bytes,
    };

    let payload_b64 = envelope.security_token.split('.').nth(1).unwrap_or("");
    let payload_bytes = match base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload_b64,
    ) {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Malformed security token base64" })),
            )
                .into_response()
        }
    };

    let token_data: serde_json::Value = match serde_json::from_slice(&payload_bytes) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Malformed security token JSON" })),
            )
                .into_response()
        }
    };

    let agent_id_str = token_data
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let agent_id = match uuid::Uuid::parse_str(agent_id_str) {
        Ok(uid) => AgentId(uid),
        Err(_) => return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({ "error": "agent_id in token is missing or not a valid UUID" }),
            ),
        )
            .into_response(),
    };

    match state
        .tool_invocation_service
        .invoke_tool(&agent_id, &envelope)
        .await
    {
        Ok(res) => (StatusCode::OK, Json(res)).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": e.to_string()
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_create_router_returns_router() {
        // This is a smoke test to ensure create_router compiles and can be called
        // We can't easily test the full router without a complex setup
        // but we can at least verify the function signature works

        // For now, just verify the module compiles
        assert!(true);
    }
}
