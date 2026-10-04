-- Migration 037: the operator escalation's durable store (AEGIS ADR-129).
--
-- operator_escalation_codes holds every code minted by the operator web
-- interface's stepped-up session (D13): the SHA-256 of the six digits, never
-- the digits (D9); the consumer-realm sub that alone may redeem it, with the
-- system sub and role of the minting token (D10); its validity (D7), its
-- single use and its failed-attempt count (D8).
--
-- operator_escalations holds every escalation, held by one API key (D10,
-- D14), with its end and why it ended (D19).
--
-- Every act is appended to admin_audit_log (migration 003; D18); nothing
-- here changes that table.
--
-- Forward-only and additive: two new tables, no existing row touched.
-- Idempotent, like every migration after 033.

CREATE TABLE IF NOT EXISTS operator_escalation_codes (
    id               UUID PRIMARY KEY,
    code_hash        TEXT NOT NULL,
    consumer_sub     TEXT NOT NULL,
    system_sub       TEXT NOT NULL,
    aegis_role       TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL,
    expires_at       TIMESTAMPTZ NOT NULL,
    consumed_at      TIMESTAMPTZ NULL,
    failed_attempts  INTEGER NOT NULL DEFAULT 0,
    invalidated_at   TIMESTAMPTZ NULL
);

CREATE INDEX IF NOT EXISTS idx_operator_escalation_codes_consumer
    ON operator_escalation_codes (consumer_sub, created_at DESC);

CREATE TABLE IF NOT EXISTS operator_escalations (
    id            UUID PRIMARY KEY,
    api_key_id    UUID NOT NULL,
    consumer_sub  TEXT NOT NULL,
    system_sub    TEXT NOT NULL,
    aegis_role    TEXT NOT NULL,
    code_id       UUID NOT NULL REFERENCES operator_escalation_codes (id),
    started_at    TIMESTAMPTZ NOT NULL,
    expires_at    TIMESTAMPTZ NOT NULL,
    ended_at      TIMESTAMPTZ NULL,
    end_reason    TEXT NULL
);

CREATE INDEX IF NOT EXISTS idx_operator_escalations_active_key
    ON operator_escalations (api_key_id)
    WHERE ended_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_operator_escalations_active_system_sub
    ON operator_escalations (system_sub)
    WHERE ended_at IS NULL;
