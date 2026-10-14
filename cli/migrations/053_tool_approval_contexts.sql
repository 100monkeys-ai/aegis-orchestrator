-- Migration 053: a gated call keeps the conversation's choice of bindings
-- (AEGIS ADR-126, Update of 2026-10-14, clause 1).
--
-- tool_approval_requests gains one nullable column:
--   contexts  the call's `_meta.contexts` as the invoke route admitted it
--             (AEGIS ADR-132 S11a), for a call of a session with no
--             execution record and no profile; NULL for a call of an
--             execution with a record (its record governs), for a call on a
--             profile (profile_id stands for it) and for a call that carried
--             none. The approved re-run acts under it. The store seals it
--             with `arguments` (a JSON string holding the Transit
--             ciphertext), and the request view never answers it.
--
-- Forward-only and additive: one nullable column, no default, no existing
-- row rewritten; a request stored before it reads as it did, with contexts
-- NULL, and its re-run acts as before. Idempotent, like every migration
-- after 033: running it again over a migrated schema changes nothing.

ALTER TABLE tool_approval_requests ADD COLUMN IF NOT EXISTS contexts JSONB NULL;
