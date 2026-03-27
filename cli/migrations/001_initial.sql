-- AEGIS Temporal Era Database Schema
-- Created: February 12, 2026
-- Description: End-state schema for Temporal-powered workflow orchestration
-- No backward compatibility - fresh start for Temporal integration

-- =============================================================================
-- AEGIS Domain Schema
-- =============================================================================

-- Drop existing tables if they exist (pre-alpha clean slate)
DROP TABLE IF EXISTS workflow_executions CASCADE;
DROP TABLE IF EXISTS workflows CASCADE;
DROP TABLE IF EXISTS executions CASCADE;
DROP TABLE IF EXISTS agents CASCADE;
DROP TABLE IF EXISTS volumes CASCADE;
DROP TABLE IF EXISTS workflow_definitions CASCADE;
DROP TABLE IF EXISTS tenants CASCADE;

-- -----------------------------------------------------------------------------
-- Tenants Table
-- Multi-tenant isolation: every domain entity belongs to exactly one tenant
-- ADR-056: Multi-Tenant Architecture Phase 1
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS tenants (
    slug            VARCHAR(63)  PRIMARY KEY,
    display_name    VARCHAR(255) NOT NULL,
    status          VARCHAR(32)  NOT NULL DEFAULT 'active',
    tier            VARCHAR(32)  NOT NULL DEFAULT 'enterprise',
    keycloak_realm  VARCHAR(255) NOT NULL,
    openbao_namespace VARCHAR(255) NOT NULL,
    max_concurrent_executions  INTEGER NOT NULL DEFAULT 50,
    max_agents                 INTEGER NOT NULL DEFAULT 500,
    max_storage_gb             NUMERIC(10, 2) NOT NULL DEFAULT 100.0,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    deleted_at      TIMESTAMPTZ  NULL,
    CONSTRAINT tenants_slug_format CHECK (slug ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
    CONSTRAINT tenants_status_check CHECK (status IN ('active', 'suspended', 'deleted')),
    CONSTRAINT tenants_tier_check CHECK (tier IN ('consumer', 'enterprise', 'system'))
);

CREATE INDEX idx_tenants_status ON tenants(status);

-- Seed tenants
INSERT INTO tenants (slug, display_name, tier, keycloak_realm, openbao_namespace)
VALUES
    ('zaru-consumer', 'Zaru Consumer Pool', 'consumer', 'zaru-consumer', 'tenant-zaru-consumer/'),
    ('aegis-system',  'AEGIS Platform',     'system',   'aegis-system',  'tenant-aegis-system/')
ON CONFLICT (slug) DO NOTHING;

-- -----------------------------------------------------------------------------
-- Workflows Table
-- Stores registered workflow definitions
-- -----------------------------------------------------------------------------
CREATE TABLE workflows (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    description TEXT,
    
    -- Source artifacts
    yaml_source TEXT NOT NULL,
    domain_json JSONB NOT NULL,       -- Serialized Rust Workflow domain object
    temporal_def_json JSONB NOT NULL, -- Generated Temporal workflow definition
    
    -- Metadata
    labels JSONB DEFAULT '{}'::jsonb,
    annotations JSONB DEFAULT '{}'::jsonb,
    
    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    -- Validation
    CONSTRAINT workflows_name_format CHECK (name ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
    CONSTRAINT workflows_tenant_format CHECK (tenant_id ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
    CONSTRAINT workflows_tenant_name_version_unique UNIQUE (tenant_id, name, version),
    CONSTRAINT workflows_version_format CHECK (version ~ '^\d+\.\d+\.\d+$'),
    CONSTRAINT workflows_tenant_fk FOREIGN KEY (tenant_id) REFERENCES tenants(slug) ON DELETE RESTRICT
);

CREATE INDEX idx_workflows_tenant_name ON workflows(tenant_id, name);
CREATE INDEX idx_workflows_created_at ON workflows(created_at DESC);

-- Trigger to update updated_at
CREATE OR REPLACE FUNCTION update_workflows_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trigger_workflows_updated_at
    BEFORE UPDATE ON workflows
    FOR EACH ROW
    EXECUTE FUNCTION update_workflows_updated_at();

-- -----------------------------------------------------------------------------
-- Workflow Executions Table
-- Links AEGIS workflow executions to Temporal workflow runs
-- Tracks FSM state transitions, blackboard data, and state outputs
-- ADR-022: Temporal Workflow Engine Integration (Phase 2)
-- -----------------------------------------------------------------------------
CREATE TABLE workflow_executions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id TEXT NOT NULL,
    workflow_id UUID NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
    
    -- Temporal identifiers (for querying Temporal Server)
    temporal_workflow_id TEXT NOT NULL,
    temporal_run_id TEXT NOT NULL,
    
    -- Execution metadata
    input_params JSONB,
    status TEXT NOT NULL DEFAULT 'running',
    
    -- FSM State Tracking (ADR-022 Phase 2)
    current_state TEXT NOT NULL DEFAULT 'start',       -- Active state in workflow graph
    state_history JSONB DEFAULT '[]'::jsonb,          -- Ordered list of visited states
    blackboard JSONB DEFAULT '{}'::jsonb,             -- Shared context data between states
    state_outputs JSONB DEFAULT '{}'::jsonb,          -- Outputs from each state execution
    
    -- Results
    final_output JSONB,
    error_message TEXT,

    -- Iteration tracking (updated by TemporalEventListener on iteration events)
    iteration_count SMALLINT DEFAULT 0,
    
    -- Timestamps
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_transition_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    
    -- Validation
    CONSTRAINT workflow_executions_status_check CHECK (
        status IN ('running', 'completed', 'failed', 'cancelled', 'timed_out', 'pending')
    ),
    CONSTRAINT workflow_executions_tenant_fk FOREIGN KEY (tenant_id) REFERENCES tenants(slug) ON DELETE RESTRICT
);

CREATE INDEX idx_workflow_executions_workflow_id ON workflow_executions(workflow_id);
CREATE INDEX idx_workflow_executions_tenant_workflow_id ON workflow_executions(tenant_id, workflow_id);
CREATE INDEX idx_workflow_executions_temporal_workflow_id ON workflow_executions(temporal_workflow_id);
CREATE INDEX idx_workflow_executions_status ON workflow_executions(status);
CREATE INDEX idx_workflow_executions_started_at ON workflow_executions(started_at DESC);
CREATE INDEX idx_workflow_executions_current_state ON workflow_executions(current_state);

-- Composite index for common queries
CREATE INDEX idx_workflow_executions_workflow_temporal ON workflow_executions(workflow_id, temporal_workflow_id);

-- -----------------------------------------------------------------------------
-- Execution Events Table
-- Append-only audit log of Temporal events for each workflow execution
-- Used by TemporalEventListener to record iteration progress and state transitions
-- -----------------------------------------------------------------------------
CREATE TABLE execution_events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    execution_id UUID NOT NULL REFERENCES workflow_executions(id) ON DELETE CASCADE,

    -- Temporal sequence number for ordering (monotonically increasing per execution)
    temporal_sequence_number BIGINT NOT NULL,

    -- Event classification
    event_type TEXT NOT NULL,
    event_payload JSONB NOT NULL DEFAULT '{}'::jsonb,

    -- Optional iteration reference (NULL for non-iteration events)
    iteration_number SMALLINT,

    -- Timestamp
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Idempotency: each sequence number is unique per execution
    CONSTRAINT execution_events_unique_sequence UNIQUE (execution_id, temporal_sequence_number)
);

CREATE INDEX idx_execution_events_execution_id ON execution_events(execution_id);
CREATE INDEX idx_execution_events_sequence ON execution_events(execution_id, temporal_sequence_number ASC);

COMMENT ON TABLE execution_events IS 'Append-only Temporal event log per workflow execution; drives iteration_count and state history';

-- -----------------------------------------------------------------------------
-- Agents Table
-- Stores agent manifest definitions
-- -----------------------------------------------------------------------------
CREATE TABLE agents (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL DEFAULT '1.0.0',
    
    -- Manifest artifacts
    manifest_yaml TEXT NOT NULL,
    manifest_json JSONB NOT NULL,
    
    -- Agent configuration
    runtime TEXT NOT NULL, -- e.g., "python:3.11", "node:20"
    timeout_seconds INTEGER NOT NULL DEFAULT 600,
    
    -- Security policy (extracted from manifest)
    security_policy JSONB NOT NULL,
    
    -- Status
    status TEXT NOT NULL DEFAULT 'active',
    
    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    -- Validation
    CONSTRAINT agents_name_format CHECK (name ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
    CONSTRAINT agents_tenant_format CHECK (tenant_id ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
    CONSTRAINT agents_tenant_name_unique UNIQUE (tenant_id, name),
    CONSTRAINT agents_status_check CHECK (status IN ('active', 'paused', 'archived')),
    CONSTRAINT agents_tenant_fk FOREIGN KEY (tenant_id) REFERENCES tenants(slug) ON DELETE RESTRICT
);

CREATE INDEX idx_agents_tenant_name ON agents(tenant_id, name);
CREATE INDEX idx_agents_status ON agents(status);
CREATE INDEX idx_agents_tenant_status ON agents(tenant_id, status);
CREATE INDEX idx_agents_created_at ON agents(created_at DESC);

-- Trigger to update updated_at
CREATE OR REPLACE FUNCTION update_agents_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trigger_agents_updated_at
    BEFORE UPDATE ON agents
    FOR EACH ROW
    EXECUTE FUNCTION update_agents_updated_at();

-- -----------------------------------------------------------------------------
-- Executions Table
-- Individual agent execution records (100monkeys iterations)
-- -----------------------------------------------------------------------------
CREATE TABLE executions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id TEXT NOT NULL,
    agent_id UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    
    -- Optional link to workflow execution (if agent was called by workflow)
    workflow_execution_id UUID REFERENCES workflow_executions(id) ON DELETE SET NULL,
    
    -- Execution context
    input JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    
    -- Iterations (100monkeys loop attempts)
    iterations JSONB NOT NULL DEFAULT '[]'::jsonb,
    current_iteration INTEGER NOT NULL DEFAULT 0,
    max_iterations INTEGER NOT NULL DEFAULT 10,
    
    -- Results
    final_output TEXT,
    error_message TEXT,
    
    -- Container UID/GID for permission squashing (ADR-036)
    container_uid INTEGER NOT NULL DEFAULT 1000,
    container_gid INTEGER NOT NULL DEFAULT 1000,
    
    -- Execution hierarchy (ADR-039)
    parent_execution_id UUID REFERENCES executions(id) ON DELETE SET NULL,

    -- Timestamps
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    
    -- Validation
    CONSTRAINT executions_status_check CHECK (
        status IN ('pending', 'running', 'completed', 'failed', 'cancelled')
    ),
    CONSTRAINT executions_tenant_fk FOREIGN KEY (tenant_id) REFERENCES tenants(slug) ON DELETE RESTRICT
);

CREATE INDEX idx_executions_agent_id ON executions(agent_id);
CREATE INDEX idx_executions_tenant_agent_id ON executions(tenant_id, agent_id);
CREATE INDEX idx_executions_workflow_execution_id ON executions(workflow_execution_id);
CREATE INDEX idx_executions_status ON executions(status);
CREATE INDEX idx_executions_started_at ON executions(started_at DESC);
CREATE INDEX idx_executions_parent_execution_id ON executions(parent_execution_id)
    WHERE parent_execution_id IS NOT NULL;

-- =============================================================================
-- Temporal Worker Schema
-- Stores workflow definitions for multi-worker coordination
-- =============================================================================

-- -----------------------------------------------------------------------------
-- Workflow Definitions Table (for TypeScript Worker)
-- Shared state across worker replicas
-- -----------------------------------------------------------------------------
CREATE TABLE workflow_definitions (
    workflow_id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    definition JSONB NOT NULL,
    
    -- Registration metadata
    registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    registered_by TEXT,
    
    -- Version tracking
    version TEXT NOT NULL DEFAULT '0.0.1',
    definition_hash TEXT NOT NULL,
    
    CONSTRAINT workflow_definitions_name_format CHECK (name ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
    CONSTRAINT workflow_definitions_tenant_fk FOREIGN KEY (tenant_id) REFERENCES tenants(slug) ON DELETE RESTRICT
);

CREATE UNIQUE INDEX idx_workflow_definitions_tenant_name_version
    ON workflow_definitions(tenant_id, name, version);

CREATE INDEX idx_workflow_definitions_name ON workflow_definitions(name);
CREATE INDEX idx_workflow_definitions_registered_at ON workflow_definitions(registered_at DESC);
CREATE INDEX idx_workflow_definitions_hash ON workflow_definitions(definition_hash);

-- =============================================================================
-- Views for Common Queries
-- =============================================================================

-- Active workflow executions with workflow details
CREATE VIEW active_workflow_executions AS
SELECT 
    we.id,
    we.temporal_workflow_id,
    we.temporal_run_id,
    w.name AS workflow_name,
    w.version AS workflow_version,
    we.status,
    we.started_at,
    EXTRACT(EPOCH FROM (NOW() - we.started_at)) AS duration_seconds
FROM workflow_executions we
JOIN workflows w ON we.workflow_id = w.id
WHERE we.status = 'running'
ORDER BY we.started_at DESC;

-- Agent execution success rates
CREATE VIEW agent_success_rates AS
SELECT 
    a.id AS agent_id,
    a.name AS agent_name,
    COUNT(*) AS total_executions,
    SUM(CASE WHEN e.status = 'completed' THEN 1 ELSE 0 END) AS successful_executions,
    ROUND(
        100.0 * SUM(CASE WHEN e.status = 'completed' THEN 1 ELSE 0 END) / NULLIF(COUNT(*), 0),
        2
    ) AS success_rate_percent,
    AVG(EXTRACT(EPOCH FROM (e.completed_at - e.started_at))) AS avg_duration_seconds
FROM agents a
LEFT JOIN executions e ON a.id = e.agent_id
WHERE e.started_at > NOW() - INTERVAL '7 days'
GROUP BY a.id, a.name
ORDER BY success_rate_percent DESC;

-- =============================================================================
-- Comments
-- =============================================================================

COMMENT ON TABLE tenants IS 'Multi-tenant isolation roots; every domain entity references exactly one tenant (ADR-056)';
COMMENT ON TABLE workflows IS 'Registered workflow definitions in AEGIS YAML format';
COMMENT ON TABLE workflow_executions IS 'Links to Temporal workflow runs with AEGIS metadata';
COMMENT ON TABLE agents IS 'Agent manifest definitions from MANIFEST_SPEC_V1';
COMMENT ON TABLE executions IS 'Individual agent execution records with 100monkeys iterations';
COMMENT ON TABLE workflow_definitions IS 'Shared workflow definition registry for TypeScript workers';

COMMENT ON COLUMN workflows.domain_json IS 'Serialized Rust Workflow domain object for validation';
COMMENT ON COLUMN workflows.temporal_def_json IS 'Generated Temporal workflow definition sent to workers';
COMMENT ON COLUMN workflow_executions.temporal_workflow_id IS 'Temporal workflow ID for querying Temporal Server';
COMMENT ON COLUMN workflow_executions.temporal_run_id IS 'Temporal run ID for specific execution instance';
COMMENT ON COLUMN executions.parent_execution_id IS 'Parent execution ID for child executions (NULL for root executions). Supports ADR-039 recursive execution hierarchy.';

-- =============================================================================
-- Sample Data (for development)
-- =============================================================================

-- Insert sample workflow (100monkeys-classic)
-- This will be populated by the RegisterWorkflowUseCase in production
-- Placeholder for testing migration

-- =============================================================================
-- Migration Complete
-- =============================================================================

-- Print success message
DO $$
BEGIN
    RAISE NOTICE 'AEGIS Temporal Era Schema Migration Complete';
    RAISE NOTICE 'Created tables: tenants, workflows, workflow_executions, agents, executions, workflow_definitions';
    RAISE NOTICE 'Created views: active_workflow_executions, agent_success_rates';
    RAISE NOTICE 'Seeded tenants: zaru-consumer, aegis-system';
END $$;

-- AEGIS Unified Storage Layer Database Schema
-- Created: February 17, 2026
-- Description: Volumes table for SeaweedFS-backed persistent and ephemeral storage
-- ADR Reference: ADR-032 Unified Storage Layer

-- =============================================================================
-- Volumes Table
-- Stores volume metadata for distributed storage management
-- =============================================================================

-- -----------------------------------------------------------------------------
-- Volumes Table
-- Represents isolated storage contexts with lifecycle independent of agent execution
-- -----------------------------------------------------------------------------
CREATE TABLE volumes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    
    -- Storage configuration
    storage_class JSONB NOT NULL,      -- {"type": "ephemeral", "ttl": "PT24H"} or {"type": "persistent"}
    backend JSONB NOT NULL,            -- ADR-047 Dynamic Storage backend definition
    size_limit_bytes BIGINT NOT NULL,
    
    -- Lifecycle state
    status JSONB NOT NULL,             -- {"type": "creating"} | {"type": "available"} etc.
    ownership JSONB NOT NULL,          -- {"type": "execution", "execution_id": "..."} etc.
    
    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    attached_at TIMESTAMPTZ,
    detached_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,            -- NULL for persistent volumes
    
    -- Constraints
    CONSTRAINT volumes_size_limit_positive CHECK (size_limit_bytes > 0),
    CONSTRAINT volumes_tenant_format CHECK (tenant_id ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
    CONSTRAINT volumes_tenant_fk FOREIGN KEY (tenant_id) REFERENCES tenants(slug) ON DELETE RESTRICT
    -- Note: Volume names are logical labels, NOT globally unique.
    -- Multiple executions can have volumes with the same name but different volume_ids.
    -- Uniqueness is enforced by: id (PRIMARY KEY).
);

-- Indexes for common queries
CREATE INDEX idx_volumes_tenant_id ON volumes(tenant_id);
CREATE INDEX idx_volumes_expires_at ON volumes(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX idx_volumes_ownership ON volumes USING GIN (ownership);
CREATE INDEX idx_volumes_status ON volumes USING GIN (status);
CREATE INDEX idx_volumes_created_at ON volumes(created_at DESC);

-- Composite indexes for ownership queries
CREATE INDEX idx_volumes_tenant_status ON volumes(tenant_id, (status->>'type'));

-- Add comment for documentation
COMMENT ON TABLE volumes IS 'Volume metadata mapping to diverse StorageBackends (ADR-047)';
COMMENT ON COLUMN volumes.tenant_id IS 'Multi-tenant namespace isolation using canonical tenant slug IDs (default: local)';
COMMENT ON COLUMN volumes.storage_class IS 'Ephemeral (TTL-based) or Persistent storage classification';
COMMENT ON COLUMN volumes.ownership IS 'Execution-scoped, Workflow-scoped, or User-owned persistent';
COMMENT ON COLUMN volumes.expires_at IS 'Auto-cleanup timestamp for ephemeral volumes (NULL for persistent)';
COMMENT ON COLUMN volumes.backend IS 'Storage backend metadata mapping the volume (SeaweedFS, OpenDAL, SMCP, LocalHost)';

-- =============================================================================
-- Migration Rollback
-- =============================================================================

-- To rollback this migration:
-- DROP TABLE volumes;

-- Storage Events Audit Trail (ADR-036)
-- Creates table for persisting file-level operations for forensic analysis
-- Complements in-memory event bus with persistent audit log

CREATE TABLE storage_events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    
    -- Context identifiers
    execution_id UUID NOT NULL REFERENCES executions(id) ON DELETE CASCADE,
    volume_id UUID NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
    
    -- Event classification
    event_type TEXT NOT NULL,
    path TEXT NOT NULL,
    
    -- Operation details (JSONB for flexibility)
    operation_details JSONB NOT NULL DEFAULT '{}'::jsonb,
    
    -- Timestamp
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    -- Validation
    CONSTRAINT storage_events_type_check CHECK (
        event_type IN (
            'FileOpened', 'FileRead', 'FileWritten', 'FileClosed',
            'DirectoryListed', 'FileCreated', 'FileDeleted',
            'PathTraversalBlocked', 'FilesystemPolicyViolation',
            'QuotaExceeded', 'UnauthorizedVolumeAccess'
        )
    )
);

-- Performance indexes
CREATE INDEX idx_storage_events_execution ON storage_events(execution_id);
CREATE INDEX idx_storage_events_volume ON storage_events(volume_id);
CREATE INDEX idx_storage_events_timestamp ON storage_events(timestamp DESC);
CREATE INDEX idx_storage_events_type ON storage_events(event_type);

-- Composite index for common queries (events by execution and type)
CREATE INDEX idx_storage_events_execution_type ON storage_events(execution_id, event_type);

-- GIN index for JSONB queries
CREATE INDEX idx_storage_events_details ON storage_events USING GIN (operation_details);

-- Partial index for security violations (fast forensic queries)
CREATE INDEX idx_storage_events_violations ON storage_events(execution_id, timestamp DESC)
WHERE event_type IN ('PathTraversalBlocked', 'FilesystemPolicyViolation', 'UnauthorizedVolumeAccess');

-- AEGIS Cluster Orchestration Schema
-- Created: February 16, 2026
-- Description: Multi-node deployment schema for cluster-level auth and routing

-- -----------------------------------------------------------------------------
-- Cluster Nodes Table
-- Stores all registered nodes in the AEGIS cluster
-- -----------------------------------------------------------------------------
CREATE TABLE cluster_nodes (
    node_id         UUID        PRIMARY KEY,
    role            TEXT        NOT NULL, -- 'controller', 'worker', 'hybrid'
    public_key      BYTEA       NOT NULL, -- Raw Ed25519 public key (32 bytes)
    grpc_address    TEXT        NOT NULL, -- "host:port" for ForwardExecution calls
    status          TEXT        NOT NULL DEFAULT 'active', -- 'active', 'draining', 'unhealthy'
    
    -- Node Capabilities (Resource Profile)
    gpu_count       INTEGER     NOT NULL DEFAULT 0,
    vram_gb         INTEGER     NOT NULL DEFAULT 0,
    cpu_cores       INTEGER     NOT NULL DEFAULT 0,
    available_mem_gb INTEGER    NOT NULL DEFAULT 0,
    supported_runtimes TEXT[]   NOT NULL DEFAULT '{}',
    tags            TEXT[]      NOT NULL DEFAULT '{}',
    
    -- Resource Snapshot (updated via Heartbeat)
    cpu_utilization_percent FLOAT4 NOT NULL DEFAULT 0.0,
    gpu_utilization_percent FLOAT4 NOT NULL DEFAULT 0.0,
    active_executions       INTEGER NOT NULL DEFAULT 0,
    
    -- Metadata
    last_heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    registered_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    -- Node Registry (ADR-061)
    hostname            TEXT        NOT NULL DEFAULT '',
    software_version    TEXT        NOT NULL DEFAULT '',
    metadata            JSONB       NOT NULL DEFAULT '{}',

    -- Config Tracking
    current_config_version TEXT
);

CREATE INDEX idx_cluster_nodes_status ON cluster_nodes (status);
CREATE INDEX idx_cluster_nodes_role   ON cluster_nodes (role, status);

-- -----------------------------------------------------------------------------
-- Node Challenges Table
-- Temporary challenge nonces for node attestation (Step 1 -> Step 2)
-- -----------------------------------------------------------------------------
CREATE TABLE node_challenges (
    challenge_id    UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    node_id         UUID        NOT NULL,
    nonce           BYTEA       NOT NULL, -- 32-byte random challenge
    public_key      BYTEA       NOT NULL,
    role            TEXT        NOT NULL,
    
    -- Cached capabilities from AttestNodeRequest to avoid re-sending in ChallengeNodeRequest
    gpu_count       INTEGER     NOT NULL DEFAULT 0,
    vram_gb         INTEGER     NOT NULL DEFAULT 0,
    cpu_cores       INTEGER     NOT NULL DEFAULT 0,
    available_mem_gb INTEGER    NOT NULL DEFAULT 0,
    supported_runtimes TEXT[]   NOT NULL DEFAULT '{}',
    tags            TEXT[]      NOT NULL DEFAULT '{}',
    
    grpc_address    TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for TTL cleanup (challenges should be deleted after 5 minutes)
CREATE INDEX idx_node_challenges_created_at ON node_challenges (created_at);

-- -----------------------------------------------------------------------------
-- Stimulus Idempotency Table
-- Multi-node stimulus idempotency store (fulfills ADR-021 Phase 2 deferral)
-- -----------------------------------------------------------------------------
CREATE TABLE stimulus_idempotency (
    id              UUID        PRIMARY KEY,
    processed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    node_id         UUID        REFERENCES cluster_nodes(node_id) ON DELETE SET NULL
);

CREATE INDEX idx_stimulus_idempotency_processed_at ON stimulus_idempotency (processed_at);

-- -----------------------------------------------------------------------------
-- Hierarchical Configuration Layers (ADR-060)
-- Precedence: global < tenant < node; local YAML overrides all (not stored here)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS config_layers (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    scope           TEXT        NOT NULL,
    scope_key       TEXT        NOT NULL DEFAULT '',
    config_type     TEXT        NOT NULL DEFAULT 'aegis-config',
    config_payload  JSONB       NOT NULL DEFAULT '{}',
    config_version  TEXT        NOT NULL DEFAULT '',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT config_layers_scope_check CHECK (scope IN ('global', 'tenant', 'node')),
    CONSTRAINT config_layers_type_check CHECK (config_type IN ('aegis-config', 'runtime-registry')),
    UNIQUE (scope, scope_key, config_type)
);

CREATE INDEX IF NOT EXISTS idx_config_layers_scope ON config_layers (scope, scope_key);
CREATE INDEX IF NOT EXISTS idx_config_layers_type ON config_layers (config_type);

-- ============================================================================
-- Rate Limiting (ADR-072)
-- ============================================================================

-- Rate limit overrides for tenant-level and user-level customization.
-- Override hierarchy: ZaruTier defaults < Tenant overrides < User overrides.
CREATE TABLE rate_limit_overrides (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       VARCHAR(63) REFERENCES tenants(slug) ON DELETE CASCADE,
    user_id         TEXT,
    resource_type   TEXT NOT NULL,
    bucket          TEXT NOT NULL,
    limit_value     BIGINT NOT NULL,
    burst_value     BIGINT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT rate_limit_overrides_scope_check CHECK (
        (tenant_id IS NOT NULL AND user_id IS NULL)
        OR (tenant_id IS NULL AND user_id IS NOT NULL)
    ),
    CONSTRAINT rate_limit_overrides_unique UNIQUE (tenant_id, user_id, resource_type, bucket)
);

CREATE INDEX idx_rate_limit_overrides_tenant ON rate_limit_overrides(tenant_id)
    WHERE tenant_id IS NOT NULL;
CREATE INDEX idx_rate_limit_overrides_user ON rate_limit_overrides(user_id)
    WHERE user_id IS NOT NULL;

-- Sliding window counters for hourly/daily/weekly/monthly rate limit windows.
-- PerMinute windows use in-memory token buckets and are NOT stored here.
CREATE TABLE rate_limit_counters (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope_type      TEXT NOT NULL,
    scope_id        TEXT NOT NULL,
    resource_type   TEXT NOT NULL,
    bucket          TEXT NOT NULL,
    window_start    TIMESTAMPTZ NOT NULL,
    counter         BIGINT NOT NULL DEFAULT 0,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT rate_limit_counters_unique UNIQUE (scope_type, scope_id, resource_type, bucket, window_start)
);

CREATE INDEX idx_rate_limit_counters_lookup
    ON rate_limit_counters(scope_type, scope_id, resource_type, bucket, window_start);
