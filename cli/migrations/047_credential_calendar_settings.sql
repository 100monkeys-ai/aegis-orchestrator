-- Migration 047: the non-secret settings of a calendar account reached over
-- CalDAV (AEGIS ADR-138 K4): the server, the principal and the address. No
-- secret is here; an OAuth binding's token lives in OpenBao at the binding's
-- secret_path.
--
-- Forward-only and additive: one nullable column, no default, no existing
-- row rewritten. Every binding that is not a calendar account keeps NULL.
-- Idempotent, like every migration after 033: running it again over a
-- migrated schema changes nothing.

ALTER TABLE credential_bindings ADD COLUMN IF NOT EXISTS calendar_settings JSONB NULL;
