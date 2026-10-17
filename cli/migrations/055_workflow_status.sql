-- Migration 055: a workflow is active or archived (AEGIS ADR-148,
-- clauses A9 and A10).
--
-- workflows gains one column:
--   status  'active' or 'archived'. The live lists and finds read active
--           rows; an archived row stays readable by its id with its
--           versions and executions, and a new run naming it is refused.
--           Agents already carry their status (001_initial.sql).
--
-- Each global (built-in) name keeps one active row: the newest by
-- created_at of its (tenant_id, name, scope), the id breaking a tie. Every
-- older global row is archived by UPDATE first, then the partial unique
-- index holds the invariant from here on (A9). Nothing is deleted: a
-- workflow execution cascades from its workflow, so a DELETE would take
-- the executions with it. Tenant rows are left as they are; a tenant's
-- versions all stay active until the tenant archives one.
--
-- Forward-only and idempotent, like every migration after 033: running it
-- again over a migrated schema changes nothing.

ALTER TABLE workflows
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'active'
    CONSTRAINT workflows_status_check CHECK (status IN ('active', 'archived'));

UPDATE workflows AS older
SET status = 'archived'
WHERE older.scope = 'global'
  AND older.status = 'active'
  AND EXISTS (
      SELECT 1
      FROM workflows AS newer
      WHERE newer.tenant_id = older.tenant_id
        AND newer.name = older.name
        AND newer.scope = older.scope
        AND (
            newer.created_at > older.created_at
            OR (newer.created_at = older.created_at AND newer.id > older.id)
        )
  );

CREATE UNIQUE INDEX IF NOT EXISTS idx_workflows_one_active_global
    ON workflows (tenant_id, name, scope)
    WHERE scope = 'global' AND status = 'active';

CREATE INDEX IF NOT EXISTS idx_workflows_name_status
    ON workflows (tenant_id, name, scope, status);
