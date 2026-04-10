-- Migration 015: support workflow-scoped storage events
--
-- ContainerStep FUSE reads fire StorageEvents with a workflow_execution_id,
-- not an agent execution_id. Make execution_id nullable and add
-- workflow_execution_id with a mutual-exclusivity CHECK constraint.

ALTER TABLE storage_events
    ADD COLUMN workflow_execution_id UUID REFERENCES workflow_executions(id) ON DELETE CASCADE,
    ALTER COLUMN execution_id DROP NOT NULL;

ALTER TABLE storage_events
    ADD CONSTRAINT storage_events_execution_context_check CHECK (
        (execution_id IS NOT NULL AND workflow_execution_id IS NULL) OR
        (execution_id IS NULL AND workflow_execution_id IS NOT NULL)
    );

CREATE INDEX idx_storage_events_workflow_execution_id
    ON storage_events (workflow_execution_id);

-- Update violations partial index to include workflow_execution_id path
DROP INDEX IF EXISTS idx_storage_events_violations;
CREATE INDEX idx_storage_events_violations ON storage_events(
    COALESCE(execution_id, workflow_execution_id), timestamp DESC
) WHERE event_type IN ('PathTraversalBlocked', 'FilesystemPolicyViolation', 'UnauthorizedVolumeAccess');
