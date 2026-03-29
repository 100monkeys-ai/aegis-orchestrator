CREATE TABLE IF NOT EXISTS admin_audit_log (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    actor_id TEXT NOT NULL,
    action TEXT NOT NULL,
    target_resource TEXT NOT NULL,
    before_state JSONB,
    after_state JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_admin_audit_log_actor ON admin_audit_log (actor_id);
CREATE INDEX idx_admin_audit_log_action ON admin_audit_log (action);
CREATE INDEX idx_admin_audit_log_created_at ON admin_audit_log (created_at);
