-- Migration 058: cluster_duties, singleton background duties held under a
-- lease with a fencing epoch (AEGIS ADR-152 D2; phase P1).
--
-- One row per duty (an expiry sweep, the health sweep, a reconcile). Acquire
-- and renew are one UPDATE that succeeds only when the lease has run out by
-- the database clock, the duty is unheld, or this very process holds it.
-- `epoch` is the fencing token: incremented every time the duty changes
-- hands and never on a renewal, so a write conditioned on a superseded
-- epoch affects no row. A node creates a duty's row the first time it runs
-- the duty (INSERT ... ON CONFLICT DO NOTHING), so a new duty needs no
-- migration.
--
-- Forward-only and idempotent: running this again changes nothing. Every
-- UPDATE the duty code issues carries a WHERE clause (pg_strict), and none
-- needs a session (PgBouncer, transaction mode).

CREATE TABLE IF NOT EXISTS cluster_duties (
    duty               TEXT        PRIMARY KEY,
    holder_node_id     UUID,
    holder_incarnation BIGINT,
    epoch              BIGINT      NOT NULL DEFAULT 0,
    lease_expires_at   TIMESTAMPTZ
);
