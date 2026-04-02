-- Agent version history: append-only log of all deployed versions
CREATE TABLE IF NOT EXISTS agent_versions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    version TEXT NOT NULL,
    manifest_yaml TEXT NOT NULL,
    manifest_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT agent_versions_unique UNIQUE (agent_id, version)
);

CREATE INDEX IF NOT EXISTS idx_agent_versions_agent_id ON agent_versions(agent_id);
CREATE INDEX IF NOT EXISTS idx_agent_versions_created ON agent_versions(created_at DESC);
