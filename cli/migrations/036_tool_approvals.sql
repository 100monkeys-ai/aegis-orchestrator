-- Migration 036: the durable store of the tool approval gate (AEGIS ADR-126 D3).
--
-- tool_approval_requests holds every call of a gated tool: the exact
-- arguments the agent passed, the summary its user reads, and what became of
-- it (pending, approved_once, approved_always, denied, expired, auto_allowed)
-- with the result of the run on approval. tool_approval_policies holds a
-- user's "always allow" for one tool on one binding, until revoked.
--
-- Two columns beyond the record's list, both needed to run a stored call
-- after its agent has ended: security_context_name (the context the call is
-- dispatched under on approval; a Zaru SEAL session has no execution record
-- to read it from) and policy_id (the policy an auto_allowed row matched,
-- which D2 says the row carries).
--
-- Forward-only and additive: two new tables, no existing row touched.
-- Idempotent, like every migration after 033: running it again over a
-- migrated schema changes nothing.

CREATE TABLE IF NOT EXISTS tool_approval_policies (
    id          UUID PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    user_sub    TEXT NOT NULL,
    tool_name   TEXT NOT NULL,
    binding_id  TEXT NULL,
    created_at  TIMESTAMPTZ NOT NULL,
    created_by  TEXT NOT NULL,
    revoked_at  TIMESTAMPTZ NULL
);

CREATE INDEX IF NOT EXISTS idx_tool_approval_policies_active
    ON tool_approval_policies (tenant_id, user_sub, tool_name)
    WHERE revoked_at IS NULL;

CREATE TABLE IF NOT EXISTS tool_approval_requests (
    id                     UUID PRIMARY KEY,
    tenant_id              TEXT NOT NULL,
    user_sub               TEXT NOT NULL,
    execution_id           UUID NOT NULL,
    agent_id               UUID NOT NULL,
    tool_name              TEXT NOT NULL,
    arguments              JSONB NOT NULL,
    summary                TEXT NOT NULL,
    binding_id             TEXT NULL,
    security_context_name  TEXT NOT NULL,
    policy_id              UUID NULL REFERENCES tool_approval_policies (id),
    status                 TEXT NOT NULL CHECK (status IN (
                               'pending', 'approved_once', 'approved_always',
                               'denied', 'expired', 'auto_allowed')),
    created_at             TIMESTAMPTZ NOT NULL,
    decided_at             TIMESTAMPTZ NULL,
    decided_by             TEXT NULL,
    result                 JSONB NULL,
    error                  TEXT NULL
);

CREATE INDEX IF NOT EXISTS idx_tool_approval_requests_user
    ON tool_approval_requests (tenant_id, user_sub, status, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_tool_approval_requests_pending
    ON tool_approval_requests (created_at)
    WHERE status = 'pending';
