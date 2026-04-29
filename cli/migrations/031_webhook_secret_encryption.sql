-- Migration 031: Encrypt webhook_secret at rest (Audit 002 §4.37.13)
--
-- The `git_repo_bindings.webhook_secret` column stored the cleartext webhook
-- secret. While the URL routing layer carries the cleartext on legitimate
-- webhook delivery (verified by HMAC over the request body), storing that
-- cleartext at rest exposes every binding to a single DB-only compromise
-- (DBA snooping, backup leakage, SQL injection in an unrelated query).
--
-- Replace the cleartext column with two new columns:
--
--   webhook_secret_ciphertext  — OpenBao Transit ciphertext of the
--                                cleartext webhook secret. Decrypted on
--                                each verification call. Never indexed.
--   webhook_lookup_hash        — HMAC-SHA256(cleartext, fixed key) used
--                                as a deterministic lookup index. Replaces
--                                the previous `webhook_secret = $1` query.
--                                The HMAC hash is irreversible without
--                                the key, so DB-only access cannot recover
--                                the cleartext from the index.
--
-- Pre-alpha: no backward-compatibility shim. Existing rows are dropped.

ALTER TABLE git_repo_bindings DROP COLUMN webhook_secret;

ALTER TABLE git_repo_bindings
    ADD COLUMN webhook_secret_ciphertext TEXT NULL,
    ADD COLUMN webhook_lookup_hash       TEXT NULL;

DROP INDEX IF EXISTS idx_grb_webhook;

-- Replace the secret-cleartext index with a hash index. The hash is a
-- 64-char hex SHA-256 digest, indexed only when present (auto-refresh
-- enabled bindings).
CREATE INDEX idx_grb_webhook_hash
    ON git_repo_bindings (webhook_lookup_hash)
    WHERE webhook_lookup_hash IS NOT NULL;
