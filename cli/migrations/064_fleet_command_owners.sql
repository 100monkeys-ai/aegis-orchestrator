-- Migration 064: which cluster member runs each fleet command (AEGIS
-- ADR-152 D4.7; phase P4).
--
-- A fleet command's per-edge tasks live in the memory of the member that
-- started it; a cancel received by another member forwards to that member.
-- A row is written when the command starts and deleted when it ends.
--
-- Forward-only and idempotent.

CREATE TABLE IF NOT EXISTS fleet_command_owners (
    fleet_command_id UUID        PRIMARY KEY,
    member_node_id   UUID        NOT NULL,
    tenant_id        TEXT        NOT NULL,
    started_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
