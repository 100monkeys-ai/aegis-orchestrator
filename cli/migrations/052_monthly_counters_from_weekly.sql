-- Migration 052: the monthly counter rows the old cleanup deleted are
-- written back from the weekly rows of the same charges (AEGIS known defect
-- `month-below-week-after-fix-2026-10-10`).
--
-- Every charge writes one rate_limit_counters row per bucket in one
-- transaction with one `now`, at window_start = now - window. A weekly row
-- (window 7 days) and its monthly twin (window 30 days) therefore share
-- scope_type, scope_id, resource_type and counter, and the twin's
-- window_start is the weekly row's less 23 days. Until the cleanup was fixed
-- it deleted every row with window_start < now - 35 days, which took each
-- monthly row 5 days after its charge while the weekly row stayed, so the
-- month read below the week.
--
-- For each weekly row, this writes the monthly twin where none exists at its
-- key, and only for a scope and resource that already has a monthly row (a
-- scope whose policy never had a month gains none). 23 days is written as
-- 552 hours so the result does not depend on the session's TimeZone.
--
-- Forward-only and data-only: rows are added, none is changed or removed.
-- Idempotent, like every migration after 033: a second run finds every twin
-- present and writes nothing.

INSERT INTO rate_limit_counters
    (scope_type, scope_id, resource_type, bucket, window_start, counter)
SELECT w.scope_type, w.scope_id, w.resource_type, 'monthly',
       w.window_start - INTERVAL '552 hours', w.counter
FROM rate_limit_counters w
WHERE w.bucket = 'weekly'
  AND NOT EXISTS (
      SELECT 1 FROM rate_limit_counters m
      WHERE m.scope_type = w.scope_type
        AND m.scope_id = w.scope_id
        AND m.resource_type = w.resource_type
        AND m.bucket = 'monthly'
        AND m.window_start = w.window_start - INTERVAL '552 hours'
  )
  AND EXISTS (
      SELECT 1 FROM rate_limit_counters m
      WHERE m.scope_type = w.scope_type
        AND m.scope_id = w.scope_id
        AND m.resource_type = w.resource_type
        AND m.bucket = 'monthly'
  )
ON CONFLICT (scope_type, scope_id, resource_type, bucket, window_start) DO NOTHING;
