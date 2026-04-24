-- Tier + quota state lives on tenant_subscriptions. Remove the duplicate
-- columns from tenants to prevent drift.
ALTER TABLE tenants DROP CONSTRAINT IF EXISTS tenants_tier_check;
ALTER TABLE tenants DROP COLUMN IF EXISTS tier;
ALTER TABLE tenants DROP COLUMN IF EXISTS max_concurrent_executions;
ALTER TABLE tenants DROP COLUMN IF EXISTS max_agents;
ALTER TABLE tenants DROP COLUMN IF EXISTS max_storage_gb;
