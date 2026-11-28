// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Provider state (ADR-157 D11): a tenant's declared state for objects at a
//! provider, the sibling of [`crate::domain::host_state`]. Its first provider
//! is Cloudflare, reached through Cloudflare's own MCP server on the SEAL
//! gateway, so planning and applying are tool calls made as the run's person
//! with the binding the run chose.
//!
//! ```yaml
//! apiVersion: 100monkeys.ai/v1
//! kind: ProviderState
//! metadata:
//!   name: zaru-dns
//! spec:
//!   provider: cloudflare
//!   resources:
//!     - id: api
//!       kind: dns_record
//!       zone: myzaru.com
//!       type: CNAME
//!       name: api.myzaru.com
//!       content: lb.myzaru.com
//!       proxied: true
//! ```
//!
//! Each resource becomes one `cloudflare.execute` call whose code is
//! generated here, never written by a model. The code reads the object,
//! compares it with the declared one and, when applying, writes only the
//! difference. Every declared value is embedded as a JSON literal, so no
//! value is ever read as code.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The providers a state may name.
pub const PROVIDERS: &[&str] = &["cloudflare"];

/// The resource kinds of each provider.
pub const CLOUDFLARE_KINDS: &[&str] = &["dns_record"];

/// DNS record types a `dns_record` may declare.
const DNS_TYPES: &[&str] = &["A", "AAAA", "CNAME", "TXT", "MX", "NS", "SRV", "CAA"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderStateManifest {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: ProviderStateMetadata,
    pub spec: ProviderStateSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderStateMetadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderStateSpec {
    /// `cloudflare`.
    pub provider: String,
    /// The Cloudflare account, for a user token that reaches several; an
    /// account token names its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// The ordered resources; each a JSON object with an `id` and a `kind`.
    pub resources: Vec<Value>,
}

fn str_field<'a>(r: &'a Value, k: &str) -> Option<&'a str> {
    r.get(k).and_then(Value::as_str)
}

impl ProviderStateManifest {
    /// Read a manifest without validating it, through JSON values as a host
    /// state is.
    pub fn from_yaml(yaml: &str) -> Result<Self, String> {
        let value: Value = serde_yaml::from_str(yaml).map_err(|e| e.to_string())?;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    /// Parse and validate, reporting every problem.
    pub fn parse(yaml: &str) -> Result<Self, Vec<String>> {
        let manifest = Self::from_yaml(yaml).map_err(|e| vec![e])?;
        let problems = manifest.problems();
        if problems.is_empty() {
            Ok(manifest)
        } else {
            Err(problems)
        }
    }

    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.api_version != "100monkeys.ai/v1" {
            p.push(format!(
                "apiVersion must be 100monkeys.ai/v1, not {:?}",
                self.api_version
            ));
        }
        if self.kind != "ProviderState" {
            p.push(format!("kind must be ProviderState, not {:?}", self.kind));
        }
        let name = &self.metadata.name;
        let name_ok = !name.is_empty()
            && name.len() <= 63
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
        if !name_ok {
            p.push(format!(
                "metadata.name {name:?} must be 1-63 lowercase letters, digits, '-' or '.', starting with a letter or digit"
            ));
        }
        if !PROVIDERS.contains(&self.spec.provider.as_str()) {
            p.push(format!(
                "spec.provider {:?} is not one of {}",
                self.spec.provider,
                PROVIDERS.join(", ")
            ));
        }
        if let Some(account) = &self.spec.account_id {
            if account.len() != 32 || !account.chars().all(|c| c.is_ascii_hexdigit()) {
                p.push("spec.account_id must be a 32-character Cloudflare account id".into());
            }
        }
        if self.spec.resources.is_empty() {
            p.push("spec.resources must name at least one resource".into());
        }
        let mut seen = std::collections::HashSet::new();
        for (i, r) in self.spec.resources.iter().enumerate() {
            let at = format!("spec.resources[{i}]");
            match str_field(r, "id") {
                Some(id) if !id.is_empty() => {
                    if !seen.insert(id.to_string()) {
                        p.push(format!("{at}: duplicate id {id:?}"));
                    }
                }
                _ => p.push(format!("{at} needs an id")),
            }
            let kind = str_field(r, "kind").unwrap_or("");
            if !CLOUDFLARE_KINDS.contains(&kind) {
                p.push(format!(
                    "{at}: kind {kind:?} is not one of {}",
                    CLOUDFLARE_KINDS.join(", ")
                ));
                continue;
            }
            if let Err(problem) = DnsRecord::from_resource(r) {
                p.push(format!("{at} (dns_record): {problem}"));
            }
        }
        p
    }

    /// One `cloudflare.execute` argument object per resource, in order:
    /// `(id, args)`. `check` plans: the code reads and compares only.
    pub fn execute_calls(&self, check: bool) -> Vec<(String, Value)> {
        self.spec
            .resources
            .iter()
            .filter_map(|r| {
                let id = str_field(r, "id")?.to_string();
                let record = DnsRecord::from_resource(r).ok()?;
                let mut args = json!({ "code": record.code(check) });
                if let Some(account) = &self.spec.account_id {
                    args["account_id"] = Value::String(account.clone());
                }
                Some((id, args))
            })
            .collect()
    }
}

/// A DNS record as a state declares it.
#[derive(Debug, Clone, PartialEq)]
pub struct DnsRecord {
    pub zone: String,
    pub record_type: String,
    pub name: String,
    pub content: Option<String>,
    pub proxied: Option<bool>,
    pub ttl: Option<u64>,
    pub present: bool,
}

fn hostname_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '*')
        })
}

impl DnsRecord {
    pub fn from_resource(r: &Value) -> Result<Self, String> {
        let zone = str_field(r, "zone").ok_or("needs zone, the zone's name")?;
        let name = str_field(r, "name").ok_or("needs name, the record's full name")?;
        let record_type = str_field(r, "type").ok_or("needs type")?;
        if !hostname_ok(zone) {
            return Err(format!("zone {zone:?} is not a domain name"));
        }
        if !hostname_ok(name) {
            return Err(format!("name {name:?} is not a domain name"));
        }
        if name != zone && !name.ends_with(&format!(".{zone}")) {
            return Err(format!("name {name:?} is not in the zone {zone:?}"));
        }
        if !DNS_TYPES.contains(&record_type) {
            return Err(format!(
                "type {record_type:?} is not one of {}",
                DNS_TYPES.join(", ")
            ));
        }
        let present = str_field(r, "state") != Some("absent");
        let content = str_field(r, "content").map(str::to_string);
        if present && content.is_none() {
            return Err("needs content, or state: absent".into());
        }
        let proxied = r.get("proxied").and_then(Value::as_bool);
        let ttl = r.get("ttl").and_then(Value::as_u64);
        if let Some(ttl) = ttl {
            if ttl != 1 && !(60..=86_400).contains(&ttl) {
                return Err("ttl must be 1 (automatic) or 60-86400 seconds".into());
            }
        }
        Ok(Self {
            zone: zone.to_string(),
            record_type: record_type.to_string(),
            name: name.to_string(),
            content,
            proxied,
            ttl,
            present,
        })
    }

    /// The code `cloudflare.execute` runs for this record. It answers
    /// `{ status: "ok" | "changed" | "failed", diff }`, as a host resource
    /// does. The declared record is one JSON literal; the code never builds
    /// a request from anything else.
    pub fn code(&self, check: bool) -> String {
        let want = json!({
            "zone": self.zone,
            "type": self.record_type,
            "name": self.name,
            "content": self.content,
            "proxied": self.proxied,
            "ttl": self.ttl,
            "present": self.present,
            "check": check,
        });
        format!(
            r#"async () => {{
  const want = {want};
  const zones = await cloudflare.request({{ method: "GET", path: "/zones?name=" + encodeURIComponent(want.zone) }});
  const zone = (zones.result || [])[0];
  if (!zone) return {{ status: "failed", diff: "no zone " + want.zone }};
  const base = "/zones/" + zone.id + "/dns_records";
  const found = await cloudflare.request({{ method: "GET", path: base + "?type=" + want.type + "&name=" + encodeURIComponent(want.name) }});
  const records = found.result || [];
  if (records.length > 1) return {{ status: "failed", diff: records.length + " " + want.type + " records named " + want.name + "; declare one" }};
  const have = records[0];
  if (!want.present) {{
    if (!have) return {{ status: "ok", diff: "absent" }};
    if (want.check) return {{ status: "changed", diff: "would delete " + have.content }};
    await cloudflare.request({{ method: "DELETE", path: base + "/" + have.id }});
    return {{ status: "changed", diff: "deleted " + have.content }};
  }}
  const body = {{ type: want.type, name: want.name, content: want.content }};
  if (want.proxied !== null) body.proxied = want.proxied;
  if (want.ttl !== null) body.ttl = want.ttl;
  if (!have) {{
    if (want.check) return {{ status: "changed", diff: "would create " + want.content }};
    await cloudflare.request({{ method: "POST", path: base, body }});
    return {{ status: "changed", diff: "created " + want.content }};
  }}
  const diffs = [];
  if (have.content !== want.content) diffs.push("content " + have.content + " -> " + want.content);
  if (want.proxied !== null && have.proxied !== want.proxied) diffs.push("proxied " + have.proxied + " -> " + want.proxied);
  if (want.ttl !== null && have.ttl !== want.ttl) diffs.push("ttl " + have.ttl + " -> " + want.ttl);
  if (diffs.length === 0) return {{ status: "ok", diff: "in place" }};
  if (want.check) return {{ status: "changed", diff: "would set " + diffs.join("; ") }};
  await cloudflare.request({{ method: "PATCH", path: base + "/" + have.id, body }});
  return {{ status: "changed", diff: "set " + diffs.join("; ") }};
}}"#
        )
    }
}

/// A provider state is active or archived, as a host state is (ADR-148).
pub type ProviderStateStatus = crate::domain::host_state::HostStateStatus;

/// One stored version of a tenant's provider state.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderState {
    pub id: uuid::Uuid,
    pub tenant_id: crate::domain::shared_kernel::TenantId,
    pub name: String,
    pub version: i32,
    pub status: ProviderStateStatus,
    pub manifest_yaml: String,
    pub manifest: ProviderStateManifest,
    pub created_by: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait::async_trait]
pub trait ProviderStateRepository: Send + Sync {
    /// Store `manifest` as the next version of its name in `tenant`, the new
    /// version active and the previous one archived.
    async fn save_version(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
        manifest_yaml: &str,
        manifest: &ProviderStateManifest,
        created_by: &str,
    ) -> Result<ProviderState, crate::domain::repository::RepositoryError>;
    async fn get_active(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
        name: &str,
    ) -> Result<Option<ProviderState>, crate::domain::repository::RepositoryError>;
    async fn list_active(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
    ) -> Result<Vec<ProviderState>, crate::domain::repository::RepositoryError>;
    async fn archive(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
        name: &str,
    ) -> Result<bool, crate::domain::repository::RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const DNS: &str = r#"
apiVersion: 100monkeys.ai/v1
kind: ProviderState
metadata:
  name: zaru-dns
spec:
  provider: cloudflare
  resources:
    - { id: api, kind: dns_record, zone: myzaru.com, type: CNAME, name: api.myzaru.com, content: lb.myzaru.com, proxied: true }
    - { id: old, kind: dns_record, zone: myzaru.com, type: A, name: old.myzaru.com, state: absent }
"#;

    #[test]
    fn a_dns_state_parses_and_becomes_one_execute_per_record() {
        let m = ProviderStateManifest::parse(DNS).expect("valid");
        let calls = m.execute_calls(true);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "api");
        let code = calls[0].1["code"].as_str().unwrap();
        assert!(code.starts_with("async () => {"));
        assert!(code.contains(r#""check":true"#), "{code}");
        assert!(code.contains(r#""content":"lb.myzaru.com""#), "{code}");
        assert!(calls[0].1.get("account_id").is_none());
        let apply = m.execute_calls(false);
        assert!(apply[1].1["code"]
            .as_str()
            .unwrap()
            .contains(r#""present":false"#));
    }

    /// A declared value is a JSON literal in the code, never code: a quote
    /// or a backtick in it cannot end the literal.
    #[test]
    fn a_value_never_escapes_its_literal() {
        let r = json!({ "id": "t", "kind": "dns_record", "zone": "example.com", "type": "TXT",
            "name": "x.example.com", "content": "\"; await cloudflare.request({method:\"DELETE\"}); \"`${1}`" });
        let code = DnsRecord::from_resource(&r).unwrap().code(true);
        let literal = code.lines().nth(1).unwrap().trim();
        let parsed: Value = serde_json::from_str(
            literal
                .trim_start_matches("const want = ")
                .trim_end_matches(';'),
        )
        .expect("the declared record is one JSON literal");
        assert_eq!(parsed["content"], r["content"]);
    }

    #[test]
    fn bad_records_are_refused_with_every_problem() {
        let bad = r#"
apiVersion: 100monkeys.ai/v1
kind: ProviderState
metadata: { name: Bad }
spec:
  provider: route53
  account_id: nope
  resources:
    - { id: a, kind: dns_record, zone: example.com, type: CNAME, name: api.other.com, content: x }
    - { id: a, kind: dns_record, zone: example.com, type: SPF, name: example.com, content: x }
    - { id: c, kind: dns_record, zone: example.com, type: A, name: example.com }
    - { id: d, kind: dns_record, zone: example.com, type: A, name: d.example.com, content: 1.2.3.4, ttl: 5 }
    - { id: e, kind: load_balancer }
"#;
        let p = ProviderStateManifest::parse(bad).unwrap_err();
        for needle in [
            "metadata.name",
            "spec.provider",
            "account_id",
            "not in the zone",
            "duplicate id",
            "type \"SPF\"",
            "needs content",
            "ttl must be",
            "kind \"load_balancer\"",
        ] {
            assert!(p.iter().any(|x| x.contains(needle)), "{needle}: {p:?}");
        }
    }
}
