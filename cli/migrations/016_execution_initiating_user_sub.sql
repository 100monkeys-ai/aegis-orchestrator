-- Migration 016: Add initiating_user_sub column to executions table
--
-- Stores the `sub` claim of the JWT-authenticated user who initiated an
-- execution via POST /v1/stimuli. Used by the dispatch gateway to reconstruct
-- a minimal UserIdentity for user-scoped rate limiting (ADR-072) when the
-- agent runtime calls back into the orchestrator with no user context.

ALTER TABLE executions
    ADD COLUMN IF NOT EXISTS initiating_user_sub TEXT;
