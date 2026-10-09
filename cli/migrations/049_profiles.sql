-- Migration 049: profiles (AEGIS ADR-140 D1, D4).
--
-- profiles holds a person's own named profile: who owns it (tenant and
-- sub), its name (unique for its owner ignoring case among the profiles not
-- deleted), its allow-list of tool patterns (a JSON array of strings, empty
-- for every tool the security context admits), and its defaults: a
-- repository binding, a Nuclear Notes workspace and instructions. A deleted
-- profile keeps its row with deleted_at set.
--
-- profile_bindings holds the profile's credential bindings in order. A
-- binding is not a foreign key: a binding revoked or deleted later stays
-- here, and a read answers it as removed (D4).
--
-- Forward-only and additive: two new tables, no existing row touched.
-- Idempotent, like every migration after 033: running it again over a
-- migrated schema changes nothing.

CREATE TABLE IF NOT EXISTS profiles (
    id                     UUID PRIMARY KEY,
    tenant_id              TEXT NOT NULL,
    user_sub               TEXT NOT NULL,
    name                   TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 64),
    tools                  JSONB NOT NULL DEFAULT '[]'::jsonb,
    repository_binding_id  UUID NULL,
    notes_workspace        TEXT NULL,
    instructions           TEXT NULL CHECK (instructions IS NULL OR char_length(instructions) <= 4000),
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at             TIMESTAMPTZ NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS profiles_owner_name_unique
    ON profiles (tenant_id, user_sub, lower(name))
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS profiles_owner
    ON profiles (tenant_id, user_sub)
    WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS profile_bindings (
    profile_id  UUID NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    binding_id  UUID NOT NULL,
    position    INTEGER NOT NULL,
    PRIMARY KEY (profile_id, binding_id)
);
