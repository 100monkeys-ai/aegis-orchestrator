-- Migration 068: webhook sources (AEGIS ADR-157 D4).
--
-- A tenant's registered sender of signed deliveries and the workflow a
-- delivery starts. The source's secret is in the tenant's own secret store,
-- never here. A name is unique per tenant; two tenants may each register
-- the same name, and a delivery is verified against each one's secret.
--
-- Forward-only and idempotent.

CREATE TABLE IF NOT EXISTS webhook_sources (
    tenant_id   TEXT        NOT NULL,
    name        TEXT        NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    spec        JSONB       NOT NULL,
    owner       JSONB       NOT NULL,
    created_by  TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, name)
);

CREATE INDEX IF NOT EXISTS webhook_sources_by_name ON webhook_sources (name);
