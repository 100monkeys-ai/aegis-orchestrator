-- Migration 039: goals and their evaluations (AEGIS ADR-131 D1, Update U1).
--
-- goals holds the user's request, stated once, to which every execution
-- started for it is bound: its owner (tenant and sub), the caller's opaque
-- reference (for Zaru Web the conversation id), its state and the
-- continuations granted so far (D1).
--
-- goal_evaluations holds every evaluation of a goal (U1): the round it
-- judged, the goal-judge execution (attempt 2 is the one re-run after a
-- judge fault, ADR-017's Update of 2026-10-01), the verdict, the outcome
-- and the answer the orchestrator gave. A round is decided by at most one
-- evaluation: one with an outcome and no waiting_on (U2, U3); the partial
-- unique index holds that against two racing calls.
--
-- executions and workflow_executions gain a nullable goal_id: the goal the
-- execution was started for (D1, U6). An intent pipeline is a workflow
-- execution.
--
-- Forward-only and additive: two new tables and two nullable columns, no
-- existing row touched. Idempotent, like every migration after 033: running
-- it again over a migrated schema changes nothing.

CREATE TABLE IF NOT EXISTS goals (
    id          UUID PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    user_sub    TEXT NOT NULL,
    statement   TEXT NOT NULL CHECK (char_length(statement) <= 32768),
    client_ref  TEXT NOT NULL,
    channel     TEXT NOT NULL CHECK (channel IN ('web', 'api')),
    state       TEXT NOT NULL CHECK (state IN
                    ('open', 'met', 'cannot_be_met', 'exhausted', 'expired', 'superseded')),
    rounds      INTEGER NOT NULL DEFAULT 0,
    created_at  TIMESTAMPTZ NOT NULL,
    closed_at   TIMESTAMPTZ NULL
);

CREATE INDEX IF NOT EXISTS idx_goals_open_client_ref
    ON goals (tenant_id, user_sub, client_ref)
    WHERE state = 'open';

CREATE INDEX IF NOT EXISTS idx_goals_open_created_at
    ON goals (created_at)
    WHERE state = 'open';

CREATE TABLE IF NOT EXISTS goal_evaluations (
    id                  UUID PRIMARY KEY,
    goal_id             UUID NOT NULL REFERENCES goals(id) ON DELETE CASCADE,
    round               INTEGER NOT NULL,
    attempt             INTEGER NOT NULL CHECK (attempt IN (1, 2)),
    judge_execution_id  UUID NULL,
    companion_answer    TEXT NOT NULL,
    verdict             JSONB NULL,
    outcome             TEXT NULL CHECK (outcome IN ('met', 'not_met', 'cannot_be_met')),
    continue            BOOLEAN NOT NULL DEFAULT FALSE,
    waiting_on          JSONB NULL,
    answer              JSONB NULL,
    created_at          TIMESTAMPTZ NOT NULL,
    decided_at          TIMESTAMPTZ NULL
);

CREATE INDEX IF NOT EXISTS idx_goal_evaluations_goal_round
    ON goal_evaluations (goal_id, round, created_at);

CREATE UNIQUE INDEX IF NOT EXISTS uq_goal_evaluations_decided_round
    ON goal_evaluations (goal_id, round)
    WHERE outcome IS NOT NULL AND waiting_on IS NULL;

ALTER TABLE executions ADD COLUMN IF NOT EXISTS goal_id UUID NULL;
CREATE INDEX IF NOT EXISTS idx_executions_goal_id
    ON executions (goal_id)
    WHERE goal_id IS NOT NULL;

ALTER TABLE workflow_executions ADD COLUMN IF NOT EXISTS goal_id UUID NULL;
CREATE INDEX IF NOT EXISTS idx_workflow_executions_goal_id
    ON workflow_executions (goal_id)
    WHERE goal_id IS NOT NULL;
