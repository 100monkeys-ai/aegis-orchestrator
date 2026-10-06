-- Migration 043: a goal the person ends, and why (AEGIS ADR-131 U33, U33a).
--
-- goals.state gains `cancelled`: the person ended the goal, by
-- aegis.goal.cancel or by cancelling an execution bound to it, and no round
-- follows.
--
-- goals gains one nullable column:
--   closed_reason  why the goal closed, where a reason was given: the
--                  person's reason on aegis.goal.cancel, or "its execution
--                  <id> was cancelled" when a cancel of a bound execution
--                  ended it. NULL on every other close.
--
-- Forward-only and additive: the state CHECK is widened (every value it
-- allowed is still allowed) and one nullable column is added; no existing
-- row is touched, and a goal stored before it reads as it did, with
-- closed_reason NULL. Idempotent, like every migration after 033: running
-- it again over a migrated schema changes nothing.

ALTER TABLE goals DROP CONSTRAINT IF EXISTS goals_state_check;
ALTER TABLE goals ADD CONSTRAINT goals_state_check CHECK (state IN
    ('open', 'met', 'cannot_be_met', 'exhausted', 'expired', 'superseded', 'stopped',
     'cancelled'));

ALTER TABLE goals ADD COLUMN IF NOT EXISTS closed_reason TEXT NULL;
