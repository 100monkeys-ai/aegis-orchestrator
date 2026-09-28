-- Migration 033: store a digest of the team invitation token, never the token.
--
-- `team_invitations.token_hash` held the invitation token itself (the hex
-- HMAC-SHA256 over the team id and the invitee's email), so anyone who could
-- read the table could read a token that accepts the invitation. From this
-- migration on the column holds `sha256:` followed by the 64 lowercase hex
-- digits of SHA-256 over the token's text. The daemon finds an invitation by
-- the digest of the token it is shown; the token itself is stored nowhere.
--
-- Every stored row is converted in place, in the migration's transaction.
-- A converted row starts with `sha256:`. A token is 64 hex digits and never
-- does, so running this again changes nothing. The token a person holds is
-- the same before and after, so an invitation link already sent still works.
--
-- The CHECK constraint refuses any value that is not a digest in this form,
-- so a token cannot be written to the column again.

UPDATE team_invitations
SET token_hash = 'sha256:' || encode(sha256(convert_to(token_hash, 'UTF8')), 'hex')
WHERE token_hash NOT LIKE 'sha256:%';

ALTER TABLE team_invitations
    DROP CONSTRAINT IF EXISTS team_invitations_token_hash_is_digest;

ALTER TABLE team_invitations
    ADD CONSTRAINT team_invitations_token_hash_is_digest
    CHECK (token_hash ~ '^sha256:[0-9a-f]{64}$');
