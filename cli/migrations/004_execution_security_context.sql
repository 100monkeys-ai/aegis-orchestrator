-- ADR-083: Execution Security Context Propagation
-- Bind security context at execution creation time instead of SMCP session fallback.

ALTER TABLE executions
  ADD COLUMN security_context_name TEXT NOT NULL DEFAULT 'MIGRATION_PLACEHOLDER';

UPDATE executions
  SET security_context_name = 'aegis-system-operator'
  WHERE security_context_name = 'MIGRATION_PLACEHOLDER';

ALTER TABLE executions
  ALTER COLUMN security_context_name DROP DEFAULT;

CREATE INDEX idx_executions_security_context_name ON executions(security_context_name);
