-- Migration 007: Add scope model to agents (ADR-076 / ADR-097 two-level model)

ALTER TABLE agents
  ADD COLUMN scope TEXT NOT NULL DEFAULT 'tenant';

ALTER TABLE agents
  DROP CONSTRAINT agents_tenant_name_unique;

ALTER TABLE agents
  ADD CONSTRAINT agents_scope_check CHECK (scope IN ('global', 'tenant')),
  ADD CONSTRAINT agents_global_requires_system_tenant CHECK (
    scope != 'global' OR tenant_id = 'aegis-system'
  );

CREATE UNIQUE INDEX idx_agents_scope_unique
  ON agents (tenant_id, name, version, scope);

CREATE INDEX idx_agents_scope ON agents(scope);
