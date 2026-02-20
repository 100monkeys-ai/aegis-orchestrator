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

-- -----------------------------------------------------------------------------
-- Workflows Table
-- Stores registered workflow definitions
-- -----------------------------------------------------------------------------
CREATE TABLE workflows (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL UNIQUE,
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
    CONSTRAINT workflows_version_format CHECK (version ~ '^\d+\.\d+\.\d+$')
);

CREATE INDEX idx_workflows_name ON workflows(name);
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
    
    -- Timestamps
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_transition_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    
    -- Validation
    CONSTRAINT workflow_executions_status_check CHECK (
        status IN ('running', 'completed', 'failed', 'cancelled', 'timed_out', 'pending')
    )
);

CREATE INDEX idx_workflow_executions_workflow_id ON workflow_executions(workflow_id);
CREATE INDEX idx_workflow_executions_temporal_workflow_id ON workflow_executions(temporal_workflow_id);
CREATE INDEX idx_workflow_executions_status ON workflow_executions(status);
CREATE INDEX idx_workflow_executions_started_at ON workflow_executions(started_at DESC);
CREATE INDEX idx_workflow_executions_current_state ON workflow_executions(current_state);

-- Composite index for common queries
CREATE INDEX idx_workflow_executions_workflow_temporal ON workflow_executions(workflow_id, temporal_workflow_id);

-- -----------------------------------------------------------------------------
-- Agents Table
-- Stores agent manifest definitions
-- -----------------------------------------------------------------------------
CREATE TABLE agents (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL UNIQUE,
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
    CONSTRAINT agents_status_check CHECK (status IN ('active', 'paused', 'archived'))
);

CREATE INDEX idx_agents_name ON agents(name);
CREATE INDEX idx_agents_status ON agents(status);
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
    
    -- Timestamps
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    
    -- Validation
    CONSTRAINT executions_status_check CHECK (
        status IN ('pending', 'running', 'completed', 'failed', 'cancelled')
    )
);

CREATE INDEX idx_executions_agent_id ON executions(agent_id);
CREATE INDEX idx_executions_workflow_execution_id ON executions(workflow_execution_id);
CREATE INDEX idx_executions_status ON executions(status);
CREATE INDEX idx_executions_started_at ON executions(started_at DESC);

-- NOTE: Cortex patterns table removed - will be implemented with Vector+RAG in future iteration

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
    name TEXT NOT NULL UNIQUE,
    definition JSONB NOT NULL,
    
    -- Registration metadata
    registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    registered_by TEXT,
    
    -- Version tracking
    definition_hash TEXT NOT NULL,
    
    CONSTRAINT workflow_definitions_name_format CHECK (name ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$')
);

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

COMMENT ON TABLE workflows IS 'Registered workflow definitions in AEGIS YAML format';
COMMENT ON TABLE workflow_executions IS 'Links to Temporal workflow runs with AEGIS metadata';
COMMENT ON TABLE agents IS 'Agent manifest definitions from MANIFEST_SPEC_V1';
COMMENT ON TABLE executions IS 'Individual agent execution records with 100monkeys iterations';
COMMENT ON TABLE workflow_definitions IS 'Shared workflow definition registry for TypeScript workers';

COMMENT ON COLUMN workflows.domain_json IS 'Serialized Rust Workflow domain object for validation';
COMMENT ON COLUMN workflows.temporal_def_json IS 'Generated Temporal workflow definition sent to workers';
COMMENT ON COLUMN workflow_executions.temporal_workflow_id IS 'Temporal workflow ID for querying Temporal Server';
COMMENT ON COLUMN workflow_executions.temporal_run_id IS 'Temporal run ID for specific execution instance';

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
    RAISE NOTICE 'Created tables: workflows, workflow_executions, agents, executions, workflow_definitions';
    RAISE NOTICE 'Created views: active_workflow_executions, agent_success_rates';
    RAISE NOTICE 'NOTE: Cortex implementation deferred to Vector+RAG iteration';
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
    tenant_id UUID NOT NULL,
    
    -- Storage configuration
    storage_class JSONB NOT NULL,      -- {"type": "ephemeral", "ttl": "PT24H"} or {"type": "persistent"}
    filer_endpoint JSONB NOT NULL,     -- {"url": "http://localhost:8888"}
    remote_path TEXT NOT NULL UNIQUE,  -- /aegis/volumes/{tenant_id}/{volume_id}
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
    CONSTRAINT volumes_size_limit_positive CHECK (size_limit_bytes > 0)
    -- Note: Volume names are logical labels, NOT globally unique.
    -- Multiple executions can have volumes with the same name but different volume_ids.
    -- Uniqueness is enforced by: id (PRIMARY KEY) and remote_path (UNIQUE).
);

-- Indexes for common queries
CREATE INDEX idx_volumes_tenant_id ON volumes(tenant_id);
CREATE INDEX idx_volumes_expires_at ON volumes(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX idx_volumes_ownership ON volumes USING GIN (ownership);
CREATE INDEX idx_volumes_status ON volumes USING GIN (status);
CREATE INDEX idx_volumes_remote_path ON volumes(remote_path);
CREATE INDEX idx_volumes_created_at ON volumes(created_at DESC);

-- Composite indexes for ownership queries
CREATE INDEX idx_volumes_tenant_status ON volumes(tenant_id, (status->>'type'));

-- Add comment for documentation
COMMENT ON TABLE volumes IS 'Volume metadata for SeaweedFS-backed distributed storage (ADR-032)';
COMMENT ON COLUMN volumes.tenant_id IS 'Multi-tenant namespace isolation (default: 00000000-0000-0000-0000-000000000001)';
COMMENT ON COLUMN volumes.storage_class IS 'Ephemeral (TTL-based) or Persistent storage classification';
COMMENT ON COLUMN volumes.ownership IS 'Execution-scoped, Workflow-scoped, or User-owned persistent';
COMMENT ON COLUMN volumes.expires_at IS 'Auto-cleanup timestamp for ephemeral volumes (NULL for persistent)';
COMMENT ON COLUMN volumes.remote_path IS 'Unique SeaweedFS filer path for this volume';

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

-- AEGIS Temporal Era Database Schema Update
-- Created: February 19, 2026
-- Description: Adds execution_events table for event sourcing of Temporal workflow events

-- -----------------------------------------------------------------------------
-- Execution Events Table
-- Append-only event store for workflow execution history
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS execution_events (
    id BIGSERIAL PRIMARY KEY,
    execution_id UUID NOT NULL REFERENCES workflow_executions(id) ON DELETE CASCADE,
    
    -- Temporal correlation and idempotency
    temporal_sequence_number BIGINT NOT NULL,
    
    -- Event tracking
    event_type VARCHAR NOT NULL,
    event_payload JSONB NOT NULL,
    
    -- 100monkeys iteration tracking
    iteration_number SMALLINT,
    
    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    -- Ensure idempotent delivery from Temporal
    CONSTRAINT execution_events_unique_seq UNIQUE (execution_id, temporal_sequence_number)
);

CREATE INDEX IF NOT EXISTS idx_execution_events_execution_id ON execution_events(execution_id);
CREATE INDEX IF NOT EXISTS idx_execution_events_type ON execution_events(event_type);
CREATE INDEX IF NOT EXISTS idx_execution_events_created_at ON execution_events(created_at DESC);

-- -----------------------------------------------------------------------------
-- Workflow Executions Updates
-- Support for iteration counting
-- -----------------------------------------------------------------------------
ALTER TABLE workflow_executions 
ADD COLUMN IF NOT EXISTS iteration_count SMALLINT DEFAULT 0;
