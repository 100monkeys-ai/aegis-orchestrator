-- ADR-041: Identity realms table for dynamic OIDC realm configuration.
-- Stores IdentityRealm aggregate roots for discovery by StandardIamService.

CREATE TABLE IF NOT EXISTS identity_realms (
    slug        VARCHAR(63)  PRIMARY KEY,
    issuer_url  TEXT         NOT NULL,
    jwks_uri    TEXT         NOT NULL,
    audience    VARCHAR(255) NOT NULL,
    realm_kind  VARCHAR(255) NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_identity_realms_kind ON identity_realms(realm_kind);
