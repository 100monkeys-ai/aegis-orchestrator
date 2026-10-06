-- Migration 042: what a remote tool server's token reaches (AEGIS ADR-132
-- (7a) S2): `{kind: apex|instance, instance_slug, instance_id,
-- workspace_id, grounded_at}`, read from the server's grounding of the
-- token when it is stored, rotated or introspected. Never a secret.
--
-- Forward-only and additive: one nullable column, no default, no existing
-- row rewritten. Every binding whose provider is not a remote server keeps
-- NULL. Idempotent, like every migration after 033: running it again over a
-- migrated schema changes nothing.

ALTER TABLE credential_bindings ADD COLUMN IF NOT EXISTS reach JSONB NULL;
