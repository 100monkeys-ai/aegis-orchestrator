-- Migration 024: Colony tier cleanup (ADR-111 Phase 2)
--
-- Applies the colony tier model redesign:
--   1. Adds `teams.status` (active|suspended) for Phase 4 suspension gating.
--   2. Destroys any pre-alpha Pro colonies — Pro is now personal-only and
--      cannot own a colony. Pre-alpha policy: no backwards compatibility,
--      no legacy rows preserved.
--   3. Tightens the tier check so only 'business' and 'enterprise' are
--      accepted for new teams going forward.

-- 1. Team lifecycle status column.
ALTER TABLE teams ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
    CHECK (status IN ('active', 'suspended'));
CREATE INDEX idx_teams_status ON teams(status);

-- 2. Destroy all Pro colonies and their supporting rows.
--    Order matters: child rows first, then subscription, tenant, team.
DELETE FROM team_memberships
    WHERE team_id IN (SELECT id FROM teams WHERE tier = 'pro');
DELETE FROM team_invitations
    WHERE team_id IN (SELECT id FROM teams WHERE tier = 'pro');
DELETE FROM tenant_subscriptions
    WHERE tenant_id IN (SELECT tenant_id FROM teams WHERE tier = 'pro');
DELETE FROM tenants
    WHERE slug IN (SELECT tenant_id FROM teams WHERE tier = 'pro');
DELETE FROM teams WHERE tier = 'pro';

-- 3. Tighten the tier CHECK constraint.
--    Migration 022_teams.sql created `teams.tier` as plain TEXT with no
--    named CHECK constraint; `DROP CONSTRAINT IF EXISTS` is a defensive
--    no-op if the constraint was added out-of-band.
ALTER TABLE teams DROP CONSTRAINT IF EXISTS teams_tier_check;
ALTER TABLE teams ADD CONSTRAINT teams_tier_check
    CHECK (tier IN ('business', 'enterprise'));
