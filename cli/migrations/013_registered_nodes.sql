-- Create dedicated registered_nodes table (ADR-061)
-- Separates durable registry (operator-managed) from transient cluster_nodes (runtime state)
CREATE TABLE registered_nodes (
    node_id             UUID        PRIMARY KEY,
    hostname            TEXT        NOT NULL DEFAULT '',
    role                TEXT        NOT NULL,
    software_version    TEXT        NOT NULL DEFAULT '',
    metadata            JSONB       NOT NULL DEFAULT '{}',
    registry_status     TEXT        NOT NULL DEFAULT 'pending',
    current_config_version TEXT,
    registered_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    decommissioned_at   TIMESTAMPTZ,
    CONSTRAINT registered_nodes_status_check CHECK (
        registry_status IN ('pending', 'active', 'decommissioned')
    )
);

CREATE INDEX idx_registered_nodes_status ON registered_nodes (registry_status);

-- Migrate existing registry data from cluster_nodes
INSERT INTO registered_nodes (node_id, hostname, role, software_version, metadata, registry_status, current_config_version, registered_at)
SELECT node_id, hostname, role, software_version, metadata, 'active', current_config_version, registered_at
FROM cluster_nodes
ON CONFLICT DO NOTHING;

-- Drop registry-specific columns from cluster_nodes (now transient-only)
ALTER TABLE cluster_nodes DROP COLUMN IF EXISTS hostname;
ALTER TABLE cluster_nodes DROP COLUMN IF EXISTS software_version;
ALTER TABLE cluster_nodes DROP COLUMN IF EXISTS metadata;
ALTER TABLE cluster_nodes DROP COLUMN IF EXISTS current_config_version;
