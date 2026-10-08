-- Migration 046: a standing choice can deny a tool as well as allow it
-- (AEGIS ADR-126, Update of 2026-10-08 (2), clause 1).
--
-- tool_approval_policies gains one column:
--   effect  'allow' (a matching call proceeds, its row `auto_allowed`) or
--           'deny' (a matching call is refused at once, its row
--           `auto_denied`, and never runs). Every row stored before this
--           migration reads 'allow', which is what it meant until now.
--
-- tool_approval_requests.status admits 'auto_denied': its CHECK is dropped
-- and added again with every value it allowed and the new one, as
-- migration 043 widened goals_state_check.
--
-- Forward-only and additive: one column with a default, a CHECK widened; no
-- row's meaning changes. Idempotent, like every migration after 033: running
-- it again over a migrated schema changes nothing.

ALTER TABLE tool_approval_policies
    ADD COLUMN IF NOT EXISTS effect TEXT NOT NULL DEFAULT 'allow'
        CHECK (effect IN ('allow', 'deny'));

ALTER TABLE tool_approval_requests DROP CONSTRAINT IF EXISTS tool_approval_requests_status_check;
ALTER TABLE tool_approval_requests ADD CONSTRAINT tool_approval_requests_status_check CHECK (status IN
    ('pending', 'approved_once', 'approved_always', 'denied', 'expired', 'auto_allowed',
     'auto_denied'));
