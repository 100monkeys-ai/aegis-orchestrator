-- Migration 051: a schedule's run asked for now has no scheduled time
-- (AEGIS ADR-139, Update of 2026-10-10: a schedule runs now on the person's
-- word).
--
-- schedule_fires.scheduled_time becomes nullable. A fire Temporal starts
-- keeps its scheduled time and stays decided once per (schedule, scheduled
-- time); a run the owner asks for now is recorded among the same fires with
-- a null scheduled time, which the unique constraint never matches, so
-- every such press is its own row.
--
-- Forward-only and additive: one NOT NULL dropped, no row touched.
-- Idempotent, like every migration after 033: dropping NOT NULL from a
-- nullable column changes nothing.

ALTER TABLE schedule_fires ALTER COLUMN scheduled_time DROP NOT NULL;
