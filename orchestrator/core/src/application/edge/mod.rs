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

/// Convert a `prost_types::Struct` back into a JSON object: an edge tool's
/// structured result, for fleet events and run records.
pub fn prost_struct_to_json(s: &prost_types::Struct) -> serde_json::Value {
    serde_json::Value::Object(
        s.fields
            .iter()
            .map(|(k, v)| (k.clone(), prost_value_to_json(v)))
            .collect(),
    )
}

fn prost_value_to_json(v: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;
    match &v.kind {
        None | Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::NumberValue(n)) => {
            if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
                serde_json::Value::from(*n as i64)
            } else {
                serde_json::Number::from_f64(*n)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
        }
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::StructValue(s)) => prost_struct_to_json(s),
        Some(Kind::ListValue(l)) => {
            serde_json::Value::Array(l.values.iter().map(prost_value_to_json).collect())
        }
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

#[cfg(test)]
mod struct_json_tests {
    #[test]
    fn a_structured_result_round_trips_through_json() {
        let report = serde_json::json!({
            "check": true, "changed": 2, "failed": 0,
            "resources": [{"id": "conf", "kind": "file", "status": "changed", "diff": "created (6 bytes)"}],
            "ratio": 0.5, "none": null
        });
        let s = super::json_value_to_prost_struct(report.clone()).unwrap();
        assert_eq!(super::prost_struct_to_json(&s), report);
    }
}
