-- ============================================================================
-- Rate Limiting (ADR-072)
-- ============================================================================

-- Rate limit overrides for tenant-level and user-level customization.
-- Override hierarchy: ZaruTier defaults < Tenant overrides < User overrides.
CREATE TABLE IF NOT EXISTS rate_limit_overrides (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       VARCHAR(63) REFERENCES tenants(slug) ON DELETE CASCADE,
    user_id         TEXT,
    resource_type   TEXT NOT NULL,
    bucket          TEXT NOT NULL,
    limit_value     BIGINT NOT NULL,
    burst_value     BIGINT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT rate_limit_overrides_scope_check CHECK (
        (tenant_id IS NOT NULL AND user_id IS NULL)
        OR (tenant_id IS NULL AND user_id IS NOT NULL)
    ),
    CONSTRAINT rate_limit_overrides_unique UNIQUE (tenant_id, user_id, resource_type, bucket)
);

CREATE INDEX IF NOT EXISTS idx_rate_limit_overrides_tenant ON rate_limit_overrides(tenant_id)
    WHERE tenant_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_rate_limit_overrides_user ON rate_limit_overrides(user_id)
    WHERE user_id IS NOT NULL;

-- Sliding window counters for hourly/daily/weekly/monthly rate limit windows.
-- PerMinute windows use in-memory token buckets and are NOT stored here.
CREATE TABLE IF NOT EXISTS rate_limit_counters (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope_type      TEXT NOT NULL,
    scope_id        TEXT NOT NULL,
    resource_type   TEXT NOT NULL,
    bucket          TEXT NOT NULL,
    window_start    TIMESTAMPTZ NOT NULL,
    counter         BIGINT NOT NULL DEFAULT 0,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT rate_limit_counters_unique UNIQUE (scope_type, scope_id, resource_type, bucket, window_start)
);

CREATE INDEX IF NOT EXISTS idx_rate_limit_counters_lookup
    ON rate_limit_counters(scope_type, scope_id, resource_type, bucket, window_start);
