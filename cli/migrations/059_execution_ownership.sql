-- Migration 059: execution ownership and the cancel request (AEGIS ADR-152
-- D5.1, D5.2, D5.6; phase P2).
--
-- On an active-active cluster an execution belongs to the node process that
-- created it: `owner_node_id` and `owner_incarnation` are set in the same
-- INSERT that creates the row (the claim), and every later write by that
-- owner is conditional on still owning it at `owner_epoch` 0. The
-- execution-adoption duty takes a dead owner's execution by incrementing
-- `owner_epoch` and naming itself, so a late write by the old owner affects
-- zero rows and the old owner knows it has been fenced. At start a node
-- ends, as failed, only the unfinished executions its own earlier
-- incarnations left. `cancel_requested_at` records that a cancel was asked
-- for, wherever it arrived, so the owner (or the adopter) ends the run as
-- cancelled. A node outside a cluster sets none of these: its rows keep
-- NULL owners and it remains the only writer of its database.
--
-- Forward-only and idempotent; existing rows keep every value and have no
-- owner. No statement here is an UPDATE or DELETE (pg_strict), and none
-- needs a session (PgBouncer in transaction mode).

ALTER TABLE executions ADD COLUMN IF NOT EXISTS owner_node_id UUID;
ALTER TABLE executions ADD COLUMN IF NOT EXISTS owner_incarnation BIGINT;
ALTER TABLE executions ADD COLUMN IF NOT EXISTS owner_epoch BIGINT NOT NULL DEFAULT 0;
ALTER TABLE executions ADD COLUMN IF NOT EXISTS cancel_requested_at TIMESTAMPTZ;

-- The restart pass and the adoption duty look up a node's unfinished
-- executions.
CREATE INDEX IF NOT EXISTS idx_executions_owner_unfinished
    ON executions (owner_node_id, owner_incarnation)
    WHERE status IN ('pending', 'running');
