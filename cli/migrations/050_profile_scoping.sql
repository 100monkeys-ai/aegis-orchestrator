-- Migration 050: a standing choice, a gated call and a schedule carry the
-- person's profile (AEGIS ADR-140 D8, D9).
--
-- tool_approval_policies gains profile_id: a standing choice made on a call
-- of a run or conversation carrying a profile is stored with that profile's
-- id and matches only calls carrying it; one made on raw bindings has none
-- and matches only raw-binding calls. Every row stored before this
-- migration has none, and keeps matching raw-binding calls as it did.
--
-- tool_approval_requests gains profile_id: the profile the gated call
-- carried, none for a raw-binding call.
--
-- schedules gains profile_id: the profile its runs start on, beside
-- contexts; a schedule carries one or the other, never both, which the
-- orchestrator refuses before anything is stored.
--
-- No foreign key: a deleted profile keeps its row (deleted_at), and a
-- policy keyed to it is revoked when it is deleted.
--
-- Forward-only and additive: three nullable columns and an index; no row's
-- meaning changes. Idempotent, like every migration after 033: running it
-- again over a migrated schema changes nothing.

ALTER TABLE tool_approval_policies ADD COLUMN IF NOT EXISTS profile_id UUID NULL;
ALTER TABLE tool_approval_requests ADD COLUMN IF NOT EXISTS profile_id UUID NULL;
ALTER TABLE schedules ADD COLUMN IF NOT EXISTS profile_id UUID NULL;

CREATE INDEX IF NOT EXISTS idx_tool_approval_policies_profile
    ON tool_approval_policies (tenant_id, user_sub, profile_id)
    WHERE profile_id IS NOT NULL AND revoked_at IS NULL;
