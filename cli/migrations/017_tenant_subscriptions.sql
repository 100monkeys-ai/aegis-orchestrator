-- Billing: tenant subscription state linking AEGIS tenants to Stripe.
CREATE TABLE IF NOT EXISTS tenant_subscriptions (
    tenant_id         TEXT PRIMARY KEY REFERENCES tenants(slug) ON DELETE CASCADE,
    stripe_customer_id TEXT NOT NULL,
    stripe_subscription_id TEXT,
    tier              TEXT NOT NULL DEFAULT 'free',
    status            TEXT NOT NULL DEFAULT 'none',
    current_period_end TIMESTAMPTZ,
    cancel_at_period_end BOOLEAN NOT NULL DEFAULT FALSE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_tenant_subscriptions_stripe_customer
    ON tenant_subscriptions(stripe_customer_id);
