-- Migration 023: Add name and archived fields to canvas_sessions
--
-- Supports session naming (user-supplied or auto-generated from first message)
-- and soft-archiving so terminated / stale sessions can be hidden from the
-- default list view while remaining available for historical reference.

ALTER TABLE canvas_sessions ADD COLUMN IF NOT EXISTS name TEXT;
ALTER TABLE canvas_sessions ADD COLUMN IF NOT EXISTS archived BOOLEAN DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_canvas_sessions_archived
    ON canvas_sessions (tenant_id, archived);
