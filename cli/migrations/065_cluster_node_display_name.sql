-- Migration 065: a cluster member's human-readable name (AEGIS ADR-152).
--
-- Written on join from the node configuration's `metadata.name`, so the
-- operator views show members by name rather than by UUID.
--
-- Forward-only and idempotent.

ALTER TABLE cluster_nodes ADD COLUMN IF NOT EXISTS display_name TEXT;
