-- The supervisor's execution bound in seconds, recorded when the execution
-- starts, so the container reaper can end an execution that has outlived it
-- (AEGIS ADR-040, Update of 2026-10-04). NULL only on rows written before
-- this migration; the reaper never ends an execution whose bound is NULL.
ALTER TABLE executions ADD COLUMN IF NOT EXISTS timeout_seconds BIGINT;
