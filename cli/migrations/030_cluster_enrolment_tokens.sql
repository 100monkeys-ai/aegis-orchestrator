-- Security audit 002 §4.9 — cluster admission gate.
--
-- Single-use, atomic enrolment tokens scoped to a node identity. Tokens are
-- issued out of band by the controller's admin path (separate from the
-- AttestNode RPC) and redeemed exactly once at attestation time. Each row
-- stores SHA-256(secret) — the bare secret is never persisted.

CREATE TABLE IF NOT EXISTS cluster_enrolment_tokens (
    token_id     UUID PRIMARY KEY,
    node_id      UUID NOT NULL,
    secret_hash  BYTEA NOT NULL,
    issued_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    exp          TIMESTAMPTZ NOT NULL,
    redeemed_at  TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS cluster_enrolment_tokens_node_idx
    ON cluster_enrolment_tokens(node_id);
CREATE INDEX IF NOT EXISTS cluster_enrolment_tokens_exp_idx
    ON cluster_enrolment_tokens(exp);
