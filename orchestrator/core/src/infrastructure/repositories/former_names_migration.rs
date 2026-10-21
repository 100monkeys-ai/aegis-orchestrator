// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Stored former server names, rewritten at the daemon's start
//!
//! Carbon Forge ADR-0198, Update of 2026-10-21, R3 step 6. A remote server
//! entry of `seal_gateway.remote_servers` may declare `former_names`; every
//! reader resolves a former name to the entry's name when it reads (step 1),
//! so a stored row naming a former name already works. This data migration
//! rewrites those stored names to the entry's own name, once the daemon has
//! applied the schema migrations, so that the stored rows read as the node
//! names the server today:
//!
//! - `credential_bindings.provider`;
//! - the keys of `schedules.contexts`;
//! - the patterns of `profiles.tools` (`<former>.<tool>`, `<former>.*`);
//! - `tool_approval_policies.tool_name`;
//! - the open (`pending`) `tool_approval_requests`: `tool_name` and the keys
//!   of `contexts`, which a sealed row keeps under its tenant's Transit key;
//! - the agents' `manifest_json` and `manifest_yaml`, in
//!   `spec.contexts[].service` and the `spec.tools` entries only.
//!
//! `executions.input` and decided approval requests are history and stay as
//! written. The Update's Alternatives reject a plain SQL migration: it would
//! run before the configuration names the server, and a manifest's YAML
//! text needs a parse to check.
//!
//! The rules are the pure functions of this module; the SQL is each
//! repository's `rewrite_former_names*` method, and [`run`] calls them in
//! turn. Every rewrite is idempotent: a row is selected only while it still
//! names a former name, and written only if it still holds what was read,
//! so a second run, or a second process starting at once, rewrites nothing.
//! A row the rules cannot rewrite is left as it was and logged with the
//! reason; step 1 keeps it working.

use std::borrow::Cow;
use std::sync::Arc;

use serde_json::Value;
use sqlx::PgPool;

use crate::domain::agent::AgentManifest;
use crate::domain::node_config::FormerNames;
use crate::domain::repository::RepositoryError;
use crate::infrastructure::secrets_manager::SecretsManager;

use super::postgres_agent::PostgresAgentRepository;
use super::postgres_credential::PostgresCredentialBindingRepository;
use super::postgres_profile::PostgresProfileRepository;
use super::postgres_schedule::PostgresScheduleRepository;
use super::postgres_tool_approval::PostgresToolApprovalRepository;

/// What a rewrite did to one table: the rows it rewrote, and the rows still
/// naming a former name that it left as they were (each logged with its
/// reason when it was left).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TableRewrite {
    pub rewritten: u64,
    pub left: u64,
}

/// One table's outcome, as the daemon logs it.
#[derive(Debug)]
pub struct TableOutcome {
    pub table: &'static str,
    pub result: Result<TableRewrite, RepositoryError>,
}

/// Rewrite every stored former name to its server's own name, table by
/// table, in this order: `credential_bindings`, `schedules`, `profiles`,
/// `tool_approval_policies`, `tool_approval_requests`, `agents`. A table
/// whose rewrite fails does not stop the next. No former name configured:
/// nothing is read and no table is answered.
pub async fn run(
    pool: &PgPool,
    secrets: Arc<SecretsManager>,
    former: &FormerNames,
) -> Vec<TableOutcome> {
    if former.is_empty() {
        return Vec::new();
    }
    let approvals = PostgresToolApprovalRepository::new(pool.clone(), secrets);
    vec![
        TableOutcome {
            table: "credential_bindings",
            result: PostgresCredentialBindingRepository::new(pool.clone())
                .rewrite_former_names(former)
                .await,
        },
        TableOutcome {
            table: "schedules",
            result: PostgresScheduleRepository::new(pool.clone())
                .rewrite_former_names(former)
                .await,
        },
        TableOutcome {
            table: "profiles",
            result: PostgresProfileRepository::new(pool.clone())
                .rewrite_former_names(former)
                .await,
        },
        TableOutcome {
            table: "tool_approval_policies",
            result: approvals.rewrite_former_names_in_policies(former).await,
        },
        TableOutcome {
            table: "tool_approval_requests",
            result: approvals
                .rewrite_former_names_in_pending_requests(former)
                .await,
        },
        TableOutcome {
            table: "agents",
            result: PostgresAgentRepository::new(pool.clone())
                .rewrite_former_names(former)
                .await,
        },
    ]
}

/// A binding's provider by its server's own name; `None` when it names no
/// former name.
pub fn provider(former: &FormerNames, provider: &str) -> Option<String> {
    let current = former.current(provider);
    (current != provider).then(|| current.to_string())
}

/// A tool name (or pattern) `<former>.<rest>` as `<current>.<rest>`; `None`
/// when its server is not a former name.
pub fn tool_name(former: &FormerNames, tool: &str) -> Option<String> {
    match former.tool(tool) {
        Cow::Owned(rewritten) => Some(rewritten),
        Cow::Borrowed(_) => None,
    }
}

/// A list of tool names or patterns with each string's server read by its
/// own name, in order, nothing added or dropped; `None` when no entry names
/// a former name or `tools` is not a list.
pub fn tools(former: &FormerNames, tools: &Value) -> Option<Value> {
    let items = tools.as_array()?;
    let mut changed = false;
    let rewritten: Vec<Value> = items
        .iter()
        .map(
            |item| match item.as_str().and_then(|t| tool_name(former, t)) {
                Some(rewritten) => {
                    changed = true;
                    Value::String(rewritten)
                }
                None => item.clone(),
            },
        )
        .collect();
    changed.then_some(Value::Array(rewritten))
}

/// A contexts map (`{"<server>": <choice>}`) with each key read as its
/// server's own name, by the rule every reader applies
/// ([`FormerNames::contexts`]): a server named under two of its names takes
/// the bindings both choose. `Ok(None)`: no key is a former name, or the
/// value is not a map. `Err`: the map chooses for one server twice and one
/// of the two chooses no binding, the readers' refusal sentence; the row is
/// left as it was.
pub fn contexts(former: &FormerNames, contexts: &Value) -> Result<Option<Value>, String> {
    let Some(map) = contexts.as_object() else {
        return Ok(None);
    };
    if !map.keys().any(|key| former.current(key) != key.as_str()) {
        return Ok(None);
    }
    former
        .contexts(map.clone())
        .map(|map| Some(Value::Object(map)))
}

/// A stored manifest's JSON with each `spec.tools` entry and each
/// `spec.contexts[].service` read by its server's own name, and nothing else
/// changed; `None` when neither names a former name.
pub fn manifest_json(former: &FormerNames, manifest: &Value) -> Option<Value> {
    let mut rewritten = manifest.clone();
    let mut changed = false;
    if let Some(spec) = rewritten.get_mut("spec").and_then(Value::as_object_mut) {
        if let Some(listed) = spec.get("tools").and_then(|listed| tools(former, listed)) {
            spec.insert("tools".to_string(), listed);
            changed = true;
        }
        if let Some(Value::Array(declared)) = spec.get_mut("contexts") {
            for declaration in declared.iter_mut() {
                if let Some(Value::String(service)) = declaration.get_mut("service") {
                    let current = former.current(service.trim()).to_string();
                    if current != service.trim() {
                        *service = current;
                        changed = true;
                    }
                }
            }
        }
    }
    changed.then_some(rewritten)
}

/// What becomes of a stored manifest.
#[derive(Debug, Clone, PartialEq)]
pub enum ManifestRewrite {
    /// Neither its JSON nor its YAML names a former name in a rewritten
    /// place: nothing to write.
    Unchanged,
    /// The rewritten pair, the YAML parsing to the manifest the rewritten
    /// JSON holds.
    Rewritten { yaml: String, json: Value },
    /// The row is left as it was, for the reason given.
    Left { reason: String },
}

/// A stored manifest's pair rewritten: the JSON by [`manifest_json`], the
/// YAML token by token by [`yaml_tokens`], then the rewritten YAML parsed
/// again. The pair is written only when that parse is the manifest the
/// rewritten JSON holds; when it is not (a former name standing whole as
/// another value of the text, such as an instruction), or either does not
/// read as a manifest, the row is left as it was.
pub fn manifest(former: &FormerNames, yaml: &str, json: &Value) -> ManifestRewrite {
    let rewritten_json = manifest_json(former, json);
    let rewritten_yaml = yaml_tokens(former, yaml);
    if rewritten_json.is_none() && rewritten_yaml == yaml {
        return ManifestRewrite::Unchanged;
    }
    let json = rewritten_json.unwrap_or_else(|| json.clone());
    let from_json: AgentManifest = match serde_json::from_value(json.clone()) {
        Ok(manifest) => manifest,
        Err(e) => {
            return ManifestRewrite::Left {
                reason: format!("the rewritten manifest_json is not a manifest: {e}"),
            }
        }
    };
    let from_yaml: AgentManifest = match serde_yaml::from_str(&rewritten_yaml) {
        Ok(manifest) => manifest,
        Err(e) => {
            return ManifestRewrite::Left {
                reason: format!("the rewritten manifest_yaml does not parse as a manifest: {e}"),
            }
        }
    };
    if from_yaml != from_json {
        return ManifestRewrite::Left {
            reason: "the rewritten manifest_yaml parses to another manifest than the rewritten \
                     manifest_json"
                .to_string(),
        };
    }
    ManifestRewrite::Rewritten {
        yaml: rewritten_yaml,
        json,
    }
}

/// `yaml` with every scalar that is wholly a former name, or a tool or tool
/// pattern of one (`<former>`, `<former>.<rest>`), written with the
/// server's own name. A scalar is whole when it stands alone as a list item
/// (`- x`), a mapping value (`key: x`), a flow item (`[x`, `, x`) or a line
/// of its own, plain or quoted, followed by nothing but its closing quote,
/// spaces, a comment, `,`, `]` or `}`. A key (`x:`) and a name inside a
/// longer text are left as written.
pub fn yaml_tokens(former: &FormerNames, yaml: &str) -> String {
    if former.is_empty() {
        return yaml.to_string();
    }
    yaml.split_inclusive('\n')
        .map(|line| yaml_line(former, line))
        .collect()
}

/// The characters a server name holds.
fn is_server_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
}

/// The characters a tool name or pattern holds after its server.
fn is_tool_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'*')
}

fn yaml_line(former: &FormerNames, line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        let starts_name = is_server_byte(bytes[at]) && (at == 0 || !is_server_byte(bytes[at - 1]));
        if starts_name {
            if let Some((name_end, token_end, current)) = whole_scalar_at(former, line, at) {
                out.push_str(&line[copied..at]);
                out.push_str(current);
                copied = name_end;
                at = token_end;
                continue;
            }
        }
        at += 1;
    }
    out.push_str(&line[copied..]);
    out
}

/// The former name starting at `start` when it begins a whole scalar:
/// where the name ends, where the scalar ends, and the server's own name.
fn whole_scalar_at<'a>(
    former: &'a FormerNames,
    line: &'a str,
    start: usize,
) -> Option<(usize, usize, &'a str)> {
    let bytes = line.as_bytes();
    let quote = match start.checked_sub(1).map(|i| bytes[i]) {
        Some(q @ (b'"' | b'\'')) => Some(q),
        _ => None,
    };
    let head = &line[..start - usize::from(quote.is_some())];
    let trimmed = head.trim_end_matches([' ', '\t']);
    let spaced = trimmed.len() < head.len();
    let opens = trimmed.trim_start().is_empty()
        || (spaced
            && trimmed.ends_with('-')
            && trimmed[..trimmed.len() - 1]
                .chars()
                .last()
                .is_none_or(char::is_whitespace))
        || (spaced && trimmed.ends_with(':'))
        || trimmed.ends_with('[')
        || trimmed.ends_with(',');
    if !opens {
        return None;
    }
    let mut end = start;
    while end < bytes.len() && is_server_byte(bytes[end]) {
        end += 1;
    }
    let name_end = end;
    let current = former.current(&line[start..name_end]);
    if current == &line[start..name_end] {
        return None;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        end += 1;
        let rest = end;
        while end < bytes.len() && is_tool_byte(bytes[end]) {
            end += 1;
        }
        if end == rest {
            return None;
        }
    }
    if let Some(q) = quote {
        if end >= bytes.len() || bytes[end] != q {
            return None;
        }
        end += 1;
    }
    let after = &line[end..];
    let rest = after.trim_start_matches([' ', '\t']);
    let closes = rest.is_empty()
        || rest.starts_with('\n')
        || rest.starts_with("\r\n")
        || (rest.starts_with('#') && rest.len() < after.len())
        || rest.starts_with(',')
        || rest.starts_with(']')
        || rest.starts_with('}');
    closes.then_some((name_end, end, current))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::node_config::SealGatewayConfig;
    use serde_json::json;

    /// The production configuration since the flip:
    /// `{name: carbon-forge, former_names: [nuclear-notes]}`.
    fn renamed() -> FormerNames {
        let config: SealGatewayConfig = serde_yaml::from_str(
            "url: https://aegis-seal-gateway:50055\nremote_servers:\n  - name: carbon-forge\n    \
             former_names: [nuclear-notes]\n  - github\n",
        )
        .expect("the renamed configuration loads");
        config.former_names()
    }

    /// A manifest stored as the agent repository stores one: its YAML and
    /// JSON both serialised from the parsed manifest.
    fn stored(instruction: &str, tools: &[&str], service: Option<&str>) -> (String, Value) {
        let listed: String = tools.iter().map(|t| format!("    - \"{t}\"\n")).collect();
        let contexts = service
            .map(|s| format!("  contexts:\n    - service: {s}\n"))
            .unwrap_or_default();
        let source = format!(
            "apiVersion: 100monkeys.ai/v1\nkind: Agent\nmetadata:\n  name: notes-agent\n  \
             version: \"1.0.0\"\nspec:\n  runtime:\n    language: python\n    version: \
             \"3.11\"\n  task:\n    instruction: \"{instruction}\"\n{contexts}  tools:\n{listed}"
        );
        let manifest: AgentManifest =
            serde_yaml::from_str(&source).expect("the fixture is a manifest");
        (
            serde_yaml::to_string(&manifest).unwrap(),
            serde_json::to_value(&manifest).unwrap(),
        )
    }

    #[test]
    fn a_provider_naming_a_former_name_reads_as_its_server() {
        let former = renamed();
        let mut complaints = Vec::new();
        for (stored, expected) in [
            ("nuclear-notes", Some("carbon-forge")),
            ("carbon-forge", None),
            ("github", None),
            ("nuclear-notes-x", None),
        ] {
            let got = provider(&former, stored);
            if got.as_deref() != expected {
                complaints.push(format!("provider {stored}: {got:?}, not {expected:?}"));
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    #[test]
    fn a_tool_name_and_a_tools_list_read_by_their_servers_own_names() {
        let former = renamed();
        let mut complaints = Vec::new();
        for (stored, expected) in [
            ("nuclear-notes.pages.read", Some("carbon-forge.pages.read")),
            ("nuclear-notes.*", Some("carbon-forge.*")),
            ("carbon-forge.pages.read", None),
            ("github.get_me", None),
            ("nuclear-notesx.pages.read", None),
        ] {
            let got = tool_name(&former, stored);
            if got.as_deref() != expected {
                complaints.push(format!("tool {stored}: {got:?}, not {expected:?}"));
            }
        }
        let list = json!(["nuclear-notes.*", "mail.reply", "nuclear-notes.pages.read"]);
        let got = tools(&former, &list);
        let expected = json!(["carbon-forge.*", "mail.reply", "carbon-forge.pages.read"]);
        if got.as_ref() != Some(&expected) {
            complaints.push(format!("tools {list}: {got:?}, not {expected}"));
        }
        let unrelated = json!(["github.*", "mail.reply"]);
        if let Some(got) = tools(&former, &unrelated) {
            complaints.push(format!("tools {unrelated} were rewritten to {got}"));
        }
        let again = tools(&former, &expected);
        if again.is_some() {
            complaints.push(format!(
                "a second application changed {expected}: {again:?}"
            ));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    #[test]
    fn a_contexts_map_keyed_by_a_former_name_is_keyed_by_its_server() {
        let former = renamed();
        let mut complaints = Vec::new();
        let stored = json!({"nuclear-notes": ["b1"], "github": ["b2"]});
        let got = contexts(&former, &stored);
        let expected = json!({"carbon-forge": ["b1"], "github": ["b2"]});
        if got != Ok(Some(expected.clone())) {
            complaints.push(format!("contexts {stored}: {got:?}, not {expected}"));
        }
        if contexts(&former, &expected) != Ok(None) {
            complaints.push(format!("a second application changed {expected}"));
        }
        let both = json!({"carbon-forge": ["b1"], "nuclear-notes": ["b3"]});
        let got = contexts(&former, &both);
        if got != Ok(Some(json!({"carbon-forge": ["b1", "b3"]}))) {
            complaints.push(format!("contexts naming the server twice: {got:?}"));
        }
        let refused = json!({"carbon-forge": null, "nuclear-notes": ["b3"]});
        if contexts(&former, &refused).is_ok() {
            complaints.push(format!("contexts {refused} were rewritten, not left"));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    #[test]
    fn a_manifest_names_its_server_by_its_own_name_in_json_and_yaml() {
        let former = renamed();
        let (yaml, json) = stored(
            "Read the pages it names; nuclear-notes.pages.read is the tool.",
            &[
                "nuclear-notes.pages.read",
                "nuclear-notes.*",
                "aegis.tools.list",
            ],
            Some("nuclear-notes"),
        );
        let (expected_yaml, expected_json) = stored(
            "Read the pages it names; nuclear-notes.pages.read is the tool.",
            &[
                "carbon-forge.pages.read",
                "carbon-forge.*",
                "aegis.tools.list",
            ],
            Some("carbon-forge"),
        );
        match manifest(&former, &yaml, &json) {
            ManifestRewrite::Rewritten {
                yaml: got_yaml,
                json: got_json,
            } => {
                let mut complaints = Vec::new();
                if got_json != expected_json {
                    complaints.push(format!("manifest_json: {got_json}, not {expected_json}"));
                }
                if got_yaml != expected_yaml {
                    complaints.push(format!("manifest_yaml:\n{got_yaml}\nnot\n{expected_yaml}"));
                }
                let again = manifest(&former, &got_yaml, &got_json);
                if again != ManifestRewrite::Unchanged {
                    complaints.push(format!("a second application answered {again:?}"));
                }
                assert!(complaints.is_empty(), "{}", complaints.join("\n"));
            }
            other => panic!("the manifest was not rewritten: {other:?}"),
        }
        let (yaml, json) = stored("Answer.", &["aegis.tools.list"], None);
        assert_eq!(
            manifest(&former, &yaml, &json),
            ManifestRewrite::Unchanged,
            "a manifest naming no former name was changed"
        );
    }

    #[test]
    fn a_manifest_whose_yaml_parses_differently_is_left_and_reported() {
        let former = renamed();
        // The instruction is the whole scalar `nuclear-notes`: the YAML's
        // token is rewritten with the tools, and the JSON's instruction is
        // not, so the rewritten YAML parses to another manifest.
        let (yaml, json) = stored("nuclear-notes", &["nuclear-notes.pages.read"], None);
        match manifest(&former, &yaml, &json) {
            ManifestRewrite::Left { reason } => assert!(
                reason.contains("parses to another manifest"),
                "the reason does not say the parse differs: {reason}"
            ),
            other => panic!("a differing parse was not left: {other:?}"),
        }
        let broken = yaml.replace("apiVersion", "apiVersion: [");
        assert!(
            matches!(
                manifest(&former, &broken, &json),
                ManifestRewrite::Left { .. }
            ),
            "a YAML that does not parse was rewritten"
        );
    }

    #[test]
    fn the_yaml_rewrite_takes_whole_scalars_only() {
        let former = renamed();
        let mut complaints = Vec::new();
        for (line, expected) in [
            (
                "  - nuclear-notes.pages.read\n",
                "  - carbon-forge.pages.read\n",
            ),
            ("  - \"nuclear-notes.*\"\n", "  - \"carbon-forge.*\"\n"),
            (
                "    service: nuclear-notes\n",
                "    service: carbon-forge\n",
            ),
            (
                "    service: 'nuclear-notes' # kept\n",
                "    service: 'carbon-forge' # kept\n",
            ),
            (
                "  tools: [nuclear-notes.a, x, nuclear-notes.b]\n",
                "  tools: [carbon-forge.a, x, carbon-forge.b]\n",
            ),
            ("  nuclear-notes: 1\n", "  nuclear-notes: 1\n"),
            (
                "  note: uses nuclear-notes daily\n",
                "  note: uses nuclear-notes daily\n",
            ),
            ("  - nuclear-notes-x.read\n", "  - nuclear-notes-x.read\n"),
            ("  - my-nuclear-notes\n", "  - my-nuclear-notes\n"),
        ] {
            let got = yaml_tokens(&former, line);
            if got != expected {
                complaints.push(format!("{line:?} became {got:?}, not {expected:?}"));
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }
}
