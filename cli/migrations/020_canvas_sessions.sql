-- Migration 020: Vibe-Code Canvas Sessions (ADR-106)
--
-- A CanvasSession ties a VibeCode-mode ChatConversation to a workspace
-- volume (ephemeral, persistent, or git-linked via a GitRepoBinding). The
-- Studio (Zaru Client) drives file writes and git operations; this table
-- records the server-side session envelope: which conversation, which
-- volume, which binding (if any), and the lifecycle status.
--
-- The workspace_mode variant + payload is split into two columns:
--   workspace_mode_kind  — discriminant ('Ephemeral' | 'Persistent' | 'GitLinked')
--   workspace_mode_label — the volume_label for Persistent, NULL otherwise
-- The GitLinked payload's binding_id is recovered from git_binding_id.
--
-- Foreign keys:
--   tenant_id           → tenants(slug)             RESTRICT  (tenant is the owner)
--   workspace_volume_id → volumes(id)               SET NULL  (ephemeral cleanup
--                                                              must not cascade
--                                                              to historic rows)
--   git_binding_id      → git_repo_bindings(id)     (no on-delete clause: the
--                                                    binding lifecycle is
--                                                    managed independently)

CREATE TABLE canvas_sessions (
    id                    UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id             TEXT        NOT NULL,
    conversation_id       UUID        NOT NULL,
    workspace_volume_id   UUID        NOT NULL,
    git_binding_id        UUID        NULL,
    workspace_mode_kind   TEXT        NOT NULL,           -- 'Ephemeral' | 'Persistent' | 'GitLinked'
    workspace_mode_label  TEXT        NULL,               -- volume_label for Persistent, NULL otherwise
    status                TEXT        NOT NULL DEFAULT 'Initializing',
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_active_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT fk_canvas_tenant FOREIGN KEY (tenant_id)
        REFERENCES tenants(slug) ON DELETE RESTRICT,
    CONSTRAINT fk_canvas_volume FOREIGN KEY (workspace_volume_id)
        REFERENCES volumes(id) ON DELETE SET NULL,
    CONSTRAINT fk_canvas_git_binding FOREIGN KEY (git_binding_id)
        REFERENCES git_repo_bindings(id)
);

-- Tenant-scoped listing (GET /v1/canvas/sessions)
CREATE INDEX idx_canvas_tenant
    ON canvas_sessions (tenant_id);

-- Conversation → session lookup (1:1 when the conversation is in VibeCode mode)
CREATE INDEX idx_canvas_conversation
    ON canvas_sessions (conversation_id);

-- Reverse lookup from volume → session (for ephemeral cleanup reconciliation)
CREATE INDEX idx_canvas_volume
    ON canvas_sessions (workspace_volume_id);

-- Git-linked sessions only — partial index keeps it narrow.
CREATE INDEX idx_canvas_git_binding
    ON canvas_sessions (git_binding_id)
    WHERE git_binding_id IS NOT NULL;
