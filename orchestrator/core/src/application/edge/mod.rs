// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Edge Mode Application Services (ADR-117)
//!
//! Use-case-oriented application layer for the AEGIS Edge Daemon. Each
//! submodule corresponds to one operator-facing or daemon-facing capability:
//!
//! | Module | Purpose |
//! | --- | --- |
//! | [`issue_enrollment_token`] | Mint a short-lived enrollment JWT. |
//! | [`enroll_edge`]            | Validate JWT, redeem `jti`, persist EdgeDaemon. |
//! | [`connect_edge`]           | Drive the bidi gRPC stream once `Hello` arrives. |
//! | [`dispatch_to_edge`]       | Single-target reverse-RPC tool dispatch. |
//! | [`revoke_edge`]            | Operator-initiated revocation. |
//! | [`manage_tags`]            | Operator-managed tag mutation. |
//! | [`manage_groups`]          | Edge group CRUD with tenant scope. |
//! | [`rotate_edge_key`]        | Atomic dual-signature key rotation. |
//! | [`fleet`]                  | Multi-target fan-out (resolver/dispatcher/registry). |

pub mod connect_edge;
pub mod dispatch_to_edge;
pub mod enroll_edge;
pub mod fleet;
pub mod issue_enrollment_token;
pub mod manage_groups;
pub mod manage_tags;
pub mod revoke_edge;
pub mod rotate_edge_key;
pub mod transit;

/// Convert a `serde_json::Value` into a `prost_types::Struct` (the wire form
/// of `google.protobuf.Struct`).
///
/// `prost_types::Struct` does not implement `serde::Deserialize`, so REST/MCP
/// boundaries that receive JSON-shaped tool args must be translated explicitly.
/// The input MUST be a JSON object — anything else is a malformed payload.
pub fn json_value_to_prost_struct(
    value: serde_json::Value,
) -> Result<prost_types::Struct, &'static str> {
    match value {
        serde_json::Value::Object(map) => {
            let fields = map
                .into_iter()
                .map(|(k, v)| (k, json_value_to_prost_value(v)))
                .collect();
            Ok(prost_types::Struct { fields })
        }
        _ => Err("expected a JSON object"),
    }
}

fn json_value_to_prost_value(value: serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(b),
        serde_json::Value::Number(n) => {
            // Best-effort numeric: protobuf Value is f64.
            Kind::NumberValue(n.as_f64().unwrap_or(0.0))
        }
        serde_json::Value::String(s) => Kind::StringValue(s),
        serde_json::Value::Array(arr) => Kind::ListValue(prost_types::ListValue {
            values: arr.into_iter().map(json_value_to_prost_value).collect(),
        }),
        serde_json::Value::Object(map) => {
            let fields = map
                .into_iter()
                .map(|(k, v)| (k, json_value_to_prost_value(v)))
                .collect();
            Kind::StructValue(prost_types::Struct { fields })
        }
    };
    prost_types::Value { kind: Some(kind) }
}
