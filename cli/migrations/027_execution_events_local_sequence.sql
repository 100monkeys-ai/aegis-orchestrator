-- 027_execution_events_local_sequence.sql
--
-- Rename `temporal_sequence_number` → `sequence_number`. The `execution_events`
-- table is now written by both the Temporal listener (sequence numbers from the
-- workflow history) and the in-process `ExecutionEventPersister` (per-execution
-- counter starting at 1_000_000_000_000 — well above any plausible Temporal
-- history index, so collisions are impossible). Keep BIGINT NOT NULL.
--
-- Pre-alpha: straight rename + drop+add constraint; no compatibility shim.

ALTER TABLE execution_events RENAME COLUMN temporal_sequence_number TO sequence_number;

ALTER TABLE execution_events DROP CONSTRAINT execution_events_unique_sequence;
ALTER TABLE execution_events ADD CONSTRAINT execution_events_unique_sequence
    UNIQUE (execution_id, sequence_number);

DROP INDEX IF EXISTS idx_execution_events_sequence;
CREATE INDEX idx_execution_events_sequence
    ON execution_events(execution_id, sequence_number ASC);
