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

