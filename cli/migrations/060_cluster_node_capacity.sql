-- Migration 060: each cluster node's advertised capacity (AEGIS ADR-152
-- D5.3; phase P2).
--
-- A new execution starts on the node that received it. When that node
-- already runs as many as it advertises here, the execution goes to the
-- live peer with the most headroom: its max_executions less the unfinished
-- executions it owns (executions.owner_node_id, migration 059). A node with
-- no capacity (NULL) is never chosen and never forwards.
--
-- Forward-only and idempotent; no UPDATE or DELETE (pg_strict).

ALTER TABLE cluster_nodes ADD COLUMN IF NOT EXISTS max_executions INTEGER;
