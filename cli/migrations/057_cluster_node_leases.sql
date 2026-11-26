-- Migration 057: membership leases and incarnations on cluster_nodes
-- (AEGIS ADR-152 D1; phase P1).
--
-- Every node of an active-active cluster is a peer. At each process start a
-- node increments its `incarnation`, which identifies that process
-- (inserting its row first when it has none), and renews
-- `lease_expires_at` every few seconds by the database clock: the lease is
-- the arbiter of ownership. `last_renewed_at` is the database's time of the
-- last renewal; `vrack_address` is where peers probe the node over the
-- private network.
--
-- Forward-only and idempotent: existing rows keep every value they had and
-- start at incarnation 0 with no lease; running this again over a migrated
-- schema changes nothing. Every UPDATE the membership code issues carries a
-- WHERE clause (pg_strict) and none needs a session, so all of it runs
-- through PgBouncer in transaction mode.

ALTER TABLE cluster_nodes ADD COLUMN IF NOT EXISTS incarnation BIGINT NOT NULL DEFAULT 0;
ALTER TABLE cluster_nodes ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ;
ALTER TABLE cluster_nodes ADD COLUMN IF NOT EXISTS last_renewed_at TIMESTAMPTZ;
ALTER TABLE cluster_nodes ADD COLUMN IF NOT EXISTS vrack_address TEXT;
