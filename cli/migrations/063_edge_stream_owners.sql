-- Migration 063: which cluster member holds each edge daemon's stream
-- (AEGIS ADR-152 D8; phase P5).
--
-- Every member runs a Relay Coordinator replica, and an edge daemon's
-- ConnectEdge stream lands on one of them. A tool call for that edge, made
-- by an execution on any member, is sent to the member whose replica holds
-- the stream. A row is written when a stream opens and deleted when it
-- closes, only if it is still that stream, so a late close never removes a
-- newer stream's row.
--
-- Forward-only and idempotent.

CREATE TABLE IF NOT EXISTS edge_stream_owners (
    edge_node_id   UUID        PRIMARY KEY,
    member_node_id UUID        NOT NULL,
    stream_id      UUID        NOT NULL,
    connected_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_edge_stream_owners_member
    ON edge_stream_owners (member_node_id);
