-- AEGIS Execution Hierarchy Migration
-- Created: 2026-03-08
-- Description: Adds parent_execution_id to the executions table to support
--              hierarchical (parent/child) execution tracking (ADR-039).

ALTER TABLE executions
ADD COLUMN IF NOT EXISTS parent_execution_id UUID REFERENCES executions(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_executions_parent_execution_id ON executions(parent_execution_id)
    WHERE parent_execution_id IS NOT NULL;

COMMENT ON COLUMN executions.parent_execution_id IS 'Parent execution ID for child executions (NULL for root executions). Supports ADR-039 recursive execution hierarchy.';
