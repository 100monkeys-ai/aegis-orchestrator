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
}

impl HostStateSpec {
    pub fn security_context(&self) -> &str {
        self.security_context
            .as_deref()
            .unwrap_or(DEFAULT_HOST_CONTEXT)
    }

    /// The `state` argument of `host.apply`.
    pub fn apply_state(&self) -> Value {
        serde_json::json!({
            "resources": self.resources,
            "continue_on_failure": self.continue_on_failure,
        })
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
            m.spec.apply_state()["resources"].as_array().unwrap().len(),
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
}
