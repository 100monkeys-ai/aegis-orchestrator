-- Migration 011: Credential bindings, grants, OAuth pending states (ADR-078)
--
-- NOTE: user_provider_keys does not exist in the aegis-orchestrator database.
-- It lives in the zaru-client database (a separate bounded context).
-- No legacy data migration is required here.

CREATE TABLE credential_bindings (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_user_id       TEXT        NOT NULL,
    tenant_id           TEXT        NOT NULL,
    credential_type     TEXT        NOT NULL,
    provider            TEXT        NOT NULL,
    label               TEXT        NOT NULL,
    secret_path         TEXT        NOT NULL,
    scope               TEXT        NOT NULL DEFAULT 'personal',
    scope_team_id       UUID        NULL,
    status              TEXT        NOT NULL DEFAULT 'active',
    oauth_scopes        TEXT[]      NULL,
    external_account_id TEXT        NULL,
    service_url         TEXT        NULL,
    tags                JSONB       NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE credential_grants (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    binding_id   UUID        NOT NULL REFERENCES credential_bindings(id) ON DELETE CASCADE,
    target_type  TEXT        NOT NULL,
    target_value TEXT        NOT NULL,
    granted_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    granted_by   TEXT        NOT NULL
);

CREATE TABLE oauth_pending_states (
    state          TEXT        PRIMARY KEY,
    binding_id     UUID        NOT NULL REFERENCES credential_bindings(id) ON DELETE CASCADE,
    pkce_verifier  TEXT        NOT NULL,
    redirect_uri   TEXT        NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_credential_bindings_owner    ON credential_bindings (tenant_id, owner_user_id);
CREATE INDEX idx_credential_bindings_provider ON credential_bindings (tenant_id, provider, status);
CREATE INDEX idx_credential_grants_binding    ON credential_grants (binding_id);
CREATE INDEX idx_oauth_pending_states_ttl     ON oauth_pending_states (created_at);
