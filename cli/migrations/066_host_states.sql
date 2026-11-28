-- Migration 066: host state, a native AEGIS resource (AEGIS ADR-157 D2).
--
-- A HostState is a tenant's declared state for a set of edge hosts: an
-- ordered list of resources and the edge target it applies to. Every
-- change writes a new version; the newest version of a name is active and
-- older ones are archived (ADR-148).
--
-- Forward-only and idempotent.

CREATE TABLE IF NOT EXISTS host_states (
    id             UUID        PRIMARY KEY,
    tenant_id      TEXT        NOT NULL,
    name           TEXT        NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9.-]{0,62}$'),
    version        INTEGER     NOT NULL CHECK (version >= 1),
    status         TEXT        NOT NULL CHECK (status IN ('active', 'archived')),
    manifest_yaml  TEXT        NOT NULL,
    spec           JSONB       NOT NULL,
    created_by     TEXT        NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name, version)
);

CREATE UNIQUE INDEX IF NOT EXISTS host_states_one_active
    ON host_states (tenant_id, name) WHERE status = 'active';
