-- Anti-fragile consumer billing identity.
--
-- Consumer billing now keys on the immutable Keycloak `sub` UUID rather than
-- the mutable `tenant_id`. When a user's JWT claims drift (e.g. `tenant_id`
-- missing then falling back to shared `zaru-consumer`, then later resolving
-- to `u-{hex}`), the previous design created duplicate Stripe customers. By
-- anchoring the identity on `user_sub` and using `Customer::search` over
-- Stripe metadata as the authoritative lookup, one user maps to exactly one
-- Stripe customer — by construction.
--
-- Team-owned subscriptions continue to key on `tenant_id` and leave
-- `user_sub` NULL; the partial UNIQUE index only applies to consumer rows.

ALTER TABLE tenant_subscriptions
    ADD COLUMN user_sub TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_tenant_subscriptions_user_sub_unique
    ON tenant_subscriptions(user_sub)
    WHERE user_sub IS NOT NULL;
