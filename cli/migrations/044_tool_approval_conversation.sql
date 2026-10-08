-- Migration 044: the conversation an approval request was made in (AEGIS
-- ADR-126, Update of 2026-10-07 (2), clause 1).
--
-- tool_approval_requests gains one nullable column:
--   conversation_id  the Zaru conversation the gated call was made in, from
--                    the call's `_meta.conversation_id` when its session has
--                    no execution record; NULL for a call no conversation
--                    started. A person's list links each request to it.
--
-- Forward-only and additive: one nullable column, no default, no existing
-- row rewritten; a request stored before it reads as it did, with
-- conversation_id NULL. Idempotent, like every migration after 033: running
-- it again over a migrated schema changes nothing.

ALTER TABLE tool_approval_requests ADD COLUMN IF NOT EXISTS conversation_id TEXT NULL;
