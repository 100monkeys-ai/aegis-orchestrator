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
    CONSTRAINT volumes_size_limit_positive CHECK (size_limit_bytes > 0),
    CONSTRAINT volumes_unique_per_tenant UNIQUE (tenant_id, name)
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
