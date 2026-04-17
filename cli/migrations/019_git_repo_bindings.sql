-- Migration 019: Git Repository Bindings (ADR-081)
--
-- First-class mapping of a user's git repository to an AEGIS volume. A
-- GitRepoBinding references an optional UserCredentialBinding (for private
-- repos, migration 011) and a Volume (migration 001) that ultimately holds
-- the cloned working tree.
--
-- The orchestrator clones into the volume asynchronously via libgit2 (default)
-- or the EphemeralCliTool fallback (ADR-053) — the agent never executes
-- `git clone` and never sees credentials (Keymaster Pattern, ADR-034).

CREATE TABLE git_repo_bindings (
    id                    UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id             TEXT        NOT NULL,
    credential_binding_id UUID        NULL,
    repo_url              TEXT        NOT NULL,
    git_ref_type          TEXT        NOT NULL,           -- 'branch' | 'tag' | 'commit'
    git_ref_value         TEXT        NOT NULL,           -- 'main' | 'v1.0.0' | 'abc123…'
    sparse_paths          JSONB       NULL,               -- ["src/", "lib/"]
    volume_id             UUID        NOT NULL,
    label                 TEXT        NOT NULL,
    status                TEXT        NOT NULL DEFAULT 'Pending',
    status_error          TEXT        NULL,               -- populated when status = 'Failed'
    clone_strategy        TEXT        NOT NULL DEFAULT 'Libgit2',
    last_cloned_at        TIMESTAMPTZ NULL,
    last_commit_sha       TEXT        NULL,
    auto_refresh          BOOLEAN     NOT NULL DEFAULT FALSE,
    webhook_secret        TEXT        NULL,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Foreign keys
    CONSTRAINT fk_tenant FOREIGN KEY (tenant_id)
        REFERENCES tenants(slug) ON DELETE RESTRICT,
    CONSTRAINT fk_volume FOREIGN KEY (volume_id)
        REFERENCES volumes(id) ON DELETE CASCADE
);

-- Tenant-scoped listing and count_by_owner (tier limit enforcement)
CREATE INDEX idx_grb_tenant
    ON git_repo_bindings (tenant_id);

-- Reverse lookup from volume → binding (1:1)
CREATE INDEX idx_grb_volume
    ON git_repo_bindings (volume_id);

-- Webhook routing (ADR-081 Phase 4). Partial index — only bindings with
-- auto-refresh enabled populate `webhook_secret`.
CREATE INDEX idx_grb_webhook
    ON git_repo_bindings (webhook_secret)
    WHERE webhook_secret IS NOT NULL;
