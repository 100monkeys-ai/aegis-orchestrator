-- Migration 018: Store identity context on API keys for validation endpoint.
-- The MCP server validates API keys via POST /v1/api-keys/validate and needs
-- the owning user's tenant_id, role, and tier without hitting the IAM provider.
ALTER TABLE api_keys ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'zaru-consumer';
ALTER TABLE api_keys ADD COLUMN aegis_role TEXT;
ALTER TABLE api_keys ADD COLUMN zaru_tier TEXT;

CREATE INDEX idx_api_keys_key_hash ON api_keys(key_hash) WHERE status = 'active';
