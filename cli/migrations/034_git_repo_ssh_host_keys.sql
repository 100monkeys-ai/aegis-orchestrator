-- Migration 034: the SSH host keys of a git repository binding's remote.
--
-- A clone, fetch or push over SSH checks the host's key before it offers
-- a credential. The keys it checks against are the ones given when the
-- binding was created, held here as a JSON array of public key lines
-- (`["ssh-ed25519 AAAA…"]`), or, when the array is empty, the published
-- keys of GitHub, GitLab and Bitbucket, which the orchestrator carries.
--
-- Existing rows get an empty array. An HTTPS binding needs nothing more. An
-- SSH binding to one of those three hosts needs nothing more. An SSH binding
-- to any other host has no key to check against, so its next clone fails
-- with a message saying to add the binding again with its host key.
--
-- Running it again changes nothing.

ALTER TABLE git_repo_bindings
    ADD COLUMN IF NOT EXISTS ssh_host_keys JSONB NOT NULL DEFAULT '[]'::jsonb;
