// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `<expr>` parser for `aegis edge fleet --target <expr>` and friends.
//!
//! Grammar:
//!   - `@<node-id>`              → EdgeTarget::Node
//!   - `group:<name>`            → EdgeTarget::Group (resolved server-side)
//!   - `tags=a,b labels=k=v tools=docker os=linux arch=x86_64` → Selector
//!   - `all`                     → EdgeTarget::All

use aegis_orchestrator_core::domain::edge::{EdgeSelector, EdgeTarget, LabelMatch, TagMatch};
use aegis_orchestrator_core::domain::shared_kernel::NodeId;
use anyhow::{anyhow, Result};

pub fn parse(expr: &str) -> Result<EdgeTarget> {
    let trimmed = expr.trim();
    if trimmed == "all" {
        return Ok(EdgeTarget::All);
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        let id = NodeId::from_string(rest).map_err(|e| anyhow!("bad node id: {e}"))?;
        return Ok(EdgeTarget::Node(id));
    }
    if let Some(_name) = trimmed.strip_prefix("group:") {
        // Server resolves group names to ids; the CLI looks them up via
        // /v1/edge/groups before calling fleet endpoints. For now expose
        // the raw selector form when the CLI resolves a group locally.
        return Err(anyhow!(
            "group:<name> requires a server lookup; use the resolved id via /v1/edge/groups"
        ));
    }
    let mut sel = EdgeSelector::default();
    for token in trimmed.split_whitespace() {
        let (k, v) = token
            .split_once('=')
            .ok_or_else(|| anyhow!("expected key=value, got {token:?}"))?;
        match k {
            "os" => sel.os = Some(v.to_string()),
            "arch" => sel.arch = Some(v.to_string()),
            "tools" => {
                sel.tools = v.split(',').map(|s| s.trim().to_string()).collect();
            }
            "tags" => {
                for t in v.split(',').map(|s| s.trim().to_string()) {
                    sel.tags.push(TagMatch::Has(t));
                }
            }
            "labels" => {
                for kv in v.split(',') {
                    if let Some((lk, lv)) = kv.split_once('=') {
                        sel.labels
                            .push(LabelMatch::Equals(lk.to_string(), lv.to_string()));
                    } else {
                        sel.labels.push(LabelMatch::Exists(kv.to_string()));
                    }
                }
            }
            other => return Err(anyhow!("unknown selector key {other:?}")),
        }
    }
    Ok(EdgeTarget::Selector(sel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all() {
        assert!(matches!(parse("all").unwrap(), EdgeTarget::All));
    }

    #[test]
    fn parse_selector_tags_labels_tools() {
        let t = parse("tags=prod,db labels=region=us tools=docker os=linux").unwrap();
        match t {
            EdgeTarget::Selector(s) => {
                assert_eq!(s.os.as_deref(), Some("linux"));
                assert_eq!(s.tools, vec!["docker"]);
                assert_eq!(s.tags.len(), 2);
                assert_eq!(s.labels.len(), 1);
            }
            _ => unreachable!("parse(\"tags=...\") must yield EdgeTarget::Selector"),
        }
    }
}
