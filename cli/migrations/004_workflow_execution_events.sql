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
