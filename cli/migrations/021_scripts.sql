-- Migration 021: Script Persistence (ADR-110 §D7)
--
-- Persists TypeScript programs authored in Zaru Live Mode / Code Mode for
-- reuse. The `scripts` table holds the CURRENT version of each script and
-- the `script_versions` table holds the full version-history audit trail.
--
-- Lifecycle:
--   Create → insert into BOTH tables
--   Update → bump `scripts.version`, update mutable fields, insert a new row
--            into `script_versions` with the new version
--   Delete → set `scripts.deleted_at` (soft-delete). Version history stays
--            queryable for audit even after deletion.
--
-- Tenant isolation:
--   `scripts.tenant_id` REFERENCES `tenants(slug)` RESTRICT. Visibility is
--   `'private'` today; `'tenant'` / `'public'` are reserved for a future
--   marketplace ADR but are accepted by the CHECK constraint so the
--   schema is forward-compatible.

CREATE TABLE scripts (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     TEXT        NOT NULL,
    created_by    TEXT        NOT NULL,
    name          TEXT        NOT NULL,
    description   TEXT        NOT NULL DEFAULT '',
    code          TEXT        NOT NULL,
    tags          TEXT[]      NOT NULL DEFAULT ARRAY[]::TEXT[],
    visibility    TEXT        NOT NULL DEFAULT 'private',
    version       INTEGER     NOT NULL DEFAULT 1,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at    TIMESTAMPTZ,

    CONSTRAINT fk_script_tenant FOREIGN KEY (tenant_id)
        REFERENCES tenants(slug) ON DELETE RESTRICT,

    CONSTRAINT scripts_visibility_chk
        CHECK (visibility IN ('private', 'tenant', 'public')),

    CONSTRAINT scripts_version_positive_chk
        CHECK (version > 0)
);

-- Unique script name per tenant, excluding soft-deleted rows. A user may
-- re-use a name after deleting the original — the partial index allows it.
CREATE UNIQUE INDEX scripts_tenant_name_active
    ON scripts (tenant_id, name)
    WHERE deleted_at IS NULL;

-- Per-owner listing (GET /v1/scripts).
CREATE INDEX scripts_tenant_created_by
    ON scripts (tenant_id, created_by)
    WHERE deleted_at IS NULL;

-- Version history. One row per version, keyed by (script_id, version).
-- Cascades when the parent row is hard-deleted — but note that today we
-- only soft-delete the parent, so rows here accumulate as an audit log.
CREATE TABLE script_versions (
    script_id   UUID         NOT NULL,
    version     INTEGER      NOT NULL,
    name        TEXT         NOT NULL,
    description TEXT         NOT NULL DEFAULT '',
    code        TEXT         NOT NULL,
    tags        TEXT[]       NOT NULL DEFAULT ARRAY[]::TEXT[],
    updated_by  TEXT         NOT NULL,
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    PRIMARY KEY (script_id, version),

    CONSTRAINT fk_script_version_parent FOREIGN KEY (script_id)
        REFERENCES scripts(id) ON DELETE CASCADE,

    CONSTRAINT script_versions_version_positive_chk
        CHECK (version > 0)
);

-- Cheap lookup for `list_versions` (`ORDER BY version ASC`) and audit
-- queries by owner.
CREATE INDEX script_versions_by_script_version
    ON script_versions (script_id, version);
