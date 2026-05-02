-- ADR-117: persist the operator-supplied friendly name (or auto-generated
-- `edge-<short>` fallback) captured at enrollment via Zaru's "Add Edge Host"
-- dialog. The friendly name flows in via the enrollment JWT `sub` claim.
--
-- Pre-existing rows have no display name on file; the empty default is
-- acceptable because the read-side falls back to a node-id-derived label
-- when the column is empty.

ALTER TABLE edge_daemons
    ADD COLUMN IF NOT EXISTS display_name TEXT NOT NULL DEFAULT '';
