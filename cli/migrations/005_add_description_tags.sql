-- 005_add_description_tags.sql
-- Add description and tags as first-class columns on agents; add tags to workflows.

ALTER TABLE agents
    ADD COLUMN IF NOT EXISTS description TEXT,
    ADD COLUMN IF NOT EXISTS tags        TEXT[] NOT NULL DEFAULT '{}';

ALTER TABLE workflows
    ADD COLUMN IF NOT EXISTS tags TEXT[] NOT NULL DEFAULT '{}';

CREATE INDEX IF NOT EXISTS idx_agents_tags   ON agents    USING GIN (tags);
CREATE INDEX IF NOT EXISTS idx_workflows_tags ON workflows USING GIN (tags);

-- Backfill description from manifest_json for existing rows
UPDATE agents
SET    description = manifest_json->'metadata'->>'description'
WHERE  description IS NULL
  AND  manifest_json->'metadata'->>'description' IS NOT NULL;
