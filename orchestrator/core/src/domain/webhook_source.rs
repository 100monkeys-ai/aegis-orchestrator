// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Webhook sources (ADR-157 D4): a tenant's registered sender of signed
//! deliveries (GitHub, GitLab, Forgejo, any sender of a signed payload) and
//! the workflow a delivery starts.
//!
//! A delivery reaches `POST /v1/webhooks/{name}`. It is verified with the
//! source's secret, which AEGIS generates and keeps in the tenant's own
//! secret store, and accepted once. A delivery whose event the source does
//! not take is acknowledged and starts nothing. Otherwise the source's
//! `inputs` map the payload to the workflow's input, each a JSON pointer
//! into the delivery. A source only asks for a run: every credential the run
//! needs is the workflow's own.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A source as a tenant declares it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookSourceSpec {
    /// The workflow a delivery starts, by name.
    pub workflow: String,
    /// The events the source takes, read from the header `event_header`
    /// names (default `X-GitHub-Event`). Empty takes every event.
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default = "default_event_header")]
    pub event_header: String,
    /// Conditions on the payload, each a JSON pointer and the string it must
    /// equal, or, ending in `*`, begin with (`sha-*`). Every condition must
    /// hold.
    #[serde(default)]
    pub when: BTreeMap<String, String>,
    /// The workflow's input: each key from a JSON pointer into the payload.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

fn default_event_header() -> String {
    "X-GitHub-Event".to_string()
}

/// One registered source.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookSource {
    pub tenant_id: crate::domain::shared_kernel::TenantId,
    pub name: String,
    pub spec: WebhookSourceSpec,
    /// Who a delivery's run acts as: the person who registered the source,
    /// as a schedule's owner (its approvals are theirs).
    pub owner: crate::domain::schedule::ScheduleOwner,
    pub created_by: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// What a delivery asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Delivery {
    /// Start `workflow` with `input`.
    Start { workflow: String, input: Value },
    /// The source does not take this delivery: the sentence says why.
    Ignored(String),
}

/// A source's name, as it appears in `/v1/webhooks/{name}`: 1-63 lowercase
/// letters, digits and '-', starting with a letter or digit.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn pointer_ok(p: &str) -> bool {
    p.is_empty() || p.starts_with('/')
}

impl WebhookSourceSpec {
    /// Every problem with a source, or none.
    pub fn problems(&self, name: &str) -> Vec<String> {
        let mut p = Vec::new();
        if !name_ok(name) {
            p.push(format!(
                "name {name:?} must be 1-63 lowercase letters, digits or '-', starting with a letter or digit"
            ));
        }
        if self.workflow.trim().is_empty() {
            p.push("workflow must name the workflow a delivery starts".into());
        }
        if self.event_header.trim().is_empty() {
            p.push("event_header must name a header".into());
        }
        for pointer in self.when.keys() {
            if !pointer_ok(pointer) {
                p.push(format!(
                    "when: {pointer:?} must be a JSON pointer (\"/a/b\")"
                ));
            }
        }
        for (key, pointer) in &self.inputs {
            if key.is_empty() {
                p.push("inputs: a key is empty".into());
            }
            if !pointer_ok(pointer) {
                p.push(format!(
                    "inputs.{key}: {pointer:?} must be a JSON pointer (\"/a/b\")"
                ));
            }
        }
        p
    }

    /// What a delivery with `headers` (lowercased names) and `payload` asks
    /// for.
    pub fn delivery(&self, headers: &BTreeMap<String, String>, payload: &Value) -> Delivery {
        if !self.events.is_empty() {
            let event = headers
                .get(&self.event_header.to_ascii_lowercase())
                .map(String::as_str)
                .unwrap_or("");
            if !self.events.iter().any(|e| e == event) {
                return Delivery::Ignored(format!(
                    "event {event:?} is not one of {}",
                    self.events.join(", ")
                ));
            }
        }
        for (pointer, want) in &self.when {
            let have = payload.pointer(pointer).map(text_of);
            let holds = match (have.as_deref(), want.strip_suffix('*')) {
                (Some(h), Some(prefix)) => h.starts_with(prefix),
                (Some(h), None) => h == want,
                (None, _) => false,
            };
            if !holds {
                return Delivery::Ignored(format!(
                    "{pointer} is {}, not {want:?}",
                    have.map(|h| format!("{h:?}"))
                        .unwrap_or_else(|| "absent".into())
                ));
            }
        }
        let mut input = serde_json::Map::new();
        for (key, pointer) in &self.inputs {
            match payload.pointer(pointer) {
                Some(v) => {
                    input.insert(key.clone(), v.clone());
                }
                None => {
                    return Delivery::Ignored(format!(
                        "the delivery has no {pointer} for the input {key}"
                    ))
                }
            }
        }
        Delivery::Start {
            workflow: self.workflow.clone(),
            input: Value::Object(input),
        }
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[async_trait::async_trait]
pub trait WebhookSourceRepository: Send + Sync {
    async fn save(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
        name: &str,
        spec: &WebhookSourceSpec,
        owner: &crate::domain::schedule::ScheduleOwner,
        created_by: &str,
    ) -> Result<WebhookSource, crate::domain::repository::RepositoryError>;
    async fn get(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
        name: &str,
    ) -> Result<Option<WebhookSource>, crate::domain::repository::RepositoryError>;
    async fn list(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
    ) -> Result<Vec<WebhookSource>, crate::domain::repository::RepositoryError>;
    async fn delete(
        &self,
        tenant: &crate::domain::shared_kernel::TenantId,
        name: &str,
    ) -> Result<bool, crate::domain::repository::RepositoryError>;
    /// Every tenant that registered a source named `name`.
    async fn tenants_for(
        &self,
        name: &str,
    ) -> Result<
        Vec<crate::domain::shared_kernel::TenantId>,
        crate::domain::repository::RepositoryError,
    >;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn worker_publish() -> WebhookSourceSpec {
        WebhookSourceSpec {
            workflow: "aegis-deploy".into(),
            events: vec!["registry_package".into()],
            event_header: default_event_header(),
            when: BTreeMap::from([("/action".into(), "published".into())]),
            inputs: BTreeMap::from([(
                "image_tag".into(),
                "/registry_package/package_version/container_metadata/tag/name".into(),
            )]),
        }
    }

    fn headers(event: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("x-github-event".into(), event.into())])
    }

    /// ADR-157 D4: an image publish starts the deploy with its tag.
    #[test]
    fn a_publish_starts_the_workflow_with_the_mapped_input() {
        let payload = json!({ "action": "published", "registry_package": { "package_version": {
            "container_metadata": { "tag": { "name": "sha-0180a38" } } } } });
        assert_eq!(
            worker_publish().delivery(&headers("registry_package"), &payload),
            Delivery::Start {
                workflow: "aegis-deploy".into(),
                input: json!({ "image_tag": "sha-0180a38" }),
            }
        );
    }

    /// A `*`-ending condition is a prefix: a sha tag starts the deploy, and
    /// the same build's `latest` does not.
    #[test]
    fn a_prefix_condition_takes_the_sha_tag_and_not_latest() {
        let mut s = worker_publish();
        s.when.insert(
            "/registry_package/package_version/container_metadata/tag/name".into(),
            "sha-*".into(),
        );
        let tagged = |tag: &str| {
            json!({ "action": "published", "registry_package": { "package_version": {
            "container_metadata": { "tag": { "name": tag } } } } })
        };
        assert!(matches!(
            s.delivery(&headers("registry_package"), &tagged("sha-0180a38")),
            Delivery::Start { .. }
        ));
        assert!(matches!(
            s.delivery(&headers("registry_package"), &tagged("latest")),
            Delivery::Ignored(_)
        ));
    }

    /// GitHub's ping, an update, or a payload without the tag starts
    /// nothing.
    #[test]
    fn other_events_and_actions_start_nothing() {
        let s = worker_publish();
        assert!(matches!(
            s.delivery(&headers("ping"), &json!({})),
            Delivery::Ignored(_)
        ));
        assert!(matches!(
            s.delivery(
                &headers("registry_package"),
                &json!({ "action": "updated" })
            ),
            Delivery::Ignored(_)
        ));
        assert!(matches!(
            s.delivery(
                &headers("registry_package"),
                &json!({ "action": "published" })
            ),
            Delivery::Ignored(_)
        ));
    }

    #[test]
    fn bad_sources_are_refused_with_every_problem() {
        let mut s = worker_publish();
        s.workflow = " ".into();
        s.inputs.insert("x".into(), "no-slash".into());
        let p = s.problems("Bad_Name");
        assert!(p.iter().any(|x| x.contains("name")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("workflow")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("inputs.x")), "{p:?}");
        assert!(worker_publish()
            .problems("aegis-temporal-worker")
            .is_empty());
    }
}
