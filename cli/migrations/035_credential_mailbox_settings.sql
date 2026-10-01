-- Migration 035: the non-secret settings of an SMTP-with-IMAP mailbox
-- (AEGIS ADR-125 D1): address, display name, IMAP and SMTP host, port and
-- security, username. The password is never here; it lives in OpenBao under
-- the field `password` at the binding's secret_path.
--
-- Forward-only and additive: one nullable column, no default, no existing
-- row rewritten. Every binding that is not an `imap` mailbox keeps NULL.
-- Idempotent, like every migration after 033: running it again over a
-- migrated schema changes nothing.

ALTER TABLE credential_bindings ADD COLUMN IF NOT EXISTS mailbox_settings JSONB NULL;
