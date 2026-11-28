-- Migration 067: provider state, a native AEGIS resource (AEGIS ADR-157 D11).
--
-- A ProviderState is a tenant's declared state for objects at a provider
-- (Cloudflare first): an ordered list of resources. Every
-- change writes a new version; the newest version of a name is active and
-- older ones are archived (ADR-148).
--
-- Forward-only and idempotent.

CREATE TABLE IF NOT EXISTS provider_states (
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

CREATE UNIQUE INDEX IF NOT EXISTS provider_states_one_active
    ON provider_states (tenant_id, name) WHERE status = 'active';
