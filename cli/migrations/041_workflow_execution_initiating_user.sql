-- The subject of the person who started a workflow execution, recorded when
-- it starts, so the agents its states run act for that person and never for
-- the Temporal worker's service account (AEGIS ADR-132's Update, G2). Set
-- once and never cleared. NULL on a system-started workflow and on rows
-- written before this migration: such a run has no acting user.
ALTER TABLE workflow_executions ADD COLUMN IF NOT EXISTS initiating_user_sub TEXT;
