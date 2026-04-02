-- Migration 007: Add scope/ownership model to agents (ADR-076 mirror)

ALTER TABLE agents
  ADD COLUMN scope TEXT NOT NULL DEFAULT 'tenant',
  ADD COLUMN owner_user_id TEXT NULL;

ALTER TABLE agents
  DROP CONSTRAINT agents_tenant_name_unique;

ALTER TABLE agents
  ADD CONSTRAINT agents_scope_check CHECK (scope IN ('global', 'tenant', 'user')),
  ADD CONSTRAINT agents_user_scope_requires_owner CHECK (
    (scope = 'user' AND owner_user_id IS NOT NULL) OR
    (scope != 'user' AND owner_user_id IS NULL)
  ),
  ADD CONSTRAINT agents_global_requires_system_tenant CHECK (
    scope != 'global' OR tenant_id = 'aegis-system'
  );

CREATE UNIQUE INDEX idx_agents_scope_unique
  ON agents (tenant_id, name, version, scope, COALESCE(owner_user_id, ''));

CREATE INDEX idx_agents_scope ON agents(scope);
CREATE INDEX idx_agents_owner ON agents(owner_user_id) WHERE owner_user_id IS NOT NULL;
