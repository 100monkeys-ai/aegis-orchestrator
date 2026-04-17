-- ADR-111: Team Tenants & Per-Seat Billing
--
-- Adds the `teams`, `team_memberships`, and `team_invitations` tables that
-- back the `TeamTenant` / `Membership` / `TeamInvitation` aggregates in
-- `orchestrator/core/src/domain/team.rs`, and extends `tenant_subscriptions`
-- with a `seat_count` column driven by `BillingService::sync_seats`.

CREATE TABLE teams (
    id UUID PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    owner_user_id TEXT NOT NULL,
    tier TEXT NOT NULL,
    tenant_id TEXT NOT NULL REFERENCES tenants(slug) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_teams_owner_user_id ON teams(owner_user_id);

CREATE TABLE team_memberships (
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    role TEXT NOT NULL,
    status TEXT NOT NULL,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at TIMESTAMPTZ NULL,
    PRIMARY KEY (team_id, user_id)
);
CREATE INDEX idx_team_memberships_user_status ON team_memberships(user_id, team_id, status);

CREATE TABLE team_invitations (
    id UUID PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    invitee_email TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    invited_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    accepted_at TIMESTAMPTZ NULL,
    cancelled_at TIMESTAMPTZ NULL
);
CREATE INDEX idx_team_invitations_team_status ON team_invitations(team_id, status);

ALTER TABLE tenant_subscriptions ADD COLUMN seat_count INT NOT NULL DEFAULT 1;
