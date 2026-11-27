-- Migration 061: SEAL replay nonces shared by the cluster (AEGIS ADR-152
-- D4.2; phase P4).
--
-- A SEAL envelope is fresh for 30 s. Each node used to remember the
-- envelopes it had seen in its own memory, so an envelope replayed to a
-- different node inside the window passed. Now the insert of a nonce decides:
-- one row inserted is a fresh envelope, none a replay, on whichever node.
-- The seal-nonce-gc cluster duty deletes the expired rows.
--
-- Forward-only and idempotent.

CREATE TABLE IF NOT EXISTS seal_replay_nonces (
    nonce      TEXT        PRIMARY KEY,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_seal_replay_nonces_expires_at
    ON seal_replay_nonces (expires_at);
