-- ADR-117: AEGIS Edge Mode persistence.
--
-- Three tables:
--   * edge_daemons       — enrolled edge daemons, tenant-bound, server-side
--                          tag column.
--   * enrollment_tokens  — JTI redemption ledger; atomic single-use enforcement.
--   * edge_groups        — saved selectors for fleet operations
--                          (tenant_id, name) unique.

CREATE TABLE IF NOT EXISTS edge_daemons (
    node_id           UUID PRIMARY KEY,
    tenant_id         TEXT NOT NULL,
    public_key        BYTEA NOT NULL,
    capabilities_json JSONB NOT NULL,
    tags              TEXT[] NOT NULL DEFAULT '{}',
    status            TEXT NOT NULL,
    enrolled_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_heartbeat_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS edge_daemons_tenant_idx ON edge_daemons(tenant_id);
CREATE INDEX IF NOT EXISTS edge_daemons_tags_idx  ON edge_daemons USING GIN(tags);

CREATE TABLE IF NOT EXISTS enrollment_tokens (
    jti         UUID PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    issued_to   TEXT NOT NULL,
    exp         TIMESTAMPTZ NOT NULL,
    redeemed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS enrollment_tokens_tenant_idx ON enrollment_tokens(tenant_id);

CREATE TABLE IF NOT EXISTS edge_groups (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       TEXT NOT NULL,
    name            TEXT NOT NULL,
    selector_json   JSONB NOT NULL,
    pinned_members  UUID[] NOT NULL DEFAULT '{}',
    created_by      TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(tenant_id, name)
);
