-- Migration 040: a goal that stops and says why (AEGIS ADR-131, Update of
-- 2026-10-04 (5), U16 and U17).
--
-- goals.state gains `stopped`: a goal whose round was not judged, because
-- the judge's input is larger than its model's context holds (U16), or
-- because the round brought nothing new against the round before it (U17).
--
-- goal_evaluations gains two nullable columns:
--   stop_reason   `too_large` or `repeated_round`, on the evaluation that
--                 stopped the goal; no judge ran for it.
--   input_digest  the SHA-256, in hex, of the judge's input without `round`
--                 and `rounds_left`, on every evaluation from this migration
--                 on, so a round can be compared with the round before it.
-- A stopping evaluation has no outcome, so it decides its round under a
-- second partial unique index beside 039's.
--
-- Forward-only and additive: the state CHECK is widened (every value it
-- allowed is still allowed), two nullable columns and one partial index
-- are added, and no existing row is touched: a goal or an evaluation stored
-- before it reads as it did, with both new columns NULL. Idempotent, like
-- every migration after 033: running it again over a migrated schema
-- changes nothing.

ALTER TABLE goals DROP CONSTRAINT IF EXISTS goals_state_check;
ALTER TABLE goals ADD CONSTRAINT goals_state_check CHECK (state IN
    ('open', 'met', 'cannot_be_met', 'exhausted', 'expired', 'superseded', 'stopped'));

ALTER TABLE goal_evaluations ADD COLUMN IF NOT EXISTS stop_reason TEXT NULL;
ALTER TABLE goal_evaluations DROP CONSTRAINT IF EXISTS goal_evaluations_stop_reason_check;
ALTER TABLE goal_evaluations ADD CONSTRAINT goal_evaluations_stop_reason_check
    CHECK (stop_reason IS NULL OR stop_reason IN ('too_large', 'repeated_round'));

ALTER TABLE goal_evaluations ADD COLUMN IF NOT EXISTS input_digest TEXT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS uq_goal_evaluations_stopped_round
    ON goal_evaluations (goal_id, round)
    WHERE stop_reason IS NOT NULL;
