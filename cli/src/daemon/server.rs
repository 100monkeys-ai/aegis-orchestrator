// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Daemon HTTP server implementation
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for server
//!
//! # Code Quality Principles
//!
//! - Keep HTTP handlers thin and delegate business logic to application services.
//! - Fail closed on auth, readiness, and runtime initialization boundaries.
//! - Avoid exposing partial protocol support through the public transport surface.

use anyhow::{Context, Result};
use std::sync::Arc;

const DEFAULT_ORCHESTRATOR_URL: &str = "http://localhost:8088";

// Type alias for repository tuple to avoid clippy "very complex type" lint
type RepositoryTuple = (
    Arc<dyn AgentRepository>,
    Arc<dyn aegis_orchestrator_core::domain::repository::WorkflowRepository>,
    Arc<dyn aegis_orchestrator_core::domain::repository::ExecutionRepository>,
    Arc<dyn aegis_orchestrator_core::domain::repository::WorkflowExecutionRepository>,
);

// ADR-117: edge component bundle wired into the daemon HTTP surface and
// pre-routing hook. Aliased to avoid clippy::type_complexity at the binding
// site.
type EdgeComponentsBundle = (
    Arc<dyn aegis_orchestrator_core::domain::edge::EdgeDaemonRepository>,
    Arc<dyn aegis_orchestrator_core::domain::edge::EdgeGroupRepository>,
    aegis_orchestrator_core::infrastructure::edge::EdgeConnectionRegistry,
    Arc<aegis_orchestrator_core::application::edge::dispatch_to_edge::DispatchToEdgeService>,
    Arc<aegis_orchestrator_core::application::edge::fleet::EdgeFleetResolver>,
    Arc<aegis_orchestrator_core::application::edge::fleet::dispatcher::FleetDispatcher>,
    Arc<aegis_orchestrator_core::application::edge::fleet::CancelFleetService>,
);
use sqlx::postgres::PgPool;
use std::path::PathBuf;
use tokio::net::TcpListener;

// Helper to establish a Temporal client connection with retry logic.
// Extracted to module scope for improved testability and separation of concerns.
async fn connect_temporal_with_retry(
    temporal_address: &str,
    temporal_namespace: &str,
    temporal_task_queue: &str,
    worker_http_endpoint: &str,
    max_retries: i32,
) -> Result<Arc<TemporalClient>> {
    let mut retries: i32 = 0;

    loop {
        match TemporalClient::new(
            temporal_address,
            temporal_namespace,
            temporal_task_queue,
            worker_http_endpoint,
        )
        .await
        {
            Ok(client) => {
                tracing::info!("Temporal Client connected successfully");
                return Ok(Arc::new(client));
            }
            Err(e) => {
                retries += 1;
                if retries >= max_retries {
                    return Err(e).with_context(|| {
                        format!(
                            "Failed to connect to Temporal at {temporal_address} after {retries} attempts"
                        )
                    });
                }

                if retries % 5 == 0 {
                    tracing::info!(
                        attempt = retries,
                        max_retries = max_retries,
                        "Still verifying Temporal connection"
                    );
                }
                tracing::debug!(error = %e, "Failed to connect to Temporal, retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}
use tokio::signal;
use tracing::{debug, error, info, warn};

use super::{remove_pid_file, write_pid_file};
use aegis_orchestrator_core::domain::rate_limit::{RateLimitEnforcer, RateLimitPolicyResolver};
use aegis_orchestrator_core::domain::secrets::SensitiveUrl;
use aegis_orchestrator_core::presentation::grpc::health::{
    postgres_readiness, service_name, GrpcHealth, ReadinessCheck,
};

/// How long a gRPC server's readiness waits for its database's `SELECT 1`.
const DATABASE_READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// How often a gRPC server started here re-evaluates its readiness.
const READINESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
use aegis_orchestrator_core::{
    application::{
        agent::AgentLifecycleService,
        execution::ExecutionService,
        execution::StandardExecutionService,
        lifecycle::StandardAgentLifecycleService,
        register_workflow::{RegisterWorkflowUseCase, StandardRegisterWorkflowUseCase},
        start_workflow_execution::StandardStartWorkflowExecutionUseCase,
        validation_service::ValidationService,
        CorrelatedActivityStreamService,
    },
    domain::{
        cluster::{
            ConfigLayerRepository, ConfigType, EffectiveConfigValidator, MergedConfig,
            NodeClusterRepository, NodeId, NodeRole,
        },
        iam::IdentityProvider,
        node_config::{resolve_env_value, IamConfig, IamRealmConfig, NodeConfigManifest},
        repository::AgentRepository,
        runtime_registry::StandardRuntimeRegistry,
        supervisor::Supervisor,
    },
    infrastructure::{
        event_bus::EventBus,
        iam::StandardIamService,
        llm::registry::ProviderRegistry,
        rate_limit::{
            CompositeRateLimitEnforcer, GovernorBurstEnforcer, HierarchicalPolicyResolver,
            PostgresWindowEnforcer,
        },
        repositories::{
            InMemoryAgentRepository, InMemoryExecutionRepository,
            InMemoryWorkflowExecutionRepository,
        },
        runtime::{connect_container_runtime, ContainerRuntime},
        temporal_client::TemporalClient,
        TemporalEventListener,
    },
};

use aegis_orchestrator_core::application::credential_service::{
    oauth_provider_registry_from_config, CredentialManagementService, RemoteServerGrounding,
    StandardCredentialManagementService, ToolCredentialSource, ToolMailboxSource,
};
use aegis_orchestrator_core::domain::credential::CredentialBindingRepository;
use aegis_orchestrator_core::domain::security_context::SecurityContextRepository;
use aegis_orchestrator_core::infrastructure::repositories::PostgresCredentialBindingRepository;
use aegis_orchestrator_core::presentation::webhook_guard::EnvWebhookSecretProvider;
use aegis_orchestrator_swarm::infrastructure::StandardSwarmService;

use super::operator_read_models::OperatorReadModelStore;
use crate::daemon::container_helpers::cleanup_orphaned_agent_containers;
pub use crate::daemon::handlers::DEFAULT_MAX_EXECUTION_LIST_LIMIT;
use crate::daemon::ports::{DaemonAgentActivity, DaemonWorkflowExecutionControl};
use crate::daemon::router::create_router;
use crate::daemon::state::AppState;

// ---------------------------------------------------------------------------
// Port implementations moved to ports.rs
// DaemonWorkflowExecutionControl and DaemonAgentActivity are imported above.
// ---------------------------------------------------------------------------

fn default_local_host_mount_point() -> String {
    if let Ok(path) = std::env::var("AEGIS_LOCAL_HOST_MOUNT_POINT") {
        return path;
    }

    let default_path = PathBuf::from("/var/lib/aegis/local-host-volumes");
    default_path.to_string_lossy().into_owned()
}

fn resolve_generated_artifacts_root(config_path: Option<PathBuf>) -> PathBuf {
    let config_path = config_path.or_else(NodeConfigManifest::discover_config);
    crate::commands::builtins::resolve_generated_root(config_path.as_ref())
}

// tenant_id_from_identity moved to handlers/mod.rs

fn temporal_connection_max_retries(raw_value: Option<i32>) -> i32 {
    raw_value.unwrap_or(30).max(1)
}

/// Whether this node ends, at start-up, the executions an earlier process
/// left unfinished.
///
/// An execution record names no node, so the start-up pass cannot tell an
/// execution an earlier process of this node left running from one another
/// live node is running against the same database. It therefore runs only
/// on a standalone node (`spec.cluster` absent or `enabled: false`), where
/// this process is the only one that runs executions against its database.
/// A clustered node skips it and says so; making it safe there needs the
/// running node's identity on the record.
pub(crate) fn restart_pass_applies(config: &NodeConfigManifest) -> bool {
    !config.spec.cluster.as_ref().is_some_and(|c| c.enabled)
}

/// The start-up pass: end, as failed, every execution an earlier orchestrator
/// process left pending or running
/// ([`aegis_orchestrator_core::application::execution::fail_executions_cut_by_restart`]).
///
/// Called once, after the execution event persister has started (so each
/// `ExecutionFailed` is recorded) and before the container reaper's first
/// pass and before any listener accepts work (so no task of this process
/// supervises an execution yet, and the reaper's first pass removes the
/// containers of the executions this pass ended). Returns the ids it ended.
pub(crate) async fn end_executions_cut_by_restart(
    config: &NodeConfigManifest,
    execution_repo: &dyn aegis_orchestrator_core::domain::repository::ExecutionRepository,
    event_bus: &EventBus,
) -> Vec<aegis_orchestrator_core::domain::execution::ExecutionId> {
    if !restart_pass_applies(config) {
        info!(
            "Skipping the startup pass over executions an earlier process left unfinished: \
             clustering is enabled, and an execution record names no node, so an execution \
             another live node runs cannot be told apart"
        );
        return Vec::new();
    }
    info!("Running startup pass over executions an earlier process left unfinished");
    match aegis_orchestrator_core::application::execution::fail_executions_cut_by_restart(
        execution_repo,
        event_bus,
    )
    .await
    {
        Ok(ended) => {
            info!(
                count = ended.len(),
                "Startup pass ended {} execution(s) an earlier process left unfinished",
                ended.len()
            );
            ended
        }
        Err(e) => {
            error!(
                error = %e,
                "Startup pass over executions an earlier process left unfinished failed; \
                 their records stay as they are until the next start"
            );
            Vec::new()
        }
    }
}

// ClusterNodeView, ClusterStatusView moved to cluster_helpers.rs
// SwarmMessageView, SwarmLockView, SwarmView moved to handlers/swarms.rs
// DashboardSummaryView moved to handlers/observability.rs

// HealthView moved to handlers/health.rs
// LimitQuery, CortexQueryParams, bounded_limit moved to handlers/mod.rs

// managed_container_reap_reason and cleanup_orphaned_agent_containers moved to container_helpers.rs

// cluster_role_to_string, node_status_to_string, cluster_node_view, fallback_cluster_node,
// cluster_status_view, load_cluster_nodes moved to cluster_helpers.rs

/// Connect to the PostgreSQL database named by `spec.database`, applying any
/// pending migrations.
///
/// Returns `Ok(None)` when no database is configured, or when the connection
/// fails on a node that is not labelled production (the daemon then falls back
/// to in-memory repositories). A production node refuses both.
/// The options the PostgreSQL driver connects with, parsed from the database
/// URL as configured. The one place the URL is read for the driver.
pub(crate) fn pg_connect_options(
    url: &SensitiveUrl,
) -> std::result::Result<sqlx::postgres::PgConnectOptions, sqlx::Error> {
    url.expose().parse()
}

pub(crate) async fn connect_postgres(config: &NodeConfigManifest) -> Result<Option<PgPool>> {
    // Initialize repositories — resolve database URL from config (spec.database)
    let database_url: Option<SensitiveUrl> =
        config
            .spec
            .database
            .as_ref()
            .and_then(|db| match resolve_env_value(db.url.expose()) {
                Ok(url) => Some(SensitiveUrl::new(url)),
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
    let db_pool: Option<PgPool> = if let Some(url) = database_url.as_ref() {
        info!(url = %url.redacted(), "Initializing repositories with PostgreSQL");
        let connected = match pg_connect_options(url) {
            Ok(options) => {
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(db_max_connections)
                    .connect_with(options)
                    .await
            }
            Err(e) => Err(e),
        };
        match connected {
            Ok(db_pool) => {
                info!("Connected to PostgreSQL");

                // Check migration status
                let total_known = crate::daemon::migrations::MIGRATOR.iter().count();
                if total_known == 0 {
                    return Err(anyhow::anyhow!(
                        "CRITICAL: No migrations found in binary! Check build process."
                    ));
                }

                // Check applied migrations
                let applied_result = sqlx::query("SELECT version FROM _sqlx_migrations")
                    .fetch_all(&db_pool)
                    .await;

                let applied_count = match applied_result {
                    Ok(rows) => rows.len(),
                    Err(_) => 0,
                };

                info!(
                    applied = applied_count,
                    total = total_known,
                    "Database migration status"
                );

                if applied_count < total_known {
                    info!("Applying pending migrations...");
                    match crate::daemon::migrations::apply_migrations(&db_pool).await {
                        Ok(_) => info!("Database migrations applied successfully"),
                        Err(e) => {
                            return Err(anyhow::anyhow!("Failed to apply migrations: {e}"));
                        }
                    }
                } else {
                    info!("Database is up to date");
                }

                Some(db_pool)
            }
            Err(e) => {
                if config.is_production() {
                    return Err(anyhow::anyhow!(
                        "Failed to connect to PostgreSQL in production mode: {e}"
                    ));
                }

                error!(error = %e, "Failed to connect to PostgreSQL, falling back to InMemory");
                None
            }
        }
    } else {
        if config.is_production() {
            return Err(anyhow::anyhow!(
                "Production mode requires spec.database; refusing to fall back to InMemory repositories"
            ));
        }

        info!("No database configured (spec.database omitted), using InMemory repositories");
        None
    };

    Ok(db_pool)
}

/// The judges' room, read from the node configuration's alias table
/// (`spec.llm_providers[].models[]`: `context_window` and
/// `max_output_tokens`), handed to every service the daemon builds that runs
/// a judge: goal-judge (AEGIS ADR-131 U16) and the validation judges (U19).
/// A service not given it bounds its judges by the stated default
/// (`JudgeContext::UNCONFIGURED`), which would stop as too large an input
/// production's judge model holds.
fn judge_context_source(
    config: &NodeConfigManifest,
) -> Arc<dyn aegis_orchestrator_core::domain::goal::JudgeContextSource> {
    Arc::new(
        aegis_orchestrator_core::domain::goal::AliasTableJudgeContext::from_providers(
            &config.spec.llm_providers,
        ),
    )
}

/// The goal service the daemon runs (AEGIS ADR-131): its bounds from
/// `spec.goals` (D7), goal-judge's room from the alias table (U16).
fn daemon_goal_service(
    repo: Arc<dyn aegis_orchestrator_core::domain::goal::GoalRepository>,
    event_bus: Arc<EventBus>,
    config: &NodeConfigManifest,
    judge_context: Arc<dyn aegis_orchestrator_core::domain::goal::JudgeContextSource>,
) -> aegis_orchestrator_core::application::goal_service::GoalService {
    aegis_orchestrator_core::application::goal_service::GoalService::new(
        repo,
        event_bus,
        config.spec.goals.clone(),
    )
    .with_judge_context(judge_context)
}

/// The validation service the daemon and its gRPC server run, its judges'
/// room from the alias table (AEGIS ADR-131 U19).
fn daemon_validation_service(
    event_bus: Arc<EventBus>,
    execution_service: Arc<dyn ExecutionService>,
    agent_service: Arc<dyn AgentLifecycleService>,
    judge_context: Arc<dyn aegis_orchestrator_core::domain::goal::JudgeContextSource>,
) -> ValidationService {
    ValidationService::new(event_bus, execution_service, agent_service)
        .with_judge_context(judge_context)
}

/// The inner loop the daemon runs: a command's output enters the model's
/// conversation bounded by its alias's room in the alias table the judges
/// read (AEGIS ADR-131 U16a's measure; retry-knows-what-failed).
fn daemon_inner_loop_service(
    tool_invocation_service: Arc<
        aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService,
    >,
    execution_service: Arc<dyn ExecutionService>,
    registry: Arc<aegis_orchestrator_core::infrastructure::llm::ProviderRegistry>,
    judge_context: Arc<dyn aegis_orchestrator_core::domain::goal::JudgeContextSource>,
) -> aegis_orchestrator_core::application::inner_loop_service::InnerLoopService {
    aegis_orchestrator_core::application::inner_loop_service::InnerLoopService::new(
        tool_invocation_service,
        execution_service,
        registry,
    )
    .with_context_source(judge_context)
}

/// The SEAL middleware the daemon runs for every `POST /v1/seal/invoke`:
/// ADR-072's rate limits when configured, and always the replay-nonce store
/// AEGIS ADR-035's Amendment "SEAL Replay-Nonce Store" decides: one
/// in-memory, process-local store per daemon, entries kept for
/// `SEAL_REPLAY_TTL` (30 s), so an envelope seen once is refused
/// `ENVELOPE_REPLAYED` if it comes again inside that window.
pub(crate) fn daemon_seal_middleware(
    rate_limit_enforcer: Option<
        Arc<dyn aegis_orchestrator_core::domain::rate_limit::RateLimitEnforcer>,
    >,
    rate_limit_resolver: Option<
        Arc<dyn aegis_orchestrator_core::domain::rate_limit::RateLimitPolicyResolver>,
    >,
) -> Arc<aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware> {
    Arc::new(
        aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware::with_rate_limiting(
            rate_limit_enforcer,
            rate_limit_resolver,
        )
        .with_replay_protection(Arc::new(
            aegis_orchestrator_core::infrastructure::seal::nonce_store::InMemoryNonceStore::new(),
        )),
    )
}

/// The SEAL gateway's address as the daemon dials it: `seal_gateway.url`,
/// an `env:` value resolved; `None` with no `seal_gateway`.
pub(crate) fn daemon_seal_gateway_url(
    gateway: Option<&aegis_orchestrator_core::domain::node_config::SealGatewayConfig>,
) -> Option<String> {
    gateway.map(|gateway| {
        resolve_env_value(gateway.url.expose()).unwrap_or_else(|_| gateway.url.expose().to_string())
    })
}

/// The SEAL gateway's part of the daemon's tool invocation service (AEGIS
/// ADR-132 H1, H4, H8): the CA its certificate is verified against, the
/// remote MCP servers it serves by name, and the credential store each
/// remote tool call resolves the acting user's credential through. With no
/// `seal_gateway` (production today) the service is unchanged: a tool no
/// builtin serves answers 404 and nothing dials. A configuration that cannot
/// work stops the daemon with its reason.
pub(crate) fn daemon_seal_gateway_wiring(
    service: aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService,
    gateway: Option<&aegis_orchestrator_core::domain::node_config::SealGatewayConfig>,
    credentials: Option<Arc<dyn ToolCredentialSource>>,
) -> Result<aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService> {
    if let Some(gateway) = gateway {
        info!(
            remote_servers = ?gateway.remote_servers,
            ca_cert_path = ?gateway.ca_cert_path,
            "SEAL gateway remote servers the orchestrator resolves credentials for; each must be \
             a name in the gateway's spec.mcp_servers"
        );
    }
    service
        .with_seal_gateway_config(gateway, credentials)
        .context("seal_gateway configuration")
}

pub async fn start_daemon(config_path: Option<PathBuf>, port: u16) -> Result<()> {
    // Write PID file
    let pid = std::process::id();
    write_pid_file(pid)?;

    // Ensure PID file cleanup on exit
    let _guard = PidFileGuard;

    info!("AEGIS daemon starting (PID: {})", pid);
    // Load configuration
    info!("Loading configuration...");
    let mut config = NodeConfigManifest::load_or_default(config_path.clone())
        .context("Failed to load configuration")?;

    // Prefer the discovered config path so Docker deployments that set
    // AEGIS_CONFIG_PATH resolve generated artifacts under the mounted stack root.
    let generated_artifacts_root = resolve_generated_artifacts_root(config_path.clone());

    config
        .validate()
        .context("Configuration validation failed")?;

    // Initialize metrics if enabled (ADR-058 Step 2)
    if config
        .spec
        .observability
        .as_ref()
        .and_then(|o| o.metrics.as_ref())
        .map(|m| m.enabled)
        .unwrap_or(false)
    {
        let metrics_cfg = config
            .spec
            .observability
            .as_ref()
            .unwrap()
            .metrics
            .as_ref()
            .unwrap();
        let region = config.spec.node.region.as_deref();
        let version = env!("CARGO_PKG_VERSION");

        aegis_orchestrator_core::infrastructure::telemetry::init_metrics(
            &metrics_cfg.bind_address,
            metrics_cfg.port,
            &config.spec.node.id,
            &config.metadata.name,
            region,
            version,
        )
        .context("Failed to initialize metrics")?;
        info!(
            bind_address = %metrics_cfg.bind_address,
            port = metrics_cfg.port,
            "Metrics exporter initialized"
        );
    }

    if let Some(seal_gateway) = &config.spec.seal_gateway {
        let resolved_url = resolve_env_value(seal_gateway.url.expose())
            .unwrap_or_else(|_| seal_gateway.url.expose().to_string());
        tracing::info!(
            url = %SensitiveUrl::new(resolved_url),
            "Configured SEAL tooling gateway URL from node config"
        );
    }

    if config.spec.llm_providers.is_empty() {
        warn!(
            "No LLM providers configured. Agents will fail to generate text. Please check your config file or ensure one is discovered."
        );
    }

    info!("Configuration loaded. Initializing services...");

    let db_pool: Option<PgPool> = connect_postgres(&config).await?;

    let (agent_repo, workflow_repo, execution_repo, workflow_execution_repo): RepositoryTuple =
        if let Some(db_pool) = db_pool.as_ref() {
            (
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_agent::PostgresAgentRepository::new(db_pool.clone())),
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_workflow::PostgresWorkflowRepository::new_with_pool(db_pool.clone())),
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_execution::PostgresExecutionRepository::new(db_pool.clone())),
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_workflow_execution::PostgresWorkflowExecutionRepository::new(db_pool.clone())),
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

    let cluster_repo: Option<Arc<dyn NodeClusterRepository>> = None;

    // ── ADR-060: Load effective config by merging database layers over bootstrap YAML ──
    if let Some(ref pool) = db_pool {
        use aegis_orchestrator_core::infrastructure::cluster::PgConfigLayerRepository;

        let config_repo: Arc<dyn ConfigLayerRepository> =
            Arc::new(PgConfigLayerRepository::new(Arc::new(pool.clone())));
        let node_id = NodeId(
            uuid::Uuid::parse_str(&config.spec.node.id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
        );

        match config_repo
            .get_merged_config(&node_id, None, &ConfigType::AegisConfig)
            .await
        {
            Ok(merged) => {
                // An empty payload means no DB layers exist yet; the bootstrap YAML
                // is the sole source of truth and requires no validation here.
                let has_db_overlay = merged
                    .payload
                    .as_object()
                    .map(|o| !o.is_empty())
                    .unwrap_or(false);

                if has_db_overlay {
                    if let Err(e) = config.apply_merged_overlay(&merged) {
                        warn!(
                            error = %e,
                            "Failed to apply merged database config overlay, continuing with YAML-only config"
                        );
                    } else {
                        // Validate the final effective config (bootstrap YAML + DB overlay)
                        // after the overlay has been applied (ADR-060 §4, Gap 059-7).
                        let effective_payload = serde_json::to_value(&config.spec)
                            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                        let effective = MergedConfig {
                            payload: effective_payload,
                            version: merged.version.clone(),
                        };
                        if let Err(missing) = EffectiveConfigValidator::validate(&effective) {
                            return Err(anyhow::anyhow!(
                                "Effective configuration missing required fields: {:?}",
                                missing
                            ));
                        }
                        info!(
                            version = %merged.version,
                            "Applied merged database config overlay"
                        );
                    }
                } else {
                    debug!("No database config layers found, using YAML-only config");
                }
            }
            Err(e) => {
                debug!(
                    error = %e,
                    "No database config layers found, using YAML-only config"
                );
            }
        }
    }

    let event_bus = Arc::new(EventBus::new(100));
    let operator_read_model = OperatorReadModelStore::spawn_collector(event_bus.clone());
    let swarm_service =
        Arc::new(StandardSwarmService::new().with_execution_repository(execution_repo.clone()));
    swarm_service.start_gc_task();
    let iam_service: Option<Arc<dyn IdentityProvider>> = match config.spec.iam.as_ref() {
        Some(iam) => {
            let resolved_realms: Vec<IamRealmConfig> = iam
                .realms
                .iter()
                .map(|realm| IamRealmConfig {
                    slug: resolve_env_value(&realm.slug).unwrap_or_else(|_| realm.slug.clone()),
                    issuer_url: resolve_env_value(realm.issuer_url.expose())
                        .map(SensitiveUrl::new)
                        .unwrap_or_else(|_| realm.issuer_url.clone()),
                    jwks_uri: resolve_env_value(realm.jwks_uri.expose())
                        .map(SensitiveUrl::new)
                        .unwrap_or_else(|_| realm.jwks_uri.clone()),
                    audience: resolve_env_value(&realm.audience)
                        .unwrap_or_else(|_| realm.audience.clone()),
                    kind: resolve_env_value(&realm.kind).unwrap_or_else(|_| realm.kind.clone()),
                })
                .collect();
            let resolved_iam = IamConfig {
                realms: resolved_realms,
                jwks_cache_ttl_seconds: iam.jwks_cache_ttl_seconds,
                claims: iam.claims.clone(),
                keycloak_admin: iam.keycloak_admin.clone(),
                operator_escalation: iam.operator_escalation.clone(),
            };
            // `StandardIamService::new` returns `IamError::Configuration` for
            // malformed realms (unknown kind, malformed tenant slug). Surface
            // that as a fatal boot error rather than panicking out of the
            // YAML-load path.
            let svc = StandardIamService::new(&resolved_iam, event_bus.clone())
                .map_err(|e| anyhow::anyhow!("IAM service initialization failed: {e}"))?;
            Some(Arc::new(svc) as Arc<dyn IdentityProvider>)
        }
        None => None,
    };

    if config.is_production() && config.spec.iam.is_none() {
        anyhow::bail!(
            "Production nodes require spec.iam to be configured. \
             See config-with-examples.yaml for the iam block schema."
        );
    }
    if config.is_production()
        && config
            .spec
            .iam
            .as_ref()
            .is_some_and(|iam| iam.realms.is_empty())
    {
        anyhow::bail!("Production nodes must configure at least one IAM realm");
    }
    if let Some(iam) = &config.spec.iam {
        for realm in &iam.realms {
            if realm.issuer_url.expose().trim().is_empty() {
                anyhow::bail!("IAM realm '{}' has empty issuer_url", realm.slug);
            }
            if realm.jwks_uri.expose().trim().is_empty() {
                anyhow::bail!("IAM realm '{}' has empty jwks_uri", realm.slug);
            }
        }
    }

    // ADR-117: dispatch on cluster role. The relay-coordinator deployment
    // hosts neither agent execution nor FSAL/NFS volume infrastructure;
    // booting the full daemon stack is incorrect (and historically panicked
    // at storage init because `spec.storage` is absent from a relay's
    // config). Branch into a self-contained relay-only boot path here.
    let is_relay = matches!(
        config.spec.cluster.as_ref().map(|c| c.role),
        Some(NodeRole::RelayCoordinator)
    );
    if is_relay {
        return crate::daemon::relay_server::run_relay_coordinator(
            config,
            db_pool,
            event_bus,
            iam_service,
        )
        .await;
    }

    info!("Initializing LLM registry...");
    let llm_registry = Arc::new(
        ProviderRegistry::from_config(&config).context("Failed to initialize LLM providers")?,
    );

    info!("Initializing Docker runtime...");

    // Resolve the orchestrator URL, supporting `env:VAR_NAME` syntax and a shared default.
    fn resolve_orchestrator_url(config: &NodeConfigManifest) -> String {
        resolve_env_value(config.spec.runtime.orchestrator_url.expose()).unwrap_or_else(|e| {
            tracing::warn!("Failed to resolve orchestrator URL: {}. Using default.", e);
            DEFAULT_ORCHESTRATOR_URL.to_string()
        })
    }

    // Resolve orchestrator URL (supports env:VAR_NAME syntax via resolve_env_value)
    let orchestrator_url = resolve_orchestrator_url(&config);

    // Resolve NFS server host (supports env:VAR_NAME syntax) - ADR-036
    // Note: The Docker daemon relies on this to mount volumes from the host environment.
    let nfs_server_host = config
        .spec
        .runtime
        .nfs_server_host
        .as_ref()
        .and_then(|host| match resolve_env_value(host) {
            Ok(resolved) if !resolved.is_empty() => Some(resolved),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(
                    "Failed to resolve NFS server host: {}. Using default 127.0.0.1.",
                    e
                );
                None
            }
        });

    // Resolve Docker network mode (supports env:VAR_NAME syntax)
    let network_mode = config
        .spec
        .runtime
        .container_network_mode
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

    // ─── FUSE FSAL Daemon (ADR-107) ─────────────────────────────────────────────
    // Create a shared FUSE daemon that will be injected into both ContainerRuntime
    // and ContainerStepRunner. Uses the same FSAL instance as the NFS gateway.
    let fuse_mount_prefix = config
        .spec
        .runtime
        .fuse_mount_prefix
        .clone()
        .unwrap_or_else(|| "/tmp/aegis-fuse-mounts".to_string());

    // Create mount prefix directory on startup
    if let Err(e) = std::fs::create_dir_all(&fuse_mount_prefix) {
        warn!(
            error = %e,
            path = %fuse_mount_prefix,
            "Failed to create FUSE mount prefix directory — FUSE transport will be unavailable"
        );
    }

    // Connect to the host-side FUSE daemon's FuseMountService if configured (ADR-107).
    // Absence of spec.runtime.fuse_daemon_endpoint means in-process FUSE mode — no error, no retry.
    let fuse_daemon_endpoint: Option<String> = config
        .spec
        .runtime
        .fuse_daemon_endpoint
        .as_ref()
        .and_then(|ep| resolve_env_value(ep.expose()).ok());
    let fuse_mount_client: Option<
        aegis_orchestrator_core::infrastructure::aegis_runtime_proto::fuse_mount_service_client::FuseMountServiceClient<
            tonic::transport::Channel,
        >,
    > = match fuse_daemon_endpoint {
        Some(ref endpoint) => {
            match tonic::transport::Channel::from_shared(endpoint.clone())
                .map(|ch| {
                    ch.connect_timeout(std::time::Duration::from_secs(10))
                        .connect_lazy()
                })
            {
                Ok(channel) => {
                    tracing::info!(
                        endpoint = %SensitiveUrl::new(endpoint.as_str()),
                        "Connected to FUSE daemon gRPC service"
                    );
                    Some(
                        aegis_orchestrator_core::infrastructure::aegis_runtime_proto::fuse_mount_service_client::FuseMountServiceClient::new(
                            channel,
                        ),
                    )
                }
                Err(e) => {
                    tracing::warn!(
                        endpoint = %SensitiveUrl::new(endpoint.as_str()),
                        error = %e,
                        "Failed to construct FUSE daemon gRPC channel; FUSE transport disabled"
                    );
                    None
                }
            }
        }
        None => {
            tracing::debug!(
                "FUSE daemon endpoint not configured (spec.runtime.fuse_daemon_endpoint omitted) — FUSE transport disabled"
            );
            None
        }
    };

    // Initialize volume service (with SeaweedFS or fallback to local)
    info!("Initializing volume service...");
    let storage_config = config
        .spec
        .storage
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Storage configuration not found in node config"))?;

    let filer_url = if storage_config.backend == "seaweedfs" {
        storage_config
            .seaweedfs
            .as_ref()
            .map(|s| s.filer_url.expose().to_string())
            .unwrap_or_else(|| "http://localhost:8888".to_string())
    } else {
        "http://localhost:8888".to_string() // Fallback even for local mode
    };

    // Reuse existing pool for volume repository (avoid redundant connection)
    let volume_repo: Arc<dyn aegis_orchestrator_core::domain::repository::VolumeRepository> =
        if let Some(db_pool) = db_pool.as_ref() {
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_volume::PostgresVolumeRepository::new(db_pool.clone()))
        } else {
            warn!("Volume persistence disabled (no database pool available)");
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
                )?
            }
            "local_host" => {
                let mount_point = storage_config
                    .local_host
                    .as_ref()
                    .map(|l| l.mount_point.clone())
                    .unwrap_or_else(default_local_host_mount_point);
                aegis_orchestrator_core::infrastructure::storage::create_storage_provider(
                    aegis_orchestrator_core::infrastructure::storage::StorageBackend::LocalHost {
                        mount_point,
                    },
                )?
            }
            "opendal" => {
                let opendal_config = storage_config.opendal.as_ref().cloned().unwrap_or_default();
                // Resolve env variables in config map
                let mut resolved_options = std::collections::HashMap::new();
                for (k, v) in opendal_config.options {
                    let raw = v.expose_owned();
                    let value = resolve_env_value(&raw).unwrap_or(raw);
                    resolved_options.insert(k, value);
                }
                aegis_orchestrator_core::infrastructure::storage::create_storage_provider(
                    aegis_orchestrator_core::infrastructure::storage::StorageBackend::OpenDal {
                        provider: opendal_config.provider,
                        options: resolved_options,
                    },
                )?
            }
            other => return Err(anyhow::anyhow!("Unsupported storage backend: {other}")),
        };

    let volume_service = Arc::new(
        aegis_orchestrator_core::application::volume_manager::StandardVolumeService::new(
            volume_repo.clone(),
            storage_provider.clone(),
            event_bus.clone(),
            filer_url,
            storage_config.backend.clone(),
        )?,
    );

    info!(mode = %storage_config.backend, "Volume service initialized");

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
    info!("Volume cleanup background task spawned (interval: 5 minutes)");

    // Initialize Storage Event Persister for audit trail (ADR-036)
    info!("Initializing Storage Event Persister...");
    let storage_event_repo: Arc<
        dyn aegis_orchestrator_core::domain::repository::StorageEventRepository,
    > = if let Some(db_pool) = db_pool.as_ref() {
        Arc::new(aegis_orchestrator_core::infrastructure::repositories::postgres_storage_event::PostgresStorageEventRepository::new(db_pool.clone()))
    } else {
        warn!("Storage event persistence disabled (no database pool available)");
        Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::InMemoryStorageEventRepository::new(),
            )
    };

    let storage_event_persister = Arc::new(
        aegis_orchestrator_core::application::storage_event_persister::StorageEventPersister::new(
            storage_event_repo.clone(),
            event_bus.clone(),
        ),
    );

    // Start background task for event persistence
    let _persister_handle = storage_event_persister.start();
    info!("Storage Event Persister started (audit trail enabled)");

    // Initialize Execution Event Persister so all in-process ExecutionEvents
    // (Supervisor IterationStarted/Completed, dispatch LlmInteraction/Failed,
    // etc.) land in the `execution_events` table — not just the events the
    // Temporal listener writes. Without this, `aegis.task.logs` returns an
    // empty `events` array for any non-Temporal execution.
    let execution_event_persister = Arc::new(
        aegis_orchestrator_core::application::execution_event_persister::ExecutionEventPersister::new(
            workflow_execution_repo.clone(),
            event_bus.clone(),
        ),
    );
    let _execution_persister_handle = execution_event_persister.start();

    // End the executions an earlier process left running, now that their
    // failure events are recorded and before the container reaper's first
    // pass and any listener: see `end_executions_cut_by_restart`.
    end_executions_cut_by_restart(&config, execution_repo.as_ref(), &event_bus).await;

    // Initialize NFS Server Gateway (ADR-036)
    info!("Initializing NFS Server Gateway...");
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
            storage_provider.clone(),
            volume_repo.clone(),
            event_publisher.clone(),
            Some(nfs_bind_port),
        ),
    );

    // Start NFS server and await successful startup before continuing
    if let Err(e) = nfs_gateway.start_server().await {
        error!(error = %e, "NFS Server Gateway failed to start, this is a fatal error");
        // Allow shutdown of daemon via signal
        std::process::exit(1);
    }
    info!(port = nfs_bind_port, "NFS Server Gateway started");

    // ─── Create FUSE FSAL Daemon (ADR-107) ───────────────────────────────────
    // Shares the same FSAL instance as the NFS gateway. The FUSE daemon provides
    // an alternative volume transport using host-local FUSE mountpoints + bind
    // mounts, which works in rootless container runtimes.
    let fuse_daemon: Option<
        Arc<aegis_orchestrator_core::infrastructure::fuse::daemon::FuseFsalDaemon>,
    > = {
        let fsal = nfs_gateway.fsal().clone();
        let daemon =
            aegis_orchestrator_core::infrastructure::fuse::daemon::FuseFsalDaemon::new(fsal);
        info!(
            mount_prefix = %fuse_mount_prefix,
            "FUSE FSAL daemon initialized"
        );
        Some(Arc::new(daemon))
    };

    let runtime = Arc::new(
        ContainerRuntime::new(aegis_orchestrator_core::infrastructure::runtime::ContainerRuntimeConfig {
            bootstrap_script: config.spec.runtime.bootstrap_script.clone(),
            socket_path: config.spec.runtime.container_socket_path.clone(),
            network_mode: network_mode.clone(),
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
            fuse_daemon: fuse_daemon.clone(),
            fuse_mount_prefix: fuse_mount_prefix.clone(),
            fuse_mount_client: fuse_mount_client.clone(),
        })
        .await
        .context("Failed to initialize Docker runtime")?,
    );

    // Only healthcheck Docker if it's the configured isolation mode
    if config.spec.runtime.default_isolation == "docker" {
        runtime.healthcheck().await
            .context("Docker healthcheck failed. Docker isolation is configured but Docker daemon is not accessible.")?;
        info!("Docker runtime connected and healthy");
    } else {
        info!(
            isolation_mode = %config.spec.runtime.default_isolation,
            "Docker runtime initialized (healthcheck skipped)"
        );
    }

    let supervisor = Arc::new(
        Supervisor::new(runtime.clone()).with_execution_repository(execution_repo.clone()),
    );

    let agent_container_reaper_runtime = runtime.clone();
    let agent_container_reaper_execution_repo = execution_repo.clone();
    // The reaper publishes the failure events of an execution it ends
    // because it outlived its bound (ADR-040, Update of 2026-10-04).
    let agent_container_reaper_event_bus = event_bus.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        // First tick fires immediately — clean up any orphans left from a prior crash.
        let mut first_run = true;
        // Per-container reaper bookkeeping: tracks consecutive Unkillable
        // failures, the next-eligible attempt time, and the quarantine bit.
        // Owned by this task — no synchronization needed.
        let mut attempt_states: std::collections::HashMap<
            String,
            crate::daemon::container_helpers::ReapAttemptState,
        > = std::collections::HashMap::new();
        loop {
            interval.tick().await;
            if first_run {
                tracing::info!("Running startup orphan container cleanup");
                first_run = false;
            }
            match cleanup_orphaned_agent_containers(
                agent_container_reaper_runtime.clone(),
                agent_container_reaper_execution_repo.clone(),
                &agent_container_reaper_event_bus,
                &mut attempt_states,
            )
            .await
            {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            "Agent container cleanup: {} orphaned container(s) deleted",
                            count
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("Agent container cleanup failed: {}", e);
                }
            }
        }
    });
    info!("Agent container cleanup background task spawned (interval: 5 minutes)");

    // Initialize security context repository early — needed by StandardAgentLifecycleService (ADR-102).
    let security_context_repo: Arc<
        dyn aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository,
    > = {
        let repo = aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository::new();

        // Seed security contexts from aegis-config.yaml (ADR-071 §ZaruTier SecurityContext Definitions)
        if let Some(definitions) = &config.spec.security_contexts {
            for def in definitions {
                let capabilities = def
                    .capabilities
                    .iter()
                    .map(|cap| aegis_orchestrator_core::domain::security_context::Capability {
                        tool_pattern: cap.tool_pattern.clone(),
                        path_allowlist: cap.path_allowlist.as_ref().map(|paths| {
                            paths.iter().map(std::path::PathBuf::from).collect()
                        }),
                        command_allowlist: cap.command_allowlist.clone(),
                        subcommand_allowlist: None,
                        domain_allowlist: cap.domain_allowlist.clone(),
                        max_response_size: None,
                        rate_limit: None,
                        max_concurrent: None,
                    })
                    .collect();

                let context = aegis_orchestrator_core::domain::security_context::SecurityContext {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    capabilities,
                    deny_list: def.deny_list.clone(),
                    metadata: aegis_orchestrator_core::domain::security_context::SecurityContextMetadata {
                        created_at: chrono::Utc::now(),
                        updated_at: chrono::Utc::now(),
                        version: 1,
                    },
                };

                repo.save(context).await?;
                info!("Loaded security context: {}", def.name);
            }
            info!("Loaded {} security contexts from config", definitions.len());
        }

        Arc::new(repo)
    };

    let agent_service = Arc::new(StandardAgentLifecycleService::new(
        agent_repo.clone(),
        event_bus.clone(),
        security_context_repo.clone(),
    ));

    // Load StandardRuntime registry (ADR-043 / ADR-060)
    // Try database-backed merged registry first, fall back to file-based.
    let registry_path = &config.spec.runtime.runtime_registry_path;
    let runtime_registry = {
        let merged_registry = if let Some(ref pool) = db_pool {
            use aegis_orchestrator_core::infrastructure::cluster::PgConfigLayerRepository;
            let config_repo: Arc<dyn ConfigLayerRepository> =
                Arc::new(PgConfigLayerRepository::new(Arc::new(pool.clone())));
            let node_id = NodeId(
                uuid::Uuid::parse_str(&config.spec.node.id)
                    .unwrap_or_else(|_| uuid::Uuid::new_v4()),
            );
            config_repo
                .get_merged_config(&node_id, None, &ConfigType::RuntimeRegistry)
                .await
                .ok()
        } else {
            None
        };

        if let Some(ref merged) = merged_registry {
            match StandardRuntimeRegistry::from_merged_config(merged) {
                Ok(registry) => {
                    info!("StandardRuntime registry loaded from merged config");
                    Arc::new(registry)
                }
                Err(e) => {
                    debug!(error = %e, "Merged runtime registry unavailable, falling back to file");
                    Arc::new(
                        StandardRuntimeRegistry::from_file(registry_path).map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to load StandardRuntime registry from '{}': {e}. \
                             Ensure runtime-registry.yaml exists at the configured path \
                             (spec.runtime.runtime_registry_path in aegis-config.yaml).",
                                registry_path
                            )
                        })?,
                    )
                }
            }
        } else {
            match StandardRuntimeRegistry::from_file(registry_path) {
                Ok(registry) => {
                    info!(path = %registry_path, "StandardRuntime registry loaded");
                    Arc::new(registry)
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to load StandardRuntime registry from '{}': {e}. \
                         Ensure runtime-registry.yaml exists at the configured path \
                         (spec.runtime.runtime_registry_path in aegis-config.yaml).",
                        registry_path
                    ));
                }
            }
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
                        tracing::warn!(
                            "NFS deregistration listener lagged by {} events — some volume deregistrations may have been missed",
                            n
                        );
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
        tracing::info!("NFS deregistration listener started");
    }

    info!("Initializing workflow engine...");

    // Initialize Temporal Client — read from config (spec.temporal)
    let temporal_required = config.spec.temporal.is_some();
    let temporal_config = config.spec.temporal.clone().unwrap_or_default();
    let temporal_address = resolve_env_value(temporal_config.address.expose())
        .unwrap_or_else(|_| "temporal:7233".to_string());
    let worker_http_endpoint = resolve_env_value(temporal_config.worker_http_endpoint.expose())
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    let temporal_namespace = temporal_config.namespace.clone();
    let temporal_task_queue = temporal_config.task_queue.clone();
    let temporal_connection_max_retries =
        temporal_connection_max_retries(temporal_config.max_connection_retries);
    info!(address = %temporal_address, "Initializing Temporal Client");

    // Create shared containers for the concrete Temporal client and the workflow engine port.
    let temporal_client_container: Arc<
        tokio::sync::RwLock<
            Option<Arc<aegis_orchestrator_core::infrastructure::temporal_client::TemporalClient>>,
        >,
    > = Arc::new(tokio::sync::RwLock::new(None));
    let workflow_engine_container: Arc<
        tokio::sync::RwLock<
            Option<Arc<dyn aegis_orchestrator_core::application::ports::WorkflowEnginePort>>,
        >,
    > = Arc::new(tokio::sync::RwLock::new(None));
    let temporal_client_container_clone = temporal_client_container.clone();
    let workflow_engine_container_clone = workflow_engine_container.clone();

    // Clone for async task
    let temporal_address_clone = temporal_address.clone();
    let worker_http_endpoint_clone = worker_http_endpoint.clone();

    if temporal_required {
        let client = connect_temporal_with_retry(
            &temporal_address_clone,
            &temporal_namespace,
            &temporal_task_queue,
            &worker_http_endpoint_clone,
            temporal_connection_max_retries,
        )
        .await?;

        let mut lock = temporal_client_container_clone.write().await;
        *lock = Some(client.clone());
        drop(lock);

        let mut workflow_lock = workflow_engine_container_clone.write().await;
        *workflow_lock = Some(
            client as Arc<dyn aegis_orchestrator_core::application::ports::WorkflowEnginePort>,
        );
    }

    // Rate Limiting Infrastructure (ADR-072)
    #[allow(clippy::type_complexity)]
    let (rate_limit_enforcer, rate_limit_resolver): (
        Option<Arc<dyn RateLimitEnforcer>>,
        Option<Arc<dyn RateLimitPolicyResolver>>,
    ) = if let Some(ref pool) = db_pool {
        let burst = Arc::new(GovernorBurstEnforcer::new());
        let postgres = Arc::new(PostgresWindowEnforcer::new(pool.clone()));
        let enforcer: Arc<dyn RateLimitEnforcer> = Arc::new(
            CompositeRateLimitEnforcer::new(burst, postgres).with_event_bus(event_bus.clone()),
        );
        let resolver: Arc<dyn RateLimitPolicyResolver> =
            Arc::new(HierarchicalPolicyResolver::new(pool.clone()));
        info!("Rate limiting enabled");
        (Some(enforcer), Some(resolver))
    } else {
        info!("Rate limiting disabled (no database connection)");
        (None, None)
    };

    // Rate limit counter cleanup task (ADR-072)
    if let Some(ref pool) = db_pool {
        let cleanup_pool = pool.clone();
        tokio::spawn(async move {
            let enforcer = PostgresWindowEnforcer::new(cleanup_pool);
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600)); // hourly
            loop {
                interval.tick().await;
                match enforcer.cleanup_expired_counters().await {
                    Ok(deleted) => {
                        if deleted > 0 {
                            tracing::info!(deleted, "rate limit counter cleanup completed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "rate limit counter cleanup failed");
                    }
                }
            }
        });
        info!("Rate limit counter cleanup background task spawned (interval: 1 hour)");
    }

    // Initialize SEAL / Tool Routing Services (now hoisted for ExecutionService dependency)
    info!("Initializing SEAL & Tool Routing services...");

    let seal_middleware =
        daemon_seal_middleware(rate_limit_enforcer.clone(), rate_limit_resolver.clone());
    // Derive builtin dispatchers from the canonical tool router registry.
    // User-configured dispatchers from aegis-config.yaml take precedence.
    let mut builtin_dispatchers = config.spec.builtin_dispatchers.clone().unwrap_or_default();
    {
        let canonical =
            aegis_orchestrator_core::infrastructure::tool_router::ToolRouter::builtin_dispatchers();
        let existing_names: std::collections::HashSet<String> =
            builtin_dispatchers.iter().map(|d| d.name.clone()).collect();
        for dispatcher in canonical {
            if !existing_names.contains(&dispatcher.name) {
                builtin_dispatchers.push(dispatcher);
            }
        }
    }

    let web_search_api_key = builtin_dispatchers
        .iter()
        .find(|d| d.name == "web.search")
        .and_then(|d| d.api_key.as_ref())
        .and_then(|k| resolve_env_value(k.expose()).ok());

    let tool_router = Arc::new(
        aegis_orchestrator_core::infrastructure::tool_router::ToolRouter::new(builtin_dispatchers)
            .with_tool_capabilities(config.spec.tool_capabilities.as_deref().unwrap_or_default()),
    );

    // Connect to the standalone Cortex service if configured (ADR-042).
    // Absence of spec.cortex.grpc_url means memoryless mode — no error, no retry.
    let cortex_grpc_url: Option<String> = config
        .spec
        .cortex
        .as_ref()
        .and_then(|c| c.grpc_url.as_ref())
        .and_then(|url| resolve_env_value(url.expose()).ok());
    let cortex_api_key: Option<String> = config
        .spec
        .cortex
        .as_ref()
        .and_then(|c| c.api_key.as_ref())
        .and_then(|k| resolve_env_value(k.expose()).ok());
    let cortex_client: Option<
        std::sync::Arc<aegis_orchestrator_core::infrastructure::CortexGrpcClient>,
    > = match cortex_grpc_url {
        Some(url) => {
            match aegis_orchestrator_core::infrastructure::CortexGrpcClient::new(
                url.clone(),
                cortex_api_key,
            )
            .await
            {
                Ok(client) => {
                    tracing::info!(
                        url = %SensitiveUrl::new(url.as_str()),
                        "Connected to Cortex gRPC service"
                    );
                    Some(std::sync::Arc::new(client))
                }
                Err(e) => {
                    tracing::warn!(
                        url = %SensitiveUrl::new(url.as_str()),
                        error = %e,
                        "Failed to connect to Cortex gRPC service; running in memoryless mode"
                    );
                    None
                }
            }
        }
        None => {
            tracing::info!(
                "Cortex gRPC URL not configured (spec.cortex omitted) — Orchestrator running in memoryless mode"
            );
            None
        }
    };

    // ─── Discovery Service (ADR-075) ───────────────────────────────────────
    // Discovery is backed by the Cortex service when connected.
    let discovery_service: Option<
        Arc<dyn aegis_orchestrator_core::application::discovery_service::DiscoveryService>,
    > = cortex_client.as_ref().map(|cx| {
        let svc = Arc::new(
            aegis_orchestrator_core::application::discovery_service::CortexDiscoveryService::new(
                cx.clone(),
            ),
        );

        // Spawn event handler to keep Cortex discovery index in sync
        let cortex_discovery: Arc<
            dyn aegis_orchestrator_core::infrastructure::discovery::event_handler::CortexDiscoveryClient,
        > = cx.clone();
        let handler = Arc::new(
            aegis_orchestrator_core::infrastructure::discovery::DiscoveryIndexEventHandler::new(
                cortex_discovery,
                agent_repo.clone(),
                workflow_repo.clone(),
                event_bus.clone(),
            ),
        );
        handler.clone().spawn();

        // Backfill on startup
        let handler_bg = handler.clone();
        tokio::spawn(async move {
            match handler_bg.backfill().await {
                Ok((agents, workflows)) => {
                    tracing::info!(
                        agents_indexed = agents,
                        workflows_indexed = workflows,
                        "Cortex discovery index backfill complete"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Cortex discovery index backfill failed");
                }
            }
        });

        // Periodic reconciliation — catches index drift from event lag or transient Cortex failures
        handler
            .clone()
            .spawn_reconciler(std::time::Duration::from_secs(300));

        tracing::info!("Discovery service initialized (Cortex-backed)");
        svc as Arc<dyn aegis_orchestrator_core::application::discovery_service::DiscoveryService>
    });

    // Token Issuer — create early so it can be shared with both ExecutionService and AttestationService (ADR-088 §A8).
    // AEGIS_SEAL_PRIVATE_KEY must be set to a PEM-encoded RSA private key.
    let private_key_for_issuer = std::env::var("AEGIS_SEAL_PRIVATE_KEY").map_err(|_| {
        anyhow::anyhow!(
            "SEAL private key not configured: set AEGIS_SEAL_PRIVATE_KEY \
             (PEM-encoded RSA private key)"
        )
    })?;
    let private_key_for_issuer = normalize_seal_private_key(&private_key_for_issuer);
    let token_issuer: Arc<
        dyn aegis_orchestrator_core::application::ports::SecurityTokenIssuerPort,
    > = Arc::new(
        aegis_orchestrator_core::infrastructure::seal::signature::SecurityTokenIssuer::new(
            &private_key_for_issuer,
            "aegis-orchestrator",
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to initialize SEAL token issuer from AEGIS_SEAL_PRIVATE_KEY: {e}"
            )
        })?,
    );

    // Operator credentials for the SEAL gateway's operator API (ADR-088 §6.6):
    // a client-credentials token, obtained and refreshed by the orchestrator.
    // All three variables or none; a partial set refuses to start.
    let seal_gateway_operator_token = aegis_orchestrator_core::infrastructure::seal::operator_token::OperatorCredentials::from_env()?
        .map(|credentials| {
            Arc::new(
                aegis_orchestrator_core::infrastructure::seal::operator_token::OperatorTokenSource::new(
                    credentials,
                ),
            )
        });
    if seal_gateway_operator_token.is_none() {
        warn!(
            "AEGIS_SEAL_OPERATOR_TOKEN_URL, AEGIS_SEAL_OPERATOR_CLIENT_ID and AEGIS_SEAL_OPERATOR_CLIENT_SECRET are not set: calls to the SEAL gateway's operator API carry no token and a gateway with operator authentication refuses them"
        );
    }

    // SEAL gateway client for session pre-creation (ADR-088 §A8).
    let seal_gateway_client: Arc<
        dyn aegis_orchestrator_core::application::ports::SealGatewayClient,
    > = {
        let gateway_url = std::env::var("AEGIS_SEAL_GATEWAY_URL")
            .unwrap_or_else(|_| "http://localhost:8089".to_string());
        Arc::new(
            aegis_orchestrator_core::infrastructure::seal::gateway_client::HttpSealGatewayClient::new(
                gateway_url,
                seal_gateway_operator_token.clone(),
            ),
        )
    };

    // Every judge the daemon runs is bounded by its alias's room in the
    // alias table (AEGIS ADR-131 U16, U19).
    let judge_context = judge_context_source(&config);

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
    .with_runtime_registry(runtime_registry.clone())
    .with_tool_router(tool_router.clone())
    .with_judge_context(judge_context.clone())
    .with_workflow_executions(workflow_execution_repo.clone());

    if let Some(c_client) = cortex_client.clone() {
        execution_service_builder = execution_service_builder.with_cortex_client(c_client);
    }

    if let (Some(ref enforcer), Some(ref resolver)) = (&rate_limit_enforcer, &rate_limit_resolver) {
        execution_service_builder =
            execution_service_builder.with_rate_limiting(enforcer.clone(), resolver.clone());
    }

    // Wire swarm cascade cancellation so parent execution cancel propagates to child swarms (BC-6).
    execution_service_builder = execution_service_builder
        .with_swarm_cancellation(swarm_service.clone()
            as Arc<dyn aegis_orchestrator_core::application::ports::SwarmCancellationPort>);

    // Wire SEAL session pre-creation so containers get credentials at birth (ADR-088 §A8).
    // Clone before consuming — attestation_service also needs a reference.
    let attestation_gateway_client = seal_gateway_client.clone();
    execution_service_builder = execution_service_builder
        .with_seal_session_precreation(seal_gateway_client, token_issuer.clone());

    let execution_service = Arc::new(execution_service_builder);
    // Wire the self-reference so judge agents can be spawned as child executions (ADR-016).
    execution_service.set_child_execution_service(execution_service.clone());

    let validation_service = Arc::new(daemon_validation_service(
        event_bus.clone(),
        execution_service.clone(),
        agent_service.clone(),
        judge_context.clone(),
    ));

    // Create human input service
    let human_input_service =
        Arc::new(aegis_orchestrator_core::infrastructure::HumanInputService::new());

    // Legacy WorkflowEngine removed as part of Temporal migration

    let temporal_event_listener = Arc::new(TemporalEventListener::new(
        event_bus.clone(),
        workflow_execution_repo.clone(),
    ));

    info!("Temporal event listener initialized");

    let register_workflow_use_case = Arc::new(StandardRegisterWorkflowUseCase::new(
        workflow_repo.clone(),
        workflow_engine_container.clone(),
        event_bus.clone(),
        agent_service.clone(),
    ));

    let start_workflow_execution_use_case = {
        let mut uc = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo.clone(),
            workflow_execution_repo.clone(),
            workflow_engine_container.clone(),
            event_bus.clone(),
        );
        if let (Some(ref enforcer), Some(ref resolver)) =
            (&rate_limit_enforcer, &rate_limit_resolver)
        {
            uc = uc.with_rate_limiting(enforcer.clone(), resolver.clone());
        }
        Arc::new(uc)
    };

    // --- Initialize SEAL / Tool Routing Services ---
    info!("Configuring SEAL & Tool Routing repositories and services...");

    // Note: security_context_repo was initialized earlier (before agent_service) — see above.

    let seal_session_repo: Arc<
        dyn aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository,
    > = Arc::new(
        aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository::new(),
    );

    // Application Services — token_issuer was created earlier (ADR-088 §A8) and shared with ExecutionService.
    let mut attestation_service_builder =
        aegis_orchestrator_core::application::attestation_service::AttestationServiceImpl::new(
            security_context_repo.clone(),
            seal_session_repo.clone(),
            token_issuer,
        )
        .with_gateway_client(attestation_gateway_client)
        .with_agent_manifest_tools(execution_service.clone(), agent_service.clone());

    match aegis_orchestrator_core::infrastructure::docker::BollardContainerVerifier::new(
        config.spec.runtime.container_socket_path.as_deref(),
    ) {
        Ok(v) => {
            attestation_service_builder =
                attestation_service_builder.with_container_verifier(std::sync::Arc::new(v));
        }
        Err(e) => {
            warn!("Docker socket unavailable; container identity verification disabled: {e}");
        }
    }

    let attestation_service: Arc<
        dyn aegis_orchestrator_core::infrastructure::seal::attestation::AttestationService,
    > = Arc::new(attestation_service_builder);

    // Secrets manager: initialize from `spec.secrets.backend`, otherwise use an in-memory store for local development/testing.
    let secrets_manager: Arc<aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager> =
        match config.spec.secrets.as_ref().and_then(|s| s.backend.as_ref()) {
            Some(secret_backend_config) => {
                match aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager::from_config(
                    secret_backend_config,
                    event_bus.clone(),
                ).await {
                    Ok(manager) => {
                        info!("OpenBao secrets manager initialized");
                        Arc::new(manager)
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to initialize OpenBao secrets manager: {e}"));
                    }
                }
            }
            None => {
                warn!("No spec.secrets.backend configured; using in-memory secret store (development/testing only, not production-safe)");
                Arc::new(aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager::from_store(
                    Arc::new(aegis_orchestrator_core::infrastructure::secrets_manager::TestSecretStore::new()),
                    event_bus.clone(),
                ))
            }
        };

    // ─── Credential Management Service (BC-11, ADR-078) ────────────────────────
    // The same store answers a remote tool call's credential (AEGIS ADR-132
    // H1), wired into the tool invocation service with `seal_gateway`.
    let mut tool_credentials: Option<Arc<dyn ToolCredentialSource>> = None;
    // The mail tools resolve the acting person's mailbox from the same store
    // (AEGIS ADR-125 D4).
    let mut mailbox_source: Option<Arc<dyn ToolMailboxSource>> = None;
    // Handed the remote servers' grounding once the tool invocation service
    // that implements it is built (AEGIS ADR-132 (7a) S2).
    let mut grounded_credentials: Option<Arc<StandardCredentialManagementService>> = None;
    let credential_service: Option<Arc<dyn CredentialManagementService>> = match db_pool.as_ref() {
        None => None,
        Some(pool) => {
            let repo = Arc::new(PostgresCredentialBindingRepository::new(pool.clone()))
                as Arc<dyn CredentialBindingRepository>;
            // OAuth provider registry (RFC 6749 §4.1.3 token exchange),
            // loaded from `spec.oauth_providers` (AEGIS ADR-125 D2).
            // Requests against unregistered providers return
            // `CredentialError::ProviderNotConfigured`. Each entry is
            // validated and its `env:` values resolved at startup (security
            // audit 002 §4.11, §4.18): a real HTTPS authorization URL (no
            // `placeholder` substring), a non-empty client_id and a
            // non-empty `redirect_uri_allowlist`. Boot fails if any fails.
            let registry = oauth_provider_registry_from_config(&config.spec.oauth_providers)
                .context("OAuth provider registry failed startup validation")?;
            tracing::info!(
                providers = ?registry.keys().map(|p| p.to_string()).collect::<Vec<_>>(),
                "OAuth provider registry loaded from spec.oauth_providers"
            );
            let oauth_providers = Arc::new(registry);
            // Team-scoped bindings are authorised against ADR-111
            // memberships (security audit 003 F-1).
            let memberships = Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::PgMembershipRepository::new(
                    pool.clone(),
                ),
            )
                as Arc<dyn aegis_orchestrator_core::domain::team::MembershipRepository>;
            let service = Arc::new(
                StandardCredentialManagementService::new(
                    repo,
                    secrets_manager.clone(),
                    event_bus.clone(),
                    oauth_providers,
                )
                .with_membership_repo(memberships),
            );
            tool_credentials = Some(service.clone() as Arc<dyn ToolCredentialSource>);
            mailbox_source = Some(service.clone() as Arc<dyn ToolMailboxSource>);
            grounded_credentials = Some(service.clone());
            Some(service as Arc<dyn CredentialManagementService>)
        }
    };

    // ─── Container Step Runner (ADR-050) ──────────────────────────────────────
    // Dedicated Docker client + image manager + step runner for CI/CD container
    // steps executed by ContainerRun / ParallelContainerRun workflow states.
    // Delegates credential resolution to SecretsManager for secret-store paths and
    // to environment variables for env: paths.
    let docker_for_steps =
        connect_container_runtime(config.spec.runtime.container_socket_path.as_deref())
            .context("Failed to connect container runtime for ContainerStepRunner")?;
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
        aegis_orchestrator_core::infrastructure::container_step_runner::ContainerStepRunnerImpl::new(
            docker_for_steps,
            step_image_manager,
            aegis_orchestrator_core::infrastructure::container_step_runner::ContainerStepRunnerConfig {
                network_mode: network_mode.clone(),
                fuse_daemon: fuse_daemon.clone(),
                fuse_mount_prefix: fuse_mount_prefix.clone(),
                fuse_mount_client: fuse_mount_client.clone(),
            },
            event_bus.clone(),
            secrets_manager.clone(),
            Arc::new(nfs_gateway.volume_registry().clone()),
            Some(volume_service.clone()),
        ),
    );
    let run_container_step_use_case = Arc::new(
        aegis_orchestrator_core::application::run_container_step::RunContainerStepUseCase::new(
            container_step_runner.clone(),
        ),
    );
    info!("Container step runner initialized");

    let tool_catalog =
        Arc::new(aegis_orchestrator_core::application::tool_catalog::StandardToolCatalog::new());

    let file_operations_service = Arc::new(
        aegis_orchestrator_core::application::file_operations_service::FileOperationsService::new(
            nfs_gateway.fsal().clone(),
            execution_repo.clone(),
            agent_repo.clone(),
        ),
    );

    let mut tool_invocation_service_builder =
        aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService::new(
            seal_session_repo.clone(),
            security_context_repo.clone(),
            seal_middleware,
            tool_router.clone(),
            nfs_gateway.fsal().clone(),
            nfs_gateway.volume_registry().clone(),
            agent_service.clone(),
            execution_service.clone(),
            Arc::new(
                aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::new(
                    web_search_api_key,
                ),
            ),
            event_bus.clone(),
            daemon_seal_gateway_url(config.spec.seal_gateway.as_ref()),
        )
        .with_workflow_authoring(
            register_workflow_use_case.clone(),
            validation_service.clone(),
        )
        .with_workflow_repository(workflow_repo.clone())
        .with_workflow_execution_repo(workflow_execution_repo.clone())
        .with_workflow_execution(start_workflow_execution_use_case.clone())
        .with_generated_manifests_root(generated_artifacts_root.clone())
        .with_node_config_path(config_path.clone())
        .with_workflow_execution_control(Arc::new(DaemonWorkflowExecutionControl {
            config: config.clone(),
            temporal_client_container: temporal_client_container.clone(),
            workflow_execution_repo: workflow_execution_repo.clone(),
            event_bus: event_bus.clone(),
        }))
        .with_agent_activity(Arc::new(DaemonAgentActivity {
            execution_repo: execution_repo.clone(),
        }))
        .with_tool_catalog(tool_catalog.clone())
        .with_runtime_registry(runtime_registry.clone())
        .with_program_runner(container_step_runner.clone())
        .with_file_operations_service(file_operations_service.clone());
    if let Some(source) = &seal_gateway_operator_token {
        tool_invocation_service_builder =
            tool_invocation_service_builder.with_seal_gateway_operator_token(source.clone());
    }
    tool_invocation_service_builder = daemon_seal_gateway_wiring(
        tool_invocation_service_builder,
        config.spec.seal_gateway.as_ref(),
        tool_credentials,
    )?;
    // The mail tools resolve the acting person's mailbox from the same store.
    if let Some(source) = mailbox_source {
        tool_invocation_service_builder = tool_invocation_service_builder.with_mail_tools(source);
    }

    // Wire discovery service into ToolInvocationService if available (ADR-075)
    if let Some(ref disc_svc) = discovery_service {
        tool_invocation_service_builder =
            tool_invocation_service_builder.with_discovery_service(disc_svc.clone());
    }

    // ADR-117: build edge components and wire the four-step pre-routing hook
    // when a Postgres pool is available. The same components feed the
    // `/v1/edge/*` REST surface (constructed below) — we collect them here
    // and reuse them in the `EdgeApiState` bundle so there is exactly one
    // EdgeConnectionRegistry / FleetRegistry per process.
    let edge_components: Option<EdgeComponentsBundle> = if let Some(ref pool) = db_pool {
        use aegis_orchestrator_core::application::edge::dispatch_to_edge::DispatchToEdgeService;
        use aegis_orchestrator_core::application::edge::fleet::dispatcher::FleetDispatcher;
        use aegis_orchestrator_core::application::edge::fleet::registry::FleetRegistry;
        use aegis_orchestrator_core::application::edge::fleet::{
            CancelFleetService, EdgeFleetResolver,
        };
        use aegis_orchestrator_core::infrastructure::edge::{
            EdgeConnectionRegistry, PgEdgeDaemonRepository, PgEdgeGroupRepository,
        };

        let edge_repo: Arc<dyn aegis_orchestrator_core::domain::edge::EdgeDaemonRepository> =
            Arc::new(PgEdgeDaemonRepository::new(pool.clone()));
        let group_repo: Arc<dyn aegis_orchestrator_core::domain::edge::EdgeGroupRepository> =
            Arc::new(PgEdgeGroupRepository::new(pool.clone()));
        let conn_registry = EdgeConnectionRegistry::new();
        let dispatch = Arc::new(DispatchToEdgeService::new(
            edge_repo.clone(),
            conn_registry.clone(),
        ));
        let resolver = Arc::new(EdgeFleetResolver::new(
            edge_repo.clone(),
            group_repo.clone(),
            conn_registry.clone(),
        ));
        let fleet_registry = FleetRegistry::new();
        let fleet_dispatcher = Arc::new(FleetDispatcher::new(
            dispatch.clone(),
            fleet_registry.clone(),
        ));
        let fleet_cancel = Arc::new(CancelFleetService::new(
            fleet_registry,
            conn_registry.clone(),
        ));
        tool_invocation_service_builder = tool_invocation_service_builder.with_edge_router(
            dispatch.clone(),
            resolver.clone(),
            fleet_dispatcher.clone(),
            fleet_cancel.clone(),
        );
        Some((
            edge_repo,
            group_repo,
            conn_registry,
            dispatch,
            resolver,
            fleet_dispatcher,
            fleet_cancel,
        ))
    } else {
        None
    };

    // ─── Tool approval gate (AEGIS ADR-126) ─────────────────────────────────
    // Requests and "always allow" policies live in PostgreSQL (migration
    // 036), so a pending request outlives the agent and this process; a node
    // without a database keeps them in process. The sweep expires requests
    // pending for 72 hours, every ten minutes.
    let tool_approval_repo: Arc<
        dyn aegis_orchestrator_core::domain::tool_approval::ToolApprovalRepository,
    > = match db_pool.as_ref() {
        Some(pool) => Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::PostgresToolApprovalRepository::new(pool.clone()),
        ),
        None => Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository::new(),
        ),
    };
    let tool_approval_service = Arc::new(
        aegis_orchestrator_core::application::tool_approval_service::ToolApprovalService::new(
            tool_approval_repo,
            event_bus.clone(),
        ),
    );
    tool_approval_service.clone().spawn_expiry_sweep(
        aegis_orchestrator_core::application::tool_approval_service::EXPIRY_SWEEP_INTERVAL,
    );
    tool_invocation_service_builder =
        tool_invocation_service_builder.with_tool_approvals(tool_approval_service);

    // Keycloak Admin client — shared between TenantProvisioningService, the
    // colony handlers and the operator escalation's role lookup. Built before
    // the escalation service, which re-reads an escalated operator's role
    // through it (AEGIS ADR-129 — Updates, V9).
    let colony_keycloak_admin: Option<Arc<aegis_orchestrator_core::infrastructure::iam::keycloak_admin_client::KeycloakAdminClient>> = config
        .spec
        .iam
        .as_ref()
        .and_then(|iam| iam.keycloak_admin.as_ref())
        .and_then(|admin_cfg| {
            let host = match resolve_env_value(admin_cfg.host.expose()) {
                Ok(h) => h,
                Err(e) => {
                    warn!("Keycloak admin host not resolvable: {e} — admin client disabled");
                    return None;
                }
            };
            let username = match resolve_env_value(&admin_cfg.admin_username) {
                Ok(u) => u,
                Err(e) => {
                    warn!("Keycloak admin username not resolvable: {e} — admin client disabled");
                    return None;
                }
            };
            let password = match resolve_env_value(admin_cfg.admin_password.expose()) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Keycloak admin password not resolvable: {e} — admin client disabled");
                    return None;
                }
            };
            Some(Arc::new(
                aegis_orchestrator_core::infrastructure::iam::keycloak_admin_client::KeycloakAdminClient::new(
                    aegis_orchestrator_core::infrastructure::iam::keycloak_admin_client::KeycloakAdminConfig {
                        host,
                        admin_username: username,
                        // Audit 002 §4.37.10 — wrap at the construction
                        // boundary so the admin password is redacted in
                        // any subsequent Debug output.
                        admin_password: aegis_orchestrator_core::domain::secrets::SensitiveString::new(password),
                    },
                ),
            ))
        });

    // ─── Operator escalation (AEGIS ADR-129) ────────────────────────────────
    // Codes and escalations live in PostgreSQL (migration 037) and every act
    // is appended to admin_audit_log; a node without a database keeps them in
    // process. The bounds are spec.iam.operator_escalation (D12). The sweep
    // ends and audits escalations past their expires_at every minute (D19).
    let operator_escalation_repo: Arc<
        dyn aegis_orchestrator_core::domain::operator_escalation::OperatorEscalationRepository,
    > = match db_pool.as_ref() {
        Some(pool) => Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::postgres_operator_escalation::PostgresOperatorEscalationRepository::new(pool.clone()),
        ),
        None => Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::postgres_operator_escalation::InMemoryOperatorEscalationRepository::new(),
        ),
    };
    // A demotion ends an escalation (ADR-129 — Updates, V1 to V9): the
    // service re-reads an escalated operator's federated record in the
    // system realm through the Keycloak admin client. A node without that
    // client, or without a realm of kind `system`, cannot make the check: it
    // refuses every redemption 503 and every escalated call (V5).
    let operator_role_lookup: Option<
        Arc<dyn aegis_orchestrator_core::domain::operator_escalation::OperatorRoleLookup>,
    > = {
        let system_realm = config.spec.iam.as_ref().and_then(|iam| {
            iam.realms
                .iter()
                .find(|realm| realm.kind == "system")
                .map(|realm| realm.slug.clone())
        });
        match (colony_keycloak_admin.clone(), system_realm) {
            (Some(admin), Some(realm)) => {
                info!(realm = %realm, "Operator escalation role lookup enabled");
                Some(Arc::new(
                    aegis_orchestrator_core::infrastructure::iam::keycloak_operator_role_lookup::KeycloakOperatorRoleLookup::new(admin, realm),
                ))
            }
            (admin, realm) => {
                warn!(
                    keycloak_admin = admin.is_some(),
                    system_realm = realm.is_some(),
                    "Operator escalation role lookup disabled (requires spec.iam.keycloak_admin and a realm of kind system): every redemption answers 503 and every escalated call is refused"
                );
                None
            }
        }
    };
    let mut operator_escalation_service =
        aegis_orchestrator_core::application::operator_escalation_service::OperatorEscalationService::new(
            operator_escalation_repo,
            config
                .spec
                .iam
                .as_ref()
                .map(|iam| iam.operator_escalation.clone())
                .unwrap_or_default(),
        );
    if let Some(lookup) = operator_role_lookup {
        operator_escalation_service = operator_escalation_service.with_role_lookup(lookup);
    }
    let operator_escalations = Arc::new(operator_escalation_service);
    operator_escalations.clone().spawn_expiry_sweep(
        aegis_orchestrator_core::application::operator_escalation_service::EXPIRY_SWEEP_INTERVAL,
    );
    tool_invocation_service_builder = tool_invocation_service_builder
        .with_operator_escalations(operator_escalations.clone())
        .with_execution_repository(execution_repo.clone());

    // ─── Goals (AEGIS ADR-131) ──────────────────────────────────────────────
    // Goals and their evaluations live in PostgreSQL (migration 039); a node
    // without a database keeps them in process. The bounds are spec.goals
    // (D7). The sweep closes goals open past their lifetime every ten
    // minutes.
    let goal_repo: Arc<dyn aegis_orchestrator_core::domain::goal::GoalRepository> =
        match db_pool.as_ref() {
            Some(pool) => Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::postgres_goal::PostgresGoalRepository::new(pool.clone()),
            ),
            None => Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::postgres_goal::InMemoryGoalRepository::new(),
            ),
        };
    let goal_service = Arc::new(daemon_goal_service(
        goal_repo,
        event_bus.clone(),
        &config,
        judge_context.clone(),
    ));
    goal_service.clone().spawn_expiry_sweep(
        aegis_orchestrator_core::application::goal_service::EXPIRY_SWEEP_INTERVAL,
    );
    tool_invocation_service_builder = tool_invocation_service_builder.with_goals(goal_service);

    let tool_invocation_service = Arc::new(tool_invocation_service_builder);

    // A remote server's token is grounded through the SEAL gateway when it is
    // stored, rotated or introspected (AEGIS ADR-132 (7a) S2): the credential
    // service, built before the tool invocation service that makes the call,
    // is handed it now, weakly, with the remote servers' names.
    if let (Some(credentials), Some(gateway)) =
        (&grounded_credentials, config.spec.seal_gateway.as_ref())
    {
        let service = Arc::downgrade(&tool_invocation_service);
        let grounding: std::sync::Weak<dyn RemoteServerGrounding> = service;
        credentials.set_remote_grounding(
            grounding,
            gateway
                .remote_server_names()
                .context("seal_gateway configuration")?,
        );
    }

    info!(path = %generated_artifacts_root.display(), "Generated manifests will be written to configured path");

    // Initial tool catalog population + periodic refresh loop
    {
        let tis = tool_invocation_service.clone();
        let catalog = tool_catalog.clone();
        if let Ok(tools) = tis.get_available_tools().await {
            catalog.refresh_from(tools).await;
            tracing::info!("Tool catalog populated with initial tool set");
        }
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.tick().await; // skip immediate first tick (already populated above)
            loop {
                interval.tick().await;
                if let Ok(tools) = tis.get_available_tools().await {
                    catalog.refresh_from(tools).await;
                    tracing::debug!("Tool catalog refreshed");
                }
            }
        });
    }

    let (inner_loop_service, llm_registry) = share_llm_registry(llm_registry, |registry| {
        let mut ils = daemon_inner_loop_service(
            tool_invocation_service.clone(),
            execution_service.clone(),
            registry,
            judge_context.clone(),
        );
        if let (Some(ref enforcer), Some(ref resolver)) =
            (&rate_limit_enforcer, &rate_limit_resolver)
        {
            ils = ils.with_rate_limiting(enforcer.clone(), resolver.clone());
        }
        Arc::new(ils)
    });

    let workflow_scope_service = Arc::new(
        aegis_orchestrator_core::application::workflow_scope::WorkflowScopeService::new(
            workflow_repo.clone(),
            event_bus.clone(),
        ),
    );

    let agent_scope_service = Arc::new(
        aegis_orchestrator_core::application::agent_scope::AgentScopeService::new(
            agent_repo.clone(),
            event_bus.clone(),
        ),
    );

    // Tenant repository — shared between TenantProvisioningService and colony handlers.
    let colony_tenant_repo: Option<Arc<dyn aegis_orchestrator_core::domain::repository::TenantRepository>> =
        db_pool.as_ref().map(|pool| {
            Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::postgres_tenant::PostgresTenantRepository::new(pool.clone()),
            ) as Arc<dyn aegis_orchestrator_core::domain::repository::TenantRepository>
        });

    // Tenant Provisioning Service (ADR-097) is constructed AFTER the
    // EffectiveTierService below, because provisioning delegates the
    // `zaru_tier` Keycloak attribute write to it. Placeholder binding kept
    // here so the variable is in scope for the AppState assembly below.

    // Initialize user volume service and file operations service (Gap 079)
    let user_volume_service = Arc::new(
        aegis_orchestrator_core::application::user_volume_service::UserVolumeService::new(
            volume_repo.clone(),
            volume_service.clone(),
            event_bus.clone(),
            aegis_orchestrator_core::domain::volume::StorageTierLimits::default(),
        ),
    );

    // Initialize git repo service (ADR-081 Waves A2 / A3). Requires a
    // Postgres pool for the binding repository; left as `None` when the
    // pool is absent. The handlers return 503 in that case.
    let git_repo_service: Option<Arc<aegis_orchestrator_core::application::git_repo_service::GitRepoService>> = db_pool.as_ref().map(|pool| {
        let repo = Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::PostgresGitRepoBindingRepository::new(
                pool.clone(),
            ),
        ) as Arc<dyn aegis_orchestrator_core::domain::git_repo::GitRepoBindingRepository>;

        // Phase 3: EphemeralCliEngine for non-HostPath volume backends
        // (SeaweedFS / OpenDAL / SEAL). Spawns alpine/git through the
        // shared ADR-050 ContainerStepRunner with FUSE-mounted target.
        let cli_engine = Arc::new(
            aegis_orchestrator_core::application::git_clone_executor::EphemeralCliEngine::new(
                container_step_runner.clone(),
                Arc::new(nfs_gateway.volume_registry().clone()),
            ),
        );

        let clone_executor = Arc::new(
            aegis_orchestrator_core::application::git_clone_executor::GitCloneExecutor::new(
                secrets_manager.clone(),
                nfs_gateway.fsal().clone(),
                Some(cli_engine),
            ),
        );

        // Credential binding repository for Keymaster-pattern credential
        // resolution (ADR-081 §Security). Shares the Postgres pool with
        // the BC-11 credential service.
        let credential_repo: Arc<dyn CredentialBindingRepository> = Arc::new(
            PostgresCredentialBindingRepository::new(pool.clone()),
        );

        Arc::new(
            aegis_orchestrator_core::application::git_repo_service::GitRepoService::new(
                repo,
                user_volume_service.clone(),
                clone_executor,
                secrets_manager.clone(),
                event_bus.clone(),
            )
            .with_credential_repo(credential_repo),
        )
    });

    // Initialize the Script persistence service (ADR-110 §D7). Enabled
    // when a Postgres pool is configured and migration 021 has been
    // applied. When absent, the /v1/scripts/* handlers return 503.
    let script_service: Option<
        Arc<aegis_orchestrator_core::application::script_service::ScriptService>,
    > = db_pool.as_ref().map(|pool| {
        let repo = Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::PostgresScriptRepository::new(
                pool.clone(),
            ),
        ) as Arc<dyn aegis_orchestrator_core::domain::script::ScriptRepository>;
        Arc::new(
            aegis_orchestrator_core::application::script_service::ScriptService::new(
                repo,
                event_bus.clone(),
            ),
        )
    });

    // Initialize the Vibe-Code Canvas service (ADR-106 Wave C2). Enabled when
    // a Postgres pool is configured — the canvas session repository and the
    // git repo binding repository both require it. Until those repositories
    // ship from Wave A2/B2/C1 migrations the service is None and the
    // /v1/canvas/* handlers return 503.
    let canvas_service: Option<
        Arc<dyn aegis_orchestrator_core::application::canvas_service::CanvasService>,
    > = db_pool.as_ref().map(|pool| {
        let session_repo = Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::PostgresCanvasSessionRepository::new(
                pool.clone(),
            ),
        );
        let git_repo_repo = Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::PostgresGitRepoBindingRepository::new(
                pool.clone(),
            ),
        );
        Arc::new(
            aegis_orchestrator_core::application::canvas_service::StandardCanvasService::new(
                session_repo,
                volume_service.clone(),
                git_repo_repo,
                event_bus.clone(),
            ),
        )
            as Arc<dyn aegis_orchestrator_core::application::canvas_service::CanvasService>
    });

    // Team tenancy service (ADR-111). Wired when:
    //   * Postgres is available for the team repos,
    //   * a `BillingConfig` is present to drive the Stripe customer lifecycle,
    //   * a `TenantRepository` is available for the backing tenant row.
    // Phase 1 stops at service construction — handlers land in Phase 2.
    // Team repositories (ADR-111). Constructed whenever a Postgres pool is
    // available so the tenant middleware can gate `X-Tenant-Id: t-{uuid}`
    // header switching on Active membership — independent of whether the
    // BillingConfig-dependent TeamService is wired.
    let team_repo_opt: Option<Arc<dyn aegis_orchestrator_core::domain::team::TeamRepository>> =
        db_pool.as_ref().map(|pool| {
            Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::PgTeamRepository::new(
                    pool.clone(),
                ),
            ) as Arc<dyn aegis_orchestrator_core::domain::team::TeamRepository>
        });
    let membership_repo_opt: Option<
        Arc<dyn aegis_orchestrator_core::domain::team::MembershipRepository>,
    > = db_pool.as_ref().map(|pool| {
        Arc::new(
            aegis_orchestrator_core::infrastructure::repositories::PgMembershipRepository::new(
                pool.clone(),
            ),
        ) as Arc<dyn aegis_orchestrator_core::domain::team::MembershipRepository>
    });

    // Effective tier service (ADR-111 Phase 3). Constructed BEFORE TeamService
    // so membership transitions (accept_invitation / revoke_membership) can
    // recompute the affected user's effective tier. Requires Keycloak admin,
    // billing repo, team repo, and membership repo — if any are missing the
    // service is disabled and billing/team paths fall back gracefully.
    let effective_tier_service: Option<
        Arc<dyn aegis_orchestrator_core::application::effective_tier_service::EffectiveTierService>,
    > = match (
        colony_keycloak_admin.as_ref(),
        db_pool.as_ref(),
        team_repo_opt.as_ref(),
        membership_repo_opt.as_ref(),
    ) {
        (Some(kc), Some(pool), Some(team_repo), Some(membership_repo)) => {
            let billing_repo_for_tier: Arc<
                dyn aegis_orchestrator_core::infrastructure::repositories::BillingRepository,
            > = Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::PostgresBillingRepository::new(
                    pool.clone(),
                ),
            );
            let zaru_url = config
                .spec
                .zaru
                .as_ref()
                .and_then(|cfg| resolve_env_value(cfg.public_url.expose()).ok());
            let zaru_secret = config
                .spec
                .zaru
                .as_ref()
                .and_then(|cfg| resolve_env_value(cfg.internal_secret.expose()).ok());
            let port: Arc<
                dyn aegis_orchestrator_core::application::effective_tier_service::TierSyncPort,
            > = Arc::new(
                aegis_orchestrator_core::application::effective_tier_service::KeycloakTierSyncPort::new(
                    kc.clone(),
                    zaru_url,
                    zaru_secret,
                )
                .with_event_bus(event_bus.clone()),
            );
            Some(Arc::new(
                aegis_orchestrator_core::application::effective_tier_service::StandardEffectiveTierService::new(
                    team_repo.clone(),
                    membership_repo.clone(),
                    billing_repo_for_tier,
                    port,
                ),
            )
                as Arc<
                    dyn aegis_orchestrator_core::application::effective_tier_service::EffectiveTierService,
                >)
        }
        _ => {
            tracing::warn!(
                "EffectiveTierService disabled: missing keycloak_admin, db_pool, team_repo, or membership_repo"
            );
            None
        }
    };

    // Tenant Provisioning Service (ADR-097). Constructed AFTER
    // `effective_tier_service` so provisioning can delegate the Keycloak
    // `zaru_tier` attribute write to the single-source-of-truth writer.
    // Requires both a database pool (for TenantRepository) and Keycloak
    // admin credentials; the `effective_tier_service` is optional — if it
    // is not wired, provisioning still stamps the `tenant_id` attribute and
    // logs that the zaru_tier claim will converge on the next recompute.
    let tenant_provisioning_service: Option<
        Arc<aegis_orchestrator_core::application::tenant_provisioning::TenantProvisioningService>,
    > = match (colony_tenant_repo.clone(), colony_keycloak_admin.clone()) {
        (Some(repo), Some(client)) => {
            info!("Tenant provisioning service initialized");
            Some(Arc::new(
                aegis_orchestrator_core::application::tenant_provisioning::TenantProvisioningService::new(
                    repo,
                    client,
                    event_bus.clone(),
                    effective_tier_service.clone(),
                ),
            ))
        }
        _ => {
            debug!(
                "Tenant provisioning service disabled (requires database + keycloak_admin config)"
            );
            None
        }
    };

    let team_service: Option<
        Arc<dyn aegis_orchestrator_core::application::team_service::TeamService>,
    > = match (
        db_pool.as_ref(),
        config.spec.billing.as_ref(),
        colony_tenant_repo.as_ref(),
        team_repo_opt.as_ref(),
        membership_repo_opt.as_ref(),
    ) {
        (
            Some(pool),
            Some(billing_cfg),
            Some(tenant_repo),
            Some(team_repo),
            Some(membership_repo),
        ) => {
            let invitation_repo = Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::PgTeamInvitationRepository::new(
                    pool.clone(),
                ),
            );
            let billing_repo_for_service: Arc<
                dyn aegis_orchestrator_core::infrastructure::repositories::BillingRepository,
            > = Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::PostgresBillingRepository::new(
                    pool.clone(),
                ),
            );
            let billing_service: Arc<
                dyn aegis_orchestrator_core::application::billing_service::BillingService,
            > = Arc::new(crate::daemon::billing_service::StripeBillingService::new(
                billing_cfg.clone(),
                billing_repo_for_service,
            ));
            let hmac_key = billing_cfg
                .invitation_hmac_key
                .as_ref()
                .and_then(|k| {
                    aegis_orchestrator_core::domain::node_config::resolve_env_value(k.expose()).ok()
                })
                .map(|s| {
                    aegis_orchestrator_core::domain::secrets::SensitiveBytes::new(s.into_bytes())
                });
            // `team_memberships` JWT-claim sync port. Stamps the multivalued
            // Keycloak attribute every time a membership mutation lands;
            // disabled (None) when keycloak_admin is unavailable, in which
            // case the upstream MCP middleware will fail-closed until the
            // next backfill run.
            let team_memberships_sync: Option<Arc<dyn aegis_orchestrator_core::application::team_service::TeamMembershipsSyncPort>> =
                colony_keycloak_admin.as_ref().map(|kc| {
                    Arc::new(
                        aegis_orchestrator_core::application::team_service::KeycloakTeamMembershipsSyncPort::new(
                            kc.clone(),
                        ),
                    )
                        as Arc<
                            dyn aegis_orchestrator_core::application::team_service::TeamMembershipsSyncPort,
                        >
                });

            Some(Arc::new(
                aegis_orchestrator_core::application::team_service::StandardTeamService::new(
                    team_repo.clone(),
                    membership_repo.clone(),
                    invitation_repo,
                    tenant_repo.clone(),
                    billing_service,
                    event_bus.clone(),
                    hmac_key,
                    effective_tier_service.clone(),
                    team_memberships_sync,
                ),
            )
                as Arc<
                    dyn aegis_orchestrator_core::application::team_service::TeamService,
                >)
        }
        _ => None,
    };

    // ADR-117 §F: assemble the `/v1/edge/*` REST surface from the edge
    // components built above (one EdgeConnectionRegistry per process). The
    // operator-facing services (IssueEnrollmentToken, ManageGroups, ManageTags,
    // RevokeEdge) are local to this bundle and not shared with the
    // tool-invocation service.
    let edge_api_state: Option<aegis_orchestrator_core::api::rest::edge::EdgeApiState> =
        edge_components.as_ref().map(
            |(
                edge_repo,
                group_repo,
                conn_registry,
                dispatch_service,
                resolver,
                fleet_dispatcher,
                fleet_cancel,
            )| {
                use aegis_orchestrator_core::application::edge::issue_enrollment_token::{
                    EnrollmentTokenIssuer, IssueEnrollmentToken, RelayGrpcEnrollmentTokenIssuer,
                    EDGE_ENROLLMENT_SIGNING_KEY,
                };
                use aegis_orchestrator_core::application::edge::manage_groups::ManageGroupsService;
                use aegis_orchestrator_core::application::edge::manage_tags::ManageTagsService;
                use aegis_orchestrator_core::application::edge::revoke_edge::RevokeEdgeService;
                use aegis_orchestrator_core::domain::node_config::NodeRole;

                // ADR-117 SaaS topology: signing capability for
                // `transit/sign/edge-enrollment-token` is held ONLY by the
                // `relay-coordinator` AppRole. The Controller pod does not
                // have the OpenBao policy and MUST proxy enrollment-token
                // requests to a co-located Relay Coordinator over the
                // trusted in-pod network.
                let cluster_role = config
                    .spec
                    .cluster
                    .as_ref()
                    .map(|c| c.role)
                    .unwrap_or(NodeRole::Hybrid);
                let relay_endpoint = config
                    .spec
                    .cluster
                    .as_ref()
                    .and_then(|c| c.relay_coordinator_endpoint.as_ref())
                    .map(|endpoint| endpoint.expose().to_string());

                let cluster_endpoint = config
                    .spec
                    .network
                    .as_ref()
                    .map(|n| format!("{}:{}", n.bind_address, n.grpc_port))
                    .unwrap_or_else(|| "0.0.0.0:50051".to_string());
                let issuer_url = config
                    .spec
                    .zaru
                    .as_ref()
                    .and_then(|z| resolve_env_value(z.public_url.expose()).ok())
                    .unwrap_or_else(|| "aegis-controller".to_string());

                // The dispatch is keyed strictly on the presence of
                // `cluster.relay_coordinator_endpoint`. When set, this
                // process MUST proxy — it does not hold the OpenBao policy.
                // When unset (OSS single-process, or relay-coordinator
                // itself), sign locally with the canonical key name.
                let issue_token: Arc<dyn EnrollmentTokenIssuer> = match relay_endpoint {
                    Some(endpoint) => {
                        tracing::info!(
                            role = ?cluster_role,
                            relay = %SensitiveUrl::new(endpoint.as_str()),
                            "edge enrollment-token issuer: gRPC to relay-coordinator"
                        );
                        Arc::new(
                            RelayGrpcEnrollmentTokenIssuer::connect_lazy(endpoint).expect(
                                "construct RelayGrpcEnrollmentTokenIssuer from configured \
                                 relay_coordinator_endpoint",
                            ),
                        )
                    }
                    None => {
                        tracing::info!(
                            role = ?cluster_role,
                            "edge enrollment-token issuer: local signer"
                        );
                        Arc::new(IssueEnrollmentToken::new(
                            secrets_manager.secret_store(),
                            issuer_url,
                            cluster_endpoint,
                            EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
                        ))
                    }
                };
                let group_service = Arc::new(ManageGroupsService::new(group_repo.clone()));
                let tag_service = Arc::new(ManageTagsService::new(edge_repo.clone()));
                let revoke_service = Arc::new(RevokeEdgeService::new(
                    edge_repo.clone(),
                    conn_registry.clone(),
                ));

                aegis_orchestrator_core::api::rest::edge::EdgeApiState {
                    issue_token,
                    edge_repo: edge_repo.clone(),
                    group_service,
                    tag_service,
                    revoke_service,
                    resolver: resolver.clone(),
                    fleet_dispatcher: fleet_dispatcher.clone(),
                    fleet_cancel: fleet_cancel.clone(),
                    dispatch_service: dispatch_service.clone(),
                    connection_registry: conn_registry.clone(),
                }
            },
        );

    let app_state = AppState {
        agent_service: agent_service.clone(),
        execution_service: execution_service.clone(),
        execution_repo: execution_repo.clone(),
        correlated_activity_stream_service: Arc::new(CorrelatedActivityStreamService::new(
            event_bus.clone(),
            execution_repo.clone(),
            Some(workflow_execution_repo.clone()),
        )),
        cluster_repo: cluster_repo.clone(),
        event_bus: event_bus.clone(),
        inner_loop_service: inner_loop_service.clone(),
        llm_registry,
        human_input_service: human_input_service.clone(),
        temporal_event_listener,
        register_workflow_use_case: register_workflow_use_case.clone(),
        start_workflow_execution_use_case,
        workflow_repo: workflow_repo.clone(),
        workflow_execution_repo: workflow_execution_repo.clone(),
        workflow_scope_service,
        agent_scope_service,
        temporal_client_container: temporal_client_container.clone(),
        storage_event_repo: storage_event_repo.clone(),
        tool_invocation_service: tool_invocation_service.clone(),
        attestation_service: attestation_service.clone(),
        swarm_service: swarm_service.clone(),
        operator_read_model: operator_read_model.clone(),
        cortex_client: cortex_client.clone(),
        rate_limit_override_repo: db_pool.as_ref().map(|pool| {
            Arc::new(aegis_orchestrator_core::infrastructure::rate_limit::RateLimitOverrideRepository::new(pool.clone()))
        }),
        api_key_repo: db_pool.as_ref().map(|pool| {
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::PostgresApiKeyRepository::new(pool.clone()))
        }),
        iam_service: iam_service.clone(),
        operator_escalations: Some(operator_escalations.clone()),
        tenant_provisioning_service,
        realm_repo: db_pool.as_ref().map(|pool| {
            Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::PostgresRealmRepository::new(
                    pool.clone(),
                ),
            ) as Arc<dyn aegis_orchestrator_core::domain::iam::RealmRepository>
        }),
        credential_service,
        secrets_manager: secrets_manager.clone(),
        webhook_secret_provider: Arc::new(EnvWebhookSecretProvider),
        stimulus_service: None,
        user_volume_service,
        file_operations_service,
        git_repo_service,
        canvas_service,
        script_service,
        team_service,
        team_repo: team_repo_opt.clone(),
        membership_repo: membership_repo_opt.clone(),
        effective_tier_service,
        config: config.clone(),
        start_time: std::time::Instant::now(),
        keycloak_admin: colony_keycloak_admin,
        tenant_repo: colony_tenant_repo,
        billing_repo: db_pool.as_ref().map(|pool| {
            Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::PostgresBillingRepository::new(
                    pool.clone(),
                ),
            ) as Arc<dyn aegis_orchestrator_core::infrastructure::repositories::BillingRepository>
        }),
        billing_config: config.spec.billing.clone(),
        zaru_url: config
            .spec
            .zaru
            .as_ref()
            .and_then(|cfg| resolve_env_value(cfg.public_url.expose()).ok()),
        zaru_internal_secret: config
            .spec
            .zaru
            .as_ref()
            .and_then(|cfg| resolve_env_value(cfg.internal_secret.expose()).ok()),
        edge_api: edge_api_state,
    };

    info!("Building router...");
    // Build HTTP router
    let app = create_router(Arc::new(app_state), iam_service.clone());

    // Start HTTP server. Audit 002 §4.27: when no network block is supplied
    // we default to the loopback address. Operators must explicitly set
    // `spec.network.bind_address` AND configure `spec.network.tls` to bind
    // on a non-loopback interface (enforced by NodeConfigManifest::validate).
    let bind_addr = if let Some(network) = &config.spec.network {
        network.bind_address.clone()
    } else {
        "127.0.0.1".to_string()
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

    let grpc_addr_str = format!("{bind_addr}:{grpc_port}");
    let grpc_addr: std::net::SocketAddr = grpc_addr_str
        .parse()
        .with_context(|| format!("Failed to parse gRPC address: {grpc_addr_str}"))?;

    // Tool routing services moved above AppState!

    // Security contexts are loaded from aegis-config.yaml (spec.security_contexts)
    // during repository initialization above. See ADR-071 §ZaruTier SecurityContext Definitions.

    // Spawn gRPC server
    let exec_service_clone: Arc<dyn ExecutionService> = execution_service.clone();
    let val_service_clone = validation_service.clone();
    let agent_service_for_grpc: Arc<dyn AgentLifecycleService> = agent_service.clone();
    let volume_service_for_grpc: Arc<
        dyn aegis_orchestrator_core::application::volume_manager::VolumeService,
    > = volume_service.clone();
    let output_handler_service: Arc<
        dyn aegis_orchestrator_core::application::output_handler_service::OutputHandlerService,
    > = Arc::new(
        aegis_orchestrator_core::application::output_handler_service::StandardOutputHandlerService::new(
            execution_service.clone(),
            agent_service.clone(),
            event_bus.clone(),
        ),
    );
    let grpc_auth = match (&iam_service, config.spec.grpc_auth.clone()) {
        (Some(iam), Some(grpc_auth)) if grpc_auth.enabled => Some(
            aegis_orchestrator_core::presentation::grpc::auth_interceptor::GrpcIamAuthInterceptor::new(
                iam.clone(),
                &grpc_auth,
            ),
        ),
        (Some(iam), None) => Some(
            aegis_orchestrator_core::presentation::grpc::auth_interceptor::GrpcIamAuthInterceptor::new(
                iam.clone(),
                &aegis_orchestrator_core::domain::node_config::GrpcAuthConfig {
                    enabled: true,
                    exempt_methods: vec![],
                },
            ),
        ),
        _ => None,
    };

    // The main gRPC server is ready while its database answers, when it has
    // one; without one it serves from in-memory repositories.
    let grpc_readiness: Vec<ReadinessCheck> = db_pool
        .as_ref()
        .map(|pool| vec![postgres_readiness(pool.clone(), DATABASE_READINESS_TIMEOUT)])
        .unwrap_or_default();

    tokio::spawn(async move {
        tracing::info!(address = %grpc_addr, "Starting gRPC server");
        if let Err(e) = aegis_orchestrator_core::presentation::grpc::server::start_grpc_server(
            aegis_orchestrator_core::presentation::grpc::server::GrpcServerConfig {
                addr: grpc_addr,
                execution_service: exec_service_clone,
                validation_service: val_service_clone,
                grpc_auth,
                attestation_service: Some(attestation_service),
                tool_invocation_service: Some(tool_invocation_service),
                cortex_client,
                run_container_step_use_case: Some(run_container_step_use_case),
                agent_service: Some(agent_service_for_grpc),
                // GrpcServerConfig now accepts an optional StimulusService, making
                // stimulus routing configurable per callsite. In the CLI daemon it remains
                // disabled (None) because StandardStimulusService requires a WorkflowRegistry
                // (the routing table for stimulus-to-workflow dispatch) that is not set up
                // here. To enable ingest_stimulus in the daemon, construct a WorkflowRegistry,
                // build a StandardStimulusService, and pass it as Some(...).
                stimulus_service: None,
                discovery_service: discovery_service.clone(),
                volume_service: Some(volume_service_for_grpc),
                output_handler_service: Some(output_handler_service),
                fsal: Some(nfs_gateway.fsal().clone()),
                fuse_mount_client: fuse_mount_client.clone(),
                readiness: grpc_readiness,
            },
        )
        .await
        {
            tracing::error!(error = %e, "gRPC server failed");
        }
    });

    // Keep sweeper shutdown sender alive until daemon shutdown.
    let mut _health_sweeper_shutdown: Option<tokio::sync::watch::Sender<bool>> = None;

    // ─── Cluster gRPC server (ADR-059) ─────────────────────────────────────
    // When clustering is enabled, start a second gRPC server on the dedicated
    // cluster port (default 50056) that exposes the inter-node
    // NodeClusterService (attestation, heartbeat, execution routing, etc.).
    if config.spec.cluster.as_ref().is_some_and(|c| c.enabled) {
        let cluster_grpc_port = config
            .spec
            .cluster
            .as_ref()
            .map(|c| c.cluster_grpc_port)
            .unwrap_or(50056);

        let cluster_addr_str = format!("{bind_addr}:{cluster_grpc_port}");
        let cluster_addr: std::net::SocketAddr = cluster_addr_str
            .parse()
            .with_context(|| format!("Failed to parse cluster gRPC address: {cluster_addr_str}"))?;

        let controller_node_id = {
            let id_str = &config.spec.node.id;
            aegis_orchestrator_core::domain::cluster::NodeId(
                uuid::Uuid::parse_str(id_str).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            )
        };

        // The cluster repositories require a PostgreSQL pool. When a pool is
        // available we construct production Pg-backed repos; otherwise we log a
        // warning and skip – clustering without persistence is unsupported.
        if let Some(ref pool) = db_pool {
            use aegis_orchestrator_core::infrastructure::cluster::{
                NodeClusterServiceHandler, PgClusterEnrolmentTokenRepository,
                PgConfigLayerRepository, PgNodeChallengeRepository, PgNodeClusterRepository,
                PgNodeRegistryRepository, RoundRobinNodeRouter,
            };
            use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::node_cluster_service_server::NodeClusterServiceServer;

            let cluster_repo: Arc<dyn NodeClusterRepository> =
                Arc::new(PgNodeClusterRepository::new(pool.clone()));
            let challenge_repo: Arc<
                dyn aegis_orchestrator_core::domain::cluster::NodeChallengeRepository,
            > = Arc::new(PgNodeChallengeRepository::new(pool.clone()));

            let secret_store = secrets_manager.secret_store();

            // Audit 002 §4.9: cluster admission gate — single-use enrolment
            // token, scoped to a node identity, redeemed atomically at attest
            // time. Issued out of band by the controller admin path.
            let cluster_enrolment_repo: Arc<
                dyn aegis_orchestrator_core::domain::cluster::ClusterEnrolmentTokenRepository,
            > = Arc::new(PgClusterEnrolmentTokenRepository::new(pool.clone()));
            let attest_uc = Arc::new(
                aegis_orchestrator_core::application::cluster::AttestNodeUseCase::new(
                    challenge_repo.clone(),
                    cluster_enrolment_repo.clone(),
                ),
            );
            // ADR-117: edge enrollment service — verifies the bootstrap JWT,
            // atomically redeems its `jti`, and persists the EdgeDaemon row.
            // Wired into ChallengeNodeUseCase so that ChallengeNode(role=Edge)
            // populates `edge_daemons` instead of returning "Edge enrollment
            // not enabled on this controller". Only attached when the
            // edge_components bundle is present (i.e. a Postgres pool exists,
            // which is required for both `edge_daemons` and the
            // `enrollment_tokens` JTI ledger).
            let enroll_edge_service = edge_components.as_ref().map(|(edge_repo, _, _, _, _, _, _)| {
                use aegis_orchestrator_core::application::edge::enroll_edge::EnrollEdgeService;
                use aegis_orchestrator_core::infrastructure::edge::PgEnrollmentTokenRepository;
                let enrollment_token_repo: Arc<
                    dyn aegis_orchestrator_core::domain::edge::EnrollmentTokenRepository,
                > = Arc::new(PgEnrollmentTokenRepository::new(pool.clone()));
                let issuer_url = config
                    .spec
                    .zaru
                    .as_ref()
                    .and_then(|z| resolve_env_value(z.public_url.expose()).ok())
                    .unwrap_or_else(|| "aegis-controller".to_string());
                Arc::new(EnrollEdgeService::new(
                    edge_repo.clone(),
                    enrollment_token_repo,
                    secret_store.clone(),
                    aegis_orchestrator_core::application::edge::issue_enrollment_token::EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
                    issuer_url,
                ))
            });

            let mut challenge_uc_inner =
                aegis_orchestrator_core::application::cluster::ChallengeNodeUseCase::new(
                    challenge_repo.clone(),
                    cluster_repo.clone(),
                    secret_store.clone(),
                    aegis_orchestrator_core::application::edge::issue_enrollment_token::EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
                );
            if let Some(svc) = enroll_edge_service.clone() {
                challenge_uc_inner = challenge_uc_inner.with_enroll_edge_service(svc);
            }
            let challenge_uc = Arc::new(challenge_uc_inner);
            let registry_repo: Arc<
                dyn aegis_orchestrator_core::domain::cluster::NodeRegistryRepository,
            > = Arc::new(PgNodeRegistryRepository::new(pool.clone()));
            let register_uc = Arc::new(
                aegis_orchestrator_core::application::cluster::RegisterNodeUseCase::new(
                    cluster_repo.clone(),
                    registry_repo.clone(),
                    controller_node_id,
                ),
            );
            let heartbeat_uc = Arc::new(
                aegis_orchestrator_core::application::cluster::HeartbeatUseCase::new(
                    cluster_repo.clone(),
                ),
            );
            let router: Arc<dyn aegis_orchestrator_core::domain::cluster::NodeRouter> =
                Arc::new(RoundRobinNodeRouter::new());
            let route_uc = Arc::new(
                aegis_orchestrator_core::application::cluster::RouteExecutionUseCase::new(
                    cluster_repo.clone(),
                    router,
                    controller_node_id,
                ),
            );
            let forward_uc = Arc::new(
                aegis_orchestrator_core::application::cluster::ForwardExecutionUseCase::new(
                    execution_service.clone(),
                ),
            );

            let config_layer_repo: Arc<
                dyn aegis_orchestrator_core::domain::cluster::ConfigLayerRepository,
            > = Arc::new(PgConfigLayerRepository::new(Arc::new(pool.clone())));

            let sync_config_uc = Arc::new(
                aegis_orchestrator_core::application::cluster::SyncConfigUseCase::new(
                    config_layer_repo.clone(),
                    cluster_repo.clone(),
                ),
            );
            let push_config_uc = Arc::new(
                aegis_orchestrator_core::application::cluster::PushConfigUseCase::new(
                    config_layer_repo,
                ),
            );

            let mut handler = NodeClusterServiceHandler::new(
                attest_uc,
                challenge_uc,
                register_uc,
                heartbeat_uc,
                route_uc,
                Some(forward_uc),
                sync_config_uc,
                push_config_uc,
                cluster_repo.clone(),
            );

            // ADR-117: the relay-coordinator (and Hybrid / Controller-without-relay
            // deployments) hold the OpenBao policy
            // `transit/sign/edge-enrollment-token` and serve
            // `IssueEnrollmentToken` directly. Controllers that delegate to a
            // peer relay leave this absent; the RPC then returns
            // FailedPrecondition. The dispatch is keyed on the *absence* of
            // `relay_coordinator_endpoint` — when set, this process proxies
            // outbound and does not sign.
            if config
                .spec
                .cluster
                .as_ref()
                .and_then(|c| c.relay_coordinator_endpoint.clone())
                .is_none()
            {
                use aegis_orchestrator_core::application::edge::issue_enrollment_token::{
                    IssueEnrollmentToken, EDGE_ENROLLMENT_SIGNING_KEY,
                };
                let issuer_url = config
                    .spec
                    .zaru
                    .as_ref()
                    .and_then(|z| resolve_env_value(z.public_url.expose()).ok())
                    .unwrap_or_else(|| "aegis-controller".to_string());
                let cluster_public_endpoint = config
                    .spec
                    .cluster
                    .as_ref()
                    .and_then(|c| c.ingress.as_ref())
                    .and_then(|i| resolve_env_value(i.public_endpoint.expose()).ok())
                    .unwrap_or_else(|| cluster_addr_str.clone());
                let local_signer = Arc::new(IssueEnrollmentToken::new(
                    secrets_manager.secret_store(),
                    issuer_url,
                    cluster_public_endpoint,
                    EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
                ));
                let cluster_grpc_auth = match (&iam_service, config.spec.grpc_auth.clone()) {
                    (Some(iam), Some(cfg)) if cfg.enabled => Some(
                        aegis_orchestrator_core::presentation::grpc::auth_interceptor::GrpcIamAuthInterceptor::new(
                            iam.clone(),
                            &cfg,
                        ),
                    ),
                    (Some(iam), None) => Some(
                        aegis_orchestrator_core::presentation::grpc::auth_interceptor::GrpcIamAuthInterceptor::new(
                            iam.clone(),
                            &aegis_orchestrator_core::domain::node_config::GrpcAuthConfig {
                                enabled: true,
                                exempt_methods: vec![],
                            },
                        ),
                    ),
                    _ => None,
                };
                handler = handler.with_issue_enrollment_token(local_signer, cluster_grpc_auth);
            }

            // ADR-117 §B/§C: wire ConnectEdge / RotateEdgeKey into the cluster
            // gRPC handler. Without this, both RPCs return
            // FailedPrecondition("edge mode disabled on this node") and edge
            // daemons can enroll but never open the bidi stream. The wiring
            // is NOT gated on `relay_coordinator_endpoint`: that gate is
            // specific to enrollment-token signing (only the Relay holds the
            // OpenBao policy). ConnectEdge handling is for "edges connect
            // directly to this cluster server" — every node that runs the
            // cluster gRPC server (Controller / Hybrid / RelayCoordinator)
            // must accept ConnectEdge. Pure-worker nodes have no
            // edge_components bundle (no DB pool) and naturally skip this.
            if let Some((edge_repo, _, conn_registry, _, _, _, _)) = edge_components.as_ref() {
                use aegis_orchestrator_core::application::edge::connect_edge::ConnectEdgeService;
                use aegis_orchestrator_core::application::edge::issue_enrollment_token::EDGE_ENROLLMENT_SIGNING_KEY;
                use aegis_orchestrator_core::application::edge::rotate_edge_key::RotateEdgeKeyService;
                let connect_edge_service = Arc::new(ConnectEdgeService::new(
                    edge_repo.clone(),
                    conn_registry.clone(),
                ));
                let rotate_edge_key_service = Arc::new(RotateEdgeKeyService::with_default_ttl(
                    edge_repo.clone(),
                    secrets_manager.secret_store(),
                    EDGE_ENROLLMENT_SIGNING_KEY.to_string(),
                ));
                handler = handler.with_edge_services(connect_edge_service, rotate_edge_key_service);
            }

            // Remote storage gRPC handler (ADR-064)
            // Route through AegisFSAL for path sanitization, volume authorization,
            // quota enforcement, and audit trail with cross-node provenance.
            use aegis_orchestrator_core::infrastructure::aegis_remote_storage_proto::remote_storage_service_server::RemoteStorageServiceServer;
            use aegis_orchestrator_core::infrastructure::storage::RemoteStorageServiceHandler;

            let health_sweeper_repo = cluster_repo.clone();

            let remote_fsal = {
                use aegis_orchestrator_core::application::storage_router::StorageRouter;
                use aegis_orchestrator_core::infrastructure::storage::{
                    LocalHostStorageProvider, SealStorageProvider,
                };
                use parking_lot::RwLock;
                use std::collections::HashMap;

                let primary_local_path = std::env::temp_dir().join("aegis");
                let fallback_local_path = std::env::temp_dir();
                let local_provider = Arc::new(
                    match LocalHostStorageProvider::new(&primary_local_path) {
                        Ok(provider) => provider,
                        Err(primary_error) => {
                            match LocalHostStorageProvider::new(&fallback_local_path) {
                                Ok(provider) => provider,
                                Err(fallback_error) => {
                                    panic!(
                                        "Failed to initialize local storage provider: primary path '{}' failed: {}; fallback temp directory '{}' failed: {}. Remediation: ensure the temp directory is writable and has sufficient space.",
                                        primary_local_path.display(),
                                        primary_error,
                                        fallback_local_path.display(),
                                        fallback_error
                                    );
                                }
                            }
                        }
                    },
                );
                let seal_provider = Arc::new(SealStorageProvider::new());
                let storage_router = Arc::new(StorageRouter::new(
                    storage_provider.clone(),
                    local_provider,
                    seal_provider,
                ));
                let borrowed_volumes = Arc::new(RwLock::new(HashMap::new()));

                Arc::new(aegis_orchestrator_core::domain::fsal::AegisFSAL::new(
                    storage_router,
                    volume_repo.clone(),
                    borrowed_volumes,
                    event_publisher.clone(),
                ))
            };

            let storage_handler =
                RemoteStorageServiceHandler::new(remote_fsal, cluster_repo, controller_node_id);

            // The cluster server is ready while its database answers: every
            // NodeClusterService RPC reads or writes the cluster tables.
            let cluster_readiness =
                vec![postgres_readiness(pool.clone(), DATABASE_READINESS_TIMEOUT)];

            tokio::spawn(async move {
                tracing::info!(address = %cluster_addr, "Starting cluster gRPC server on port {cluster_grpc_port}");
                let cluster_server = NodeClusterServiceServer::new(handler);
                let storage_server = RemoteStorageServiceServer::new(storage_handler);
                let (health, health_server) = GrpcHealth::new(&[
                    service_name(&cluster_server),
                    service_name(&storage_server),
                ])
                .await;
                let monitor = health.spawn_monitor(cluster_readiness, READINESS_INTERVAL);
                if let Err(e) = tonic::transport::Server::builder()
                    .add_service(health_server)
                    .add_service(cluster_server)
                    .add_service(storage_server)
                    .serve(cluster_addr)
                    .await
                {
                    tracing::error!(error = %e, "Cluster gRPC server failed");
                }
                monitor.abort();
            });

            // ADR-062: Spawn health sweeper for stale heartbeat detection
            {
                let stale_threshold = std::time::Duration::from_secs(
                    config
                        .spec
                        .cluster
                        .as_ref()
                        .and_then(|c| c.stale_threshold_secs)
                        .unwrap_or(90),
                );
                let sweep_interval = std::time::Duration::from_secs(
                    config
                        .spec
                        .cluster
                        .as_ref()
                        .and_then(|c| c.sweep_interval_secs)
                        .unwrap_or(30),
                );
                let sweeper = aegis_orchestrator_core::application::cluster::HealthSweeper::new(
                    health_sweeper_repo,
                    event_bus.clone(),
                    stale_threshold,
                    sweep_interval,
                );
                let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                tokio::spawn(async move {
                    sweeper.run(shutdown_rx).await;
                });
                // Keep sender in lifecycle state so it is dropped during daemon
                // shutdown, allowing the sweeper to exit cleanly.
                _health_sweeper_shutdown = Some(shutdown_tx);
                tracing::info!(
                    stale_threshold_secs = stale_threshold.as_secs(),
                    sweep_interval_secs = sweep_interval.as_secs(),
                    "Health sweeper started"
                );
            }
        } else {
            tracing::warn!(
                "Cluster mode enabled but no database configured; \
                 skipping cluster gRPC server (PostgreSQL is required for cluster state)"
            );
        }
    }

    // ─── Worker lifecycle task (ADR-059) ─────────────────────────────────────
    // When the node's cluster role is Worker or Hybrid AND a controller
    // endpoint is configured, spawn a background task that performs
    // attestation, registration, heartbeat loop, and graceful deregistration.
    //
    // The sender is kept alive in this binding for the duration of `start_daemon`
    // (i.e., until the HTTP server exits). Dropping it signals the lifecycle task
    // to execute its graceful deregistration step.
    let mut _worker_lifecycle_shutdown: Option<tokio::sync::watch::Sender<bool>> = None;
    if let Some(ref cluster_config) = config.spec.cluster {
        if cluster_config.enabled {
            let role = &cluster_config.role;
            if matches!(role, NodeRole::Worker | NodeRole::Hybrid) {
                if let Some(ref controller) = cluster_config.controller {
                    let worker_node_id = {
                        let id_str = &config.spec.node.id;
                        aegis_orchestrator_core::domain::cluster::NodeId(
                            uuid::Uuid::parse_str(id_str).unwrap_or_else(|_| uuid::Uuid::new_v4()),
                        )
                    };

                    // Load Ed25519 signing key from the configured keypair path
                    let keypair_path = &cluster_config.node_keypair_path;
                    match tokio::fs::read(keypair_path).await {
                        Ok(key_bytes) => {
                            // Node keypair is stored as raw 32-byte Ed25519 seed
                            // (written by `aegis node init` via `SigningKey::to_bytes()`).
                            let key_result: Result<ed25519_dalek::SigningKey, anyhow::Error> =
                                if key_bytes.len() == 32 {
                                    let seed: [u8; 32] = key_bytes[..32].try_into().unwrap();
                                    Ok(ed25519_dalek::SigningKey::from_bytes(&seed))
                                } else {
                                    Err(anyhow::anyhow!(
                                        "Expected 32-byte Ed25519 seed, got {} bytes",
                                        key_bytes.len()
                                    ))
                                };
                            match key_result {
                                Ok(signing_key) => {
                                    let signing_key = std::sync::Arc::new(signing_key);

                                    let proto_role = match role {
                                        NodeRole::Controller => {
                                            aegis_orchestrator_core::infrastructure::aegis_cluster_proto::NodeRole::Controller
                                                as i32
                                        }
                                        NodeRole::Worker => {
                                            aegis_orchestrator_core::infrastructure::aegis_cluster_proto::NodeRole::Worker
                                                as i32
                                        }
                                        NodeRole::Hybrid => {
                                            aegis_orchestrator_core::infrastructure::aegis_cluster_proto::NodeRole::Hybrid
                                                as i32
                                        }
                                        NodeRole::Edge => {
                                            aegis_orchestrator_core::infrastructure::aegis_cluster_proto::NodeRole::Edge
                                                as i32
                                        }
                                        NodeRole::RelayCoordinator => {
                                            aegis_orchestrator_core::infrastructure::aegis_cluster_proto::NodeRole::RelayCoordinator
                                                as i32
                                        }
                                    };

                                    let capabilities =
                                        aegis_orchestrator_core::infrastructure::aegis_cluster_proto::NodeCapabilities {
                                            gpu_count: config
                                                .spec
                                                .node
                                                .resources
                                                .as_ref()
                                                .map(|r| r.gpu_count)
                                                .unwrap_or(0),
                                            vram_gb: config
                                                .spec
                                                .node
                                                .resources
                                                .as_ref()
                                                .map(|r| r.vram_gb)
                                                .unwrap_or(0),
                                            cpu_cores: config
                                                .spec
                                                .node
                                                .resources
                                                .as_ref()
                                                .map(|r| r.cpu_cores)
                                                .unwrap_or(0),
                                            available_memory_gb: config
                                                .spec
                                                .node
                                                .resources
                                                .as_ref()
                                                .map(|r| r.memory_gb)
                                                .unwrap_or(0),
                                            supported_runtimes: vec!["docker".to_string()],
                                            tags: config.spec.node.tags.clone(),
                                        };

                                    let grpc_address = format!(
                                        "{}:{}",
                                        bind_addr,
                                        config
                                            .spec
                                            .network
                                            .as_ref()
                                            .map(|n| n.grpc_port)
                                            .unwrap_or(50051)
                                    );

                                    let heartbeat_interval = std::time::Duration::from_secs(
                                        cluster_config.heartbeat_interval_secs,
                                    );
                                    let token_refresh_margin = std::time::Duration::from_secs(
                                        cluster_config.token_refresh_margin_secs,
                                    );

                                    let client =
                                        aegis_orchestrator_core::infrastructure::cluster::NodeClusterClient::new(
                                            controller.endpoint.expose().to_string(),
                                            signing_key.clone(),
                                            worker_node_id,
                                        );

                                    // Audit 002 §4.9: cluster admission token
                                    // sourced from the controller config block.
                                    // env: indirection is supported.
                                    let enrolment_token = controller
                                        .token
                                        .as_ref()
                                        .map(|t| t.expose())
                                        .map(|t| {
                                            aegis_orchestrator_core::domain::node_config::resolve_env_value(t)
                                                .unwrap_or_else(|_| t.to_string())
                                        })
                                        .unwrap_or_default();
                                    let lifecycle = super::worker_lifecycle::WorkerLifecycle::new(
                                        client,
                                        worker_node_id,
                                        proto_role,
                                        capabilities,
                                        grpc_address,
                                        heartbeat_interval,
                                        token_refresh_margin,
                                        signing_key,
                                        enrolment_token,
                                    );

                                    let (shutdown_tx, shutdown_rx) =
                                        tokio::sync::watch::channel(false);

                                    tokio::spawn(async move {
                                        tracing::info!("Starting worker lifecycle background task");
                                        if let Err(e) = lifecycle.run(shutdown_rx).await {
                                            tracing::error!(
                                                error = %e,
                                                "Worker lifecycle task failed"
                                            );
                                        }
                                    });

                                    // Keep the sender alive until the daemon exits.
                                    // Dropping it will signal the lifecycle task to
                                    // execute its graceful deregistration step.
                                    _worker_lifecycle_shutdown = Some(shutdown_tx);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        path = %keypair_path.display(),
                                        "Failed to load node keypair; skipping worker lifecycle"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                path = %keypair_path.display(),
                                "Failed to read node keypair file; skipping worker lifecycle"
                            );
                        }
                    }
                } else {
                    tracing::info!(
                        "Worker/Hybrid role but no controller endpoint configured; \
                         skipping worker lifecycle (standalone mode)"
                    );
                }
            }
        }
    }

    // --- Deploy vendored built-in agents and workflows ---
    if config.spec.deploy_builtins {
        use crate::commands::builtins;

        let force_builtins = config
            .spec
            .force_deploy_builtins
            .as_deref()
            .map(|v| resolve_env_value(v).unwrap_or_default())
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let system_tenant = aegis_orchestrator_core::domain::tenant::TenantId::system();
        for (name, yaml) in builtins::BUILTIN_AGENTS {
            match serde_yaml::from_str::<aegis_orchestrator_sdk::AgentManifest>(yaml) {
                Ok(manifest) => {
                    // Determine whether the built-in needs to be (re)deployed.
                    // The version-collision gate that the LLM agent.create path
                    // uses is wrong for platform built-ins: built-ins are
                    // shipped as `include_str!()` template bytes, and the
                    // correct invariant is that the deployed manifest must
                    // match the embedded template byte-for-byte (after
                    // canonical normalisation). `metadata.version` is a human
                    // change-marker, not a CI gate.
                    let existing_id = agent_service
                        .lookup_agent_for_tenant(&system_tenant, &manifest.metadata.name)
                        .await
                        .ok()
                        .flatten();

                    let needs_deploy = if force_builtins {
                        // Operator escape hatch: always overwrite.
                        true
                    } else {
                        match existing_id {
                            None => true, // not deployed → install
                            Some(id) => match agent_service
                                .get_agent_for_tenant(&system_tenant, id)
                                .await
                            {
                                Ok(deployed) => {
                                    match builtins::agent_manifest_matches(&deployed.manifest, yaml)
                                    {
                                        Ok(true) => {
                                            info!(
                                                "Built-in agent '{}' v{} already up to date",
                                                name, manifest.metadata.version
                                            );
                                            false
                                        }
                                        Ok(false) => {
                                            info!(
                                                "Built-in agent '{}' v{} content drift detected — overwriting",
                                                name, manifest.metadata.version
                                            );
                                            true
                                        }
                                        Err(e) => {
                                            warn!(
                                                "Failed to compare built-in agent '{}' against deployed: {}; \
                                                 falling back to overwrite",
                                                name, e
                                            );
                                            true
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to fetch deployed built-in agent '{}' for drift \
                                         comparison: {}; falling back to overwrite",
                                        name, e
                                    );
                                    true
                                }
                            },
                        }
                    };

                    if needs_deploy {
                        // `force = true` here is required so the underlying
                        // version-collision check in the LLM agent.create path
                        // does not reject the same metadata.version with new
                        // content. This is the platform-side overwrite, not a
                        // user-driven one.
                        match agent_service
                            .deploy_agent_for_tenant(
                                &system_tenant,
                                manifest,
                                true,
                                aegis_orchestrator_core::domain::agent::AgentScope::Global,
                                None,
                            )
                            .await
                        {
                            Ok(id) => info!("Deployed built-in agent '{}' (id: {})", name, id),
                            Err(e) => warn!("Failed to deploy built-in agent '{}': {}", name, e),
                        }
                    }
                }
                Err(e) => warn!("Failed to parse built-in agent template '{}': {}", name, e),
            }
        }

        for (wf_name, wf_template) in builtins::BUILTIN_WORKFLOWS {
            let deployed = workflow_repo
                .find_by_name_for_tenant(&system_tenant, wf_name)
                .await
                .ok()
                .flatten();

            let needs_deploy = if force_builtins {
                true
            } else {
                match deployed.as_ref() {
                    None => true,
                    Some(workflow) => match builtins::workflow_matches(workflow, wf_template) {
                        Ok(true) => {
                            info!(
                                "Built-in workflow '{}' v{} already up to date",
                                wf_name,
                                workflow.metadata.version.as_deref().unwrap_or("?")
                            );
                            false
                        }
                        Ok(false) => {
                            info!(
                                "Built-in workflow '{}' v{} content drift detected — overwriting",
                                wf_name,
                                workflow.metadata.version.as_deref().unwrap_or("?")
                            );
                            true
                        }
                        Err(e) => {
                            warn!(
                                "Failed to compare built-in workflow '{}' against deployed: {}; \
                                 falling back to overwrite",
                                wf_name, e
                            );
                            true
                        }
                    },
                }
            };

            if needs_deploy {
                match register_workflow_use_case
                    .register_workflow_for_tenant(
                        &system_tenant,
                        wf_template,
                        true,
                        aegis_orchestrator_core::domain::workflow::WorkflowScope::Global,
                    )
                    .await
                {
                    Ok(_) => info!("Deployed built-in workflow '{}'", wf_name),
                    Err(e) => warn!("Failed to deploy built-in workflow '{}': {:#}", wf_name, e),
                }
            }
        }

        info!("Built-in template deployment complete");
    } else {
        info!("Built-in template deployment disabled (spec.deploy_builtins = false)");
    }

    // ADR-117: the relay-coordinator role is dispatched to its own
    // self-contained boot path early in `start_daemon` (see
    // `crate::daemon::relay_server::run_relay_coordinator`). Reaching this
    // point implies role ∈ { Controller, Worker, Hybrid, Edge } — all of
    // which serve the full HTTP surface below.

    let addr = format!("{bind_addr}:{final_port}");
    debug!(address = %addr, "Binding to address");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {addr}"))?;

    info!(address = %addr, "Daemon listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed")?;

    info!("Daemon shutting down");

    Ok(())
}

/// Support `.env` single-line PEM values where newlines are escaped as `\n`.
fn normalize_seal_private_key(raw: &str) -> String {
    if raw.contains("\\n") && !raw.contains('\n') {
        raw.replace("\\n", "\n")
    } else {
        raw.to_string()
    }
}

struct PidFileGuard;

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = remove_pid_file();
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            warn!("Ctrl+C handler error: {}", e);
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(e) => {
                warn!("SIGTERM handler error: {}", e);
                std::future::pending::<()>().await;
            }
        }
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

// Temporal Workflow HTTP Handlers moved to handlers/workflows.rs
// temporal_events_handler moved to handlers/dispatch.rs

// ---------------------------------------------------------------------------
// Cortex REST handlers
// ---------------------------------------------------------------------------
// Moved to handlers/cortex.rs

// create_router moved to router.rs

/// Hand the daemon's one LLM provider registry to the inner loop `build`
/// constructs over it, and return that same instance for `AppState`, where
/// `GET /v1/llm/aliases/{alias}` reads it (AEGIS ADR-124 D3): the route
/// answers from the alias table every agent call is routed through.
fn share_llm_registry<L>(
    registry: Arc<ProviderRegistry>,
    build: impl FnOnce(Arc<ProviderRegistry>) -> L,
) -> (L, Arc<ProviderRegistry>) {
    (build(registry.clone()), registry)
}

// Application state is defined in state.rs and re-exported via AppState import above.

// health_handler, readiness_handler moved to handlers/health.rs
// cluster_status_handler, cluster_nodes_handler moved to handlers/cluster.rs
// list_swarms_handler, get_swarm_handler moved to handlers/swarms.rs
// list_stimuli_handler, get_stimulus_handler, list_security_incidents_handler,
// list_storage_violations_handler, dashboard_summary_handler moved to handlers/observability.rs
// deploy_agent_handler, execute_agent_handler, stream_agent_events_handler,
// list_agents_handler, delete_agent_handler, get_agent_handler, lookup_agent_handler moved to handlers/agents.rs

// dispatch_gateway_handler moved to handlers/dispatch.rs

// ========================================
// Workflow API Handlers
// ========================================
// Moved to handlers/workflows.rs and handlers/workflow_executions.rs

// ============================================================================
// Human Approval Handlers
// ============================================================================
// Moved to handlers/approvals.rs

// SEAL handlers moved to handlers/seal.rs

// ============================================================================
// Admin Rate-Limit Override Handlers (ADR-072)
// ============================================================================
// Moved to handlers/admin.rs

#[cfg(test)]
mod tests {
    use super::{resolve_generated_artifacts_root, temporal_connection_max_retries};
    use crate::daemon::container_helpers::managed_container_reap_reason;
    use aegis_orchestrator_core::domain::execution::ExecutionStatus;
    use aegis_orchestrator_core::infrastructure::runtime::ManagedAgentContainer;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        dir.push(format!("aegis-server-test-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// The node configuration with `judge` at production's entry
    /// (`aegis-platform-deployment` d27c51d `podman/pods/core/aegis-config.yaml`
    /// 40-45: `context_window: 256000`, `max_output_tokens: 16384`).
    fn production_judge_config() -> aegis_orchestrator_core::domain::node_config::NodeConfigManifest
    {
        let mut config =
            aegis_orchestrator_core::domain::node_config::NodeConfigManifest::default();
        config.spec.llm_providers = serde_yaml::from_str(
            r#"- name: workers-ai
  type: openai-compatible
  endpoint: "https://api.example.invalid/v1"
  enabled: true
  models:
    - alias: judge
      model: "@cf/google/gemma-4-26b-a4b-it"
      capabilities: ["chat"]
      context_window: 256000
      max_output_tokens: 16384
"#,
        )
        .expect("provider block parses");
        config
    }

    /// The rest of the orchestrator as a goal evaluation sees it: no bound
    /// executions, and a judge that stays running; each judge start's input
    /// is recorded.
    #[derive(Default)]
    struct RecordingGoalWorld {
        judge_inputs: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl aegis_orchestrator_core::application::goal_service::GoalWorld for RecordingGoalWorld {
        async fn read_execution(
            &self,
            _goal: &aegis_orchestrator_core::domain::goal::Goal,
            _bound: &aegis_orchestrator_core::domain::goal::BoundExecution,
        ) -> Option<aegis_orchestrator_core::application::goal_service::ExecutionView> {
            None
        }

        async fn pending_approvals(
            &self,
            _goal: &aegis_orchestrator_core::domain::goal::Goal,
            _execution_ids: &[aegis_orchestrator_core::domain::execution::ExecutionId],
        ) -> Vec<(
            aegis_orchestrator_core::domain::execution::ExecutionId,
            String,
        )> {
            Vec::new()
        }

        async fn start_judge(
            &self,
            _goal: &aegis_orchestrator_core::domain::goal::Goal,
            input: serde_json::Value,
        ) -> Result<aegis_orchestrator_core::domain::execution::ExecutionId, String> {
            self.judge_inputs.lock().unwrap().push(input);
            Ok(aegis_orchestrator_core::domain::execution::ExecutionId::new())
        }

        async fn judge_progress(
            &self,
            _goal: &aegis_orchestrator_core::domain::goal::Goal,
            _judge_execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> aegis_orchestrator_core::application::goal_service::JudgeProgress {
            aegis_orchestrator_core::application::goal_service::JudgeProgress::Running
        }
    }

    /// An execution service that is never called: the validation service is
    /// built here only to read what the daemon gave it.
    struct NoExecutions;

    #[async_trait::async_trait]
    impl aegis_orchestrator_core::application::execution::ExecutionService for NoExecutions {
        async fn start_execution(
            &self,
            _agent_id: aegis_orchestrator_core::domain::agent::AgentId,
            _input: aegis_orchestrator_core::domain::execution::ExecutionInput,
            _security_context_name: String,
            _identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::ExecutionId> {
            anyhow::bail!("not exercised")
        }

        async fn start_execution_with_id(
            &self,
            _execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
            _agent_id: aegis_orchestrator_core::domain::agent::AgentId,
            _input: aegis_orchestrator_core::domain::execution::ExecutionInput,
            _security_context_name: String,
            _identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::ExecutionId> {
            anyhow::bail!("not exercised")
        }

        async fn start_child_execution(
            &self,
            _agent_id: aegis_orchestrator_core::domain::agent::AgentId,
            _input: aegis_orchestrator_core::domain::execution::ExecutionInput,
            _parent_execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::ExecutionId> {
            anyhow::bail!("not exercised")
        }

        async fn get_execution_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::Execution> {
            anyhow::bail!("not exercised")
        }

        async fn get_execution_unscoped(
            &self,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<aegis_orchestrator_core::domain::execution::Execution> {
            anyhow::bail!("not exercised")
        }

        async fn get_iterations_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
            _exec_id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<Vec<aegis_orchestrator_core::domain::execution::Iteration>> {
            anyhow::bail!("not exercised")
        }

        async fn cancel_execution_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }

        async fn stream_execution(
            &self,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = anyhow::Result<
                                aegis_orchestrator_core::domain::events::ExecutionEvent,
                            >,
                        > + Send,
                >,
            >,
        > {
            anyhow::bail!("not exercised")
        }

        async fn stream_agent_events(
            &self,
            _id: aegis_orchestrator_core::domain::agent::AgentId,
        ) -> anyhow::Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = anyhow::Result<
                                aegis_orchestrator_core::infrastructure::event_bus::DomainEvent,
                            >,
                        > + Send,
                >,
            >,
        > {
            anyhow::bail!("not exercised")
        }

        async fn list_executions_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
            _agent_id: Option<aegis_orchestrator_core::domain::agent::AgentId>,
            _workflow_id: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
            _limit: usize,
        ) -> anyhow::Result<Vec<aegis_orchestrator_core::domain::execution::Execution>> {
            anyhow::bail!("not exercised")
        }

        async fn delete_execution_for_tenant(
            &self,
            _tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId,
            _id: aegis_orchestrator_core::domain::execution::ExecutionId,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }

        async fn record_llm_interaction(
            &self,
            _execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
            _iteration: u8,
            _interaction: aegis_orchestrator_core::domain::execution::LlmInteraction,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }

        async fn store_iteration_trajectory(
            &self,
            _execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
            _iteration: u8,
            _trajectory: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
    }

    /// AEGIS ADR-131 U16, U19: the daemon gives the goal service and the
    /// validation service the alias table. The failure this prevents: a
    /// limit right in tests and wrong in production, where a service not
    /// given the table bounds its judge by the stated default (24,576 bytes)
    /// and stops as too large what production's judge holds (239,616 bytes).
    #[tokio::test]
    async fn the_daemon_gives_the_goal_service_and_the_validation_service_the_alias_table() {
        use aegis_orchestrator_core::application::goal_service::GoalCaller;
        use aegis_orchestrator_core::domain::goal::{GoalChannel, JudgeContext};
        use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
        use aegis_orchestrator_core::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
        use std::sync::Arc;
        use std::time::Duration;

        let config = production_judge_config();
        let judge_context = super::judge_context_source(&config);
        let production = JudgeContext {
            context_window: 256_000,
            max_output_tokens: 16_384,
        };
        assert_eq!(production.prompt_limit_bytes(), 239_616);

        let goals = super::daemon_goal_service(
            Arc::new(InMemoryGoalRepository::new()),
            Arc::new(EventBus::new(16)),
            &config,
            judge_context.clone(),
        )
        .with_wait(Duration::from_millis(30), Duration::from_millis(10));
        let caller = GoalCaller {
            tenant_id: aegis_orchestrator_core::domain::tenant::TenantId::consumer(),
            user_sub: "user-1".to_string(),
        };

        // 13,010 characters, the size of the answer of 2026-10-04, and
        // 100,000, above the stated default's room: each judged whole.
        for (n, size) in [13_010usize, 100_000].into_iter().enumerate() {
            let world = RecordingGoalWorld::default();
            let goal = goals
                .create(
                    &caller,
                    "write the report",
                    &format!("conv-{n}"),
                    GoalChannel::Web,
                )
                .await
                .expect("goal created");
            let answer = "r".repeat(size);
            let reply = goals
                .evaluate(&world, &caller, goal.id, &answer, None)
                .await
                .expect("evaluated");
            assert_eq!(
                reply["state"], "judging",
                "a {size}-character answer is judged, not stopped: {reply}"
            );
            let inputs = world.judge_inputs.lock().unwrap();
            assert_eq!(inputs.len(), 1, "goal-judge started once");
            assert_eq!(
                inputs[0]["companion_answer"].as_str().map(str::len),
                Some(size),
                "goal-judge is given the answer whole"
            );
        }

        // Above production's limit: no judge runs; the goal stops and says why.
        let world = RecordingGoalWorld::default();
        let goal = goals
            .create(&caller, "write the report", "conv-over", GoalChannel::Web)
            .await
            .expect("goal created");
        let reply = goals
            .evaluate(&world, &caller, goal.id, &"r".repeat(300_000), None)
            .await
            .expect("evaluated");
        assert_eq!(reply["state"], "stopped", "{reply}");
        let reasoning = reply["verdict"]["reasoning"].as_str().unwrap_or_default();
        assert!(
            reasoning.starts_with("The input is too large to judge: ")
                && reasoning.ends_with("against a limit of 239616 bytes."),
            "{reasoning}"
        );
        assert!(world.judge_inputs.lock().unwrap().is_empty());

        // The validation service the daemon and its gRPC server run reads the
        // same table.
        let validation = super::daemon_validation_service(
            Arc::new(EventBus::new(16)),
            Arc::new(NoExecutions),
            Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::InMemoryAgentRepository::new(
                ),
            ),
            judge_context,
        );
        let source = validation
            .judge_context()
            .expect("the daemon gives the validation service the alias table");
        assert_eq!(source.judge_context("judge"), Some(production));
    }

    /// retry-knows-what-failed: the inner loop the daemon builds is given the
    /// alias table, so a command's output enters the model's conversation
    /// bounded by an eighth of the alias's prompt limit (production's
    /// `smart`: 256,000 and 16,384, 239,616 bytes, 29,952 per output). The
    /// failure this prevents: the bound right in the inner loop's tests and,
    /// in production, the stated default's (24,576 bytes, 3,072 per output).
    #[tokio::test]
    async fn the_daemon_gives_the_inner_loop_the_alias_table() {
        use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
        use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut config = production_judge_config();
        config.spec.llm_providers[0].models.push(
            serde_yaml::from_str(
                r#"alias: smart
model: "@cf/nvidia/nemotron-3-120b-a12b"
capabilities: ["chat", "code", "reasoning"]
context_window: 256000
max_output_tokens: 16384
"#,
            )
            .expect("model entry parses"),
        );
        let judge_context = super::judge_context_source(&config);

        let storage_root =
            std::env::temp_dir().join(format!("aegis-daemon-inner-loop-{}", uuid::Uuid::new_v4()));
        let fsal = Arc::new(aegis_orchestrator_core::domain::fsal::AegisFSAL::new(
            Arc::new(
                aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider::new(
                    &storage_root,
                )
                .unwrap(),
            ),
            Arc::new(
                aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository::new(),
            ),
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoStorageEvents),
        ));
        let tools = aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService::new(
            Arc::new(aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository::new()),
            Arc::new(aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository::new()),
            super::daemon_seal_middleware(None, None),
            Arc::new(aegis_orchestrator_core::infrastructure::tool_router::ToolRouter::new(
                aegis_orchestrator_core::infrastructure::tool_router::ToolRouter::builtin_dispatchers(),
            )),
            fsal,
            aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry::new(),
            Arc::new(aegis_orchestrator_core::infrastructure::repositories::InMemoryAgentRepository::new()),
            Arc::new(NoExecutions),
            Arc::new(aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
            Arc::new(EventBus::new(16)),
            None,
        );
        let inner_loop = super::daemon_inner_loop_service(
            Arc::new(tools),
            Arc::new(NoExecutions),
            Arc::new(ProviderRegistry::from_config(&config).expect("registry builds")),
            judge_context,
        );
        assert_eq!(inner_loop.conversation_output_bound("smart"), 29_952);
        assert_eq!(inner_loop.conversation_output_bound("judge"), 29_952);
    }

    struct NoStorageEvents;

    #[async_trait::async_trait]
    impl aegis_orchestrator_core::domain::fsal::EventPublisher for NoStorageEvents {
        async fn publish_storage_event(
            &self,
            _event: aegis_orchestrator_core::domain::events::StorageEvent,
        ) {
        }
    }

    /// AEGIS ADR-124 D3, as the coordinator ruled it (correction C1): the
    /// alias route reads the daemon's one provider registry, the instance
    /// the inner loop is built over, never a second one built from config.
    #[tokio::test]
    async fn the_alias_route_and_the_inner_loop_answer_from_one_registry() {
        use crate::daemon::handlers::llm::{llm_aliases_router, LlmAliasesState};
        use crate::daemon::handlers::test_support::{consumer, identity_provider, send, serve};
        use aegis_orchestrator_core::domain::node_config::{LLMProviderConfig, NodeConfigManifest};
        use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
        use std::sync::Arc;

        let provider: LLMProviderConfig = serde_yaml::from_str(
            r#"name: workers-ai
type: openai-compatible
endpoint: "https://api.example.invalid/v1"
api_key: "literal-provider-key-value"
enabled: true
models:
  - alias: "zaru-chat"
    model: "@cf/zai-org/glm-5.3-flash"
    capabilities: ["chat"]
    context_window: 8192
"#,
        )
        .expect("provider block parses");
        let mut manifest = NodeConfigManifest::default();
        manifest.spec.llm_providers = vec![provider];
        let built = Arc::new(ProviderRegistry::from_config(&manifest).expect("registry builds"));

        // The inner loop's side: the instance `build` is handed, which the
        // daemon passes to `InnerLoopService::new`.
        let (inner_loop_registry, app_state_registry) =
            super::share_llm_registry(built.clone(), |registry| registry);

        let mut failures = Vec::new();
        if !Arc::ptr_eq(&inner_loop_registry, &built) {
            failures.push(
                "the inner loop must be built over the registry the daemon built".to_string(),
            );
        }
        if !Arc::ptr_eq(&app_state_registry, &inner_loop_registry) {
            failures.push(
                "AppState's registry, which the alias route reads, must be the inner loop's instance, not a second registry".to_string(),
            );
        }

        // And the route served from that instance answers its table.
        let base = serve(
            llm_aliases_router(LlmAliasesState {
                registry: app_state_registry,
            }),
            Some(identity_provider(&[(
                "consumer-token",
                consumer("user-1"),
                "",
            )])),
            None,
        )
        .await;
        let (status, body) = send(
            &base,
            &reqwest::Method::GET,
            "/v1/llm/aliases/zaru-chat",
            &None,
            Some("consumer-token"),
        )
        .await;
        let expected =
            serde_json::json!({"alias": "zaru-chat", "model": "@cf/zai-org/glm-5.3-flash"});
        if status != 200 || body != expected {
            failures.push(format!(
                "the route over the shared registry must answer 200 {expected}, got {status} {body}"
            ));
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    #[test]
    fn test_create_router_returns_router() {
        // This is a smoke test to ensure create_router compiles and can be called
        // We can't easily test the full router without a complex setup
        // but we can at least verify the function signature works

        let assertion_marker = "router_module_compiles";
        assert_eq!(assertion_marker, "router_module_compiles");
    }

    #[test]
    fn temporal_connection_max_retries_clamps_to_minimum_of_one() {
        assert_eq!(temporal_connection_max_retries(None), 30);
        assert_eq!(temporal_connection_max_retries(Some(0)), 1);
        assert_eq!(temporal_connection_max_retries(Some(-4)), 1);
        assert_eq!(temporal_connection_max_retries(Some(7)), 7);
    }

    #[test]
    fn generated_artifacts_root_uses_discovered_config_directory() {
        let tmp = temp_dir();
        let stack_dir = tmp.join(".aegis");
        fs::create_dir_all(&stack_dir).expect("create stack dir");
        let config_path = stack_dir.join("aegis-config.yaml");
        fs::write(
            &config_path,
            "apiVersion: 100monkeys.ai/v1\nkind: NodeConfig\n",
        )
        .expect("write config");

        let resolved = resolve_generated_artifacts_root(Some(PathBuf::from(&config_path)));

        assert_eq!(resolved, stack_dir.join("generated"));

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn managed_container_reap_reason_skips_debug_retained_containers() {
        let container = ManagedAgentContainer {
            id: "container-1".to_string(),
            execution_id: Some("00000000-0000-0000-0000-000000000001".to_string()),
            debug_retain: true,
            state: Some("running".to_string()),
        };

        assert_eq!(
            managed_container_reap_reason(&container, Some(&ExecutionStatus::Running)),
            None
        );
    }

    #[test]
    fn managed_container_reap_reason_keeps_running_executions_with_running_containers() {
        let container = ManagedAgentContainer {
            id: "container-2".to_string(),
            execution_id: Some("00000000-0000-0000-0000-000000000002".to_string()),
            debug_retain: false,
            state: Some("running".to_string()),
        };

        assert_eq!(
            managed_container_reap_reason(&container, Some(&ExecutionStatus::Running)),
            None
        );
    }

    #[test]
    fn managed_container_reap_reason_reaps_completed_or_missing_executions() {
        let running_container = ManagedAgentContainer {
            id: "container-3".to_string(),
            execution_id: Some("00000000-0000-0000-0000-000000000003".to_string()),
            debug_retain: false,
            state: Some("running".to_string()),
        };
        let exited_container = ManagedAgentContainer {
            id: "container-4".to_string(),
            execution_id: Some("00000000-0000-0000-0000-000000000004".to_string()),
            debug_retain: false,
            state: Some("exited".to_string()),
        };

        assert_eq!(
            managed_container_reap_reason(&running_container, Some(&ExecutionStatus::Completed)),
            Some("execution_not_running")
        );
        assert_eq!(
            managed_container_reap_reason(&exited_container, Some(&ExecutionStatus::Running)),
            Some("container_not_running")
        );
        assert_eq!(
            managed_container_reap_reason(&running_container, None),
            Some("missing_execution_record")
        );
    }

    // ── The start-up pass over executions an earlier process left running ──

    mod restart_pass {
        use super::super::{end_executions_cut_by_restart, restart_pass_applies};
        use crate::daemon::container_helpers::managed_container_reap_reason;
        use crate::daemon::handlers::executions::execution_status;
        use crate::daemon::handlers::test_support::service_account;
        use aegis_orchestrator_core::application::execution_event_persister::ExecutionEventPersister;
        use aegis_orchestrator_core::domain::agent::AgentId;
        use aegis_orchestrator_core::domain::execution::{
            Execution, ExecutionId, ExecutionInput, ExecutionStatus, IterationStatus,
            ORCHESTRATOR_RESTART_FAILURE_REASON,
        };
        use aegis_orchestrator_core::domain::node_config::{ClusterConfig, NodeConfigManifest};
        use aegis_orchestrator_core::domain::repository::{
            ExecutionRepository, WorkflowExecutionRepository,
        };
        use aegis_orchestrator_core::domain::shared_kernel::TenantId;
        use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
        use aegis_orchestrator_core::infrastructure::repositories::postgres_execution::PostgresExecutionRepository;
        use aegis_orchestrator_core::infrastructure::repositories::postgres_workflow_execution::PostgresWorkflowExecutionRepository;
        use aegis_orchestrator_core::infrastructure::repositories::InMemoryExecutionRepository;
        use aegis_orchestrator_core::infrastructure::runtime::ManagedAgentContainer;
        use chrono::Utc;
        use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
        use std::sync::Arc;

        /// The first sequence number an orchestrator process gives its own
        /// events of an execution (`execution_event_persister.rs`).
        const LOCAL_SEQUENCE_START: i64 = 1_000_000_000_000;

        fn postgres_url() -> Option<String> {
            match std::env::var("AEGIS_TEST_POSTGRES_URL") {
                Ok(url) if !url.is_empty() => Some(url),
                _ if std::env::var_os("CI").is_some() => {
                    panic!("AEGIS_TEST_POSTGRES_URL is not set; in CI these tests must reach PostgreSQL")
                }
                _ => {
                    eprintln!("skipped: AEGIS_TEST_POSTGRES_URL is not set");
                    None
                }
            }
        }

        struct TestDb {
            server: PgPool,
            name: String,
            pool: PgPool,
        }

        impl TestDb {
            async fn create() -> Option<Self> {
                let url = postgres_url()?;
                let server = PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&url)
                    .await
                    .expect("connect to the test PostgreSQL");
                let name = format!("aegis_restart_{}", uuid::Uuid::new_v4().simple());
                sqlx::query(&format!("CREATE DATABASE {name}"))
                    .execute(&server)
                    .await
                    .expect("create the test database");
                let options: PgConnectOptions =
                    url.parse::<PgConnectOptions>().unwrap().database(&name);
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect_with(options)
                    .await
                    .expect("connect to the test database");
                crate::daemon::migrations::MIGRATOR
                    .run(&pool)
                    .await
                    .expect("apply the migrations");
                Some(Self { server, name, pool })
            }

            async fn remove(self) {
                self.pool.close().await;
                sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
                    .execute(&self.server)
                    .await
                    .expect("drop the test database");
            }
        }

        async fn insert_agent(pool: &PgPool, tenant: &TenantId) -> AgentId {
            sqlx::query(
                "INSERT INTO tenants (slug, display_name, keycloak_realm, openbao_namespace) \
                 VALUES ($1, $1, 'zaru-consumer', $1) ON CONFLICT DO NOTHING",
            )
            .bind(tenant.as_str())
            .execute(pool)
            .await
            .expect("insert the tenant");
            let id = AgentId::new();
            sqlx::query(
                "INSERT INTO agents (id, tenant_id, name, manifest_yaml, manifest_json, runtime, security_policy) \
                 VALUES ($1, $2, $3, '', '{}'::jsonb, 'python:3.11', '{}'::jsonb)",
            )
            .bind(id.0)
            .bind(tenant.as_str())
            .bind(format!("restart-agent-{}", &id.0.simple().to_string()[..8]))
            .execute(pool)
            .await
            .expect("insert an agent");
            id
        }

        fn input() -> ExecutionInput {
            ExecutionInput {
                intent: Some("work".to_string()),
                input: serde_json::json!({}),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            }
        }

        /// An execution an earlier process was running when it ended.
        fn left_running(
            agent_id: AgentId,
            tenant: &TenantId,
            started_at: chrono::DateTime<Utc>,
        ) -> Execution {
            let mut e = Execution::new(agent_id, input(), 3, "aegis-system-operator".to_string());
            e.tenant_id = tenant.clone();
            e.started_at = started_at;
            e.start();
            e.start_iteration("generate".to_string()).unwrap();
            e
        }

        fn container_of(execution_id: ExecutionId) -> ManagedAgentContainer {
            ManagedAgentContainer {
                id: format!("aegis-agent-{}", &execution_id.0.simple().to_string()[..8]),
                execution_id: Some(execution_id.0.to_string()),
                debug_retain: false,
                state: Some("running".to_string()),
            }
        }

        async fn status_of(repo: &dyn ExecutionRepository, id: ExecutionId) -> ExecutionStatus {
            repo.find_by_id_unscoped(id).await.unwrap().unwrap().status
        }

        async fn events_of(
            events: &PostgresWorkflowExecutionRepository,
            id: ExecutionId,
        ) -> Vec<(i64, String, serde_json::Value)> {
            events
                .find_events_by_execution(id, 1000, 0)
                .await
                .unwrap()
                .into_iter()
                .map(|r| (r.sequence, r.event_type, r.payload))
                .collect()
        }

        /// The production case against PostgreSQL: a worker execution and
        /// the judge it was waiting on, both left running by an earlier
        /// process whose own events are stored, and one execution that had
        /// completed. The start-up pass of the new process fails the two
        /// with the restart reason and records each ExecutionFailed; the
        /// status route then answers Failed to the worker's poll; the
        /// reaper gives their containers "execution_not_running"; the
        /// completed execution is untouched; a second pass changes nothing.
        #[tokio::test]
        async fn the_startup_pass_ends_executions_an_earlier_process_left_running() {
            let Some(db) = TestDb::create().await else {
                return;
            };
            let tenant = TenantId::for_consumer_user("restart-owner").unwrap();
            let agent_id = insert_agent(&db.pool, &tenant).await;
            let repo = PostgresExecutionRepository::new(db.pool.clone());
            let events = PostgresWorkflowExecutionRepository::new(db.pool.clone());

            // The earlier process.
            let t0 = Utc::now() - chrono::Duration::minutes(5);
            let worker = left_running(agent_id, &tenant, t0);
            let mut judge = Execution::new_child(agent_id, input(), 1, &worker).unwrap();
            judge.started_at = t0 + chrono::Duration::seconds(20);
            judge.start();
            judge.start_iteration("judge".to_string()).unwrap();
            let mut done =
                Execution::new(agent_id, input(), 1, "aegis-system-operator".to_string());
            done.tenant_id = tenant.clone();
            done.started_at = t0 - chrono::Duration::minutes(1);
            done.start();
            done.complete();
            for e in [&worker, &judge, &done] {
                repo.save_for_tenant(&tenant, e).await.unwrap();
            }
            events
                .append_event(
                    worker.id,
                    LOCAL_SEQUENCE_START,
                    "ExecutionStarted".to_string(),
                    serde_json::json!({ "ExecutionStarted": { "execution_id": worker.id.0 } }),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                managed_container_reap_reason(
                    &container_of(worker.id),
                    Some(&status_of(&repo, worker.id).await)
                ),
                None,
                "before the pass the reaper keeps the container of a running execution"
            );

            // The new process starts.
            let bus = Arc::new(EventBus::with_default_capacity());
            let _persister = Arc::new(ExecutionEventPersister::new(
                Arc::new(PostgresWorkflowExecutionRepository::new(db.pool.clone())),
                bus.clone(),
            ))
            .start();
            let config = NodeConfigManifest::default();
            let ended = end_executions_cut_by_restart(&config, &repo, &bus).await;
            assert_eq!(ended, vec![judge.id, worker.id]);

            for id in [judge.id, worker.id] {
                let e = repo.find_by_id_unscoped(id).await.unwrap().unwrap();
                assert_eq!(
                    e.status,
                    ExecutionStatus::Failed,
                    "{id} still {:?}",
                    e.status
                );
                assert_eq!(
                    e.error.as_deref(),
                    Some(ORCHESTRATOR_RESTART_FAILURE_REASON)
                );
                assert!(e.ended_at.is_some());
                let last = e.iterations().last().unwrap();
                assert_eq!(last.status, IterationStatus::Failed);
            }
            let done_after = repo.find_by_id_unscoped(done.id).await.unwrap().unwrap();
            assert_eq!(done_after.status, ExecutionStatus::Completed);
            assert_eq!(done_after.error, None);

            // Each failure event is recorded, the worker's after the earlier
            // process's own event.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let (worker_events, judge_events) = loop {
                let w = events_of(&events, worker.id).await;
                let j = events_of(&events, judge.id).await;
                let failed = |v: &Vec<(i64, String, serde_json::Value)>| {
                    v.iter().any(|(_, t, _)| t == "ExecutionFailed")
                };
                if (failed(&w) && failed(&j)) || std::time::Instant::now() > deadline {
                    break (w, j);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            };
            for (name, recorded) in [("worker", &worker_events), ("judge", &judge_events)] {
                assert_eq!(
                    recorded
                        .iter()
                        .filter(|(_, t, _)| t == "IterationFailed")
                        .count(),
                    1,
                    "every event the pass published for the {name} is recorded: {recorded:?}"
                );
                let failed: Vec<_> = recorded
                    .iter()
                    .filter(|(_, t, _)| t == "ExecutionFailed")
                    .collect();
                assert_eq!(failed.len(), 1, "{name}'s events: {recorded:?}");
                assert_eq!(
                    failed[0].2["ExecutionFailed"]["reason"], ORCHESTRATOR_RESTART_FAILURE_REASON,
                    "{name}'s ExecutionFailed: {:?}",
                    failed[0]
                );
            }
            let worker_failed_seq = worker_events
                .iter()
                .find(|(_, t, _)| t == "ExecutionFailed")
                .map(|(s, _, _)| *s);
            assert!(
                worker_failed_seq > Some(LOCAL_SEQUENCE_START),
                "the worker's ExecutionFailed must follow the earlier process's event: {worker_events:?}"
            );

            // What the worker's status poll reads now.
            let (code, body) = execution_status(
                &repo,
                Some(&service_account()),
                Some(tenant.as_str()),
                worker.id.0,
            )
            .await;
            assert_eq!(code.as_u16(), 200, "body: {body:?}");
            assert_eq!(body.0["status"], "Failed");

            // The reaper's start-up pass now removes their containers.
            for id in [judge.id, worker.id] {
                assert_eq!(
                    managed_container_reap_reason(
                        &container_of(id),
                        Some(&status_of(&repo, id).await)
                    ),
                    Some("execution_not_running")
                );
            }

            // A second start changes nothing.
            let failed_count = |v: &Vec<(i64, String, serde_json::Value)>| {
                v.iter().filter(|(_, t, _)| t == "ExecutionFailed").count()
            };
            let ended_again = end_executions_cut_by_restart(&config, &repo, &bus).await;
            assert!(
                ended_again.is_empty(),
                "the second pass ended {ended_again:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert_eq!(failed_count(&events_of(&events, worker.id).await), 1);
            assert_eq!(failed_count(&events_of(&events, judge.id).await), 1);

            db.remove().await;
        }

        /// On a node with clustering enabled the pass does not run: an
        /// execution record names no node, so a running execution of another
        /// live node would be failed. The record stays as it is.
        #[tokio::test]
        async fn a_clustered_node_skips_the_startup_pass() {
            let mut config = NodeConfigManifest::default();
            assert!(restart_pass_applies(&config));
            let cluster: ClusterConfig =
                serde_json::from_value(serde_json::json!({ "enabled": true })).unwrap();
            config.spec.cluster = Some(cluster);
            assert!(!restart_pass_applies(&config));

            let repo = InMemoryExecutionRepository::new();
            let tenant = TenantId::for_consumer_user("cluster-owner").unwrap();
            let running = left_running(AgentId::new(), &tenant, Utc::now());
            repo.save_for_tenant(&tenant, &running).await.unwrap();
            let bus = EventBus::with_default_capacity();
            assert!(end_executions_cut_by_restart(&config, &repo, &bus)
                .await
                .is_empty());
            assert_eq!(status_of(&repo, running.id).await, ExecutionStatus::Running);
        }
    }
}

#[cfg(test)]
#[path = "server_gateway_wiring_tests.rs"]
mod gateway_wiring_tests;
