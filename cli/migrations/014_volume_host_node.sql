-- Add authoritative host node reference to volumes (ADR-064)
ALTER TABLE volumes ADD COLUMN IF NOT EXISTS host_node_id UUID REFERENCES registered_nodes(node_id);
