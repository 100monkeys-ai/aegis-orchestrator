// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Host state (ADR-157 D2): a tenant's declared state for a set of edge
//! hosts, a manifest kind beside `Agent` and `Workflow`.
//!
//! ```yaml
//! apiVersion: 100monkeys.ai/v1
//! kind: HostState
//! metadata:
//!   name: chrony
//!   description: Time sync on every member
//! spec:
//!   target: { Selector: { tags: [member] } }
//!   resources:
//!     - { id: pkg, kind: package, name: chrony }
//!     - { id: svc, kind: service, name: chrony, enabled: true, running: true }
//! ```
//!
//! The resources are applied on each target edge by `host.apply`, which
//! reads the host first and acts only where it differs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::domain::edge::EdgeTarget;
use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::TenantId;

/// The security context a host state is applied under on its edges, unless
/// the manifest names another.
pub const DEFAULT_HOST_CONTEXT: &str = "host-admin";

/// The resource kinds `host.apply` implements.
pub const RESOURCE_KINDS: &[&str] = &[
    "file",
    "directory",
    "package",
    "service",
    "user",
    "group",
    "sysctl",
    "firewall",
    "command",
    "pod",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostStateManifest {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: HostStateMetadata,
    pub spec: HostStateSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostStateMetadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub labels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostStateSpec {
    /// The edges the state applies to: a node, a group, a selector, or all.
    pub target: EdgeTarget,
    /// The security context the edges apply it under (default `host-admin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_context: Option<String>,
    /// Keep applying after a failed resource.
    #[serde(default)]
    pub continue_on_failure: bool,
    /// The ordered resources; each a JSON object with an `id` and a `kind`.
    pub resources: Vec<Value>,
    /// Plain values a resource names as `((vars.NAME))`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,
    /// Secrets a resource names as `((secret.NAME))`, each `path#field` in
    /// the tenant's `host-states/` store in OpenBao. The orchestrator reads
    /// them when it plans or applies, and the state holds only their names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, String>,
}

/// One secret a state names: its OpenBao path under `host-states/` and its
/// field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub path: String,
    pub field: String,
}

impl SecretRef {
    /// `path#field`, the path relative to the tenant's `host-states/`:
    /// segments of letters, digits, `-`, `_` and `.`, no `..`, no leading
    /// `/`.
    pub fn parse(reference: &str) -> Option<Self> {
        let (path, field) = reference.split_once('#')?;
        let segment_ok = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        let field_ok = !field.is_empty()
            && field
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        (path.split('/').all(segment_ok) && field_ok).then(|| Self {
            path: path.to_string(),
            field: field.to_string(),
        })
    }
}

/// `((secret.NAME))` and `((vars.NAME))`, spaces allowed inside the parens.
fn placeholders(s: &str) -> Vec<(std::ops::Range<usize>, &str, &str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(open) = s[from..].find("((").map(|i| i + from) {
        let Some(close) = s[open + 2..].find("))").map(|i| i + open + 2) else {
            break;
        };
        let inner = s[open + 2..close].trim();
        if let Some((scope, name)) = inner.split_once('.') {
            if matches!(scope, "secret" | "vars") && ident_ok(name) {
                out.push((open..close + 2, scope, name));
            }
        }
        from = close + 2;
    }
    out
}

fn ident_ok(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn each_string(v: &Value, f: &mut dyn FnMut(&str)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(a) => a.iter().for_each(|x| each_string(x, f)),
        Value::Object(o) => o.values().for_each(|x| each_string(x, f)),
        _ => {}
    }
}

fn map_strings(v: &Value, f: &dyn Fn(&str) -> String) -> Value {
    match v {
        Value::String(s) => Value::String(f(s)),
        Value::Array(a) => Value::Array(a.iter().map(|x| map_strings(x, f)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), map_strings(x, f)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Replace every occurrence of a secret's value in `v` with
/// `[secret NAME]`, so a report that echoes content (a file's diff, an
/// error) never carries it.
pub fn redact_secrets(v: &Value, secrets: &BTreeMap<String, String>) -> Value {
    let mut by_length: Vec<(&String, &String)> = secrets
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .collect();
    by_length.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
    map_strings(v, &|s| {
        let mut s = s.to_string();
        for (name, value) in &by_length {
            s = s.replace(value.as_str(), &format!("[secret {name}]"));
        }
        s
    })
}

impl HostStateSpec {
    pub fn security_context(&self) -> &str {
        self.security_context
            .as_deref()
            .unwrap_or(DEFAULT_HOST_CONTEXT)
    }

    /// The `state` argument of `host.apply`, with every `((vars.NAME))` and
    /// `((secret.NAME))` replaced from `vars` and `secrets` (the values read
    /// for this run). Validation refuses a placeholder that names neither.
    pub fn apply_state(&self, secrets: &BTreeMap<String, String>) -> Value {
        let render = |s: &str| {
            let mut out = String::with_capacity(s.len());
            let mut last = 0;
            for (range, scope, name) in placeholders(s) {
                out.push_str(&s[last..range.start]);
                let value = match scope {
                    "secret" => secrets.get(name),
                    _ => self.vars.get(name),
                };
                out.push_str(value.map(String::as_str).unwrap_or(&s[range.clone()]));
                last = range.end;
            }
            out.push_str(&s[last..]);
            out
        };
        serde_json::json!({
            "resources": map_strings(&Value::Array(self.resources.clone()), &render),
            "continue_on_failure": self.continue_on_failure,
        })
    }

    /// The secrets the state declares, parsed.
    pub fn secret_refs(&self) -> Vec<(String, SecretRef)> {
        self.secrets
            .iter()
            .filter_map(|(name, r)| SecretRef::parse(r).map(|r| (name.clone(), r)))
            .collect()
    }
}

fn str_field<'a>(r: &'a Value, k: &str) -> Option<&'a str> {
    r.get(k).and_then(Value::as_str)
}

impl HostStateManifest {
    /// Parse and validate a manifest. Every problem is reported, not just
    /// the first, so an author or an agent fixes them in one pass.
    pub fn parse(yaml: &str) -> Result<Self, Vec<String>> {
        let manifest: Self = serde_yaml::from_str(yaml).map_err(|e| vec![e.to_string()])?;
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
        if self.kind != "HostState" {
            p.push(format!("kind must be HostState, not {:?}", self.kind));
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
        if self.spec.resources.is_empty() {
            p.push("spec.resources must name at least one resource".into());
        }
        for name in self.spec.vars.keys() {
            if !ident_ok(name) {
                p.push(format!("spec.vars: {name:?} must be letters, digits and '_', not starting with a digit"));
            }
        }
        for (name, reference) in &self.spec.secrets {
            if !ident_ok(name) {
                p.push(format!("spec.secrets: {name:?} must be letters, digits and '_', not starting with a digit"));
            }
            if SecretRef::parse(reference).is_none() {
                p.push(format!(
                    "spec.secrets.{name}: {reference:?} must be path#field, the path under host-states/ (segments of letters, digits, '-', '_', '.'; no '..')"
                ));
            }
        }
        for resource in &self.spec.resources {
            each_string(resource, &mut |s| {
                for (_, scope, name) in placeholders(s) {
                    let declared = match scope {
                        "secret" => self.spec.secrets.contains_key(name),
                        _ => self.spec.vars.contains_key(name),
                    };
                    let key = if scope == "secret" { "secrets" } else { "vars" };
                    let problem = format!(
                        "(({scope}.{name})) is used but spec.{key} does not declare {name}"
                    );
                    if !declared && !p.contains(&problem) {
                        p.push(problem);
                    }
                }
            });
        }
        let mut seen = std::collections::HashSet::new();
        for (i, r) in self.spec.resources.iter().enumerate() {
            let at = format!("spec.resources[{i}]");
            if !r.is_object() {
                p.push(format!("{at} must be an object"));
                continue;
            }
            match str_field(r, "id") {
                Some(id) if !id.is_empty() => {
                    if !seen.insert(id.to_string()) {
                        p.push(format!("{at}: duplicate id {id:?}"));
                    }
                }
                _ => p.push(format!("{at} needs an id")),
            }
            let kind = str_field(r, "kind").unwrap_or("");
            if !RESOURCE_KINDS.contains(&kind) {
                p.push(format!(
                    "{at}: kind {kind:?} is not one of {}",
                    RESOURCE_KINDS.join(", ")
                ));
                continue;
            }
            let need = |fields: &[&str], p: &mut Vec<String>| {
                for f in fields {
                    if r.get(*f).is_none() {
                        p.push(format!("{at} ({kind}) needs {f}"));
                    }
                }
            };
            let absent = str_field(r, "state") == Some("absent");
            match kind {
                "file" => {
                    need(&["path"], &mut p);
                    if !absent && r.get("content").is_none() {
                        p.push(format!("{at} (file) needs content, or state: absent"));
                    }
                }
                "directory" => need(&["path"], &mut p),
                "package" | "service" | "user" | "group" => need(&["name"], &mut p),
                "sysctl" => need(&["key", "value"], &mut p),
                "firewall" => need(&["rule"], &mut p),
                "pod" => need(&["name", "spec", "user"], &mut p),
                "command" => {
                    need(&["run"], &mut p);
                    if ["creates", "unless", "onlyif"]
                        .iter()
                        .all(|g| r.get(*g).is_none())
                    {
                        p.push(format!(
                            "{at} (command) needs creates, unless or onlyif, so it is idempotent"
                        ));
                    }
                }
                _ => {}
            }
            if let Some(w) = r.get("watch") {
                if kind != "service" {
                    p.push(format!("{at}: only a service may watch other resources"));
                } else if let Some(ids) = w.as_array() {
                    for id in ids.iter().filter_map(Value::as_str) {
                        if !seen.contains(id) {
                            p.push(format!(
                                "{at}: watches {id:?}, which no earlier resource is"
                            ));
                        }
                    }
                }
            }
        }
        p
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostStateStatus {
    Active,
    Archived,
}

impl HostStateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }
}

/// One stored version of a tenant's host state.
#[derive(Debug, Clone, Serialize)]
pub struct HostState {
    pub id: uuid::Uuid,
    pub tenant_id: TenantId,
    pub name: String,
    pub version: i32,
    pub status: HostStateStatus,
    pub manifest_yaml: String,
    pub manifest: HostStateManifest,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

#[async_trait::async_trait]
pub trait HostStateRepository: Send + Sync {
    /// Store `manifest` as the next version of its name in `tenant`, the new
    /// active one, archiving the version before it.
    async fn save_version(
        &self,
        tenant: &TenantId,
        manifest_yaml: &str,
        manifest: &HostStateManifest,
        created_by: &str,
    ) -> Result<HostState, RepositoryError>;
    /// The active version of a name.
    async fn get_active(
        &self,
        tenant: &TenantId,
        name: &str,
    ) -> Result<Option<HostState>, RepositoryError>;
    /// A tenant's active host states, by name.
    async fn list_active(&self, tenant: &TenantId) -> Result<Vec<HostState>, RepositoryError>;
    /// Archive the active version of a name; `false` when there is none.
    async fn archive(&self, tenant: &TenantId, name: &str) -> Result<bool, RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
apiVersion: 100monkeys.ai/v1
kind: HostState
metadata: { name: chrony }
spec:
  target: All
  resources:
    - { id: pkg, kind: package, name: chrony }
    - { id: conf, kind: file, path: /etc/chrony/conf.d/pool.conf, content: "pool x\n" }
    - { id: svc, kind: service, name: chrony, enabled: true, running: true, watch: [conf] }
"#;

    #[test]
    fn a_valid_manifest_parses() {
        let m = HostStateManifest::parse(OK).expect("valid");
        assert_eq!(m.spec.security_context(), "host-admin");
        assert_eq!(
            m.spec.apply_state(&BTreeMap::new())["resources"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let bad = r#"
apiVersion: 100monkeys.ai/v1
kind: HostState
metadata: { name: Bad_Name }
spec:
  target: All
  resources:
    - { id: a, kind: command, run: [true] }
    - { id: a, kind: mystery }
    - { id: b, kind: file, path: /x }
    - { id: c, kind: service, name: x, watch: [later] }
"#;
        let problems = HostStateManifest::parse(bad).unwrap_err();
        let all = problems.join("\n");
        for needle in [
            "metadata.name",
            "idempotent",
            "duplicate id",
            "mystery",
            "needs content",
            "watches \"later\"",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in:\n{all}");
        }
    }

    const WITH_SECRETS: &str = r#"
apiVersion: 100monkeys.ai/v1
kind: HostState
metadata:
  name: api
spec:
  target: All
  vars:
    port: "8080"
  secrets:
    db: "api/db#password"
  resources:
    - id: env
      kind: file
      path: /etc/api.env
      content: "PORT=((vars.port))\nDB_PASSWORD=(( secret.db ))\n"
"#;

    /// ADR-157 D2: a state names a secret by path; apply_state fills it in
    /// for the run, and the stored state carries only the name.
    #[test]
    fn a_secret_is_rendered_for_the_run_and_never_stored() {
        let m = HostStateManifest::parse(WITH_SECRETS).expect("valid");
        assert!(!serde_json::to_string(&m).unwrap().contains("s3cr3t"));
        let mut values = BTreeMap::new();
        values.insert("db".to_string(), "s3cr3t".to_string());
        let state = m.spec.apply_state(&values);
        assert_eq!(
            state["resources"][0]["content"],
            "PORT=8080\nDB_PASSWORD=s3cr3t\n"
        );
        assert_eq!(
            m.spec.secret_refs()[0].1,
            SecretRef {
                path: "api/db".into(),
                field: "password".into()
            }
        );
    }

    /// A report that echoes a secret's value carries its name instead.
    #[test]
    fn a_report_never_carries_a_secret_value() {
        let mut values = BTreeMap::new();
        values.insert("db".to_string(), "s3cr3t".to_string());
        let report = serde_json::json!({"results": [{"diff": "+DB_PASSWORD=s3cr3t", "error": "bad s3cr3t"}]});
        let clean = redact_secrets(&report, &values).to_string();
        assert!(!clean.contains("s3cr3t"), "{clean}");
        assert!(clean.contains("[secret db]"));
    }

    /// An undeclared placeholder and a path that climbs out of
    /// host-states/ are refused.
    #[test]
    fn undeclared_placeholders_and_escaping_paths_are_refused() {
        let bad = WITH_SECRETS
            .replace("api/db#password", "../c/x#password")
            .replace("((vars.port))", "((vars.nope))");
        let problems = HostStateManifest::parse(&bad).unwrap_err();
        assert!(
            problems.iter().any(|p| p.contains("spec.secrets.db")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("((vars.nope))")),
            "{problems:?}"
        );
        for refused in ["/abs#f", "a/../b#f", "a#", "#f", "a b#f", "a#f.g"] {
            assert!(SecretRef::parse(refused).is_none(), "{refused}");
        }
    }
}
