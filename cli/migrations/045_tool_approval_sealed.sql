-- Migration 045: a tool-approval request's arguments, summary, result and
-- error are stored sealed (AEGIS ADR-126, Updates of 2026-10-04 clause 2 and
-- 2026-10-08).
--
-- The store seals the four fields with OpenBao Transit under the tenant's key
-- `tool-approvals-<tenant_id>` and keeps the ciphertext in the same columns: a
-- JSON string in `arguments` and `result`, the text itself in `summary` and
-- `error`. `sealed` tells a sealed row from one written before this
-- migration, which keeps its fields as stored and is read as stored; there is
-- no backfill, since SQL cannot call Transit.
--
-- Additive and idempotent: running it again changes nothing.

ALTER TABLE tool_approval_requests
    ADD COLUMN IF NOT EXISTS sealed BOOLEAN NOT NULL DEFAULT FALSE;
