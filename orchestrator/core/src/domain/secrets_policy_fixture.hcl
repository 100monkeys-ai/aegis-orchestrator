path "secret/data/aegis/*" {
  capabilities = ["create", "read", "update", "delete", "list"]
}
path "secret/metadata/aegis/*" {
  capabilities = ["list", "read", "delete"]
}
path "tenant-+/kv/data/*" {
  capabilities = ["create", "read", "update", "delete", "list"]
}
path "tenant-+/kv/metadata/*" {
  capabilities = ["list", "read", "delete"]
}
# A person's tenant (u-<hex>) keeps its credentials in its realm's mount,
# tenant-zaru-consumer/kv, at users/<tenant>/<user>/credentials/<id>
# (AEGIS ADR-056, ADR-125 Update). "+" matches only a whole path segment,
# so the two rules above admit no tenant-<slug> mount.
path "tenant-zaru-consumer/kv/data/users/*" {
  capabilities = ["create", "read", "update", "delete", "list"]
}
path "tenant-zaru-consumer/kv/metadata/users/*" {
  capabilities = ["list", "read", "delete"]
}
path "aegis-system/kv/data/*" {
  capabilities = ["create", "read", "update", "delete", "list"]
}
path "aegis-system/kv/metadata/*" {
  capabilities = ["list", "read", "delete"]
}
# ADR-117 single-node hybrid: when the controller signs NodeSecurityToken
# itself (instead of proxying to a Relay), `ChallengeNodeUseCase::execute`
# invokes Transit `/sign/edge-enrollment-token` with the same key the Relay
# uses. Without this grant, worker enrollment via `aegis node join` 403s
# against real OpenBao. The relay-coordinator policy below mirrors this
# capability for the dedicated relay AppRole.
path "transit/sign/edge-enrollment-token" {
  capabilities = ["update"]
}
path "transit/verify/edge-enrollment-token" {
  capabilities = ["update"]
}
# Core seals a tool-approval row's arguments, summary, result and error under
# its tenant's Transit key, tool-approvals-<tenant>, and a git binding's
# webhook secret under webhook-secret (AEGIS ADR-126, Update of 2026-10-08,
# clauses 4 and 5). The first encrypt creates the key, hence "create".
path "transit/encrypt/tool-approvals-*" {
  capabilities = ["create", "update"]
}
path "transit/decrypt/tool-approvals-*" {
  capabilities = ["update"]
}
path "transit/encrypt/webhook-secret" {
  capabilities = ["create", "update"]
}
path "transit/decrypt/webhook-secret" {
  capabilities = ["update"]
}
