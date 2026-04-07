-- Migration 012: Add owner_user_id column to volumes for efficient user-scoped queries
-- Extracts Persistent volume owner from ownership JSONB into a dedicated indexed column.

ALTER TABLE volumes ADD COLUMN IF NOT EXISTS owner_user_id TEXT;

-- Backfill existing Persistent ownership records
UPDATE volumes
SET owner_user_id = ownership->>'owner'
WHERE ownership->>'type' = 'persistent'
  AND owner_user_id IS NULL;

CREATE INDEX IF NOT EXISTS idx_volumes_tenant_owner
    ON volumes(tenant_id, owner_user_id)
    WHERE owner_user_id IS NOT NULL;
