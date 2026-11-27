-- Migration 062: SEAL sessions shared by the cluster (AEGIS ADR-152 D4.1;
-- phase P4).
--
-- A session attested on one node was unknown to the others, so a SEAL
-- client sent to another member by the load balancer failed. Every member
-- now reads and writes the same sessions.
--
-- The session's bearer token is never stored: token_hash is its SHA-256, by
-- which a presented token finds its session. The rest of the session is
-- body (JSON), whose status column mirrors the body's for the queries.
--
-- Forward-only and idempotent.

CREATE TABLE IF NOT EXISTS seal_sessions (
    id           UUID        PRIMARY KEY,
    agent_id     UUID        NOT NULL,
    execution_id UUID        NOT NULL,
    token_hash   BYTEA       NOT NULL,
    status       TEXT        NOT NULL,
    body         JSONB       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_seal_sessions_token_active
    ON seal_sessions (token_hash) WHERE status = 'active';
CREATE INDEX IF NOT EXISTS idx_seal_sessions_agent_active
    ON seal_sessions (agent_id) WHERE status = 'active';
CREATE INDEX IF NOT EXISTS idx_seal_sessions_expires_at
    ON seal_sessions (expires_at);
