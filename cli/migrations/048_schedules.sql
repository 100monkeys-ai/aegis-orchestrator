-- Migration 048: schedules and their fires (AEGIS ADR-139 N1, N6, N7, N9).
--
-- schedules holds a person's own schedule: who owns it (the identity's
-- projection, as api_keys keeps it since migration 018, written again from
-- the owner's live token at every create, update and resume), what it runs
-- (an agent or a workflow, by the name or UUID the starting tool takes, with
-- its intent, input, attachments, repositories and contexts), when (exactly
-- one of a single instant `at` or a recurrence: a five-field cron, an IANA
-- time zone and a jitter), its state, why it paused itself when it did, and
-- the id of the Temporal Schedule that backs it. A deleted schedule keeps
-- its row with deleted_at set, so its runs keep their schedule_id.
--
-- schedule_fires holds one row per (schedule, scheduled time): a repeated
-- fire answers with the first one's row (N6). Its outcome is `starting`
-- while the fire is being decided, then `started`, `refused`,
-- `skipped_paused` or `skipped_overlap` (N7).
--
-- executions, workflow_executions and tool_approval_requests gain a
-- nullable schedule_id: the schedule that started the run, or that the
-- gated call's run was started by (N7, N9).
--
-- Forward-only and additive: two new tables and three nullable columns, no
-- existing row touched. Idempotent, like every migration after 033: running
-- it again over a migrated schema changes nothing.

CREATE TABLE IF NOT EXISTS schedules (
    id                    UUID PRIMARY KEY,
    tenant_id             TEXT NOT NULL,
    owner_sub             TEXT NOT NULL,
    owner_realm           TEXT NOT NULL,
    owner_kind            TEXT NOT NULL CHECK (owner_kind IN ('consumer_user', 'tenant_user')),
    owner_zaru_tier       TEXT NULL,
    name                  TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 80),
    target_kind           TEXT NOT NULL CHECK (target_kind IN ('agent', 'workflow')),
    target                TEXT NOT NULL,
    target_version        TEXT NULL,
    intent                TEXT NULL,
    input                 JSONB NOT NULL DEFAULT '{}'::jsonb,
    attachments           JSONB NOT NULL DEFAULT '[]'::jsonb,
    repositories          JSONB NULL,
    contexts              JSONB NULL,
    run_at                TIMESTAMPTZ NULL,
    cron                  TEXT NULL,
    timezone              TEXT NULL,
    jitter_seconds        INTEGER NULL CHECK (jitter_seconds IS NULL OR jitter_seconds >= 0),
    state                 TEXT NOT NULL CHECK (state IN ('active', 'paused', 'completed')),
    paused_reason         TEXT NULL,
    temporal_schedule_id  TEXT NOT NULL,
    created_at            TIMESTAMPTZ NOT NULL,
    updated_at            TIMESTAMPTZ NOT NULL,
    deleted_at            TIMESTAMPTZ NULL,
    CONSTRAINT schedules_one_timing CHECK (
        (run_at IS NOT NULL AND cron IS NULL AND timezone IS NULL AND jitter_seconds IS NULL)
        OR (run_at IS NULL AND cron IS NOT NULL AND timezone IS NOT NULL AND jitter_seconds IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_schedules_owner
    ON schedules (owner_sub, created_at DESC)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_schedules_tenant
    ON schedules (tenant_id, created_at DESC)
    WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS schedule_fires (
    id              UUID PRIMARY KEY,
    schedule_id     UUID NOT NULL REFERENCES schedules (id),
    scheduled_time  TIMESTAMPTZ NOT NULL,
    fired_at        TIMESTAMPTZ NOT NULL,
    outcome         TEXT NOT NULL CHECK (outcome IN
                        ('starting', 'started', 'refused', 'skipped_paused', 'skipped_overlap')),
    execution_id    UUID NULL,
    detail          TEXT NULL,
    CONSTRAINT uq_schedule_fires_scheduled_time UNIQUE (schedule_id, scheduled_time)
);

CREATE INDEX IF NOT EXISTS idx_schedule_fires_newest
    ON schedule_fires (schedule_id, scheduled_time DESC);

ALTER TABLE executions ADD COLUMN IF NOT EXISTS schedule_id UUID NULL;
ALTER TABLE workflow_executions ADD COLUMN IF NOT EXISTS schedule_id UUID NULL;
ALTER TABLE tool_approval_requests ADD COLUMN IF NOT EXISTS schedule_id UUID NULL;
