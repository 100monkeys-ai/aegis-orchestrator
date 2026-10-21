-- Migration 056: the data steps that have run (AEGIS ADR-141, Update of
-- 2026-10-21, clause 13).
--
-- A data step changes stored rows once, at the daemon's start, in Rust
-- (a manifest's YAML needs a parse to change). It records its name here in
-- the transaction that changes the rows, so it never runs twice: a second
-- start, or a second process starting at once, finds the name and changes
-- nothing. The first step is `two_tries_bound_floor`.
--
-- Forward-only and idempotent, like every migration after 033: running it
-- again over a migrated schema changes nothing.

CREATE TABLE IF NOT EXISTS data_steps (
    name   TEXT PRIMARY KEY,
    ran_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
